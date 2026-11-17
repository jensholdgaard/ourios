//! A replicated node's start (RFC 0065 §6.1, §6.6): the steps before it
//! serves, in an order that lets a start that fails change as little as it
//! can. Whether it holds Raft state is read first, so a disk it cannot read
//! changes nothing; the voting layout is recorded or checked before its
//! identity is created, so a node that could never join leaves no identity;
//! the bootstrap lock comes before its groups open. A node holding Raft
//! state starts while the store is unreachable, its layout unchecked until
//! a start that can read it (RFC0065.31).

use std::fmt;
use std::io;
use std::path::Path;
use std::time::Duration;

use ourios_parquet::Store;
use ourios_wal::WalConfig;

use super::layout::{self, LayoutError, VotingLayout};
use super::raft::RaftTimings;
use super::raft::host::{GroupHost, HostError, HostRole, HostSpec};
use super::raft::log_ids::{self, LockStart, RaftState, StartError, Waiting};
use super::raft::transport::{Identity, Peers};
use super::raft::types::NodeId;
use super::replica_id::{self, ReplicaId, ReplicaIdError};
use crate::template_ids::ScanOptions;

/// What a node starts from.
pub struct NodeStart {
    /// The node's configured `node_id`.
    pub node_id: String,
    /// The node's WAL settings, `root` being its `wal_root`.
    pub wal: WalConfig,
    pub layout: VotingLayout,
    /// The groups it votes in, or as a tail replica is placed in.
    pub groups: Vec<u32>,
    pub role: HostRole,
    pub peers: Peers,
    pub timings: RaftTimings,
    pub store: Store,
    /// This node's `receiver.template_ids_allow_bootstrap` (RFC 0059 §3.5).
    pub allow_bootstrap: bool,
    pub scan: ScanOptions,
    /// How long a start waits before passing the bootstrap lock again.
    pub retry: Duration,
}

/// Why a node did not start.
#[derive(Debug)]
#[non_exhaustive]
pub enum NodeError {
    /// It could not tell whether it holds Raft state.
    RaftState(io::Error),
    Layout(LayoutError),
    Identity(ReplicaIdError),
    LogIds(StartError),
    Host(HostError),
}

impl fmt::Display for NodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RaftState(e) => write!(f, "cannot read the node's Raft state: {e}"),
            Self::Layout(e) => write!(f, "{e}"),
            Self::Identity(e) => write!(f, "{e}"),
            Self::LogIds(e) => write!(f, "{e}"),
            Self::Host(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for NodeError {}

/// A started node: its groups open, its replication server identified and
/// not yet served.
pub struct Node {
    pub id: NodeId,
    pub host: GroupHost,
}

/// Start the node `start` describes, reporting each wait for the bootstrap
/// lock to `waiting`.
///
/// # Errors
///
/// The [`NodeError`] of the first step that failed.
pub async fn start(
    start: NodeStart,
    waiting: &mut impl FnMut(&Waiting),
) -> Result<Node, NodeError> {
    let NodeStart {
        node_id,
        wal,
        layout,
        groups,
        role,
        peers,
        timings,
        store,
        allow_bootstrap,
        scan,
        retry,
    } = start;
    let (raft_state, replica) = {
        let (root, groups, store) = (wal.root.clone(), groups.clone(), store.clone());
        tokio::task::spawn_blocking(move || prepare(&root, &groups, &store, &layout))
            .await
            .map_err(|e| NodeError::RaftState(io::Error::other(e)))??
    };
    let lock = LockStart {
        store: &store,
        node_id: &node_id,
        raft_state,
        allow_bootstrap,
        scan: &scan,
        retry,
    };
    log_ids::start(&lock, waiting)
        .await
        .map_err(NodeError::LogIds)?;
    let id = NodeId::from(replica.uuid());
    let spec = HostSpec {
        id,
        wal,
        groups,
        role,
        peers,
        timings,
    };
    let host = GroupHost::open(spec).await.map_err(NodeError::Host)?;
    host.server().identify_as(Identity::new(node_id, id));
    Ok(Node { id, host })
}

/// The steps before the lock, blocking, in the order that leaves the
/// least behind a start that fails: whether this node holds Raft state,
/// then the voting layout, then its identity.
fn prepare(
    wal_root: &Path,
    groups: &[u32],
    store: &Store,
    layout: &VotingLayout,
) -> Result<(RaftState, ReplicaId), NodeError> {
    let held = log_ids::raft_state(wal_root, groups).map_err(NodeError::RaftState)?;
    match (layout::record_or_check(store, layout), held) {
        // RFC0065.31: holding its groups' state, it opens them with the store
        // unreachable; a start that can read the layout checks it.
        (Err(LayoutError::Store { .. }), RaftState::Held) => {
            tracing::warn!(
                "the voting layout is not checked: the store cannot be read, and this node \
                 opens the groups whose Raft state it holds"
            );
        }
        (checked, _) => {
            checked.map_err(NodeError::Layout)?;
        }
    }
    let identity = replica_id::load_or_create(wal_root).map_err(NodeError::Identity)?;
    Ok((held, identity.id()))
}

#[cfg(test)]
mod tests;
