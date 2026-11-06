//! Snapshot installs over gRPC, and the requests no leader sends.

use std::collections::BTreeMap;
use std::time::Duration;

use openraft::network::v2::RaftNetworkV2 as _;
use openraft::raft::VoteRequest;
use tonic::Code;

use super::super::super::cluster::{Node, name};
use super::super::super::types::{Membership, NodeId, RaftTypes, Vote};
use super::super::wire::{AppendRequest, SnapshotRequest, WireSnapshotResponse, WireVote};
use super::super::{INSTALL_SNAPSHOT, Peers, ReplicationServer, VERSION};
use super::{
    GROUP, append, bind, client_to, entry, header, log, option, raw, raw_append, silent_peer, start,
};

fn snapshot_of(
    last: openraft::type_config::alias::LogIdOf<RaftTypes>,
) -> openraft::type_config::alias::SnapshotOf<RaftTypes, ()> {
    let voters = std::collections::BTreeSet::from([NodeId::from(1), NodeId::from(2)]);
    let nodes = BTreeMap::from([
        (NodeId::from(1), name(NodeId::from(1))),
        (NodeId::from(2), name(NodeId::from(2))),
    ]);
    let membership = Membership::new(vec![voters], nodes).expect("membership");
    openraft::type_config::alias::SnapshotOf::<RaftTypes, ()> {
        meta: openraft::type_config::alias::SnapshotMetaOf::<RaftTypes> {
            last_log_id: Some(last),
            last_membership: openraft::StoredMembership::new(Some(last), membership),
        },
        snapshot: (),
    }
}

#[tokio::test]
async fn a_full_snapshot_installs_over_grpc() {
    let (incoming, addr) = bind();
    let peers = Peers::plaintext();
    let mut node = start(1, incoming, &peers, ReplicationServer::default()).await;
    let mut client = client_to(&peers, node.id, addr).await;
    let vote = Vote::new_committed(2, NodeId::from(2));
    let snapshot = snapshot_of(log(2, 5));
    let meta = snapshot.meta.clone();
    let never = std::future::pending::<openraft::errors::ReplicationClosed>();
    let response = client.full_snapshot(vote, snapshot, never, option()).await;
    assert_eq!(response.expect("installed").vote, vote);
    node.applied_through(GROUP, 5).await;
    let installed = {
        let sm = node.host.state_machine(GROUP).expect("hosted");
        (sm.applied(), sm.membership())
    };
    assert_eq!(installed, (meta.last_log_id, meta.last_membership));
    node.kill().await;
}

#[tokio::test]
async fn a_pending_snapshot_install_ends_when_replication_closes() {
    let (_listener, addr) = silent_peer();
    let mut client = client_to(&Peers::plaintext(), NodeId::from(1), addr).await;
    let closed = async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        openraft::errors::ReplicationClosed::new("replication stopped")
    };
    let vote = Vote::new_committed(2, NodeId::from(2));
    let response = client
        .full_snapshot(vote, snapshot_of(log(2, 5)), closed, option())
        .await;
    assert!(
        matches!(response, Err(openraft::errors::StreamingError::Closed(_))),
        "{response:?}"
    );
}

