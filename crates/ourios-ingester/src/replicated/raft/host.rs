//! Every group a node votes in or is a tail replica of, open and running
//! (RFC 0065 §6.1): one `Raft` per group over its group log, all served by
//! one [`ReplicationServer`] and reaching their peers through one
//! [`Peers`] pool. On a tail replica each group's [`GroupMiner`] mines the
//! entries its state machine applies (§6.5).

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, Weak};
use std::time::Duration;

use openraft::async_runtime::WatchReceiver as _;
use openraft::errors::Fatal;
use openraft::storage::RaftLogStorage as _;
use openraft::type_config::alias::WatchReceiverOf;
use openraft::{Raft, RaftMetrics};
use ourios_wal::WalConfig;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use super::checkpointer::Checkpointer;
use super::flushing::{self, Current, Flushing, Need, Publishing, Source};
use super::gate::SyncGate;
use ourios_core::tenant::TenantId;

use lookup::{Asker, Lookup};
use tailed::FormedLog;
pub use tailed::Tailed;

use super::kept::{Bounds, Definitions, Kept, TemplateKey};
use super::log_store::{Disk, GroupLog, GroupLogReader, group_dir};
use super::mining::{GroupMiner, GroupMinerError, Mined, SharedLog};
use super::proposer::Proposer;
use super::proposing::Proposing;
use super::reporting::Reporting;
use super::restoring::Restoring;
use super::state_machine::{Applied, AppliedEvent, GroupSm};
use super::telemetry::{KeptDrops, Meters, ReplicationTelemetry, Storage};
use super::transport::{Peers, ReplicationServer};
use super::types::{LogId, NodeId, RaftTypes};
use super::vote_log::VoteLog;
use super::{RaftTimings, group_config};
use crate::replicated::write::COMMIT_TIMEOUT;

pub type GroupRaft = Raft<RaftTypes, GroupSm>;

/// What a node is to the groups it hosts (RFC 0065 §3.8): it votes in
/// them or is a tail replica of them, never both.
#[derive(Debug, Clone)]
pub enum HostRole {
    /// Votes in its groups and mines none of them: the leader acknowledges
    /// and replicates, and mining never sits on the ack path (§6.5).
    Voter,
    /// A tail replica of its groups: mines each one's entries as its state
    /// machine applies them, keeps the output for a takeover within
    /// `kept` (§6.7), and hands it to `mined`, in log order per group,
    /// waiting while the channel is full. Opening the host replays the
    /// committed log through the miner, so `mined` must be read while it
    /// opens. With `publishing`, a group whose latest flusher assignment
    /// names this replica is also published from it (§6.5, §6.7).
    Tail {
        mined: mpsc::Sender<GroupMined>,
        kept: Bounds,
        publishing: Option<Arc<Publishing>>,
    },
}

/// What a tail replica's miner mined from one of `group`'s entries.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupMined {
    pub group: u32,
    pub mined: Mined,
}

/// What a node hosts.
pub struct HostSpec {
    /// The replica's Raft node id.
    pub id: NodeId,
    /// The node's WAL settings, `root` being its `wal_root`; each group's
    /// WAL takes them with its root at [`group_dir`].
    pub wal: WalConfig,
    /// The groups the node votes in
    /// ([`VotingLayout::groups_of`](crate::replicated::layout::VotingLayout::groups_of))
    /// or, as a tail replica, is placed in.
    pub groups: Vec<u32>,
    pub role: HostRole,
    pub peers: Peers,
    /// The Raft timings every group runs on.
    pub timings: RaftTimings,
}

/// Why a node's groups could not be opened.
#[derive(Debug)]
pub enum HostError {
    Config(openraft::ConfigError),
    Votes(io::Error),
    Log {
        group: u32,
        source: io::Error,
    },
    Snapshot {
        group: u32,
        source: io::Error,
    },
    Raft {
        group: u32,
        source: Fatal<RaftTypes>,
    },
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(e) => write!(f, "the group Raft settings: {e}"),
            Self::Votes(e) => write!(f, "open the vote log: {e}"),
            Self::Log { group, source } => write!(f, "open group {group}'s log: {source}"),
            Self::Snapshot { group, source } => {
                write!(f, "read group {group}'s snapshot record: {source}")
            }
            Self::Raft { group, source } => write!(f, "start group {group}: {source}"),
        }
    }
}

impl std::error::Error for HostError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Config(e) => Some(e),
            Self::Votes(e) | Self::Log { source: e, .. } | Self::Snapshot { source: e, .. } => {
                Some(e)
            }
            Self::Raft { source, .. } => Some(source),
        }
    }
}

struct Hosted {
    raft: GroupRaft,
    sm: GroupSm,
    log: GroupLogReader,
    /// The task feeding a tail replica's miner.
    mining: Option<Mining>,
    #[cfg(test)]
    probe: probe::Probe,
}

/// What opening each of a node's groups shares.
struct Launch<'a> {
    spec: &'a HostSpec,
    votes: VoteLog,
    gate: Arc<SyncGate>,
    config: Arc<openraft::Config>,
    meters: &'a Meters,
}

