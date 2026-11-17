//! The replicated WAL's node (RFC 0065) from the receiver's configuration:
//! what it starts from, what its background work needs, and, on a tail
//! replica, where its groups' mined output arrives.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use ourios_ingester::replicated::layout::VotingLayout;
use ourios_ingester::replicated::node::{Background, NodeStart};
use ourios_ingester::replicated::raft::flushing::Publishing;
use ourios_ingester::replicated::raft::host::{GroupMined, HostRole};
use ourios_ingester::replicated::raft::kept::Bounds;
use ourios_ingester::replicated::raft::learners::TailPool;
use ourios_ingester::replicated::raft::transport::Peers;
use ourios_ingester::template_ids::ScanOptions;
use ourios_server::config::wal::{NodeRole, Peer, ReplicatedWal};
use ourios_serving::tls::TlsSettings;
use tokio::sync::mpsc;

use crate::receiver::ReceiverConfig;

/// How long a start holding no Raft state waits between passes of the
/// bootstrap lock.
const LOCK_RETRY: Duration = Duration::from_secs(1);

/// How many groups' mined output a tail replica holds before its miners
/// wait for it to be taken.
const MINED_CAPACITY: usize = 64;

/// A replicated node, planned.
pub(crate) struct Plan {
    pub(crate) start: NodeStart,
    pub(crate) background: Background,
    /// Where a tail replica's groups' mined output arrives.
    pub(crate) mined: Option<mpsc::Receiver<GroupMined>>,
}

/// The node `replicated` and `config` describe, its template-id scan
/// stopping once `shutdown` is set.
///
/// # Errors
///
/// The peer TLS settings do not load.
pub(crate) fn plan(
    replicated: &ReplicatedWal,
    config: &ReceiverConfig,
    shutdown: Arc<AtomicBool>,
) -> Result<Plan, String> {
    let ReplicatedWal {
        node_id,
        role,
        peers,
        tail_peers,
        groups,
        replication_factor,
        tail_replicas,
        tls,
        raft_timings,
        ..
    } = replicated;
    let ids =
        |peers: &[Peer]| -> Vec<String> { peers.iter().map(|peer| peer.id.clone()).collect() };
    let layout = VotingLayout::new(ids(peers), *groups, replication_factor.voters());
    let placed = placement(
        &TailPool::new(ids(tail_peers), tail_replicas.get()),
        groups.get(),
    );
    let Hosting {
        groups: hosted,
        role: host_role,
        mined,
    } = match role {
        NodeRole::Voter => Hosting {
            groups: layout.groups_of(node_id),
            role: HostRole::Voter,
            mined: None,
        },
        NodeRole::TailReplica => tail(
            node_id,
            &placed,
            &publishing((&layout, groups.get()), config),
        ),
    };
    let pool = peer_pool(peers.iter().chain(tail_peers), tls)?;
    let start = NodeStart {
        node_id: node_id.clone(),
        wal: config.wal.clone(),
        layout,
        groups: hosted,
        role: host_role,
        peers: pool,
        timings: *raft_timings,
        store: config.store.clone(),
        allow_bootstrap: config.template_ids_allow_bootstrap,
        scan: ScanOptions {
            shutdown,
            ..ScanOptions::default()
        },
        retry: LOCK_RETRY,
    };
    let background = Background {
        store: config.store.clone(),
        miner: config.miner,
        placed,
    };
    Ok(Plan {
        start,
        background,
        mined,
    })
}

/// Each of the `groups`' tail replicas, in placement order.
fn placement(tails: &TailPool, groups: u32) -> BTreeMap<u32, Vec<String>> {
    (0..groups)
        .map(|group| {
            let placed = tails.placed(group).into_iter().map(str::to_owned).collect();
            (group, placed)
        })
        .collect()
}

/// The groups a node hosts, as what, and where their mined output arrives.
struct Hosting {
    groups: Vec<u32>,
    role: HostRole,
    mined: Option<mpsc::Receiver<GroupMined>>,
}

/// Tail replica `node_id`: the groups `placed` puts it in, publishing as
/// `publishing` says when named a group's flusher.
fn tail(node_id: &str, placed: &BTreeMap<u32, Vec<String>>, publishing: &Publishing) -> Hosting {
    let groups = placed
        .iter()
        .filter(|(_, placed)| placed.iter().any(|replica| replica == node_id))
        .map(|(group, _)| *group)
        .collect();
    let (mined, receiving) = mpsc::channel(MINED_CAPACITY);
    Hosting {
        groups,
        role: HostRole::Tail {
            mined,
            kept: Bounds::default(),
            publishing: Some(Arc::new(publishing.clone())),
        },
        mined: Some(receiving),
    }
}

/// The mutual-TLS pool reaching every peer, voter and tail, at its address.
fn peer_pool<'a>(
    peers: impl Iterator<Item = &'a Peer>,
    tls: &TlsSettings,
) -> Result<Peers, String> {
    let addresses = peers.map(|peer| (peer.id.clone(), format!("{}:{}", peer.host, peer.port)));
    Peers::mutual_tls(tls)
        .map(|pool| pool.with_addresses(addresses))
        .map_err(|e| format!("receiver.wal.replicated.tls: {e}"))
}

/// How a tail replica named its group's flusher publishes: to the data
/// store, each group under its voters, as the local receiver's sink does.
fn publishing((layout, groups): (&VotingLayout, u32), config: &ReceiverConfig) -> Publishing {
    let voters = (0..groups)
        .map(|group| {
            let voters = layout
                .voters(group)
                .into_iter()
                .map(str::to_owned)
                .collect();
            (group, voters)
        })
        .collect();
    Publishing {
        store: config.store.clone(),
        voters,
        flush: crate::receiver::flush_config(config.max_buffer_age),
        promoted: config.promoted.clone(),
        audit_ceiling: crate::receiver::AUDIT_SINK_CEILING_EVENTS,
    }
}

#[cfg(test)]
mod tests;
