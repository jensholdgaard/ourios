//! A tail replica's read point (RFC 0065 §6.7) across a leader change and
//! a restore from a snapshot (#1366).

use std::sync::Arc;

use openraft::async_runtime::WatchReceiver as _;
use std::time::Duration;

use super::super::super::mining::tests::{batch_at, corpus, time};
use super::super::super::read_point::read_point;
use super::super::super::reads::Reads;
use super::super::super::transport::read_index::{ReadIndexTarget as _, Refusal};
use super::super::Cluster;
use super::GROUP;
use super::mining::commit_all;
use super::publication::mining_group;
use super::read_index::{PATIENCE, reading};
use super::restore::snapshot_at_the_checkpoint;

/// Node `i` answers `ReadIndex`, as a running node does.
fn serve_reads(cluster: &Cluster, i: usize) {
    let host = &cluster.node(i).host;
    host.server()
        .serve_read_indexes(Arc::new(Reads::new(host)) as _);
}

/// Wait until tail replica `tail` knows node `leader` leads the group.
async fn knows_leader(cluster: &Cluster, tail: usize, leader: usize) {
    let id = cluster.id(leader);
    cluster
        .node(tail)
        .raft(GROUP)
        .wait(Some(PATIENCE))
        .metrics(|m| m.current_leader == Some(id), "the new leader known")
        .await
        .expect("the tail replica learns the new leader");
}

/// #1366: once the leader that confirmed a read point is deposed, a tail
/// replica's next read point is the new leader's, never behind the first,
/// and the deposed leader, back as a follower, confirms none of its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_read_after_the_leader_is_deposed_is_the_new_leaders() {
    let (mut cluster, leader, _) = reading().await;
    let tail = 3;
    let first = read_point(&cluster.node(tail).host, cluster.peers(), GROUP, PATIENCE)
        .await
        .expect("a read point");

    cluster.kill(leader).await;
    let elected = cluster.leader(GROUP, Some(leader)).await;
    knows_leader(&cluster, tail, elected).await;
    // A commit only the new leader can have made: a point that covers it
    // is the new leader's, never the old one's again.
    let lines = corpus();
    let after = vec![batch_at(time(9), "acme", &lines[10..15])];
    let since = commit_all(&cluster, elected, &after).await;
    let second = read_point(&cluster.node(tail).host, cluster.peers(), GROUP, PATIENCE)
        .await
        .expect("a read point from the new leader");
    assert!(
        second >= since && since > first,
        "{second} does not cover {since}"
    );

    cluster.restart(leader).await;
    serve_reads(&cluster, leader);
    let deposed = Reads::new(&cluster.node(leader).host)
        .read_index(GROUP)
        .await;
    assert!(
        matches!(
            deposed,
            Err(Refusal::NotLeader { .. } | Refusal::Unconfirmed(_))
        ),
        "{deposed:?}"
    );
    cluster.shutdown().await;
}

/// #1366: a tail replica that reopens from a snapshot reads from a point
/// at or past it, and only once its restored miner has mined through all
/// it was handed: the point never runs ahead of what it can serve.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tail_replica_restored_from_a_snapshot_reads_only_what_it_has_mined() {
    let (mut cluster, stores, last, (leader, _), _mined) = mining_group().await;
    for i in 0..5 {
        serve_reads(&cluster, i);
    }
    snapshot_at_the_checkpoint(&cluster, &stores[0], last).await;
    let snapshot = cluster
        .node(3)
        .raft(GROUP)
        .metrics()
        .borrow_watched()
        .snapshot
        .map(|s| s.index)
        .expect("a snapshot");

    cluster.kill(3).await;
    tokio::time::timeout(Duration::from_secs(20), cluster.restart(3))
        .await
        .expect("the tail replica reopens");
    serve_reads(&cluster, 3);
    let lines = corpus();
    let next: Vec<_> = lines[30..40]
        .chunks(5)
        .enumerate()
        .map(|(n, chunk)| batch_at(time(n as u64 + 7), "acme", chunk))
        .collect();
    let committed = commit_all(&cluster, leader, &next).await;

    // What it was handed through the last commit, fixed before the read:
    // a checkpoint applied afterwards moves `handed` on, but the read point
    // only answers for what was handed by the time it confirmed.
    let host = &cluster.node(3).host;
    host.raft(GROUP)
        .expect("hosted")
        .wait(Some(Duration::from_secs(20)))
        .applied_index_at_least(Some(committed), "applied the last commit")
        .await
        .expect("applied");
    let handed = host.state_machine(GROUP).expect("hosted").handed_through();
    let point = read_point(host, cluster.peers(), GROUP, Duration::from_secs(20))
        .await
        .expect("a read point once restored");
    assert!(
        point >= snapshot.max(committed),
        "{point} is behind the restore"
    );
    let mined = *host.mined_through(GROUP).expect("mining").borrow();
    assert!(
        mined >= handed,
        "served at {point} with {mined:?} mined of {handed:?}"
    );
    cluster.shutdown().await;
}
