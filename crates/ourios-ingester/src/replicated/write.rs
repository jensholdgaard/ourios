//! The replicated write path (RFC 0065 §6.3, with `openraft`): a tenant's
//! export is proposed to its group's log and acknowledged once
//! `client_write` returns `Ok`, which `openraft` resolves after a majority
//! of the group's voters have fsynced the entry and the leader has applied
//! it.

use std::collections::BTreeMap;
use std::time::Duration;

use openraft::async_runtime::WatchReceiver as _;
use openraft::errors::{ClientWriteError, RaftError};

use super::layout::VotingLayout;
use super::raft::host::{GroupHost, GroupRaft};
use super::raft::types::{RaftTypes, TenantBatchBytes};
use crate::metrics::{
    ERROR_TYPE_GROUP_UNAVAILABLE, ERROR_TYPE_NOT_ENOUGH_REPLICAS, ERROR_TYPE_REQUEST_TOO_LARGE,
};

/// How long a commit waits for `client_write`. An internal constant, not
/// a setting: the rate of `not_enough_replicas` rejections is the signal
/// that would move it (§6.3).
pub const COMMIT_TIMEOUT: Duration = Duration::from_secs(2);

/// Where a committed export sits: its group, and its entry's Raft index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct GroupPosition {
    pub group: u32,
    pub seq: u64,
}

type ClientWriteFailure = RaftError<RaftTypes, ClientWriteError<RaftTypes>>;

/// Why an export was not committed; none is acknowledged. `TooLarge` is
/// the permanent size rejection, every other one the retryable rejection.
#[derive(Debug)]
pub enum WriteError {
    /// `client_write` did not return within [`COMMIT_TIMEOUT`]. The entry
    /// may still commit later, the uncertain outcome B3 accepts.
    NotEnoughReplicas { group: u32 },
    /// This node does not lead the group, and nothing was appended.
    /// `leader` is the `node_id` of the leader it knows of, if any.
    NotLeader { group: u32, leader: Option<String> },
    /// This node is not a voter of the group, so nothing was appended.
    NotHosted { group: u32 },
    /// The export's entry would not fit one WAL frame, so it was never
    /// proposed: the permanent size rejection, as on the local WAL.
    TooLarge { len: usize, limit: usize },
    /// `client_write` failed otherwise; the outcome may be uncertain.
    Raft {
        group: u32,
        source: Box<ClientWriteFailure>,
    },
    /// The export was forwarded and no leader confirmed a commit (§6.8):
    /// every node tried proved it did not take it, or an answer left the
    /// outcome uncertain, in which case it may still commit (B3).
    Unforwarded { group: u32, reason: String },
}

impl WriteError {
    fn from_raft(group: u32, error: ClientWriteFailure) -> Self {
        match error {
            RaftError::APIError(ClientWriteError::ForwardToLeader(forward)) => Self::NotLeader {
                group,
                leader: forward.leader_node,
            },
            source => Self::Raft {
                group,
                source: Box::new(source),
            },
        }
    }

    /// The `error.type` the rejection counts under (§6.9).
    #[must_use]
    pub fn error_type(&self) -> &'static str {
        match self {
            Self::NotEnoughReplicas { .. } => ERROR_TYPE_NOT_ENOUGH_REPLICAS,
            Self::TooLarge { .. } => ERROR_TYPE_REQUEST_TOO_LARGE,
            Self::NotLeader { .. }
            | Self::NotHosted { .. }
            | Self::Raft { .. }
            | Self::Unforwarded { .. } => ERROR_TYPE_GROUP_UNAVAILABLE,
        }
    }
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotEnoughReplicas { group } => write!(
                f,
                "group {group} did not commit the export within the commit timeout"
            ),
            Self::NotLeader {
                group,
                leader: Some(leader),
            } => write!(f, "this node does not lead group {group}; {leader} does"),
            Self::NotLeader {
                group,
                leader: None,
            } => write!(f, "group {group} has no known leader"),
            Self::NotHosted { group } => write!(f, "this node is not a voter of group {group}"),
            Self::TooLarge { len, limit } => write!(
                f,
                "the export's {len} bytes exceed the {limit} a WAL frame's entry can hold"
            ),
            Self::Raft { group, source } => write!(f, "group {group}: {source}"),
            Self::Unforwarded { group, reason } => write!(
                f,
                "no leader of group {group} confirmed committing the forwarded export, \
                 which may still commit: {reason}"
            ),
        }
    }
}

