//! Every group a node votes in, open and running (RFC 0065 §6.1): one
//! `Raft` per group over its group log, all served by one
//! [`ReplicationServer`] and reaching their peers through one [`Peers`]
//! pool.

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
use tokio::sync::watch;
use tokio::time::MissedTickBehavior;

use super::gate::SyncGate;
use super::log_store::{GroupLog, GroupLogReader, group_dir};
use super::state_machine::GroupSm;
use super::telemetry::{Meters, ReplicationTelemetry};
use super::transport::{Peers, ReplicationServer};
use super::types::{LogId, NodeId, RaftTypes};
use super::vote_log::VoteLog;
use super::{RaftTimings, group_config};

pub type GroupRaft = Raft<RaftTypes, GroupSm>;

/// What a node hosts.
pub struct HostSpec {
    /// The replica's Raft node id.
    pub id: NodeId,
    /// The node's WAL settings, `root` being its `wal_root`; each group's
    /// WAL takes them with its root at [`group_dir`].
    pub wal: WalConfig,
    /// The groups the node votes in
    /// ([`VotingLayout::groups_of`](crate::replicated::layout::VotingLayout::groups_of)).
    pub groups: Vec<u32>,
    pub peers: Peers,
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
        let mut log = blocking(move || GroupLog::open_with(wal, group, votes, gate))
            .await
            .map_err(|source| HostError::Log { group, source })?;
        let reader = log.get_log_reader().await;
        #[cfg(test)]
        let probe = probe::Probe::of(&log);
        log.count_takeovers(self.meters.takeovers(self.spec.id, group));
        let gate = Arc::clone(&self.gate);
        let sm = blocking(move || GroupSm::open(&dir, gate))
            .await
            .map_err(|source| HostError::Snapshot { group, source })?;
        let (id, network) = (self.spec.id, self.spec.peers.network(self.spec.id, group));
        let raft = Raft::new(id, Arc::clone(&self.config), network, log, sm.clone())
            .await
            .map_err(|source| HostError::Raft { group, source })?;
        let builds = RaftBuilds {
            raft: raft.clone(),
            metrics: raft.metrics(),
        };
        tokio::spawn(snapshot_at_checkpoints(builds, sm.checkpoints()));
        Ok(Hosted {
            raft,
            sm,
            log: reader,
            #[cfg(test)]
            probe,
        })
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

/// What a host is opened with besides its spec: the Raft settings, the
/// server its groups register with, and the gate their fsyncs pass.
pub(super) struct Wiring {
    pub(super) config: openraft::Config,
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
        let config = group_config(RaftTimings::default()).map_err(HostError::Config)?;
        let wiring = Wiring {
            config,
            server: ReplicationServer::default(),
            gate: Arc::default(),
        };
        Self::open_with(spec, wiring, &meters).await
    }

    pub(super) async fn open_with(
        spec: HostSpec,
        Wiring {
            config,
            server,
            gate,
        }: Wiring,
        meters: &Meters,
    ) -> Result<Self, HostError> {
        let config = Arc::new(config);
        let root = spec.wal.root.clone();
        let votes = blocking(move || VoteLog::open(&root))
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
            hosted.raft.shutdown().await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use openraft::vote::leader_id_std::CommittedLeaderId;

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
    }
}
