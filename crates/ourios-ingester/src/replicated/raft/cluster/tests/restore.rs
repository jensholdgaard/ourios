//! RFC 0065 §6.5 *Recovery* on a running group: a tail replica that
//! reopens from a snapshot restores its miner from the bundle at the cut.

use std::time::Duration;

use openraft::RaftMetrics;

use super::super::super::mining::tests::{batch_at, corpus, time};
use super::GROUP;
use super::mining::{commit_all, mined_rows};
use super::publication::{applied_everywhere, checkpointed_through, mining_group};

/// RFC0065.31 on a running group: the named flusher, restarted once its
/// checkpoint's snapshot is built, restores from the bundle and mines
/// the group's next entries into the same rows, under the same template
/// ids, as the tail replica that mined straight through.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restarted_tail_replica_restores_from_the_bundle_and_mines_on() {
    let (mut cluster, stores, last, (leader, _), mut mined) = mining_group().await;
    let cut = checkpointed_through(&stores[0], last).await;
    applied_everywhere(&cluster, cut.seq).await;
    let built = cluster
        .node(3)
        .raft(GROUP)
        .wait(Some(Duration::from_secs(20)));
    let at_cut = |m: &RaftMetrics<_>| m.snapshot.is_some_and(|s| s.index >= cut.seq);
    built
        .metrics(at_cut, "a snapshot at the checkpoint")
        .await
        .expect("the named flusher builds a snapshot at its checkpoint");

    cluster.kill(3).await;
    tokio::time::timeout(Duration::from_secs(20), cluster.restart(3))
        .await
        .expect("the tail replica reopens");
    let lines = corpus();
    let next: Vec<_> = lines[30..40]
        .chunks(5)
        .enumerate()
        .map(|(n, chunk)| batch_at(time(n as u64 + 7), "acme", chunk))
        .collect();
    commit_all(&cluster, leader, &next).await;

    let restored = mined_rows(&mut mined[0], 10).await;
    let through = mined_rows(&mut mined[1], 10).await;
    assert_eq!(
        restored, through,
        "the restored miner mines what one that mined through does"
    );
    cluster.shutdown().await;
}