impl Launch<'_> {
    /// Where a tail replica's miner for `group` hands its output and keeps
    /// it within `bounds`, its gauges registered.
    fn follow(
        &self,
        group: u32,
        mined: mpsc::Sender<GroupMined>,
        (tail, sm): (&Tail, &GroupSm),
    ) -> Follow {
        let kept = Arc::new(Mutex::new(Kept::new(tail.kept)));
        self.meters.keep(group, &kept);
        let publishing = tail.publishing.as_ref();
        let flushing = publishing.and_then(|publishing| {
            let source = publishing.source(group)?;
            let log: SharedLog = Arc::new(tail.log.clone());
            let proposing = Proposing {
                group,
                proposer: Proposer::new(
                    self.spec.peers.clone(),
                    group,
                    source.voters.clone(),
                    COMMIT_TIMEOUT,
                ),
                replica: self.spec.id,
                raft: Arc::clone(&tail.raft),
                log: tail.log.clone(),
                sm: sm.clone(),
                refused: Arc::default(),
            };
            let reporting = Reporting::new(proposing.clone());
            let checkpointer = Checkpointer::new(
                group,
                publishing.barrier(),
                source.store.clone(),
                self.meters.checkpoints(),
            )
            .proposing(proposing);
            let flushing =
                Flushing::new(self.spec.id, group, source, log).with_replays(self.meters.replays());
            Some((flushing, publishing.sweep(), (checkpointer, reporting)))
        });
        let restoring = publishing
            .and_then(|publishing| publishing.source(group))
            .map(|source| reinstall::restoring(group, source, tail, sm));
        let sweep = flushing.as_ref().map(|(f, every, _)| (f.current(), *every));
        let (flushing, flusher) = flushing.map(|(f, _, c)| (f, c)).unzip();
        let (checkpointer, reporting) = flusher.unzip();
        Follow {
            group,
            mined,
            followed: watch::Sender::new(None),
            keeping: Keeping {
                group,
                kept,
                drops: self.meters.kept(),
                flushing: flushing.map(|f| Arc::new(Mutex::new(f))),
            },
            sweep,
            storage: self.meters.storage(),
            checkpointer,
            reporting,
            restoring,
            lookups: None,
        }
    }

    async fn group(&self, group: u32) -> Result<Hosted, HostError> {
        let dir = group_dir(&self.spec.wal.root, group);
        let wal = WalConfig {
            root: dir.clone(),
            ..self.spec.wal.clone()
        };
        let (votes, gate) = (self.votes.clone(), Arc::clone(&self.gate));
        let disk = Disk {
            gate,
            storage: self.meters.storage(),
        };
        let mut log = blocking(move || GroupLog::open_with(wal, group, votes, disk))
            .await
            .map_err(|source| HostError::Log { group, source })?;
        let reader = log.get_log_reader().await;
        #[cfg(test)]
        let probe = probe::Probe::of(&log);
        log.count_takeovers(self.meters.takeovers(self.spec.id, group));
        let gate = Arc::clone(&self.gate);
        let cell: Arc<OnceLock<GroupRaft>> = Arc::default();
        let (hold, builds_held) = watch::channel(false);
        let (sm, applied) = match &self.spec.role {
            HostRole::Voter => (blocking(move || GroupSm::open(&dir, gate)).await, None),
            HostRole::Tail {
                mined,
                kept,
                publishing,
            } => {
                let (events, applied) = mpsc::channel(APPLIED_CAPACITY);
                let sm = blocking(move || GroupSm::open_mining(&dir, gate, events)).await;
                let tail = Tail {
                    kept: *kept,
                    publishing: publishing.clone(),
                    log: reader.clone(),
                    raft: Arc::clone(&cell),
                    hold: Arc::new(hold),
                };
                (sm, Some((applied, mined.clone(), tail)))
            }
        };
        let mut sm = sm.map_err(|source| HostError::Snapshot { group, source })?;
        sm.count_stops(group, self.meters.storage());
        // `Raft::new` applies the committed entries the log replays before
        // it returns, so the miner must already be taking them.
        let mining = applied.map(|(applied, mined, tail)| {
            let follow = self.follow(group, mined, (&tail, &sm));
            Mining::start(follow, GroupMiner::new(reader.clone()), applied)
        });
        let (id, network) = (self.spec.id, self.spec.peers.network(self.spec.id, group));
        let opened = Raft::new(id, Arc::clone(&self.config), network, log, sm.clone()).await;
        let raft = match opened {
            Ok(raft) => raft,
            Err(source) => {
                if let Some(mining) = &mining {
                    mining.stop().await;
                }
                return Err(HostError::Raft { group, source });
            }
        };
        // Set once: this is the group's only open.
        let _ = cell.set(raft.clone());
        build_at_checkpoints(&raft, &sm, builds_held);
        Ok(Hosted {
            raft,
            sm,
            log: reader,
            mining,
            #[cfg(test)]
            probe,
        })
    }
}

/// Build `raft`'s snapshot at each checkpoint `sm` applies, while `held`
/// does not say a restore holds them.
fn build_at_checkpoints(raft: &GroupRaft, sm: &GroupSm, held: watch::Receiver<bool>) {
    let builds = RaftBuilds {
        raft: raft.clone(),
        metrics: raft.metrics(),
    };
    tokio::spawn(snapshot_at_checkpoints(builds, sm.checkpoints(), held));
}

/// How many applied entries a tail replica's state machine hands its
/// miner ahead of what it has mined, before applying waits for it.
const APPLIED_CAPACITY: usize = 64;

/// A tail replica's running miner.
struct Mining {
    stop: watch::Sender<bool>,
    /// How far the miner has followed the log.
    followed: watch::Receiver<Option<u64>>,
    /// Cleared only once the task has ended, so every stop, a concurrent
    /// one or one retried after a cancelled one, waits for it.
    task: tokio::sync::Mutex<Option<JoinHandle<()>>>,
    /// The group's age sweep, when it can be published from this replica;
    /// it stops with the miner.
    sweep: tokio::sync::Mutex<Option<JoinHandle<()>>>,
    /// What the miner keeps, which tail reads page through while its task
    /// runs; held weakly, as the kept gauges hold it, so an ended task's
    /// output is neither reported nor served.
    kept: Weak<Mutex<Kept>>,
    /// Where tail reads ask the miner for template definitions.
    asker: Asker,
    /// The group's log id, which served rows' positions carry.
    formed: Arc<FormedLog>,
}

impl Mining {
    fn start(mut follow: Follow, miner: GroupMiner, applied: mpsc::Receiver<Applied>) -> Self {
        let (stop, stopped) = watch::channel(false);
        let (asker, lookups) = lookup::channel();
        follow.lookups = Some(lookups);
        let kept = Arc::downgrade(&follow.keeping.kept);
        let formed = Arc::new(FormedLog::new(follow.source(), follow.group));
        let followed = follow.followed.subscribe();
        let sweep = follow
            .sweep
            .clone()
            .map(|(current, every)| tokio::spawn(flushing::sweep(current, every, stopped.clone())));
        let task = tokio::spawn(follow.run(miner, applied, stopped));
        Self {
            stop,
            followed,
            task: tokio::sync::Mutex::new(Some(task)),
            sweep: tokio::sync::Mutex::new(sweep),
            kept,
            asker,
            formed,
        }
    }

    #[cfg(test)]
    fn read_kept<T>(&self, read: impl FnOnce(&Kept) -> T) -> Option<T> {
        tailed::read_kept(&self.kept, &self.followed, read)
    }

    #[cfg(test)]
    fn is_running(&self) -> bool {
        self.task
            .try_lock()
            .map_or(true, |task| task.as_ref().is_some_and(|t| !t.is_finished()))
    }

    /// Stop it and wait until it has: a step already mining runs to its
    /// end, so nothing the miner holds, the group's log reader among
    /// them, outlives the stop.
    async fn stop(&self) {
        self.stop.send_replace(true);
        for task in [&self.task, &self.sweep] {
            let mut task = task.lock().await;
            if let Some(running) = task.as_mut() {
                // A task that panicked has ended too.
                running.await.ok();
            }
            *task = None;
        }
    }
}

/// The next applied event, or `None` once the group has ended or the host
/// stops the miner.
pub(super) async fn next(
    applied: &mut mpsc::Receiver<Applied>,
    stop: &mut watch::Receiver<bool>,
) -> Option<Applied> {
    tokio::select! {
        biased;
        () = stopped(stop) => None,
        event = applied.recv() => event,
    }
}

/// What the miner's task does next.
/// How following an event left the miner.
enum Followed {
    On(GroupMiner),
    /// The log no longer held the entry at `index`, which the miner still
    /// had to mine under the first configuration at `at`.
    Purged {
        at: LogId,
        index: u64,
    },
}

enum Work {
    Event(Applied),
    /// The checkpoint barrier's tick.
    Barrier,
    /// The report tick, which forces a report, so one goes out at least
    /// every patience; reports are checked after every piece of work.
    Report,
    /// A tail read's question to the miner.
    Lookup(Lookup),
    /// The publishing tick, which takes an applied entry's publishing step
    /// with no output, for a group gone quiet.
    Drive,
}

/// A tick every `every`, delayed rather than bunched after a slow step.
fn interval(every: Duration) -> tokio::time::Interval {
    let mut interval = tokio::time::interval(every);
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    interval
}