impl std::error::Error for WriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Raft { source, .. } => Some(source.as_ref()),
            Self::NotEnoughReplicas { .. }
            | Self::NotLeader { .. }
            | Self::NotHosted { .. }
            | Self::TooLarge { .. }
            | Self::Unforwarded { .. } => None,
        }
    }
}

/// Who leads a tenant's group, as this node's Raft last knew.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Leading {
    /// This node does.
    Here,
    /// Another node does, or none this node knows of, or this node does
    /// not host the group: `leader` is the `node_id` it knows of, if any.
    Elsewhere { group: u32, leader: Option<String> },
}

/// Commits exports to the groups this node votes in.
pub struct GroupWriter {
    layout: VotingLayout,
    groups: BTreeMap<u32, GroupRaft>,
    timeout: Duration,
}

impl GroupWriter {
    /// A writer over every group `host` runs, routing tenants by `layout`.
    #[must_use]
    pub fn new(layout: VotingLayout, host: &GroupHost) -> Self {
        let groups = host
            .groups()
            .filter_map(|group| host.raft(group).map(|raft| (group, raft.clone())))
            .collect();
        Self {
            layout,
            groups,
            timeout: COMMIT_TIMEOUT,
        }
    }

    /// The group `tenant`'s exports commit to.
    #[must_use]
    pub fn group_of(&self, tenant: &str) -> u32 {
        self.layout.group_of(tenant)
    }

    /// Whether `batch` fits one WAL frame's entry, on any node.
    ///
    /// # Errors
    ///
    /// [`WriteError::TooLarge`], the permanent size rejection.
    pub fn fits(batch: &TenantBatchBytes) -> Result<(), WriteError> {
        let (len, limit) = (
            batch.as_bytes().len(),
            ourios_wal::ReplicatedRecord::MAX_ENTRY_PAYLOAD_BYTES,
        );
        if len > limit {
            return Err(WriteError::TooLarge { len, limit });
        }
        Ok(())
    }

    /// Whether this node leads `tenant`'s group, as its Raft last knew.
    #[must_use]
    pub fn leading(&self, tenant: &str) -> Leading {
        let group = self.layout.group_of(tenant);
        let Some(raft) = self.groups.get(&group) else {
            return Leading::Elsewhere {
                group,
                leader: None,
            };
        };
        let metrics = raft.metrics().borrow_watched().clone();
        match metrics.current_leader {
            Some(id) if id == metrics.id => Leading::Here,
            leader => Leading::Elsewhere {
                group,
                leader: leader.and_then(|id| {
                    metrics
                        .membership_config
                        .membership()
                        .get_node(&id)
                        .cloned()
                }),
            },
        }
    }

    #[cfg(test)]
    pub(crate) fn with_timeout(self, timeout: Duration) -> Self {
        Self { timeout, ..self }
    }

    /// Commit `batch`, `tenant`'s `TenantOtlpBatch` payload, to the
    /// tenant's group.
    ///
    /// # Errors
    ///
    /// [`WriteError`]: the export is not acknowledged.
    pub async fn commit(
        &self,
        tenant: &str,
        batch: TenantBatchBytes,
    ) -> Result<GroupPosition, WriteError> {
        Self::fits(&batch)?;
        let group = self.layout.group_of(tenant);
        let raft = self
            .groups
            .get(&group)
            .ok_or(WriteError::NotHosted { group })?;
        match tokio::time::timeout(self.timeout, raft.client_write(batch.into())).await {
            Ok(Ok(response)) => Ok(GroupPosition {
                group,
                seq: response.log_id.index,
            }),
            Ok(Err(e)) => Err(WriteError::from_raft(group, e)),
            Err(_) => Err(WriteError::NotEnoughReplicas { group }),
        }
    }
}

#[cfg(test)]
mod tests;
