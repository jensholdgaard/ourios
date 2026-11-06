//! Groups on the in-process cluster: log repair after a failover, and a
//! crash under §8's model.

use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;

use super::super::types::Entry;
use super::{Cluster, batch, write};

const GROUP: u32 = 0;

/// Write `ns` through node `i`, returning the last index.
async fn commit(cluster: &Cluster, i: usize, ns: std::ops::Range<u64>) -> u64 {
    let mut last = 0;
    for n in ns {
        last = write(cluster.node(i).raft(GROUP), n).await;
    }
    last
}

/// Cut node `i` off from `others`, have it append an entry it cannot
/// commit, and return that entry and the write's task.
async fn strand(
    cluster: &Cluster,
    i: usize,
    others: &[usize],
    after: u64,
) -> (Entry, JoinHandle<Result<(), String>>) {
    for &other in others {
        cluster.cut(i, other);
    }
    let raft = cluster.node(i).raft(GROUP).clone();
    let write = tokio::spawn(async move {
        raft.client_write(batch(100))
            .await
            .map(drop)
            .map_err(|e| e.to_string())
    });
    loop {
        let log = cluster.node(i).log(GROUP).await;
        if let Some(last) = log.last().filter(|e| e.log_id.index > after) {
            return (last.clone(), write);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Scenario RFC0065.7 — a diverged follower is repaired (B6).
/// A leader cut off from both followers appends an entry it can never
/// commit and is killed; the others elect a leader and commit past it.
/// The old leader comes back holding that entry; once its links heal its
/// log converges to the new leader's and no committed entry is removed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0065_7_a_diverged_follower_is_repaired() {
    let mut cluster = Cluster::start(3, &[GROUP]).await;
    let old = cluster.leader(GROUP, None).await;
    let committed = commit(&cluster, old, 0..3).await;
    for i in 0..3 {
        cluster.node(i).applied_through(GROUP, committed).await;
    }
    let before = cluster.node(old).log(GROUP).await;
    assert_eq!(before.last().map(|e| e.log_id.index), Some(committed));

    let others: Vec<usize> = (0..3).filter(|&i| i != old).collect();
    let (diverged, stranded) = strand(&cluster, old, &others, committed).await;
    cluster.kill(old).await;
    let stranded = tokio::time::timeout(Duration::from_secs(10), stranded)
        .await
        .expect("the stranded write ends")
        .expect("join");
    assert!(stranded.is_err(), "the cut-off write never committed");

    let new = cluster.leader(GROUP, Some(old)).await;
    let last = commit(&cluster, new, 3..6).await;
    cluster.restart(old).await;
    let held = cluster.node(old).log(GROUP).await;
    assert!(
        held.contains(&diverged),
        "the returning replica holds the uncommitted entry"
    );

    for &i in &others {
        cluster.heal(old, i);
    }
    cluster.node(old).applied_through(GROUP, last).await;
    let repaired = cluster.node(old).log(GROUP).await;
    assert_eq!(repaired, cluster.node(new).log(GROUP).await);
    assert!(
        !repaired.contains(&diverged),
        "the uncommitted entry is gone"
    );
    for entry in &before {
        let kept = repaired
            .iter()
            .find(|e| e.log_id.index == entry.log_id.index);
        assert_eq!(kept, Some(entry), "committed index {}", entry.log_id.index);
    }
    cluster.shutdown().await;
}

/// A follower crashed with appends written but not fsynced loses only
/// those, restarts from what it fsynced, and catches up.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crashed_follower_restarts_from_its_fsynced_log_and_catches_up() {
    let mut cluster = Cluster::start(3, &[GROUP]).await;
    let leader = cluster.leader(GROUP, None).await;
    let follower = (leader + 1) % 3;
    let first = write(cluster.node(leader).raft(GROUP), 0).await;
    cluster.node(follower).applied_through(GROUP, first).await;

    let gate = Arc::clone(&cluster.node(follower).gate);
    gate.close();
    let last = commit(&cluster, leader, 1..5).await;
    tokio::task::spawn_blocking(move || gate.wait_for_writer())
        .await
        .expect("join");
    let dropped = cluster.crash(follower).await;
    assert!(dropped > 0, "the held appends were on disk, unsynced");

    cluster.restart(follower).await;
    cluster.node(follower).applied_through(GROUP, last).await;
    assert_eq!(
        cluster.node(follower).log(GROUP).await,
        cluster.node(leader).log(GROUP).await
    );
    cluster.shutdown().await;
}
