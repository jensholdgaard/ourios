//! Failover on the write path: RFC0065.3's takeover arm, RFC0065.4,
//! RFC0065.5 and RFC0065.6. Each ends by crashing every node and checking
//! that every acknowledged export survived on at least two (§3.4), so a
//! node these tests take down goes down by crash, never by a clean kill.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use openraft::EntryPayload;
use openraft::async_runtime::WatchReceiver as _;
use openraft::storage::RaftLogReader as _;

use super::{committed, export, followers, ready_leader, rejected_as, writer};
use crate::receiver::ReceiveError;
use crate::replicated::ingest::ReplicatedIngest;
use crate::replicated::raft::cluster::{Cluster, wal_config, write};
use crate::replicated::raft::log_store::{GroupLog, group_dir};
use crate::replicated::raft::types::Entry;
use crate::replicated::raft::vote_log::VoteLog;
use crate::replicated::write::GroupPosition;

/// An acknowledged export: the marker its record carries, and where it
/// committed.
type Acked = (String, GroupPosition);

/// Delimited, so no marker is a substring of another.
fn marker(n: u32) -> String {
    format!("<export {n}>")
}

fn carries(entry: &Entry, marker: &str) -> bool {
    match &entry.payload {
        EntryPayload::Normal(batch) => batch
            .as_bytes()
            .windows(marker.len())
            .any(|w| w == marker.as_bytes()),
        _ => false,
    }
}

fn holds(log: &[Entry], (marker, position): &Acked) -> bool {
    log.iter()
        .any(|e| e.log_id.index == position.seq && carries(e, marker))
}

async fn acked(ingest: &ReplicatedIngest, n: u32) -> Acked {
    let marker = marker(n);
    let ingested = ingest.ingest(export(&marker)).await;
    (marker, committed(ingested.expect("acknowledged")))
}

fn assert_retryable(error: &ReceiveError) {
    let kind = rejected_as(error);
    assert!(
        matches!(kind, "not_enough_replicas" | "group_unavailable"),
        "{kind} is not the retryable rejection"
    );
}

/// Group 0's entries in `dir`, as its last fsync left them.
async fn fsynced(dir: &Path) -> Vec<Entry> {
    let votes = VoteLog::open(dir).expect("vote log");
    let config = wal_config(&group_dir(dir, 0));
    let mut log = GroupLog::open(config, 0, votes).expect("group log");
    log.try_get_log_entries(..).await.expect("read")
}

/// §3.4: crash every node, then check each acknowledged export is in at
/// least two of the logs the crashes left.
async fn assert_two_fsynced_copies(cluster: &mut Cluster, acked: &[Acked]) {
    cluster.crash_all().await;
    let mut logs = Vec::new();
    for i in 0..3 {
        logs.push(fsynced(cluster.dir(i)).await);
    }
    for ack in acked {
        let copies = logs.iter().filter(|log| holds(log, ack)).count();
        assert!(
            copies >= 2,
            "{} at {:?} survived on {copies} nodes",
            ack.0,
            ack.1
        );
    }
}

/// RFC0065.3 (B2, B7), the takeover arm: the leader crashes while exports
/// are in flight, a follower takes over with no operator step, every
/// export acknowledged before the crash is in its log, and ingest resumes
/// on it. The Parquet and compaction arms wait for checkpoints.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0065_3_a_leader_crashed_mid_stream_is_replaced_with_every_ack() {
    let mut cluster = Cluster::start(3, &[0]).await;
    let old = ready_leader(&cluster).await;
    let ingest = Arc::new(ReplicatedIngest::new(writer(&cluster, old)));
    let acks = Arc::new(Mutex::new(Vec::new()));
    let senders: Vec<_> = (0..4)
        .map(|sender: u32| {
            let (ingest, acks) = (Arc::clone(&ingest), Arc::clone(&acks));
            tokio::spawn(async move {
                let mut n = sender * 100_000;
                loop {
                    let marker = marker(n);
                    match ingest.ingest(export(&marker)).await {
                        Ok(ingested) => acks
                            .lock()
                            .expect("acks")
                            .push((marker, committed(ingested))),
                        Err(e) => return e,
                    }
                    n += 1;
                }
            })
        })
        .collect();
    while acks.lock().expect("acks").len() < 20 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    cluster.crash(old).await;
    for sender in senders {
        assert_retryable(&sender.await.expect("join"));
    }
    let mut all = acks.lock().expect("acks").clone();

    let new = cluster.leader(0, Some(old)).await;
    write(cluster.node(new).raft(0), 0).await;
    let log = cluster.node(new).log(0).await;
    for ack in &all {
        assert!(holds(&log, ack), "{} is missing on the new leader", ack.0);
    }
    let resumed = ReplicatedIngest::new(writer(&cluster, new));
    for n in 0..5 {
        all.push(acked(&resumed, 900_000 + n).await);
    }
    assert_two_fsynced_copies(&mut cluster, &all).await;
}