/// Resolves at `barrier`'s next tick, never without one.
async fn ticked(barrier: &mut Option<tokio::time::Interval>) {
    match barrier {
        Some(barrier) => {
            barrier.tick().await;
        }
        None => std::future::pending().await,
    }
}

/// Resolves once the host stops the miner, or drops the means to.
pub(super) async fn stopped(stop: &mut watch::Receiver<bool>) {
    stop.wait_for(|stopped| *stopped).await.ok();
}

/// Where a tail replica's miner for `group` hands what it mined, and
/// reports its stop.
struct Follow {
    group: u32,
    mined: mpsc::Sender<GroupMined>,
    /// The index of the last entry its miner followed, which a read point
    /// waits on (RFC 0065 §6.7).
    followed: watch::Sender<Option<u64>>,
    keeping: Keeping,
    /// The publication the age sweep drives, and how often, when the group
    /// can be published from this replica.
    sweep: Option<(Current, Duration)>,
    storage: Storage,
    /// The group's checkpoint cadence, when it can be published from this
    /// replica.
    checkpointer: Option<Checkpointer>,
    /// The flusher's reports to its leader, when it can be published from
    /// this replica.
    reporting: Option<Reporting>,
    /// Where the miner restores from at a snapshot, when this replica
    /// reads the group's store; without one, a snapshot stops the miner.
    restoring: Option<Restoring<SharedLog>>,
    /// Tail reads' questions to the miner, once it runs.
    lookups: Option<mpsc::Receiver<Lookup>>,
}

/// What a tail replica's settings say of its groups' output.
struct Tail {
    kept: Bounds,
    publishing: Option<Arc<Publishing>>,
    /// The group's log, which a flusher replays a gap from.
    log: GroupLogReader,
    /// This replica's raft once it runs, which a flusher learns its leader
    /// from.
    raft: Arc<OnceLock<GroupRaft>>,
    /// Held while its miner restores, which holds its snapshot builds.
    hold: Arc<watch::Sender<bool>>,
}

/// What a tail replica keeps of `group`'s mined output for a takeover,
/// and where it counts what it drops.
#[derive(Clone)]
struct Keeping {
    group: u32,
    kept: Arc<Mutex<Kept>>,
    drops: KeptDrops,
    flushing: Option<Arc<Mutex<Flushing>>>,
}

impl Keeping {
    /// Keep what the entry at `index` mined, then drop what an applied
    /// checkpoint's `cut` passes; then publish, as the replica `assigned`
    /// by a flusher assignment at this entry allows.
    fn keep(&self, index: u64, out: &Mined, applied: Applying) {
        let mut kept = self.kept.lock().unwrap_or_else(PoisonError::into_inner);
        let before = kept.dropped();
        if !(out.audit.is_empty() && out.rows.is_empty()) {
            kept.keep(index, out.clone());
        }
        if let Some(cut) = applied.cut {
            kept.checkpoint(cut);
        }
        self.drops.dropped(self.group, kept.dropped(), before);
        if let Some(flushing) = &self.flushing {
            let mut flushing = flushing.lock().unwrap_or_else(PoisonError::into_inner);
            self.publish(&mut flushing, kept, (applied.assigned, out));
        }
    }

    /// Take the step an applied entry would, with no output of its own: a
    /// named flusher of an idle group builds its publication once the log
    /// id reads, then publishes or replays what it keeps (#1301).
    fn drive(&self) {
        let Some(flushing) = &self.flushing else {
            return;
        };
        // In `keep`'s order: what it keeps, then the publication.
        let kept = self.kept.lock().unwrap_or_else(PoisonError::into_inner);
        let mut flushing = flushing.lock().unwrap_or_else(PoisonError::into_inner);
        self.publish(&mut flushing, kept, (None, &Mined::default()));
    }

    /// Publish as `flushing` plans from `kept`, holding `kept` only while
    /// planning, and drop from it what a replay rebuilt.
    fn publish<'k>(
        &'k self,
        flushing: &mut Flushing,
        mut kept: MutexGuard<'k, Kept>,
        (mut assigned, out): (Option<NodeId>, &Mined),
    ) {
        loop {
            let need = flushing.plan(assigned.take(), &kept);
            // Publishing does store I/O, and the kept gauges read `kept`.
            drop(kept);
            let need = match need {
                Need::Prepare if flushing.prepare() => {
                    let kept = self.kept.lock().unwrap_or_else(PoisonError::into_inner);
                    flushing.plan(None, &kept)
                }
                Need::Prepare => return,
                need => need,
            };
            let Some(through) = flushing.publish(need, out) else {
                return;
            };
            kept = self.kept.lock().unwrap_or_else(PoisonError::into_inner);
            kept.replayed(through);
        }
    }
}

/// What an applied event changes besides the miner's output.
#[derive(Clone, Copy, Default)]
struct Applying {
    /// The cut of a checkpoint applied at it.
    cut: Option<u64>,
    /// The replica a flusher assignment applied at it names.
    assigned: Option<NodeId>,
}

impl Applying {
    fn of(event: &AppliedEvent) -> Self {
        match event {
            AppliedEvent::Checkpoint(cut) => Self {
                cut: Some(cut.index),
                ..Self::default()
            },
            AppliedEvent::Flusher(replica) => Self {
                assigned: Some(*replica),
                ..Self::default()
            },
            _ => Self::default(),
        }
    }
}

impl Follow {
    /// Feed `miner` each event `applied` carries, off the runtime's
    /// workers, and hand on whatever an event mined, the batches a first
    /// configuration held one at a time. A miner that stops is reported
    /// and dropped with `applied`, and the replica applies on without it.
    /// Ends with the group or once `stop` says so, never in the middle of
    /// a step.
    async fn run(
        mut self,
        mut miner: GroupMiner,
        mut applied: mpsc::Receiver<Applied>,
        mut stop: watch::Receiver<bool>,
    ) {
        let mut checkpointer = self.checkpointer.take();
        let mut reporting = self.reporting.take();
        let mut barrier = checkpointer.as_ref().map(|c| interval(c.every()));
        let mut reports = reporting.as_ref().map(|_| interval(Reporting::every()));
        // A group that can be published from here takes its publishing step
        // on this task's tick too, so a quiet group still publishes (#1301).
        let mut drives = self
            .keeping
            .flushing
            .as_ref()
            .and(self.sweep.as_ref())
            .map(|(_, every)| interval(*every));
        let mut lookups = self.lookups.take();
        self.followed_through(&miner);
        loop {
            // Unbiased, so a queue that is never empty does not starve the
            // ticks; `next` still puts a stop before another event.
            let work = tokio::select! {
                event = next(&mut applied, &mut stop) => event.map(Work::Event),
                () = ticked(&mut barrier) => Some(Work::Barrier),
                () = ticked(&mut reports) => Some(Work::Report),
                () = ticked(&mut drives) => Some(Work::Drive),
                lookup = lookup::asked(&mut lookups) => Some(Work::Lookup(lookup)),
            };
            let Some(work) = work else {
                return;
            };
            match self.installing(&work, &miner) {
                Some(Install::At(at, state)) => {
                    let installed = (at, state);
                    let Some(restored) = self.reinstall(installed, &mut applied, &mut stop).await
                    else {
                        return;
                    };
                    miner = restored;
                    self.followed_through(&miner);
                    continue;
                }
                Some(Install::Passed) => continue,
                None => {}
            }
            let forced = matches!(work, Work::Report);
            let followed = self
                .work(work, miner, (checkpointer.as_mut(), &mut stop))
                .await;
            let Some(back) = self.resumed(followed, &mut applied, &mut stop).await else {
                return;
            };
            miner = back;
            self.followed_through(&miner);
            if let Some(reporting) = reporting.as_mut() {
                reporting.maybe(&miner, forced);
            }
        }
    }

