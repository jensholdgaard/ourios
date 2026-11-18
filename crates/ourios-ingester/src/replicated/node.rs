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
use std::sync::Arc;
use std::time::Duration;

use ourios_core::audit::AuditSink;
use ourios_parquet::Store;
use ourios_serving::AuthResolver;
use ourios_serving::tls_serve::ReloadingAcceptor;
use ourios_wal::WalConfig;
use tokio::task::JoinHandle;
use tonic::transport::server::TcpIncoming;

use super::forward::LeaderTarget;
use super::ingest::ReplicatedIngest;
use super::layout::{self, LayoutError, VotingLayout};
use super::raft::RaftTimings;
use super::raft::host::{GroupHost, HostError, HostRole, HostSpec};
use super::raft::log_ids::{self, LockStart, RaftState, StartError, Waiting};
use super::raft::proposals::Proposals;
use super::raft::reads::Reads;
use super::raft::tailing::TailReads;
use super::raft::transport::tail::Clients;
use super::raft::transport::{Identity, Peers};
use super::raft::types::NodeId;
use super::replica_id::{self, ReplicaId, ReplicaIdError};
use super::route::Router;
use super::write::{COMMIT_TIMEOUT, GroupWriter};
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
    /// The querier identities that may read the tail, and make no other
    /// peer call (RFC0065.13).
    pub tail_clients: Vec<String>,
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
    /// Its configured `node_id`.
    name: String,
    layout: VotingLayout,
    peers: Peers,
    /// Whether it votes in its groups, rather than tailing them.
    voter: bool,
    /// The flushers' proposals it answers, and the reports its stewards
    /// top up id blocks from.
    proposals: Arc<Proposals>,
}

impl Node {
    /// Answer the peers' forwarded exports, authenticated by `auth` and with
    /// each tenant denial recorded on `denials` (RFC 0026 §3.4), the
    /// flushers' proposals, the tail replicas' read points and, on a tail
    /// replica, its tail's clients (RFC 0065 §6.7), then serve replication over mutual TLS on
    /// `incoming` through `acceptor` until `shutdown` (RFC 0065 §6.8, B21).
    pub fn serve(
        &self,
        (auth, denials): (AuthResolver, Box<dyn AuditSink + Send>),
        (incoming, acceptor): (TcpIncoming, ReloadingAcceptor),
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> JoinHandle<Result<(), tonic::transport::Error>> {
        let server = self.host.server();
        let writer = GroupWriter::new(self.layout.clone(), &self.host);
        let forwards = LeaderTarget::new(writer, auth).with_denial_audit_sink(denials);
        server.serve_forwards(Arc::new(forwards));
        server.serve_proposals(Arc::clone(&self.proposals) as _);
        server.serve_read_indexes(Arc::new(Reads::new(&self.host)));
        if !self.voter {
            server.serve_tail_target(Arc::new(TailReads::new(&self.host, self.peers.clone())));
        }
        tokio::spawn(server.serve(incoming, acceptor, shutdown))
    }

    /// The ingest the OTLP transports hand exports to: committed here for a
    /// group this node leads, and forwarded to its leader otherwise.
    #[must_use]
    pub fn ingest(&self) -> ReplicatedIngest {
        let router = Router::new(
            self.peers.clone(),
            self.layout.clone(),
            self.name.clone(),
            COMMIT_TIMEOUT,
        );
        ReplicatedIngest::new(GroupWriter::new(self.layout.clone(), &self.host)).with_router(router)
    }
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
        tail_clients,
    } = start;
    let Prepared {
        held: raft_state,
        replica,
        unchecked,
    } = {
        let (root, groups, store, layout) = (
            wal.root.clone(),
            groups.clone(),
            store.clone(),
            layout.clone(),
        );
        tokio::task::spawn_blocking(move || prepare(&root, &groups, &store, &layout))
            .await
            .map_err(|e| NodeError::RaftState(io::Error::other(e)))??
    };
    if unchecked {
        report_unchecked();
    }
    let lock = LockStart {
        store: &store,
        node_id: &node_id,
        raft_state,
        allow_bootstrap,
        scan: &scan,
        retry,
    };
    pass_lock(&lock, waiting).await?;
    let id = NodeId::from(replica.uuid());
    let voter = matches!(role, HostRole::Voter);
    let spec = HostSpec {
        id,
        wal,
        groups,
        role,
        peers: peers.clone(),
        timings,
    };
    let identity = Identity::new(node_id.clone(), id);
    let host = open_host(spec, identity, Clients::only(tail_clients)).await?;
    let proposals = Arc::new(Proposals::new(&host));
    Ok(Node {
        id,
        host,
        name: node_id,
        layout,
        peers,
        voter,
        proposals,
    })
}

