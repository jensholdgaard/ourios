//! Tail replicas mining their group on the in-process cluster (RFC 0065
//! §6.5): two tail replicas mine the same rows and audit events, a voter
//! mines nothing, and a miner that stops is counted while its replica
//! applies on.

use std::time::{Duration, SystemTime};

use ourios_semconv as semconv;
use tokio::sync::mpsc;

use super::super::super::host::GroupMined;
use super::super::super::mining::Mined;
use super::super::super::mining::tests::{batch_at, block, configured, corpus, time};
use super::super::super::telemetry::Meters;
use super::super::super::telemetry::tests::{Reader, until};
use super::super::super::types::GroupEntry;
use super::{Cluster, GROUP, name, propose, write};

/// Commit the group's first miner configuration, which also waits until
/// its leader takes writes, then add nodes `tail` as learners through
/// that leader, and return it.
async fn with_learners(cluster: &Cluster, tail: std::ops::Range<usize>) -> usize {
    let leader = cluster.leader(GROUP, None).await;
    propose(cluster.node(leader).raft(GROUP), configured(0.7)).await;
    for i in tail {
        let id = cluster.id(i);
        cluster
            .node(leader)
            .raft(GROUP)
            .add_learner(id, name(id), true)
            .await
            .expect("learner added");
    }
    leader
}

/// Commit `entries` through node `leader`, returning the last index.
async fn commit_all(cluster: &Cluster, leader: usize, entries: &[GroupEntry]) -> u64 {
    let mut last = 0;
    for entry in entries {
        last = propose(cluster.node(leader).raft(GROUP), entry.clone()).await;
    }
    last
}

/// What `mined` hands on until it holds `rows` rows; fails after 10 s.
async fn mined_rows(mined: &mut mpsc::Receiver<GroupMined>, rows: usize) -> Vec<Mined> {
    let mut received = Vec::new();
    let mut held = 0;
    while held < rows {
        let next = tokio::time::timeout(Duration::from_secs(10), mined.recv()).await;
        let Ok(Some(GroupMined { group, mined })) = next else {
            panic!("{held} of {rows} rows mined: {next:?}");
        };
        assert_eq!(group, GROUP);
        held += mined.rows.len();
        received.push(mined);
    }
    assert_eq!(held, rows, "no more rows than were written");
    received
}

/// RFC0065.20's id-block and second-tenant arms on a running group: the
/// group's two tail replicas mine every row alike, `template_id`
/// included, and emit the same audit events, each stamped with the time
/// of the batch that emitted it; the voters, the leader among them, mine
/// nothing (RFC0065.23).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_tail_replicas_mine_the_same_rows_and_audit_events() {
    let (cluster, mut mined) = Cluster::start_mining((3, 2), &[GROUP], |_| Meters::global()).await;
    let leader = with_learners(&cluster, 3..5).await;
    let lines = corpus();
    let (first, second) = lines[..60].split_at(30);
    let mut entries = vec![block(0, 4)];
    let mut times = Vec::new();
    for (n, (tenant, chunk)) in (1..).zip(
        first
            .chunks(5)
            .map(|c| ("acme", c))
            .chain(second.chunks(5).map(|c| ("globex", c))),
    ) {
        if n == 3 {
            entries.push(block(4, 10_000));
        }
        let at = time(1_700_000_000_000_000_000 + n);
        entries.push(batch_at(at, tenant, chunk));
        times.push(at.system_time());
    }
    commit_all(&cluster, leader, &entries).await;

    let one = mined_rows(&mut mined[0], 60).await;
    let other = mined_rows(&mut mined[1], 60).await;
    assert_eq!(one, other, "the two tail replicas mined alike");
    let rows: Vec<_> = one.iter().flat_map(|m| &m.rows).collect();
    assert!(
        rows.iter().any(|row| row.record.template_id > 4),
        "the second block was drawn"
    );
    let stamps: Vec<SystemTime> = one
        .iter()
        .flat_map(|m| m.audit.iter().map(|event| event.timestamp))
        .collect();
    assert!(!stamps.is_empty(), "the miners emitted audit events");
    assert!(
        stamps.iter().all(|stamp| times.contains(stamp)),
        "every audit event carries a batch's time"
    );
    for i in 0..3 {
        assert!(!cluster.node(i).host.mines(GROUP), "voter {i} mines");
    }
    for i in 3..5 {
        assert!(cluster.node(i).host.mines(GROUP), "tail replica {i}");
    }
    cluster.shutdown().await;
}

/// A batch the miner cannot decode stops it, counted once with the
/// registered `error.type`, and the tail replica applies on without it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_undecodable_batch_stops_the_miner_and_its_replica_applies_on() {
    let reader = Reader::new();
    let meters = |i| match i {
        3 => Meters::on(&reader.meter()),
        _ => Meters::global(),
    };
    let (cluster, mut mined) = Cluster::start_mining((3, 1), &[GROUP], meters).await;
    let leader = with_learners(&cluster, 3..4).await;
    let lines = corpus();
    let good = batch_at(time(1), "acme", &lines[..5]);
    commit_all(&cluster, leader, &[block(0, 100), good]).await;
    mined_rows(&mut mined[0], 5).await;

    write(cluster.node(leader).raft(GROUP), 1).await;
    let after = batch_at(time(2), "acme", &lines[5..10]);
    let last = commit_all(&cluster, leader, &[after]).await;
    cluster.node(3).applied_through(GROUP, last).await;
    let stops = semconv::OURIOS_WAL_REPLICATION_STOPS;
    let expected = (["undecodable", "0", "miner"].map(str::to_owned).to_vec(), 1);
    until(&reader, stops, |points| points == [expected.clone()]).await;
    assert!(mined[0].try_recv().is_err(), "nothing mined past the stop");
    cluster.shutdown().await;
}