/// Requests a leader never sends, which `openraft` would assert on or
/// the group log fail on, are refused before dispatch and leave the group
/// serving.
#[tokio::test]
async fn requests_no_leader_sends_are_refused_and_the_group_keeps_serving() {
    let (incoming, addr) = bind();
    let peers = Peers::plaintext();
    let mut node = start(1, incoming, &peers, ReplicationServer::default()).await;
    let vote = Vote::new_committed(1, NodeId::from(2));
    let to = super::super::wire::Recipient {
        group: GROUP,
        node: node.id,
    };
    let after_vote = snapshot_of(log(9, 5)).meta;
    let mut membership_ahead = snapshot_of(log(1, 5)).meta;
    membership_ahead.last_membership = openraft::StoredMembership::new(
        Some(log(1, 6)),
        membership_ahead.last_membership.membership().clone(),
    );
    for (vote, meta) in [
        (vote, after_vote),
        (vote, membership_ahead),
        (Vote::new(1, NodeId::from(2)), snapshot_of(log(1, 5)).meta),
    ] {
        let snapshot = SnapshotRequest::new(to, &vote, &meta).expect("encode");
        let refused = raw::<_, WireSnapshotResponse>(addr, INSTALL_SNAPSHOT, snapshot).await;
        let code = refused.map(drop).map_err(|s| s.code());
        assert_eq!(code, Err(Code::InvalidArgument), "{vote} {meta:?}");
    }

    let uncommitted = AppendRequest {
        header: Some(header(VERSION)),
        vote: Some(WireVote::from(&Vote::new(1, NodeId::from(2)))),
        ..AppendRequest::default()
    };
    assert_eq!(
        raw_append(addr, uncommitted).await.code(),
        Code::InvalidArgument
    );

    let oversized = AppendRequest {
        header: Some(header(VERSION)),
        vote: Some(WireVote::from(&vote)),
        entries: vec![vec![0; ourios_wal::MAX_FRAME_BYTES + 1]],
        ..AppendRequest::default()
    };
    assert_eq!(
        raw_append(addr, oversized).await.code(),
        Code::InvalidArgument
    );

    let mut client = client_to(&peers, node.id, addr).await;
    let gap = append(vec![entry(3, 16)]);
    assert!(client.append_entries(gap, option()).await.is_err());

    let higher = VoteRequest::new(Vote::new(5, NodeId::from(2)), None);
    let granted = client.vote(higher, option()).await.expect("still serving");
    assert!(granted.vote_granted);
    node.kill().await;
}

/// A node restarts applied through a snapshot it installed, with its
/// membership, though `openraft` purged the log up to it.
#[tokio::test]
async fn an_installed_snapshot_survives_a_restart() {
    let (incoming, addr) = bind();
    let peers = Peers::plaintext();
    let mut node = start(1, incoming, &peers, ReplicationServer::default()).await;
    let mut client = client_to(&peers, node.id, addr).await;
    let vote = Vote::new_committed(2, NodeId::from(2));
    let snapshot = snapshot_of(log(2, 5));
    let meta = snapshot.meta.clone();
    let never = std::future::pending::<openraft::errors::ReplicationClosed>();
    let response = client.full_snapshot(vote, snapshot, never, option()).await;
    response.expect("installed");
    node.applied_through(GROUP, 5).await;
    node.kill().await;

    let replica = node.into_replica();
    let mut node = Node::open(replica, bind().0, &peers, ReplicationServer::default()).await;
    let restored = {
        let sm = node.host.state_machine(GROUP).expect("hosted");
        (sm.applied(), sm.membership())
    };
    assert_eq!(restored, (meta.last_log_id, meta.last_membership));
    node.kill().await;
}

/// An install whose record never became durable fails, so `openraft`
/// purges nothing and the node restarts without the snapshot.
#[tokio::test]
async fn an_install_whose_record_is_not_durable_fails_and_purges_nothing() {
    let (incoming, addr) = bind();
    let peers = Peers::plaintext();
    let mut node = start(1, incoming, &peers, ReplicationServer::default()).await;
    let mut client = client_to(&peers, node.id, addr).await;
    node.gate.crash();
    let vote = Vote::new_committed(2, NodeId::from(2));
    let never = std::future::pending::<openraft::errors::ReplicationClosed>();
    let response = client
        .full_snapshot(vote, snapshot_of(log(2, 5)), never, option())
        .await;
    assert!(response.is_err(), "{response:?}");
    node.kill().await;

    let replica = node.into_replica();
    let mut node = Node::open(replica, bind().0, &peers, ReplicationServer::default()).await;
    assert_eq!(
        node.host.state_machine(GROUP).expect("hosted").applied(),
        None
    );
    node.kill().await;
}