    /// Where this group's store is, when it can be published from here.
    fn source(&self) -> Option<Source> {
        let flushing = self.keeping.flushing.as_ref()?;
        let flushing = flushing.lock().unwrap_or_else(PoisonError::into_inner);
        Some(flushing.source().clone())
    }

    /// Report how far `miner` has mined the log.
    fn followed_through(&self, miner: &GroupMiner) {
        self.followed.send_replace(miner.mined_through());
    }

    /// Do one piece of `work` with `miner`; `None` once the miner has
    /// stopped or the host says to.
    async fn work(
        &self,
        work: Work,
        miner: GroupMiner,
        (checkpointer, stop): (Option<&mut Checkpointer>, &mut watch::Receiver<bool>),
    ) -> Option<Followed> {
        let back = match (work, checkpointer) {
            (Work::Event(event), _) => return self.follow(miner, event, stop).await,
            (Work::Barrier, Some(checkpointer)) => self.checkpoint(checkpointer, miner).await,
            // Off the runtime's workers, as a step is: an answer formats
            // templates.
            (Work::Lookup(lookup), _) => {
                let kept = Arc::clone(&self.keeping.kept);
                tokio::task::spawn_blocking(move || {
                    lookup.answer(&miner, &kept);
                    miner
                })
                .await
                .ok()
            }
            (Work::Drive, _) => {
                // On this task, as an applied entry's step is, so it takes
                // `Kept` and the publication in the same order as `keep`
                // and never alongside it.
                let keeping = self.keeping.clone();
                tokio::task::spawn_blocking(move || {
                    keeping.drive();
                    miner
                })
                .await
                .ok()
            }
            (Work::Barrier | Work::Report, _) => Some(miner),
        };
        back.map(Followed::On)
    }

    /// Follow `event`, then mine the batches a first configuration holds,
    /// handing on each output. `None` once the miner has stopped or the
    /// host says to.
    async fn follow(
        &self,
        mut miner: GroupMiner,
        event: Applied,
        stop: &mut watch::Receiver<bool>,
    ) -> Option<Followed> {
        let index = event.at.index;
        let mut work = Some(event);
        while work.is_some() || miner.holds() {
            let (back, followed) = self.mine(miner, index, work.take()).await?;
            miner = back;
            let handed_on = match followed {
                None => break,
                Some(Ok(out)) => self.hand_on(out, stop).await,
                // A replica that restores takes the miner back from the
                // snapshot whose purge took the entry (#1305).
                Some(Err(GroupMinerError::Purged { at, index })) if self.restoring.is_some() => {
                    return Some(Followed::Purged { at, index });
                }
                Some(Err(error)) => {
                    self.storage.miner_stopped(self.group, &error);
                    false
                }
            };
            if !handed_on || *stop.borrow() {
                return None;
            }
        }
        Some(Followed::On(miner))
    }

    /// One step of `miner` off the runtime's workers: follow `event`, or
    /// with none mine the next held batch, keeping its output under
    /// `index`. `None` if the miner panicked; the panic is reported where
    /// it happened.
    async fn mine(
        &self,
        mut miner: GroupMiner,
        index: u64,
        event: Option<Applied>,
    ) -> Option<(GroupMiner, Option<Result<Mined, GroupMinerError>>)> {
        let keeping = self.keeping.clone();
        let mining = tokio::task::spawn_blocking(move || {
            let (followed, applying) = match event {
                Some(event) => {
                    let applying = Applying::of(&event.event);
                    (Some(miner.step(event)), applying)
                }
                None => (miner.next_held(), Applying::default()),
            };
            if let Some(Ok(out)) = &followed {
                keeping.keep(index, out, applying);
            }
            (miner, followed)
        });
        mining.await.ok()
    }

    /// Cut a checkpoint at `miner`'s last entry, off the runtime's
    /// workers, and start writing it, unless one is still writing or the
    /// cut would not pass the last one written. `None` if the capture
    /// panicked; the panic is reported where it happened.
    async fn checkpoint(
        &self,
        checkpointer: &mut Checkpointer,
        miner: GroupMiner,
    ) -> Option<GroupMiner> {
        let Some(flushing) = self
            .keeping
            .flushing
            .as_ref()
            .filter(|_| checkpointer.ready())
        else {
            return Some(miner);
        };
        let written = checkpointer.written();
        let (target, kept, flushing) = (
            checkpointer.target(),
            Arc::clone(&self.keeping.kept),
            Arc::clone(flushing),
        );
        let capturing = tokio::task::spawn_blocking(move || {
            let cut = target.capture(&miner, (&kept, &flushing), written);
            (miner, cut)
        });
        let (miner, cut) = capturing.await.ok()?;
        if let Some(cut) = cut {
            checkpointer.start(cut);
        }
        Some(miner)
    }

    /// Send `out` on, unless it is empty; `false` once told to stop. With
    /// no one reading, the miner still mines, so a stop is still reported.
    async fn hand_on(&self, out: Mined, stop: &mut watch::Receiver<bool>) -> bool {
        if out.audit.is_empty() && out.rows.is_empty() {
            return true;
        }
        let sent = self.mined.send(GroupMined {
            group: self.group,
            mined: out,
        });
        tokio::select! {
            biased;
            () = stopped(stop) => false,
            _ = sent => true,
        }
    }
}

/// How often a trigger is sent again while the snapshot is behind the cut:
/// `openraft` drops one that arrives while a build runs.
const TRIGGER_RETRY: Duration = Duration::from_millis(200);

/// What [`snapshot_at_checkpoints`] reads and drives of a group's `Raft`.
trait Builds {
    /// The snapshot's last log id.
    fn snapshot(&mut self) -> Option<LogId>;
    /// The snapshot's last log id once it is no longer `seen`, or `None`
    /// once the group has stopped.
    async fn snapshot_after(&mut self, seen: Option<LogId>) -> Option<Option<LogId>>;
    /// Ask for a build; `false` once the group has stopped.
    async fn trigger(&self) -> bool;
}

struct RaftBuilds {
    raft: GroupRaft,
    metrics: WatchReceiverOf<RaftTypes, RaftMetrics<RaftTypes>>,
}

impl Builds for RaftBuilds {
    fn snapshot(&mut self) -> Option<LogId> {
        self.metrics.borrow_and_update().snapshot
    }

    async fn snapshot_after(&mut self, seen: Option<LogId>) -> Option<Option<LogId>> {
        loop {
            self.metrics.changed().await.ok()?;
            let snapshot = self.snapshot();
            if snapshot != seen {
                return Some(snapshot);
            }
        }
    }

    async fn trigger(&self) -> bool {
        self.raft.trigger().snapshot().await.is_ok()
    }
}

