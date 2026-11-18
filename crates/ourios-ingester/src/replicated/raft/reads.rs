//! The leader's side of `ReadIndex` (RFC 0065 §6.7): a group's read point,
//! given only once the leader has confirmed with a quorum that it still
//! leads, so a deposed leader or one whose commit point lags gives none.

use std::collections::BTreeMap;
use std::sync::Arc;

use openraft::ReadPolicy;
use openraft::errors::{LinearizableReadError, RaftError};

use super::host::{GroupHost, GroupRaft};
use super::transport::read_index::{ReadIndexTarget, Reading, Refusal};
use super::types::RaftTypes;

/// A node's groups, as `ReadIndex` reaches them.
pub struct Reads {
    groups: Arc<BTreeMap<u32, GroupRaft>>,
}

impl Reads {
    /// The groups `host` runs.
    #[must_use]
    pub fn new(host: &GroupHost) -> Self {
        let groups = host
            .groups()
            .filter_map(|group| Some((group, host.raft(group)?.clone())))
            .collect();
        Self {
            groups: Arc::new(groups),
        }
    }
}

impl ReadIndexTarget for Reads {
    fn read_index(&self, group: u32) -> Reading {
        let Some(raft) = self.groups.get(&group).cloned() else {
            return Box::pin(async { Err(Refusal::NotHosted) });
        };
        Box::pin(async move {
            // ReadIndex, not a lease: a quorum confirms the leadership for
            // this read, so no clock decides it.
            let linearizer = raft
                .get_read_linearizer(ReadPolicy::ReadIndex)
                .await
                .map_err(refusal)?;
            // The boundary any replica but the leader must apply through.
            Ok(*linearizer.read_log_id().log_id())
        })
    }
}

/// Why the leader gave no read point: it does not lead, naming the leader
/// it knows of, or it could not confirm with a quorum.
fn refusal(error: RaftError<RaftTypes, LinearizableReadError<RaftTypes>>) -> Refusal {
    match error {
        RaftError::APIError(LinearizableReadError::ForwardToLeader(forward)) => {
            Refusal::NotLeader {
                leader: forward.leader_node,
            }
        }
        other => Refusal::Unconfirmed(other.to_string()),
    }
}
