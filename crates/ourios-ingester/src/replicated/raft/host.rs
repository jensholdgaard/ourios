//! Every group a node votes in or is a tail replica of, open and running
//! (RFC 0065 §6.1): one `Raft` per group over its group log, all served by
//! one [`ReplicationServer`] and reaching their peers through one
//! [`Peers`] pool. On a tail replica each group's [`GroupMiner`] mines the
//! entries its state machine applies (§6.5).

use std::collections::BTreeMap;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use openraft::async_runtime::WatchReceiver as _;
use openraft::errors::Fatal;
use openraft::storage::RaftLogStorage as _;
use openraft::type_config::alias::{JoinErrorOf, WatchReceiverOf};
use openraft::{Raft, RaftMetrics};
use ourios_wal::WalConfig;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use super::gate::SyncGate;
use super::log_store::{Disk, GroupLog, GroupLogReader, group_dir};
use super::mining::{GroupMiner, Mined};
use super::state_machine::{Applied, GroupSm};
use super::telemetry::{Meters, ReplicationTelemetry, Storage};
use super::transport::{Peers, ReplicationServer};
use super::types::{LogId, NodeId, RaftTypes};
use super::vote_log::VoteLog;
use super::{RaftTimings, group_config};

pub type GroupRaft = Raft<RaftTypes, GroupSm>;

