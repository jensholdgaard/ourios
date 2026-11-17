//! §3.4 on a follower (#1339): an entry a follower has acknowledged, which
//! is what the leader records as its match, survives that follower's crash,
//! even one taken while appends are still arriving: a crash keeps only what
//! each WAL synced.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use openraft::async_runtime::WatchReceiver as _;

use super::{export, followers, ready_leader, writer};
use crate::replicated::ingest::ReplicatedIngest;
use crate::replicated::raft::cluster::Cluster;

/// The follower `follower`'s match as leader `leader`'s Raft records it.
fn matched(cluster: &Cluster, leader: usize, follower: usize) -> Option<u64> {
    let metrics = cluster
        .node(leader)
        .raft(0)
        .metrics()
        .borrow_watched()
        .clone();
    let replication = metrics.replication?;
    replication.get(&cluster.id(follower))?.map(|id| id.index)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_follower_crashed_mid_append_keeps_every_entry_it_acknowledged() {
    for round in 0..5 {
        let mut cluster = Cluster::start(3, &[0]).await;
        let leader = ready_leader(&cluster).await;
        let follower = followers(leader)[0];
        let ingest = ReplicatedIngest::new(
            writer(&cluster, leader).with_timeout(Duration::from_millis(500)),
        );
        let writing = Arc::new(AtomicBool::new(true));
        let still = Arc::clone(&writing);
        let writes = tokio::spawn(async move {
            let mut n = 0;
            while still.load(Ordering::SeqCst) {
                // A write the other follower alone still commits.
                let _ = ingest.ingest(export(&format!("line {n}"))).await;
                n += 1;
            }
        });
        tokio::time::sleep(Duration::from_millis(200)).await;

        let acknowledged = matched(&cluster, leader, follower);
        assert!(acknowledged.is_some(), "round {round}: no match recorded");
        cluster.crash(follower).await;
        cluster.restart(follower).await;
        let held = cluster
            .node(follower)
            .log(0)
            .await
            .last()
            .map(|e| e.log_id.index);
        writing.store(false, Ordering::SeqCst);
        writes.await.expect("writes");

        assert!(
            held >= acknowledged,
            "round {round}: the follower acknowledged {acknowledged:?} and reopened through {held:?}"
        );
        cluster.shutdown().await;
    }
}
