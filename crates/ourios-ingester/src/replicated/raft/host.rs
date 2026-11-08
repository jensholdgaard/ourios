//! Every group a node votes in, open and running (RFC 0065 §6.1): one
//! `Raft` per group over its group log, all served by one
//! [`ReplicationServer`] and reaching their peers through one [`Peers`]
//! pool.

use std::collections::BTreeMap;
use std::io;
use std::sync::Arc;

use openraft::Raft;
use openraft::errors::Fatal;
use openraft::storage::RaftLogStorage as _;
use openraft::type_config::alias::JoinErrorOf;
use ourios_wal::WalConfig;

use super::gate::SyncGate;
use super::group_config;
use super::log_store::{GroupLog, GroupLogReader, group_dir};
use super::state_machine::GroupSm;
use super::telemetry::{Meters, ReplicationTelemetry};
use super::transport::{Peers, ReplicationServer};
use super::types::{NodeId, RaftTypes};
use super::vote_log::VoteLog;

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
        Ok(Hosted {
            raft,
            sm,
            log: reader,
            #[cfg(test)]
            probe,
        })
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
        let config = group_config().map_err(HostError::Config)?;
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