/// Open the groups `spec` describes, their peer listener answering as
/// `identity` and restricting `tail_clients` to the tail calls.
async fn open_host(
    spec: HostSpec,
    identity: Identity,
    tail_clients: Clients,
) -> Result<GroupHost, NodeError> {
    let host = GroupHost::open(spec).await.map_err(NodeError::Host)?;
    let server = host.server();
    server.identify_as(identity);
    server.admit_tail_clients(tail_clients);
    Ok(host)
}

/// Pass the bootstrap lock, reporting each new reason it waits.
async fn pass_lock(
    lock: &LockStart<'_>,
    waiting: &mut impl FnMut(&Waiting),
) -> Result<(), NodeError> {
    let mut once = background::Once::default();
    let mut report = |why: &Waiting| {
        if let Some(reason) = once.changed(Some(why.as_str())) {
            background::report_waiting(None, reason, why);
        }
        waiting(why);
    };
    log_ids::start(lock, &mut report)
        .await
        .map(drop)
        .map_err(NodeError::LogIds)
}

/// What would refuse a start of the node under `wal_root`, checked writing
/// nothing, so a server can refuse before it binds: a voting layout other
/// than the recorded one, an identity it cannot read. As at the start, a
/// node holding Raft state in any of `groups` passes with the layout
/// unreadable (RFC0065.31); `groups` is listed only then, so a readable
/// layout is checked before anything is read per group.
///
/// # Errors
///
/// The [`NodeError`] the start would fail with.
pub fn preflight(
    wal_root: &Path,
    groups: impl FnOnce() -> Vec<u32>,
    store: &Store,
    layout: &VotingLayout,
) -> Result<(), NodeError> {
    match layout::check_recorded(store, layout) {
        Err(LayoutError::Store { op: "read", .. })
            if log_ids::raft_state(wal_root, &groups()).map_err(NodeError::RaftState)?
                == RaftState::Held => {}
        checked => checked.map_err(NodeError::Layout)?,
    }
    replica_id::load_existing(wal_root).map_err(NodeError::Identity)?;
    Ok(())
}

/// The steps before the lock, blocking, in the order that leaves the
/// least behind a start that fails: whether this node holds Raft state,
/// then the voting layout, then its identity.
fn prepare(
    wal_root: &Path,
    groups: &[u32],
    store: &Store,
    layout: &VotingLayout,
) -> Result<Prepared, NodeError> {
    let held = log_ids::raft_state(wal_root, groups).map_err(NodeError::RaftState)?;
    let unchecked = match (layout::record_or_check(store, layout), held) {
        // RFC0065.31: holding its groups' state, it opens them with the store
        // unreachable; a start that can read the layout checks it. A layout
        // it read but could not record still fails it.
        (Err(LayoutError::Store { op: "read", .. }), RaftState::Held) => true,
        (checked, _) => {
            checked.map_err(NodeError::Layout)?;
            false
        }
    };
    let identity = replica_id::load_or_create(wal_root).map_err(NodeError::Identity)?;
    Ok(Prepared {
        held,
        replica: identity.id(),
        unchecked,
    })
}

/// What the steps before the lock found.
struct Prepared {
    held: RaftState,
    replica: ReplicaId,
    /// The layout was not checked: the store could not be read.
    unchecked: bool,
}

/// Report a start that did not check its voting layout.
fn report_unchecked() {
    tracing::warn!(
        name: ourios_semconv::EVENT_OURIOS_WAL_REPLICATION_LAYOUT_UNCHECKED,
        { { crate::metrics::ERROR_TYPE } = "store_read" },
        "the voting layout is not checked: the store cannot be read, and this node \
         opens the groups whose Raft state it holds"
    );
}

mod background;

pub use background::{Background, FORMATION, STEWARD_INTERVAL};

#[cfg(test)]
mod serving_tests;
#[cfg(test)]
mod tests;
