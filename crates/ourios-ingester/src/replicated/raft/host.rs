//! Every group a node votes in, open and running (RFC 0065 §6.1): one
//! `Raft` per group over its group log, all served by one
//! [`ReplicationServer`] and reaching their peers through one [`Peers`]
//! pool.

use std::collections::BTreeMap;
use std::io;
use std::sync::Arc;

use openraft::Raft;
use openraft::errors::Fatal;
use openraft::type_config::alias::JoinErrorOf;
use ourios_wal::WalConfig;

use super::gate::SyncGate;
use super::group_config;
use super::log_store::{GroupLog, group_dir};
use super::state_machine::GroupSm;
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
    #[cfg(test)]
    probe: probe::Probe,
}

/// What opening each of a node's groups shares.
struct Launch<'a> {
    spec: &'a HostSpec,
    votes: VoteLog,
    gate: Arc<SyncGate>,
    config: Arc<openraft::Config>,
}

impl Launch<'_> {
    async fn group(&self, group: u32) -> Result<Hosted, HostError> {
        let dir = group_dir(&self.spec.wal.root, group);
        let wal = WalConfig {
            root: dir.clone(),
            ..self.spec.wal.clone()
        };
        let (votes, gate) = (self.votes.clone(), Arc::clone(&self.gate));
        #[cfg_attr(not(test), allow(unused_mut))] // the test probe borrows it mutably
        let mut log = blocking(move || GroupLog::open_with(wal, group, votes, gate))
            .await
            .map_err(|source| HostError::Log { group, source })?;
        #[cfg(test)]
        let probe = probe::Probe::of(&mut log).await;
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
            #[cfg(test)]
            probe,
        })
    }
}

/// The node's running groups.
pub struct GroupHost {
    groups: BTreeMap<u32, Hosted>,
    server: ReplicationServer<GroupSm>,
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .unwrap_or_else(|e| Err(io::Error::other(e)))
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
        Self::open_with(spec, ReplicationServer::default(), Arc::default()).await
    }

    pub(super) async fn open_with(
        spec: HostSpec,
        server: ReplicationServer<GroupSm>,
        gate: Arc<SyncGate>,
    ) -> Result<Self, HostError> {
        let config = Arc::new(group_config().map_err(HostError::Config)?);
        let root = spec.wal.root.clone();
        let votes = blocking(move || VoteLog::open(&root))
            .await
            .map_err(HostError::Votes)?;
        let launch = Launch {
            spec: &spec,
            votes,
            gate,
            config,
        };
        let mut host = Self {
            groups: BTreeMap::new(),
            server,
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

    use openraft::storage::RaftLogStorage as _;

    use super::super::log_store::{GroupLog, GroupLogReader, test_hooks::Hooks};
    use super::{GroupHost, Hosted};

    pub(super) struct Probe {
        pub(super) reader: GroupLogReader,
        pub(super) hooks: Arc<Hooks>,
    }

    impl Probe {
        pub(super) async fn of(log: &mut GroupLog) -> Self {
            Self {
                reader: log.get_log_reader().await,
                hooks: log.hooks(),
            }
        }
    }

    impl GroupHost {
        fn probe(&self, group: u32) -> Option<&Probe> {
            self.groups.get(&group).map(|hosted: &Hosted| &hosted.probe)
        }

        pub(in crate::replicated::raft) fn log_reader(&self, group: u32) -> Option<GroupLogReader> {
            self.probe(group).map(|probe| probe.reader.clone())
        }

        pub(in crate::replicated::raft) fn log_hooks(&self, group: u32) -> Option<Arc<Hooks>> {
            self.probe(group).map(|probe| Arc::clone(&probe.hooks))
        }
    }
}
