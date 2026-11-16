//! `Propose` on the in-process cluster (RFC 0065 §6.8): the leader checks
//! a flusher's proposal in the RFC's order and appends the checkpoint it
//! accepts, which every replica then applies.

use std::sync::Arc;
use std::time::Duration;

use ourios_wal::Control;

use super::super::super::mining::GroupLogRead as _;
use super::super::super::mining::tests::{batch_at, block, corpus, time};
use super::super::super::proposals::Proposals;
use super::super::super::telemetry::Meters;
use super::super::super::transport::propose::{Outcome, ProposedCheckpoint, Report};
use super::super::super::types::{GroupEntry, LogId, NodeId};
use super::super::Shape;
use super::mining::{commit_all, with_learners};
use super::{Cluster, GROUP, name};

const COMMIT: Duration = Duration::from_secs(5);
/// The miner configuration's kind, the highest the group's log holds.
const HIGHEST_KIND: u8 = 0x04;

/// A group of three voters and two tail learners, node 3 named its
/// flusher, every node answering proposals; its leader, and the log id
/// of its last entry.
async fn proposing() -> (Cluster, usize, LogId) {
    let shape = Shape::from((3, 2));
    let (cluster, _mined) = Cluster::start_mining(shape, &[GROUP], |_| Meters::global()).await;
    for i in 0..5 {
        let host = &cluster.node(i).host;
        host.server()
            .serve_proposals(Arc::new(Proposals::new(host)));
    }
    let leader = with_learners(&cluster, 3..5).await;
    let named = *cluster.id(3).uuid().as_bytes();
    let mut entries = vec![
        block(0, 10_000),
        GroupEntry::Control(Control::FlusherAssignment(named)),
    ];
    let lines = corpus();
    entries.extend(
        lines[..20]
            .chunks(5)
            .enumerate()
            .map(|(n, chunk)| batch_at(time(n as u64 + 1), "acme", chunk)),
    );
    let last = commit_all(&cluster, leader, &entries).await;
    let log = cluster.node(leader).host.log(GROUP).expect("hosted");
    let cut = log.entry(last).expect("the last entry").log_id;
    (cluster, leader, cut)
}

fn report(proposer: NodeId, cut: LogId) -> Report {
    Report {
        proposer,
        mined: Some(cut),
        unstarted: 0,
        checkpoint: Some(ProposedCheckpoint {
            cut,
            highest_kind: HIGHEST_KIND,
        }),
    }
}

async fn propose(cluster: &Cluster, to: usize, report: &Report) -> Outcome {
    cluster
        .peers()
        .propose(&name(cluster.id(to)), report.request(GROUP), COMMIT)
        .await
}

/// Every node's checkpoint cut once it reaches `cut`; fails after 10 s.
async fn applied_everywhere(cluster: &Cluster, cut: LogId) {
    for i in 0..5 {
        let sm = cluster.node(i).host.state_machine(GROUP).expect("hosted");
        let mut cuts = sm.checkpoints();
        let reached = tokio::time::timeout(
            Duration::from_secs(10),
            cuts.wait_for(|applied| *applied >= Some(cut)),
        )
        .await;
        assert!(matches!(reached, Ok(Ok(_))), "node {i} never applied {cut}");
    }
}

/// RFC 0065 §6.8: the leader appends the named flusher's checkpoint and
/// answers its committed log id, and every replica applies it; a second
/// copy is refused as `stale`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_leader_appends_the_named_flushers_checkpoint() {
    let (cluster, leader, cut) = proposing().await;
    let flusher = report(cluster.id(3), cut);

    let outcome = propose(&cluster, leader, &flusher).await;

    let Outcome::Accepted {
        appended: Some(appended),
    } = outcome
    else {
        panic!("accepted with an entry, not {outcome:?}");
    };
    assert!(appended.index > cut.index);
    applied_everywhere(&cluster, cut).await;
    let again = propose(&cluster, leader, &flusher).await;
    assert!(matches!(again, Outcome::Stale(_)), "{again:?}");
    cluster.shutdown().await;
}

/// RFC 0065 §6.8's checks 2 to 4: a follower refuses as `not_leader`,
/// naming the leader; the leader refuses another replica as
/// `not_flusher` and a malformed request as `malformed`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_leader_refuses_a_follower_call_another_replica_and_a_malformed_request() {
    let (cluster, leader, cut) = proposing().await;
    let follower = (0..3).find(|&i| i != leader).expect("a follower");
    let flusher = report(cluster.id(3), cut);

    let not_leader = propose(&cluster, follower, &flusher).await;
    let not_flusher = propose(&cluster, leader, &report(cluster.id(4), cut)).await;
    let mut malformed = flusher.request(GROUP);
    malformed.entry = Some(
        Control::IdBlock {
            after: 0,
            through: 1,
        }
        .to_bytes(),
    );
    let malformed = cluster
        .peers()
        .propose(&name(cluster.id(leader)), malformed, COMMIT)
        .await;

    assert_eq!(
        not_leader,
        Outcome::NotLeader {
            leader: Some(name(cluster.id(leader)))
        }
    );
    assert_eq!(not_flusher, Outcome::NotFlusher);
    assert!(matches!(malformed, Outcome::Malformed(_)), "{malformed:?}");
    cluster.shutdown().await;
}