/// Build the snapshot at each checkpoint the group applies (§6.5): the
/// snapshot policy is `Never`, so nothing else does. A trigger goes out
/// when the cut advances or a build ends short of it, and then every
/// [`TRIGGER_RETRY`] until the snapshot reaches the cut, never in reply
/// to the trigger itself. Ends with the group.
async fn snapshot_at_checkpoints(
    mut builds: impl Builds,
    mut cuts: watch::Receiver<Option<LogId>>,
    mut held: watch::Receiver<bool>,
) {
    let mut built = builds.snapshot();
    let mut retry = tokio::time::interval(TRIGGER_RETRY);
    retry.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        let due = due(built, *cuts.borrow_and_update(), &mut held);
        if due {
            if !builds.trigger().await {
                return;
            }
            retry.reset();
        }
        tokio::select! {
            changed = cuts.changed() => if changed.is_err() {
                return;
            },
            snapshot = builds.snapshot_after(built) => match snapshot {
                Some(snapshot) => built = snapshot,
                None => return,
            },
            _ = retry.tick(), if due => {}
            // A closed hold holds nothing.
            Ok(()) = held.changed() => {}
        }
    }
}

/// Whether a snapshot is due: the replica has applied a checkpoint past the
/// last one `built`, and no restore holds its builds.
fn due(built: Option<LogId>, cut: Option<LogId>, held: &mut watch::Receiver<bool>) -> bool {
    built < cut && !*held.borrow_and_update()
}

/// The node's running groups.
pub struct GroupHost {
    groups: BTreeMap<u32, Hosted>,
    server: ReplicationServer<GroupSm>,
    /// RFC 0065 §6.9's signals for the groups, once every one has opened.
    telemetry: Option<ReplicationTelemetry>,
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .unwrap_or_else(|e| Err(io::Error::other(e)))
}

/// What a host is opened with besides its spec: the server its groups
/// register with, and the gate their fsyncs pass.
pub(super) struct Wiring {
    pub(super) server: ReplicationServer<GroupSm>,
    pub(super) gate: Arc<SyncGate>,
}

impl GroupHost {
    /// Open every group in `spec`, replaying each log off the runtime's
    /// workers, and start its `Raft`. A group that fails stops the ones
    /// already started.
    ///
    /// # Errors
    ///
    /// [`HostError`] naming the vote log or the group that failed.
    pub async fn open(spec: HostSpec) -> Result<Self, HostError> {
        let meters = Meters::global();
        let wiring = Wiring {
            server: ReplicationServer::default(),
            gate: Arc::default(),
        };
        Self::open_with(spec, wiring, &meters).await
    }

    pub(super) async fn open_with(
        spec: HostSpec,
        Wiring { server, gate }: Wiring,
        meters: &Meters,
    ) -> Result<Self, HostError> {
        let config = Arc::new(group_config(spec.timings).map_err(HostError::Config)?);
        let root = spec.wal.root.clone();
        let storage = meters.storage();
        let votes = blocking(move || VoteLog::open_storing(&root, storage))
            .await
            .map_err(HostError::Votes)?;
        let launch = Launch {
            spec: &spec,
            votes,
            gate,
            config,
            meters,
        };
        let mut host = Self {
            groups: BTreeMap::new(),
            server,
            telemetry: None,
        };
        for &group in &spec.groups {
            match launch.group(group).await {
                Ok(hosted) => {
                    host.server.insert(group, hosted.raft.clone());
                    host.groups.insert(group, hosted);
                }
                Err(e) => {
                    // The open's error is the one to report.
                    let _ = host.shutdown().await;
                    return Err(e);
                }
            }
        }
        let rafts = host.groups.iter().map(|(&g, hosted)| (g, &hosted.raft));
        host.telemetry = Some(meters.watch(rafts));
        Ok(host)
    }

    #[must_use]
    pub fn raft(&self, group: u32) -> Option<&GroupRaft> {
        self.groups.get(&group).map(|hosted| &hosted.raft)
    }

    /// How far a tail replica's miner for `group` has followed its log,
    /// the index of the last entry it followed; `None` where `group` is
    /// not mined here (RFC 0065 §6.7).
    #[must_use]
    pub fn mined_through(&self, group: u32) -> Option<watch::Receiver<Option<u64>>> {
        let mining = self.groups.get(&group)?.mining.as_ref()?;
        Some(mining.followed.clone())
    }

    /// `read` of what a tail replica keeps of `group`'s mined output,
    /// under its lock; `None` where `group` is not mined here or its
    /// miner's task has ended.
    pub fn read_kept<T>(&self, group: u32, read: impl FnOnce(&Kept) -> T) -> Option<T> {
        self.tailed(group)?.read_kept(read)
    }

    /// The definitions of `wanted` for `tenant` that `group`'s tail replica
    /// holds: its miner's and the older versions its kept audit events
    /// bind, both read by its task in one step. `None` where `group` is not
    /// mined here or its miner has stopped.
    pub async fn definitions(
        &self,
        group: u32,
        tenant: &TenantId,
        wanted: BTreeSet<TemplateKey>,
    ) -> Option<Definitions> {
        self.tailed(group)?.definitions(tenant, wanted).await
    }

    /// `group`'s handles for tail reads; `None` where it is not mined here.
    #[must_use]
    pub fn tailed(&self, group: u32) -> Option<Tailed> {
        let hosted = self.groups.get(&group)?;
        let mining = hosted.mining.as_ref()?;
        Some(Tailed {
            raft: hosted.raft.clone(),
            sm: hosted.sm.clone(),
            mined: mining.followed.clone(),
            kept: mining.kept.clone(),
            asker: mining.asker.clone(),
            formed: Arc::clone(&mining.formed),
        })
    }

    #[must_use]
    pub fn state_machine(&self, group: u32) -> Option<&GroupSm> {
        self.groups.get(&group).map(|hosted| &hosted.sm)
    }

    /// A read-only view of `group`'s log.
    #[must_use]
    pub fn log(&self, group: u32) -> Option<&GroupLogReader> {
        self.groups.get(&group).map(|hosted| &hosted.log)
    }

    /// The groups hosted, ascending.
    pub fn groups(&self) -> impl Iterator<Item = u32> + '_ {
        self.groups.keys().copied()
    }

    /// The server for every hosted group; clones share its registry.
    #[must_use]
    pub fn server(&self) -> ReplicationServer<GroupSm> {
        self.server.clone()
    }

    /// Stop every group's `Raft`; their logs' writers finish what they
    /// hold and exit. A group that fails to stop does not keep the others
    /// running.
    ///
    /// # Errors
    ///
    /// A group's core or tick task could not be joined.
    pub async fn shutdown(&self) -> Result<(), openraft::errors::ShutdownError> {
        let mut stopped = Ok(());
        for hosted in self.groups.values() {
            // First, so a miner waiting on unread output cannot hold up
            // the state machine `Raft` waits for.
            if let Some(mining) = &hosted.mining {
                mining.stop().await;
            }
            // `Raft::shutdown` discards the join's result.
            let joined = hosted.raft.try_shutdown().await;
            stopped = stopped.and(joined);
        }
        stopped
    }
}

mod lookup;
mod reinstall;
mod tailed;