/// What a node is to the groups it hosts (RFC 0065 §3.8): it votes in
/// them or is a tail replica of them, never both.
#[derive(Debug, Clone)]
pub enum HostRole {
    /// Votes in its groups and mines none of them: the leader acknowledges
    /// and replicates, and mining never sits on the ack path (§6.5).
    Voter,
    /// A tail replica of its groups: mines each one's entries as its state
    /// machine applies them and hands what it mined to `mined`, in log
    /// order per group, waiting while the channel is full. Opening the
    /// host replays the committed log through the miner, so `mined` must
    /// be read while it opens.
    Tail { mined: mpsc::Sender<GroupMined> },
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
        let (sm, applied) = match &self.spec.role {
            HostRole::Voter => (blocking(move || GroupSm::open(&dir, gate)).await, None),
            HostRole::Tail { mined } => {
                let (events, applied) = mpsc::channel(APPLIED_CAPACITY);
                let sm = blocking(move || GroupSm::open_mining(&dir, gate, events)).await;
                (sm, Some((applied, mined.clone())))
            }
        };
        let mut sm = sm.map_err(|source| HostError::Snapshot { group, source })?;
        sm.count_stops(group, self.meters.storage());
        // `Raft::new` applies the committed entries the log replays before
        // it returns, so the miner must already be taking them.
        let mining = applied.map(|(applied, mined)| {
            let miner = GroupMiner::new(reader.clone());
            let follow = Follow {
                group,
                mined,
                storage: self.meters.storage(),
            };
            Mining::start(follow, miner, applied)
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
        let builds = RaftBuilds {
            raft: raft.clone(),
            metrics: raft.metrics(),
        };
        tokio::spawn(snapshot_at_checkpoints(builds, sm.checkpoints()));
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

/// How many applied entries a tail replica's state machine hands its
/// miner ahead of what it has mined, before applying waits for it.
const APPLIED_CAPACITY: usize = 64;

/// A tail replica's running miner.
struct Mining {
    stop: watch::Sender<bool>,
    /// Cleared only once the task has ended, so every stop, a concurrent
    /// one or one retried after a cancelled one, waits for it.
    task: tokio::sync::Mutex<Option<JoinHandle<()>>>,
}

impl Mining {
    fn start(follow: Follow, miner: GroupMiner, applied: mpsc::Receiver<Applied>) -> Self {
        let (stop, stopped) = watch::channel(false);
        let task = tokio::spawn(follow.run(miner, applied, stopped));
        Self {
            stop,
            task: tokio::sync::Mutex::new(Some(task)),
        }
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
        let mut task = self.task.lock().await;
        if let Some(running) = task.as_mut() {
            // A miner that panicked has ended too.
            running.await.ok();
        }
        *task = None;
    }
}

/// The next applied event, or `None` once the group has ended or the host
/// stops the miner.
async fn next(
    applied: &mut mpsc::Receiver<Applied>,
    stop: &mut watch::Receiver<bool>,
) -> Option<Applied> {
    tokio::select! {
        biased;
        () = stopped(stop) => None,
        event = applied.recv() => event,
    }
}

/// Resolves once the host stops the miner, or drops the means to.
async fn stopped(stop: &mut watch::Receiver<bool>) {
    stop.wait_for(|stopped| *stopped).await.ok();
}

/// Where a tail replica's miner for `group` hands what it mined, and
/// reports its stop.
struct Follow {
    group: u32,
    mined: mpsc::Sender<GroupMined>,
    storage: Storage,
}

impl Follow {
    /// Feed `miner` each event `applied` carries, off the runtime's
    /// workers, and hand on whatever an event mined. A miner that stops is
    /// reported and dropped with `applied`, and the replica applies on
    /// without it. Ends with the group or once `stop` says so, never in
    /// the middle of a step.
    async fn run(
        self,
        mut miner: GroupMiner,
        mut applied: mpsc::Receiver<Applied>,
        mut stop: watch::Receiver<bool>,
    ) {
        while let Some(event) = next(&mut applied, &mut stop).await {
            let mining = tokio::task::spawn_blocking(move || {
                let followed = miner.apply(event);
                (miner, followed)
            });
            // A miner that panicked is gone; the panic is reported where it happened.
            let Ok((back, followed)) = mining.await else {
                return;
            };
            miner = back;
            let handed_on = match followed {
                Ok(out) => self.hand_on(out, &mut stop).await,
                Err(error) => {
                    self.storage.miner_stopped(self.group, &error);
                    false
                }
            };
            if !handed_on {
                return;
            }
        }
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
) {
    let mut built = builds.snapshot();
    let mut retry = tokio::time::interval(TRIGGER_RETRY);
    retry.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        let cut = *cuts.borrow_and_update();
        if built < cut {
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
            _ = retry.tick(), if built < cut => {}
        }
    }
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
                    // `Raft::shutdown` always returns `Ok`; the open's error is the one to report.
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
    /// hold and exit.
    ///
    /// # Errors
    ///
    /// A group's core task could not be joined.
    pub async fn shutdown(&self) -> Result<(), JoinErrorOf<RaftTypes>> {
        for hosted in self.groups.values() {
            // First, so a miner waiting on unread output cannot hold up
            // the state machine `Raft` waits for.
            if let Some(mining) = &hosted.mining {
                mining.stop().await;
            }
            hosted.raft.shutdown().await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use openraft::vote::leader_id_std::CommittedLeaderId;
    use ourios_wal::Control;

    use super::super::mining::GroupLogRead;
    use super::super::mining::tests::{batch_at, configured, time};
    use super::super::snapshot_record::GroupMinerConfig;
    use super::super::state_machine::AppliedEvent;
    use super::super::types::{Entry, GroupEntry};
    use super::*;

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
    /// when it is dropped.
    struct SlowLog {
        pause: Duration,
        dropped: Arc<std::sync::atomic::AtomicBool>,
    }

    impl GroupLogRead for SlowLog {
        fn entry(&self, _: u64) -> Option<Entry> {
            std::thread::sleep(self.pause);
            None
        }
    }

    impl Drop for SlowLog {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    /// Stopping a miner waits for the step it is running, so nothing it
    /// holds, the group's log reader among them, outlives the stop.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stopping_a_miner_waits_for_its_running_step() {
        let dropped = Arc::default();
        let log = SlowLog {
            pause: Duration::from_millis(500),
            dropped: Arc::clone(&dropped),
        };
        let mining = reading_back(log).await;

        mining.stop().await;
        assert!(
            dropped.load(Ordering::SeqCst),
            "the running step ended and released the log before the stop returned"
        );
        assert!(!mining.is_running());
    }

    /// A miner mid-way through reading a batch back from `log`.
    async fn reading_back(log: SlowLog) -> Mining {
        let (events, applied) = mpsc::channel(4);
        let (mined, _unread) = mpsc::channel(4);
        let follow = Follow {
            group: 0,
            mined,
            storage: Storage::global(),
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
        // The configuration reads the batch back from the log, which sleeps.
        tokio::time::sleep(Duration::from_millis(100)).await;
        mining
    }

    /// Two stops at once, and a stop retried after one was cancelled,
    /// each return only once the running step has released the log.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn every_stop_waits_for_the_running_step() {
        let dropped: Arc<std::sync::atomic::AtomicBool> = Arc::default();
        let log = SlowLog {
            pause: Duration::from_millis(500),
            dropped: Arc::clone(&dropped),
        };
        let mining = Arc::new(reading_back(log).await);
        let stops = (0..2).map(|_| {
            let (mining, dropped) = (Arc::clone(&mining), Arc::clone(&dropped));
            tokio::spawn(async move {
                mining.stop().await;
                dropped.load(Ordering::SeqCst)
            })
        });
        for stop in stops.collect::<Vec<_>>() {
            assert!(stop.await.expect("join"), "a stop returned mid-step");
        }

        let dropped: Arc<std::sync::atomic::AtomicBool> = Arc::default();
        let log = SlowLog {
            pause: Duration::from_millis(500),
            dropped: Arc::clone(&dropped),
        };
        let mining = reading_back(log).await;
        let cancelled = tokio::time::timeout(Duration::from_millis(50), mining.stop()).await;
        assert!(cancelled.is_err(), "the first stop was cancelled mid-step");
        mining.stop().await;
        assert!(
            dropped.load(Ordering::SeqCst),
            "the retried stop returned mid-step"
        );
    }

    fn log_id(index: u64) -> LogId {
        LogId {
            leader_id: CommittedLeaderId::new(2),
            index,
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
        let task = tokio::spawn(snapshot_at_checkpoints(held, cuts));
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
