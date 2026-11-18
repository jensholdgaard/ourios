//! `ReadIndex` on the in-process cluster (RFC 0065 §6.7): only the group's
//! leader gives a read point, confirmed with a quorum and at or past
//! every entry it committed; any other replica names the leader.

use std::sync::Arc;
use std::time::Duration;

use super::super::super::mining::GroupLogRead as _;
use super::super::super::mining::tests::{batch_at, block, corpus, time};
use super::super::super::reads::Reads;
use super::super::super::telemetry::Meters;
use super::super::super::transport::read_index::Outcome;
use super::super::Shape;
use super::mining::{commit_all, with_learners};
use super::{Cluster, GROUP, name};

const PATIENCE: Duration = Duration::from_secs(5);

/// A group of three voters and two tail learners, every node answering
/// reads; its leader and the index of the last entry it committed.
async fn reading() -> (Cluster, usize, u64) {
    let shape = Shape::from((3, 2));
    let (cluster, _mined) = Cluster::start_mining(shape, &[GROUP], |_| Meters::global()).await;
    for i in 0..5 {
        let host = &cluster.node(i).host;
        host.server()
            .serve_read_indexes(Arc::new(Reads::new(host)) as _);
    }
    let leader = with_learners(&cluster, 3..5).await;
    let mut entries = vec![block(0, 10_000)];
    let lines = corpus();
    entries.extend(
        lines[..10]
            .chunks(5)
            .enumerate()
            .map(|(n, chunk)| batch_at(time(n as u64 + 1), "acme", chunk)),
    );
    let last = commit_all(&cluster, leader, &entries).await;
    (cluster, leader, last)
}

async fn read(cluster: &Cluster, from: usize) -> Outcome {
    cluster
        .peers()
        .read_index(&name(cluster.id(from)), GROUP, PATIENCE)
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_leader_gives_a_read_point_at_or_past_what_it_committed() {
    let (cluster, leader, last) = reading().await;

    let outcome = read(&cluster, leader).await;

    let Outcome::Read(point) = outcome else {
        panic!("a read point: {outcome:?}");
    };
    assert!(point.index >= last, "{point:?} is behind {last}");
    let log = cluster.node(leader).host.log(GROUP).expect("hosted");
    assert!(log.entry(point.index).is_some(), "the point is in the log");
    cluster.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_follower_and_a_learner_name_the_leader_instead() {
    let (cluster, leader, _) = reading().await;
    let leader_name = Some(name(cluster.id(leader)));
    let follower = (0..3).find(|&i| i != leader).expect("a follower");

    for other in [follower, 3] {
        assert_eq!(
            read(&cluster, other).await,
            Outcome::NotLeader {
                leader: leader_name.clone()
            },
            "node {other}"
        );
    }
    cluster.shutdown().await;
}

/// A leader that has lost its quorum, though it still believes it leads
/// and still answers, gives no read point: the next read is unavailable,
/// never one a partitioned leader could serve stale.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_leader_without_its_quorum_gives_no_read_point() {
    let (mut cluster, leader, _) = reading().await;
    for follower in (0..3).filter(|&i| i != leader) {
        cluster.kill(follower).await;
    }

    let outcome = cluster
        .peers()
        .read_index(&name(cluster.id(leader)), GROUP, Duration::from_secs(1))
        .await;

    assert!(matches!(outcome, Outcome::Unavailable(_)), "{outcome:?}");
    cluster.shutdown().await;
}

/// A leader whose raft has stopped refuses as unconfirmed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stopped_leader_refuses_as_unconfirmed() {
    use super::super::super::transport::read_index::{ReadIndexTarget as _, Refusal};

    let (cluster, leader, _) = reading().await;
    let host = &cluster.node(leader).host;
    let reads = Reads::new(host);
    host.shutdown().await.expect("shutdown");

    let refused = reads.read_index(GROUP).await;

    assert!(
        matches!(refused, Err(Refusal::Unconfirmed(_))),
        "{refused:?}"
    );
    cluster.shutdown().await;
}

/// A tail replica reports how far its miner has followed the log, which a
/// read point waits on: through the last entry the group committed, once
/// it has mined it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tail_replica_reports_how_far_it_has_mined() {
    let (cluster, _, last) = reading().await;
    let mut through = cluster
        .node(3)
        .host
        .mined_through(GROUP)
        .expect("a tail replica mines");

    tokio::time::timeout(PATIENCE, through.wait_for(|mined| *mined >= Some(last)))
        .await
        .expect("mined in time")
        .expect("still mining");
    assert!(
        cluster.node(0).host.mined_through(GROUP).is_none(),
        "a voter mines nothing"
    );
    cluster.shutdown().await;
}

/// A tail replica's read point (RFC 0065 §6.7): confirmed by its leader,
/// and only once the replica has applied and mined through it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tail_replica_reads_from_a_confirmed_point_it_has_mined_through() {
    use super::super::super::read_point::read_point;

    let (cluster, _, last) = reading().await;
    let tail = &cluster.node(3).host;

    let point = read_point(tail, cluster.peers(), GROUP, PATIENCE)
        .await
        .expect("a read point");

    assert!(point >= last, "{point} is behind {last}");
    let mined = *tail.mined_through(GROUP).expect("mining").borrow();
    assert!(mined >= Some(point), "mined through {mined:?}, not {point}");
    cluster.shutdown().await;
}

/// Without its leader's quorum a tail replica has no read point, rather
/// than one it could serve stale.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tail_replica_without_a_confirmed_point_is_unavailable() {
    use super::super::super::read_point::read_point;

    let (mut cluster, leader, _) = reading().await;
    for follower in (0..3).filter(|&i| i != leader) {
        cluster.kill(follower).await;
    }

    let point = read_point(
        &cluster.node(3).host,
        cluster.peers(),
        GROUP,
        Duration::from_secs(1),
    )
    .await;

    assert!(point.is_err(), "{point:?}");
    cluster.shutdown().await;
}