use reinstall::Install;

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use openraft::vote::leader_id_std::CommittedLeaderId;
    use ourios_wal::Control;

    use super::super::formation::{Formation, formed_key};
    use super::super::kept::tests::{entries, mined_log};
    use super::super::mining::GroupLogRead;
    use super::super::mining::tests::{
        HeldLog as InMemoryLog, batch_at, block, configured, corpus, logged, time,
    };
    use super::super::publication::held::first_put_held;
    use super::super::snapshot_record::{GroupMinerConfig, IdBlock};
    use super::super::state_machine::AppliedEvent;
    use super::super::telemetry::tests::{Reader, until};
    use super::super::types::{Entry, GroupEntry};
    use super::*;
    use crate::record_sink::FlushConfig;
    use ourios_parquet::Store;
    use ourios_semconv as semconv;

    /// A group whose build never ends, counting the triggers it gets.
    struct Held {
        snapshots: watch::Receiver<Option<LogId>>,
        triggers: Arc<AtomicU64>,
    }

    impl Builds for Held {
        fn snapshot(&mut self) -> Option<LogId> {
            *self.snapshots.borrow_and_update()
        }

        async fn snapshot_after(&mut self, seen: Option<LogId>) -> Option<Option<LogId>> {
            loop {
                self.snapshots.changed().await.ok()?;
                let snapshot = self.snapshot();
                if snapshot != seen {
                    return Some(snapshot);
                }
            }
        }

        async fn trigger(&self) -> bool {
            self.triggers.fetch_add(1, Ordering::Relaxed);
            tokio::task::yield_now().await;
            true
        }
    }

    /// A log that takes `pause` to read an entry, holds none, and records
    /// A log whose read of an entry says it has started, then holds until
    /// the test releases it; it holds none, and records when it is
    /// dropped.
    struct HeldLog {
        started: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
        dropped: Arc<std::sync::atomic::AtomicBool>,
    }

    impl GroupLogRead for HeldLog {
        fn entry(&self, _: u64) -> Option<Entry> {
            let started = self.started.lock().expect("started").take();
            started.map(|started| started.send(()));
            // A dropped sender releases it too.
            let _ = self.release.lock().expect("release").recv();
            None
        }
    }

    impl Drop for HeldLog {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    /// A miner mid-way through reading a batch back from its log, once
    /// the read has started: it holds until `release` sends or drops, and
    /// `dropped` turns true when the miner lets go of the log.
    struct ReadingBack {
        mining: Mining,
        release: std::sync::mpsc::Sender<()>,
        dropped: Arc<std::sync::atomic::AtomicBool>,
    }

    async fn reading_back() -> ReadingBack {
        let (started, read_started) = tokio::sync::oneshot::channel();
        let (release, held) = std::sync::mpsc::channel();
        let dropped: Arc<std::sync::atomic::AtomicBool> = Arc::default();
        let log = HeldLog {
            started: std::sync::Mutex::new(Some(started)),
            release: std::sync::Mutex::new(held),
            dropped: Arc::clone(&dropped),
        };
        let (events, applied) = mpsc::channel(4);
        let (mined, _unread) = mpsc::channel(4);
        let follow = Follow {
            group: 0,
            mined,
            followed: watch::Sender::new(None),
            keeping: Keeping {
                group: 0,
                kept: Arc::default(),
                drops: Meters::global().kept(),
                flushing: None,
            },
            sweep: None,
            storage: Storage::global(),
            checkpointer: None,
            reporting: None,
            restoring: None,
            lookups: None,
        };
        let mining = Mining::start(follow, GroupMiner::new(log), applied);
        let GroupEntry::Batch(batch) = batch_at(time(1), "acme", &["a b c"]) else {
            unreachable!("a batch");
        };
        let GroupEntry::Control(Control::MinerConfig(body)) = configured(0.7) else {
            unreachable!("a configuration");
        };
        let config = GroupMinerConfig::new(body).expect("valid");
        for (index, event) in [
            (1, AppliedEvent::Batch(batch)),
            (2, AppliedEvent::MinerConfig(config)),
        ] {
            let at = log_id(index);
            events.send(Applied { at, event }).await.expect("sent");
        }
        // The configuration reads the batch back from the log.
        read_started.await.expect("the read back started");
        ReadingBack {
            mining,
            release,
            dropped,
        }
    }

    /// How long a held stop is given to return early before the test
    /// takes it as waiting.
    const HELD: Duration = Duration::from_millis(100);

    /// Stopping a miner waits for the step it is running, so nothing it
    /// holds, the group's log reader among them, outlives the stop.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stopping_a_miner_waits_for_its_running_step() {
        let ReadingBack {
            mining,
            release,
            dropped,
        } = reading_back().await;
        let mining = Arc::new(mining);
        let stopping = tokio::spawn({
            let mining = Arc::clone(&mining);
            async move { mining.stop().await }
        });
        tokio::time::sleep(HELD).await;
        assert!(!stopping.is_finished(), "the stop returned mid-step");

        release.send(()).expect("released");
        stopping.await.expect("join");
        assert!(dropped.load(Ordering::SeqCst), "the log outlived the stop");
        assert!(!mining.is_running());
    }

    /// Two stops at once, and a stop retried after one was cancelled,
    /// each return only once the running step has released the log.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn every_stop_waits_for_the_running_step() {
        let ReadingBack {
            mining,
            release,
            dropped,
        } = reading_back().await;
        let mining = Arc::new(mining);
        let stops: Vec<_> = (0..2)
            .map(|_| {
                let (mining, dropped) = (Arc::clone(&mining), Arc::clone(&dropped));
                tokio::spawn(async move {
                    mining.stop().await;
                    dropped.load(Ordering::SeqCst)
                })
            })
            .collect();
        tokio::time::sleep(HELD).await;
        assert!(
            stops.iter().all(|stop| !stop.is_finished()),
            "a stop returned mid-step"
        );
        release.send(()).expect("released");
        for stop in stops {
            assert!(
                stop.await.expect("join"),
                "a stop returned before the log was let go"
            );
        }

        let ReadingBack {
            mining,
            release,
            dropped,
        } = reading_back().await;
        let cancelled = tokio::time::timeout(HELD, mining.stop()).await;
        assert!(cancelled.is_err(), "the first stop returned mid-step");
        let retried = tokio::spawn(async move { mining.stop().await });
        tokio::time::sleep(HELD).await;
        assert!(!retried.is_finished(), "the retried stop returned mid-step");
        release.send(()).expect("released");
        retried.await.expect("join");
        assert!(
            dropped.load(Ordering::SeqCst),
            "the log outlived the retried stop"
        );
    }

    fn log_id(index: u64) -> LogId {
        LogId {
            leader_id: CommittedLeaderId::new(2),
            index,
        }
    }

    /// The event a tail replica's state machine sends for `entry`.
    fn event_of(entry: GroupEntry) -> AppliedEvent {
        match entry {
            GroupEntry::Batch(batch) => AppliedEvent::Batch(batch),
            GroupEntry::Control(Control::MinerConfig(body)) => {
                AppliedEvent::MinerConfig(GroupMinerConfig::new(body).expect("valid"))
            }
            GroupEntry::Control(Control::IdBlock { after, through }) => {
                AppliedEvent::IdBlock(IdBlock::new(after, through).expect("valid"))
            }
            GroupEntry::Control(other) => panic!("not sent here: {other:?}"),
        }
    }

    /// A tail replica's mining task for group 0, keeping within `bounds`.
    struct Following {
        events: mpsc::Sender<Applied>,
        handed: mpsc::Receiver<GroupMined>,
        mining: Mining,
        kept: Arc<Mutex<Kept>>,
    }

    fn following(bounds: Bounds, meters: &Meters) -> Following {
        following_over(bounds, meters, EmptyLog)
    }

    /// As [`following`], over `log`.
    fn following_over(
        bounds: Bounds,
        meters: &Meters,
        log: impl GroupLogRead + 'static,
    ) -> Following {
        let (events, applied) = mpsc::channel(16);
        let (mined, handed) = mpsc::channel(16);
        let kept = Arc::new(Mutex::new(Kept::new(bounds)));
        meters.keep(0, &kept);
        let follow = Follow {
            group: 0,
            mined,
            followed: watch::Sender::new(None),
            keeping: Keeping {
                group: 0,
                kept: Arc::clone(&kept),
                drops: meters.kept(),
                flushing: None,
            },
            sweep: None,
            storage: Storage::global(),
            checkpointer: None,
            reporting: None,
            restoring: None,
            lookups: None,
        };
        let mining = Mining::start(follow, GroupMiner::new(log), applied);
        Following {
            events,
            handed,
            mining,
            kept,
        }
    }

    /// Six batches of five corpus lines at indexes 3 to 8 after a
    /// configuration and an id block, then a checkpoint at 9 cut at 5.
    async fn through_a_cut(events: &mpsc::Sender<Applied>) {
        let lines = corpus();
        let mut entries = vec![configured(0.7), block(0, 10_000)];
        let chunks = (1..).zip(lines.chunks(5).skip(1).take(6));
        entries.extend(chunks.map(|(n, chunk)| batch_at(time(n), "acme", chunk)));
        for (index, entry) in (1..).zip(entries) {
            let event = event_of(entry);
            let at = log_id(index);
            events.send(Applied { at, event }).await.expect("sent");
        }
        let cut = AppliedEvent::Checkpoint(log_id(5));
        let at = log_id(9);
        events.send(Applied { at, event: cut }).await.expect("sent");
    }

    /// What a tail replica keeping within `bounds` kept of
    /// [`through_a_cut`], and everything it handed on.
    async fn follow_through_a_cut(bounds: Bounds, meters: &Meters) -> (Kept, Vec<Mined>) {
        let Following {
            events,
            mut handed,
            mining,
            kept,
        } = following(bounds, meters);
        through_a_cut(&events).await;
        drop(events);
        let mut out = Vec::new();
        while let Some(GroupMined { mined, .. }) = handed.recv().await {
            out.push(mined);
        }
        mining.stop().await;
        let kept = Arc::into_inner(kept).expect("the task let go");
        (kept.into_inner().expect("kept"), out)
    }

    /// RFC 0065 §6.7: a tail replica keeps what its miner mined, hands the
    /// same on, and drops what an applied checkpoint's cut passes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_tail_replica_keeps_its_output_until_a_checkpoint_passes_it() {
        let (kept, out) = follow_through_a_cut(Bounds::default(), &Meters::global()).await;
        let rows: Vec<_> = out.into_iter().flat_map(|mined| mined.rows).collect();
        assert_eq!(rows.len(), 30, "every row handed on");
        let above: Vec<_> = rows
            .into_iter()
            .filter(|row| row.position.seq > 5)
            .collect();
        assert_eq!(kept.rows().cloned().collect::<Vec<_>>(), above);
        assert_eq!(above.len(), 15, "three batches above the cut");
        assert_eq!(kept.gap(), None);
    }

    /// RFC 0065 §6.7: a tail replica keeps and hands on the batches held
    /// for a late first configuration one at a time, each under the
    /// configuration's index, so no step holds more than one batch's rows.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_tail_replica_hands_on_each_held_batch_on_its_own() {
        let lines = corpus();
        let mut entries = vec![block(0, 10_000)];
        let chunks = (1..).zip(lines.chunks(5).take(4));
        entries.extend(chunks.map(|(n, chunk)| batch_at(time(n), "acme", chunk)));
        entries.push(configured(0.7));
        let log = InMemoryLog(
            logged(&entries)
                .into_iter()
                .map(|e| (e.log_id.index, e))
                .collect(),
        );
        let Following {
            events,
            mut handed,
            mining,
            kept,
        } = following_over(Bounds::default(), &Meters::global(), log);
        for (index, entry) in (1..).zip(entries) {
            let event = event_of(entry);
            let at = log_id(index);
            events.send(Applied { at, event }).await.expect("sent");
        }
        drop(events);
        let mut seqs = Vec::new();
        while let Some(GroupMined { mined, .. }) = handed.recv().await {
            let mut handed_seqs: Vec<u64> = mined.rows.iter().map(|r| r.position.seq).collect();
            handed_seqs.dedup();
            seqs.push(handed_seqs);
        }
        mining.stop().await;
        assert_eq!(seqs, [vec![2], vec![3], vec![4], vec![5]]);
        let kept = kept.lock().expect("kept");
        assert_eq!(kept.rows().count(), 20, "every held row kept");
    }

    /// What a tail replica drops past its bounds is counted per group.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_tail_replica_counts_what_it_drops() {
        let reader = Reader::new();
        let bounds = Bounds { audit: 1, rows: 1 };
        let (kept, out) = follow_through_a_cut(bounds, &Meters::on(&reader.meter())).await;
        let events = out
            .iter()
            .map(|mined| mined.audit.len() as u64)
            .sum::<u64>();
        assert!(events > 0 && kept.gap().is_some_and(|gap| gap > 5));

        let group = || vec!["0".to_owned()];
        let points = |name| reader.points(name);
        assert_eq!(
            points(semconv::OURIOS_WAL_REPLICATION_KEPT_AUDIT_DROPPED),
            [(group(), events)]
        );
        assert_eq!(
            points(semconv::OURIOS_WAL_REPLICATION_KEPT_ROW_DROPPED),
            [(group(), 30)]
        );
    }

    /// The kept gauges report a group while its mining task runs, and
    /// nothing once it has ended.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_kept_gauges_report_only_while_mining_runs() {
        let reader = Reader::new();
        let bounds = Bounds {
            rows: 1,
            ..Bounds::default()
        };
        let Following {
            events,
            mut handed,
            mining,
            kept,
        } = following(bounds, &Meters::on(&reader.meter()));
        through_a_cut(&events).await;
        let gap = semconv::OURIOS_WAL_REPLICATION_KEPT_GAP;
        let usage = semconv::OURIOS_WAL_REPLICATION_KEPT_AUDIT_USAGE;
        let group = || vec!["0".to_owned()];
        until(&reader, gap, |points| points == [(group(), 3)]).await;
        let held = reader.points(usage);
        assert!(
            matches!(&held[..], [(g, bytes)] if *g == group() && *bytes > 0),
            "{held:?}"
        );
        drop(kept);

        drop(events);
        while handed.recv().await.is_some() {}
        mining.stop().await;
        assert_eq!(reader.points(gap), [], "an ended task reports no gap");
        assert_eq!(reader.points(usage), [], "nor kept audit bytes");
    }

    /// A tail read reaches what the miner keeps only while its task runs,
    /// and holds none of it past the call: once the task has ended, there
    /// is nothing to read.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn kept_output_is_read_only_while_mining_runs() {
        let Following {
            events,
            mut handed,
            mining,
            kept,
        } = following(Bounds::default(), &Meters::global());
        drop(kept);
        through_a_cut(&events).await;
        assert!(
            mining.read_kept(Kept::cut).is_some(),
            "a running task's output"
        );

        drop(events);
        while handed.recv().await.is_some() {}
        mining.stop().await;
        assert!(
            mining.read_kept(Kept::cut).is_none(),
            "an ended task's output"
        );
    }

    /// A read still running as the task ends holds `Kept` for its call,
    /// so a read that reaches `Kept` through that hold after the task has
    /// ended must still get nothing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_read_after_the_task_ends_gets_nothing_while_another_still_reads() {
        let Following {
            events,
            mut handed,
            mining,
            kept,
        } = following(Bounds::default(), &Meters::global());
        drop(kept);
        let (entered, first_in) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel::<()>();
        std::thread::scope(|scope| {
            let mining = &mining;
            let first = scope.spawn(move || {
                mining.read_kept(|_| {
                    entered.send(()).ok();
                    released.recv().ok();
                })
            });
            first_in.recv().expect("the first read holds Kept");
            drop(events);
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async {
                    while handed.recv().await.is_some() {}
                    mining.stop().await;
                });
            });
            let second = scope.spawn(move || mining.read_kept(|_| ()));
            // Let it reach `Kept` through the first read's hold and wait.
            std::thread::sleep(Duration::from_millis(100));
            release.send(()).ok();
            assert_eq!(first.join().expect("joined"), Some(()));
            assert_eq!(
                second.join().expect("joined"),
                None,
                "an ended task's output"
            );
        });
    }

    /// The kept gauges read `Kept` under its lock, so a step must not hold
    /// it across publication I/O: with the store's first write held, the
    /// lock is still free.
    #[test]
    fn the_kept_lock_is_free_while_publication_writes() {
        let tmp = tempfile::TempDir::new().expect("temp");
        let local = Store::local(tmp.path()).expect("store");
        let voters = ["v1", "v2", "v3"];
        let members = (1..)
            .zip(voters)
            .map(|(n, v)| (v.to_owned(), NodeId::from(n)))
            .collect();
        let formation = Formation::new(0, 5, members, &voters).expect("formation");
        local
            .put_blocking(&formed_key(0), formation.encode().expect("encode"))
            .expect("record");
        let (store, held) = first_put_held(local);
        let source = Source {
            store,
            voters: voters.map(str::to_owned).to_vec(),
            flush: FlushConfig {
                target_bytes: 1,
                max_buffer_age: Duration::MAX,
                ceiling_bytes: 1,
            },
            promoted: ourios_parquet::PromotedAttributes::default(),
            audit_ceiling: usize::MAX,
            #[cfg(feature = "openfga")]
            graph_emitter: None,
        };
        let me = NodeId::from(7);
        let kept: Arc<Mutex<Kept>> = Arc::default();
        let keeping = Keeping {
            group: 0,
            kept: Arc::clone(&kept),
            drops: Meters::global().kept(),
            flushing: Some(Arc::new(Mutex::new(Flushing::new(
                me,
                0,
                source,
                Arc::new(EmptyLog),
            )))),
        };
        let log = mined_log(&entries(2));
        let (index, mined) = log
            .iter()
            .find(|(_, mined)| !mined.rows.is_empty())
            .expect("a batch");
        let assigned = Applying {
            assigned: Some(me),
            ..Applying::default()
        };
        std::thread::scope(|scope| {
            let step = scope.spawn(|| keeping.keep(*index, mined, assigned));
            held.reached.recv().expect("publication is writing");
            let free = (0..50).any(|_| {
                let free = kept.try_lock().is_ok();
                std::thread::sleep(Duration::from_millis(10));
                free
            });
            held.release.send(()).expect("released");
            step.join().expect("join");
            assert!(free, "the step held `Kept` across its store write");
        });
    }

    /// A log that holds no entry.
    struct EmptyLog;

    impl GroupLogRead for EmptyLog {
        fn entry(&self, _: u64) -> Option<Entry> {
            None
        }
    }

    /// While a build is held short of the cut, metrics that change every
    /// millisecond draw no trigger: one goes out when the cut advances and
    /// one per retry interval after, and none once the snapshot is built.
    #[tokio::test(start_paused = true)]
    async fn a_held_build_is_retriggered_only_at_the_retry_interval() {
        let (snapshots_tx, snapshots) = watch::channel(None);
        let snapshots_tx = Arc::new(snapshots_tx);
        let (cuts_tx, cuts) = watch::channel(None);
        let triggers = Arc::new(AtomicU64::new(0));
        let held = Held {
            snapshots,
            triggers: Arc::clone(&triggers),
        };
        let task = tokio::spawn(snapshot_at_checkpoints(held, cuts, watch::channel(false).1));
        let churning = Arc::clone(&snapshots_tx);
        let churn = tokio::spawn(async move {
            loop {
                churning.send_modify(|_| {});
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        });

        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(triggers.load(Ordering::Relaxed), 0, "no cut, no trigger");
        cuts_tx.send_replace(Some(log_id(5)));
        tokio::time::sleep(Duration::from_secs(2)).await;
        let retries = 2_000 / TRIGGER_RETRY.as_millis();
        let sent = u128::from(triggers.load(Ordering::Relaxed));
        assert!(
            (retries..=retries + 1).contains(&sent),
            "{sent} triggers in 2 s"
        );

        snapshots_tx.send_replace(Some(log_id(5)));
        tokio::time::sleep(Duration::from_millis(10)).await;
        let built = triggers.load(Ordering::Relaxed);
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(triggers.load(Ordering::Relaxed), built, "none once built");

        churn.abort();
        drop(cuts_tx);
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("the task ends with the group")
            .expect("join");
    }
}