/// RFC0065.4 (B5): a leader cut off from both followers while one of them
/// takes over commits nothing it queued once it is reconnected: every
/// such export gets the retryable rejection and none is in the new
/// leader's log. The cut stands in for the pause; the arbitrary-clock arm
/// is not exercised here.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0065_4_a_replaced_writer_cannot_acknowledge() {
    let mut cluster = Cluster::start(3, &[0]).await;
    let old = ready_leader(&cluster).await;
    let before = ReplicatedIngest::new(writer(&cluster, old));
    let mut all = Vec::new();
    for n in 0..3 {
        all.push(acked(&before, n).await);
    }
    for f in followers(old) {
        cluster.cut(old, f);
    }
    // Long enough that the queued turns are still waiting once the cut
    // heals, rather than timing out while it stands.
    let stale = Arc::new(ReplicatedIngest::new(
        writer(&cluster, old).with_timeout(Duration::from_secs(10)),
    ));
    let queued: Vec<_> = (100..105)
        .map(|n| {
            let stale = Arc::clone(&stale);
            tokio::spawn(async move { stale.ingest(export(&marker(n))).await })
        })
        .collect();

    let new = cluster.leader(0, Some(old)).await;
    write(cluster.node(new).raft(0), 0).await;
    let fresh = ReplicatedIngest::new(writer(&cluster, new));
    for n in 3..6 {
        all.push(acked(&fresh, n).await);
    }
    for f in followers(old) {
        cluster.heal(old, f);
    }
    for turn in queued {
        match turn.await.expect("join") {
            Ok(ingested) => panic!("the replaced leader acknowledged {ingested:?}"),
            Err(e) => assert_retryable(&e),
        }
    }
    let log = cluster.node(new).log(0).await;
    for n in 100..105 {
        assert!(
            !log.iter().any(|e| carries(e, &marker(n))),
            "the replaced leader's export {n} reached the new leader's log"
        );
    }
    assert_two_fsynced_copies(&mut cluster, &all).await;
}

/// RFC0065.5 (B6, B8): when only `a` and `b` hold every acknowledged
/// export and both go down, lagging `c` does not take over and its
/// exports get the retryable rejection; `b` takes over when it returns,
/// holding every one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0065_5_takeover_only_by_a_replica_with_every_ack() {
    let mut cluster = Cluster::start(3, &[0]).await;
    let a = ready_leader(&cluster).await;
    let [b, c] = followers(a)[..] else {
        unreachable!("three voters")
    };
    cluster.cut(a, c);
    cluster.cut(b, c);
    let ingest = ReplicatedIngest::new(writer(&cluster, a));
    let mut all = Vec::new();
    for n in 0..5 {
        all.push(acked(&ingest, n).await);
    }
    let lagging = cluster.node(c).log(0).await;
    assert!(all.iter().all(|ack| !holds(&lagging, ack)), "c lags");

    cluster.crash(a).await;
    cluster.crash(b).await;
    cluster.heal(a, c);
    cluster.heal(b, c);
    // Several election timeouts, with nothing to vote for c.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let seen = cluster
        .node(c)
        .raft(0)
        .metrics()
        .borrow_watched()
        .current_leader;
    assert_ne!(seen, Some(cluster.id(c)), "c took over without every ack");
    let refused = ReplicatedIngest::new(writer(&cluster, c))
        .ingest(export("refused"))
        .await
        .expect_err("c cannot commit");
    assert_retryable(&refused);

    cluster.restart(b).await;
    assert_eq!(cluster.leader(0, None).await, b, "b takes over");
    let log = cluster.node(b).log(0).await;
    for ack in &all {
        assert!(holds(&log, ack), "{} is missing on b", ack.0);
    }
    assert_two_fsynced_copies(&mut cluster, &all).await;
}

/// RFC0065.6 (B1, B3): with one follower down, acks continue on two
/// replicas; the returning follower catches up before it counts toward an
/// ack again; and no ack counted a replica that had not fsynced it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0065_6_losing_and_regaining_a_follower() {
    let mut cluster = Cluster::start(3, &[0]).await;
    let leader = ready_leader(&cluster).await;
    let [lost, kept] = followers(leader)[..] else {
        unreachable!("three voters")
    };
    let ingest = ReplicatedIngest::new(writer(&cluster, leader));
    let mut all = Vec::new();
    for n in 0..5 {
        all.push(acked(&ingest, n).await);
    }
    cluster.crash(lost).await;
    for n in 5..10 {
        all.push(acked(&ingest, n).await);
    }

    cluster.restart(lost).await;
    // With the other follower's fsyncs held, only the returning one can
    // complete an ack, and its log must first hold everything before it.
    cluster.node(kept).gate.close();
    let rejoined = acked(&ingest, 10).await;
    all.push(rejoined);
    let log = cluster.node(lost).log(0).await;
    for ack in &all {
        assert!(
            holds(&log, ack),
            "{} is missing on the returned follower",
            ack.0
        );
    }
    cluster.node(kept).gate.open();
    assert_two_fsynced_copies(&mut cluster, &all).await;
}