/// What the cluster harness reads of a running group's log.
#[cfg(test)]
mod probe {
    use std::sync::Arc;

    use super::super::log_store::{GroupLog, GroupLogReader, test_hooks::Hooks};
    use super::{GroupHost, Hosted};

    pub(super) struct Probe {
        pub(super) hooks: Arc<Hooks>,
    }

    impl Probe {
        pub(super) fn of(log: &GroupLog) -> Self {
            Self { hooks: log.hooks() }
        }
    }

    impl GroupHost {
        fn probe(&self, group: u32) -> Option<&Probe> {
            self.groups.get(&group).map(|hosted: &Hosted| &hosted.probe)
        }

        pub(in crate::replicated::raft) fn log_reader(&self, group: u32) -> Option<GroupLogReader> {
            self.log(group).cloned()
        }

        pub(in crate::replicated::raft) fn log_hooks(&self, group: u32) -> Option<Arc<Hooks>> {
            self.probe(group).map(|probe| Arc::clone(&probe.hooks))
        }

        /// Whether a miner follows `group` on this node.
        pub(in crate::replicated::raft) fn mines(&self, group: u32) -> bool {
            self.groups.get(&group).is_some_and(|hosted| {
                hosted
                    .mining
                    .as_ref()
                    .is_some_and(super::Mining::is_running)
            })
        }
    }
}
