//! The flusher choice: the plan for each view, and the leader's pass on
//! the in-process cluster, RFC0065.29's kind gate and RFC0065.30.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use openraft::ChangeMembers;
use openraft::Instant as _;
use openraft::async_runtime::WatchReceiver as _;
use ourios_wal::Control;

use super::{Assignment, Gate, Hold, Learner, Pass, Reason, Steward, View, plan, verdict};
use crate::replicated::raft::cluster::{Cluster, WAIT, name, propose, write};
use crate::replicated::raft::learners::{self, Changes, Group, Tail, TailPool, reconcile};
use crate::replicated::raft::log_store::GroupLogReader;
use crate::replicated::raft::telemetry::Meters;
use crate::replicated::raft::telemetry::tests::Reader;
use crate::replicated::raft::transport::Identity;
use crate::replicated::raft::types::{GroupEntry, LogId, NodeId};
use ourios_semconv as semconv;

mod config;
mod id_blocks;

const P: Duration = Duration::from_secs(10);

fn id(n: u64) -> NodeId {
    NodeId::from(n)
}

fn secs(s: u64) -> Duration {
    Duration::from_secs(s)
}

/// Placed learners `1..`, each acknowledged `acked[i]` ago.
fn learners(acked: &[Option<u64>]) -> Vec<Learner> {
    (1..)
        .zip(acked)
        .map(|(n, &acked)| Learner {
            replica: id(n),
            placed: true,
            acked: acked.map(secs),
        })
        .collect()
}

fn assignment(n: u64, reason: Reason) -> Assignment {
    Assignment {
        replica: id(n),
        reason,
    }
}

fn view(flusher: Option<u64>, learners: Vec<Learner>, led_for: u64) -> View {
    View {
        flusher: flusher.map(id),
        learners,
        led_for: secs(led_for),
    }
}

#[test]
fn with_no_flusher_the_first_healthy_learner_in_order_is_assigned() {
    let planned = plan(&view(None, learners(&[Some(11), Some(1), Some(0)]), 0), P);
    assert_eq!(
        planned,
        Some(assignment(2, Reason::Unassigned)),
        "the first is unhealthy"
    );
}

/// A flusher is unhealthy once its last acknowledgement is the patience
/// old, and replaced once it has been unhealthy for the patience.
#[test]
fn an_unhealthy_flusher_is_kept_for_the_patience_then_replaced() {
    let kept = plan(&view(Some(1), learners(&[Some(19), Some(0)]), 60), P);
    assert_eq!(kept, None);
    let replaced = plan(&view(Some(1), learners(&[Some(20), Some(0)]), 60), P);
    assert_eq!(replaced, Some(assignment(2, Reason::Unhealthy)));
}

#[test]
fn a_recovered_replica_does_not_take_the_role_back() {
    let planned = plan(&view(Some(2), learners(&[Some(0), Some(0)]), 60), P);
    assert_eq!(planned, None);
}

/// A flusher that is no longer a learner is replaced at once; one still a
/// learner but out of the placement is never chosen as a replacement.
#[test]
fn a_flusher_that_left_the_learners_is_replaced() {
    let mut rest = learners(&[Some(0), Some(0)]);
    rest[0].placed = false;
    let planned = plan(&view(Some(9), rest, 60), P);
    assert_eq!(planned, Some(assignment(2, Reason::NotLearner)));
}

#[test]
fn with_none_healthy_nothing_is_assigned() {
    let none = learners(&[Some(30), None]);
    assert_eq!(plan(&view(Some(1), none.clone(), 60), P), None);
    assert_eq!(plan(&view(None, none, 60), P), None);
    assert_eq!(plan(&view(None, Vec::new(), 60), P), None);
}

/// A new leader keeps the flusher its log names until it has led for the
/// patience, even one that left the learners; a flusher that never
/// acknowledged it is unhealthy from when it took over.
#[test]
fn a_new_leader_keeps_the_flusher_for_the_patience() {
    let silent = learners(&[None, Some(0)]);
    assert_eq!(plan(&view(Some(1), silent.clone(), 9), P), None);
    assert_eq!(plan(&view(Some(9), silent.clone(), 9), P), None);
    assert_eq!(
        plan(&view(Some(1), silent, 10), P),
        Some(assignment(2, Reason::Unhealthy))
    );
}

/// The cluster tests' patience: long against a heartbeat, short against a
/// test run.
const PATIENCE: Duration = Duration::from_secs(1);
const IDENTIFY: Duration = Duration::from_secs(2);

fn log(cluster: &Cluster, i: usize) -> &GroupLogReader {
    cluster.node(i).host.log(0).expect("hosted")
}

fn group(cluster: &Cluster, i: usize) -> Group<'_> {
    Group {
        raft: cluster.node(i).raft(0),
        log: log(cluster, i),
        sm: cluster.node(i).host.state_machine(0).expect("hosted"),
    }
}

fn tail<'a>(placed: &'a [&'a str], cluster: &'a Cluster) -> Tail<'a> {
    Tail {
        placed,
        peers: cluster.peers(),
        patience: IDENTIFY,
    }
}

/// The leader of group 0, once it has committed an entry of its term.
async fn ready_leader(cluster: &Cluster) -> usize {
    let leader = cluster.leader(0, None).await;
    write(cluster.node(leader).raft(0), 0).await;
    leader
}

/// The tail nodes `tail` in placement order, as `node_id`s and indexes.
fn placement(cluster: &Cluster, tail: std::ops::Range<usize>) -> (Vec<String>, Vec<usize>) {
    let pool: Vec<_> = tail.clone().map(|i| name(cluster.id(i))).collect();
    let placed: Vec<String> = TailPool::new(pool.clone(), pool.len())
        .placed(0)
        .into_iter()
        .map(str::to_owned)
        .collect();
    let order = placed
        .iter()
        .map(|p| {
            tail.clone()
                .find(|&i| name(cluster.id(i)) == *p)
                .expect("placed")
        })
        .collect();
    (placed, order)
}

fn strs(placed: &[String]) -> Vec<&str> {
    placed.iter().map(String::as_str).collect()
}

/// Answer `Identify` on node `i` as a release reading up to `kind`.
fn reads_up_to(cluster: &Cluster, i: usize, kind: u8) {
    let identity = Identity {
        highest_control_kind: kind,
        ..Identity::new(name(cluster.id(i)), cluster.id(i))
    };
    cluster.node(i).host.server().identify_as(identity);
}

/// Wait until the leader has heard from each of `nodes` within a tenth of
/// the patience.
async fn heard_from(cluster: &Cluster, leader: usize, nodes: &[usize]) {
    let replicas: Vec<_> = nodes.iter().map(|&i| cluster.id(i)).collect();
    cluster
        .node(leader)
        .raft(0)
        .wait(WAIT)
        .metrics(
            |m| {
                replicas.iter().all(|r| {
                    let acked = m
                        .heartbeat
                        .as_ref()
                        .and_then(|h| h.get(r).copied().flatten());
                    acked.is_some_and(|t| t.elapsed() < PATIENCE / 10)
                })
            },
            "every node acknowledged",
        )
        .await
        .expect("acknowledged");
}

/// Reconcile on `leader` until each of `placed` is a committed learner:
/// on a loaded runner an `Identify` can outlast its patience, which leaves
/// a pass with nothing to add.
async fn admit(cluster: &Cluster, leader: usize, placed: &[&str]) {
    let deadline = tokio::time::Instant::now() + PATIENCE * 10;
    loop {
        let _ = reconcile(group(cluster, leader), tail(placed, cluster)).await;
        let metrics = cluster
            .node(leader)
            .raft(0)
            .metrics()
            .borrow_watched()
            .clone();
        let membership = metrics.committed_membership_config.membership();
        let learners: BTreeSet<_> = membership
            .non_write_voter_ids()
            .filter_map(|id| membership.get_node(&id).cloned())
            .collect();
        if placed.iter().all(|p| learners.contains(*p)) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{placed:?} never admitted"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Run passes on `leader` until one assigns a flusher, returning it.
async fn pass_until_assigned(
    steward: &mut Steward,
    cluster: &Cluster,
    leader: usize,
    placed: &[&str],
) -> NodeId {
    let deadline = tokio::time::Instant::now() + PATIENCE * 10;
    loop {
        let pass = steward
            .pass(group(cluster, leader), tail(placed, cluster))
            .await;
        match pass {
            Ok(Pass {
                assigned: Some(replica),
                ..
            }) => return replica,
            _ if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            other => panic!("no assignment: {other:?}"),
        }
    }
}

/// One pass on `leader`, which must succeed.
async fn pass(steward: &mut Steward, cluster: &Cluster, leader: usize, placed: &[&str]) -> Pass {
    steward
        .pass(group(cluster, leader), tail(placed, cluster))
        .await
        .expect("pass")
}

/// The flusher node `i` applied last.
fn applied_flusher(cluster: &Cluster, i: usize) -> Option<NodeId> {
    cluster
        .node(i)
        .host
        .state_machine(0)
        .expect("hosted")
        .flusher()
}

fn learners_of(cluster: &Cluster, i: usize) -> BTreeSet<NodeId> {
    let metrics = cluster.node(i).raft(0).metrics().borrow_watched().clone();
    metrics
        .committed_membership_config
        .membership()
        .non_write_voter_ids()
        .collect()
}

/// RFC0065.29, the leader's kind gate: while the log has never held kind
/// `0x01`, a voter or a learner that answers reading below it holds the
/// assignment back; once every member reads it, it is appended; and once
/// the log holds it, a reassignment is appended with a voter lost.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0065_29_a_kind_not_every_member_reads_is_not_appended() {
    let mut cluster = Cluster::start_with_tail(3, 2, &[0]).await;
    let leader = ready_leader(&cluster).await;
    let (placed, order) = placement(&cluster, 3..5);
    let placed = strs(&placed);
    let voter = (0..3).find(|&i| i != leader).expect("a follower");
    let mut steward = Steward::with_patience(PATIENCE);

    reads_up_to(&cluster, voter, 0);
    pass(&mut steward, &cluster, leader, &placed).await;
    heard_from(&cluster, leader, &order).await;
    let held = pass(&mut steward, &cluster, leader, &placed).await;
    assert_eq!(held.assigned, None, "a voter reads below the kind");

    reads_up_to(&cluster, voter, 1);
    reads_up_to(&cluster, order[1], 0);
    let held = pass(&mut steward, &cluster, leader, &placed).await;
    assert_eq!(held.assigned, None, "a learner reads below the kind");
    assert_eq!(log(&cluster, leader).controls().highest_kind, 0);

    reads_up_to(&cluster, order[1], 1);
    let first = pass(&mut steward, &cluster, leader, &placed).await;
    assert_eq!(first.assigned, Some(cluster.id(order[0])));
    assert_eq!(log(&cluster, leader).controls().highest_kind, 1);

    cluster.kill(voter).await;
    cluster.kill(order[0]).await;
    let next = pass_until_assigned(&mut steward, &cluster, leader, &placed).await;
    assert_eq!(next, cluster.id(order[1]), "appended with a voter lost");
    let index = write(cluster.node(leader).raft(0), 1).await;
    cluster.node(order[1]).applied_through(0, index).await;
    assert_eq!(applied_flusher(&cluster, order[1]), Some(next));
    cluster.shutdown().await;
}

/// RFC0065.29: a leader holding an uncommitted entry of a new kind admits
/// no learner until it commits, and the learner it then admits applies
/// that entry.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_uncommitted_new_kind_holds_back_a_learner() {
    let cluster = Cluster::start_with_tail(3, 2, &[0]).await;
    let leader = ready_leader(&cluster).await;
    let (placed, order) = placement(&cluster, 3..5);
    let raft = cluster.node(leader).raft(0);
    let first = [placed[0].as_str()];
    admit(&cluster, leader, &first).await;

    let followers: Vec<_> = (0..3).filter(|&i| i != leader).collect();
    for &f in &followers {
        cluster.node(f).gate.close();
    }
    let flusher = cluster.id(order[0]);
    let proposing = raft.clone();
    let assignment = GroupEntry::Control(Control::FlusherAssignment(*flusher.uuid().as_bytes()));
    let writing = tokio::spawn(async move { proposing.client_write(assignment).await });
    let reader = log(&cluster, leader).clone();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while reader.controls().introduced.is_none() {
        assert!(tokio::time::Instant::now() < deadline, "never appended");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let both = strs(&placed);
    let held = reconcile(group(&cluster, leader), tail(&both, &cluster)).await;
    assert_eq!(held, Ok(Changes::default()), "admitted under a new kind");
    assert_eq!(learners_of(&cluster, leader), BTreeSet::from([flusher]));

    for &f in &followers {
        cluster.node(f).gate.open();
    }
    let committed = writing.await.expect("join").expect("committed");
    let added = reconcile(group(&cluster, leader), tail(&both, &cluster))
        .await
        .expect("admitted once committed");
    let second = cluster.id(order[1]);
    assert_eq!(added.add, BTreeMap::from([(second, placed[1].clone())]));
    cluster
        .node(order[1])
        .applied_through(0, committed.log_id.index)
        .await;
    assert_eq!(applied_flusher(&cluster, order[1]), Some(flusher));
    cluster.shutdown().await;
}

/// RFC0065.29: with a membership change in flight the leader appends no
/// new kind, and counts the hold as one; once it commits, the assignment
/// is appended.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "superseded: a pass also appends the miner configuration, kind 0x04, so it holds \
            back two new kinds; see a_membership_change_in_flight_holds_back_each_new_kind"]
async fn a_membership_change_in_flight_holds_back_a_new_kind() {
    let cluster = Cluster::start_with_tail(3, 2, &[0]).await;
    let leader = ready_leader(&cluster).await;
    let (placed, order) = placement(&cluster, 3..5);
    let raft = cluster.node(leader).raft(0);
    let first = [placed[0].as_str()];
    admit(&cluster, leader, &first).await;
    heard_from(&cluster, leader, &order[..1]).await;

    let followers: Vec<_> = (0..3).filter(|&i| i != leader).collect();
    for &f in &followers {
        cluster.node(f).gate.close();
    }
    let second = cluster.id(order[1]);
    let changing = raft.clone();
    let add = ChangeMembers::AddNodes(BTreeMap::from([(second, placed[1].clone())]));
    let membership = tokio::spawn(async move { changing.change_membership(add, false).await });
    raft.wait(WAIT)
        .metrics(
            |m| m.membership_config.log_id() != m.committed_membership_config.log_id(),
            "a membership change in flight",
        )
        .await
        .expect("in flight");

    let reader = Reader::new();
    let mut steward = counted(&reader);
    let held = pass(&mut steward, &cluster, leader, &first).await;
    assert_eq!(
        held,
        Pass::default(),
        "a new kind beside a membership change"
    );
    assert_eq!(log(&cluster, leader).controls().highest_kind, 0);
    let holds = reader.points(semconv::OURIOS_WAL_REPLICATION_CONTROL_HOLDS);
    assert_eq!(holds, [counted_as("membership_change", 1)]);

    for &f in &followers {
        cluster.node(f).gate.open();
    }
    membership.await.expect("join").expect("committed");
    let assigned = pass(&mut steward, &cluster, leader, &first).await;
    assert_eq!(assigned.assigned, Some(cluster.id(order[0])));
    cluster.shutdown().await;
}

/// Start a membership change on `leader` that adds `learner` and stays
/// in flight until the returned followers' gates open.
async fn membership_change_in_flight(
    cluster: &Cluster,
    leader: usize,
    (learner, node_id): (NodeId, String),
) -> (tokio::task::JoinHandle<()>, Vec<usize>) {
    let followers: Vec<_> = (0..3).filter(|&i| i != leader).collect();
    for &f in &followers {
        cluster.node(f).gate.close();
    }
    let raft = cluster.node(leader).raft(0);
    let changing = raft.clone();
    let add = ChangeMembers::AddNodes(BTreeMap::from([(learner, node_id)]));
    let membership = tokio::spawn(async move {
        changing
            .change_membership(add, false)
            .await
            .expect("committed");
    });
    raft.wait(WAIT)
        .metrics(
            |m| m.membership_config.log_id() != m.committed_membership_config.log_id(),
            "a membership change in flight",
        )
        .await
        .expect("in flight");
    (membership, followers)
}

/// Join the change [`membership_change_in_flight`] started, then wait
/// for the leader's metrics, which the steward reads, to show it
/// committed: the change resolves first.
async fn membership_committed(
    membership: tokio::task::JoinHandle<()>,
    cluster: &Cluster,
    leader: usize,
) {
    membership.await.expect("join");
    cluster
        .node(leader)
        .raft(0)
        .wait(WAIT)
        .metrics(
            |m| m.membership_config.log_id() == m.committed_membership_config.log_id(),
            "the membership change committed",
        )
        .await
        .expect("committed");
}

/// RFC0065.29: with a membership change in flight the leader appends no
/// new kind, the assignment nor the miner configuration, and counts each
/// hold; once it commits, both are appended.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "superseded: a pass also gates the id block, kind 0x03, which counts its own hold; \
            see id_blocks::a_membership_change_in_flight_holds_back_every_new_kind"]
async fn a_membership_change_in_flight_holds_back_each_new_kind() {
    let cluster = Cluster::start_with_tail(3, 2, &[0]).await;
    let leader = ready_leader(&cluster).await;
    let (placed, order) = placement(&cluster, 3..5);
    let first = [placed[0].as_str()];
    admit(&cluster, leader, &first).await;
    heard_from(&cluster, leader, &order[..1]).await;
    let second = (cluster.id(order[1]), placed[1].clone());
    let (membership, followers) = membership_change_in_flight(&cluster, leader, second).await;

    let reader = Reader::new();
    let mut steward = counted(&reader);
    let held = pass(&mut steward, &cluster, leader, &first).await;
    assert_eq!(
        held,
        Pass::default(),
        "a new kind beside a membership change"
    );
    assert_eq!(log(&cluster, leader).controls().highest_kind, 0);
    let holds = reader.points(semconv::OURIOS_WAL_REPLICATION_CONTROL_HOLDS);
    assert_eq!(holds, [counted_as("membership_change", 2)], "one per kind");

    for &f in &followers {
        cluster.node(f).gate.open();
    }
    membership_committed(membership, &cluster, leader).await;
    let both = [placed[0].as_str(), placed[1].as_str()];
    let appended = pass(&mut steward, &cluster, leader, &both).await;
    assert_eq!(appended.assigned, Some(cluster.id(order[0])));
    assert!(appended.configured.is_some(), "the configuration held back");
    cluster.shutdown().await;
}

/// A leader whose log purged the last assignment below its snapshot
/// still sees the snapshot's flusher, so a steward new to the lead keeps
/// it for the patience rather than treating the group as unassigned.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "superseded: RFC 0065 §6.5 builds a snapshot only at a checkpoint's cut; see \
            a_flusher_purged_below_a_checkpoint_is_kept_for_the_patience"]
async fn a_flusher_purged_below_the_snapshot_is_kept_for_the_patience() {
    let cluster = Cluster::start_with_tail(3, 2, &[0]).await;
    let leader = ready_leader(&cluster).await;
    let (placed, _) = placement(&cluster, 3..5);
    let placed = strs(&placed);
    let mut steward = Steward::with_patience(PATIENCE);
    let flusher = pass_until_assigned(&mut steward, &cluster, leader, &placed).await;

    let raft = cluster.node(leader).raft(0);
    let index = write(raft, 1).await;
    cluster.node(leader).applied_through(0, index).await;
    let reached = |id: Option<&LogId>| id.is_some_and(|id| id.index >= index);
    raft.trigger().snapshot().await.expect("snapshot");
    raft.wait(WAIT)
        .metrics(|m| reached(m.snapshot.as_ref()), "built")
        .await
        .expect("built");
    raft.trigger().purge_log(index).await.expect("purge");
    raft.wait(WAIT)
        .metrics(|m| reached(m.purged.as_ref()), "purged")
        .await
        .expect("purged");
    assert_eq!(log(&cluster, leader).controls().flusher, None);
    assert_eq!(group(&cluster, leader).controls().flusher, Some(flusher));

    let mut new_to_the_lead = Steward::with_patience(PATIENCE);
    let kept = pass(&mut new_to_the_lead, &cluster, leader, &placed).await;
    assert_eq!(kept.assigned, None, "reassigned within the patience");
    cluster.shutdown().await;
}

/// A leader whose log purged the last assignment below a checkpoint's
/// cut still sees the snapshot's flusher, so a steward new to the lead
/// keeps it for the patience rather than treating the group as unassigned.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "superseded: the assigning pass also appends the miner configuration, so the cut's \
            highest kind is 0x04, not 0x01; see \
            a_flusher_purged_below_a_checkpoint_at_the_kind_in_force_is_kept"]
async fn a_flusher_purged_below_a_checkpoint_is_kept_for_the_patience() {
    let cluster = Cluster::start_with_tail(3, 2, &[0]).await;
    let leader = ready_leader(&cluster).await;
    let (placed, _) = placement(&cluster, 3..5);
    let placed = strs(&placed);
    let mut steward = Steward::with_patience(PATIENCE);
    let flusher = pass_until_assigned(&mut steward, &cluster, leader, &placed).await;

    let raft = cluster.node(leader).raft(0);
    let index = write(raft, 1).await;
    let entries = cluster.node(leader).log(0).await;
    let written = entries.iter().find(|e| e.log_id.index == index);
    let term = written.expect("in the log").log_id.leader_id.term;
    let checkpoint = Control::Checkpoint {
        proposer: *flusher.uuid().as_bytes(),
        cut: ourios_wal::RaftLogId { term, index },
        highest_kind: 1,
    };
    propose(raft, GroupEntry::Control(checkpoint)).await;
    let reached = |id: Option<&LogId>| id.is_some_and(|id| id.index >= index);
    raft.wait(WAIT)
        .metrics(|m| reached(m.purged.as_ref()), "purged at the cut")
        .await
        .expect("purged");
    assert_eq!(log(&cluster, leader).controls().flusher, None);
    assert_eq!(group(&cluster, leader).controls().flusher, Some(flusher));

    let mut new_to_the_lead = Steward::with_patience(PATIENCE);
    let kept = pass(&mut new_to_the_lead, &cluster, leader, &placed).await;
    assert_eq!(kept.assigned, None, "reassigned within the patience");
    cluster.shutdown().await;
}

/// Write an entry on `leader`, have `flusher` propose a checkpoint cut at
/// it with the highest kind the log holds, and wait for every node in
/// `nodes` to purge through the cut, returning its index.
async fn checkpoint_purging(
    cluster: &Cluster,
    (leader, flusher): (usize, NodeId),
    nodes: &[usize],
) -> u64 {
    let raft = cluster.node(leader).raft(0);
    let index = write(raft, 1).await;
    let entries = cluster.node(leader).log(0).await;
    let written = entries.iter().find(|e| e.log_id.index == index);
    let term = written.expect("in the log").log_id.leader_id.term;
    let checkpoint = Control::Checkpoint {
        proposer: *flusher.uuid().as_bytes(),
        cut: ourios_wal::RaftLogId { term, index },
        highest_kind: group(cluster, leader).controls().highest_kind,
    };
    propose(raft, GroupEntry::Control(checkpoint)).await;
    let reached = |id: Option<&LogId>| id.is_some_and(|id| id.index >= index);
    for &i in nodes {
        cluster
            .node(i)
            .raft(0)
            .wait(WAIT)
            .metrics(|m| reached(m.purged.as_ref()), "purged at the cut")
            .await
            .expect("purged");
    }
    index
}

/// A leader whose log purged the last assignment below a checkpoint's
/// cut still sees the snapshot's flusher, so a steward new to the lead
/// keeps it for the patience rather than treating the group as unassigned.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_flusher_purged_below_a_checkpoint_at_the_kind_in_force_is_kept() {
    let cluster = Cluster::start_with_tail(3, 2, &[0]).await;
    let leader = ready_leader(&cluster).await;
    let (placed, _) = placement(&cluster, 3..5);
    let placed = strs(&placed);
    let mut steward = Steward::with_patience(PATIENCE);
    let flusher = pass_until_assigned(&mut steward, &cluster, leader, &placed).await;

    checkpoint_purging(&cluster, (leader, flusher), &[leader]).await;
    assert_eq!(log(&cluster, leader).controls().flusher, None);
    assert_eq!(group(&cluster, leader).controls().flusher, Some(flusher));

    let mut new_to_the_lead = Steward::with_patience(PATIENCE);
    let kept = pass(&mut new_to_the_lead, &cluster, leader, &placed).await;
    assert_eq!(kept.assigned, None, "reassigned within the patience");
    cluster.shutdown().await;
}

/// A steward on [`PATIENCE`] counting to `reader`.
fn counted(reader: &Reader) -> Steward {
    Steward::counting(PATIENCE, Meters::on(&reader.meter()).flusher())
}

/// A point of group 0 with `reason`.
fn counted_as(reason: &str, value: u64) -> (Vec<String>, u64) {
    (vec!["0".to_owned(), reason.to_owned()], value)
}

/// The leader counts each pass its kind gate holds back for a member
/// reading below the kind, and the assignment once it commits, each by
/// group and reason; a pass that assigns nothing counts neither.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "superseded: each pass also gates the miner configuration, kind 0x04, which counts \
            its own holds; see each_held_kind_and_committed_assignment_is_counted"]
async fn held_kinds_and_committed_assignments_are_counted() {
    let cluster = Cluster::start_with_tail(3, 2, &[0]).await;
    let leader = ready_leader(&cluster).await;
    let (placed, order) = placement(&cluster, 3..5);
    let placed = strs(&placed);
    let voter = (0..3).find(|&i| i != leader).expect("a follower");
    let reader = Reader::new();
    let mut steward = counted(&reader);
    let holds = semconv::OURIOS_WAL_REPLICATION_CONTROL_HOLDS;
    let assignments = semconv::OURIOS_WAL_REPLICATION_FLUSHER_ASSIGNMENTS;

    admit(&cluster, leader, &placed).await;
    heard_from(&cluster, leader, &order).await;
    reads_up_to(&cluster, voter, 0);
    for n in 1..=2 {
        let held = pass(&mut steward, &cluster, leader, &placed).await;
        assert_eq!(held.assigned, None, "a voter reads below the kind");
        let counted = reader.points(holds);
        assert_eq!(counted, [counted_as("member_unconfirmed", n)]);
    }
    assert!(reader.points(assignments).is_empty(), "nothing committed");

    reads_up_to(&cluster, voter, 1);
    let first = pass(&mut steward, &cluster, leader, &placed).await;
    assert_eq!(first.assigned, Some(cluster.id(order[0])));
    let kept = pass(&mut steward, &cluster, leader, &placed).await;
    assert_eq!(kept.assigned, None);
    let counted = reader.points(assignments);
    assert_eq!(counted, [counted_as("unassigned", 1)]);
    let counted = reader.points(holds);
    assert_eq!(
        counted,
        [counted_as("member_unconfirmed", 2)],
        "an assignment held"
    );
    cluster.shutdown().await;
}

/// The leader counts each kind its gate holds back in a pass for a member
/// reading below it, the flusher assignment's and the miner
/// configuration's, and the assignment once it commits, each by group and
/// reason; a pass that holds back nothing counts nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "superseded: each pass also gates the id block, kind 0x03, which counts its own \
            holds; see id_blocks::every_held_kind_and_committed_assignment_is_counted"]
async fn each_held_kind_and_committed_assignment_is_counted() {
    let cluster = Cluster::start_with_tail(3, 2, &[0]).await;
    let leader = ready_leader(&cluster).await;
    let (placed, order) = placement(&cluster, 3..5);
    let placed = strs(&placed);
    let voter = (0..3).find(|&i| i != leader).expect("a follower");
    let reader = Reader::new();
    let mut steward = counted(&reader);
    let holds = semconv::OURIOS_WAL_REPLICATION_CONTROL_HOLDS;
    let assignments = semconv::OURIOS_WAL_REPLICATION_FLUSHER_ASSIGNMENTS;

    admit(&cluster, leader, &placed).await;
    heard_from(&cluster, leader, &order).await;
    reads_up_to(&cluster, voter, 0);
    for n in 1..=2 {
        let held = pass(&mut steward, &cluster, leader, &placed).await;
        assert_eq!(held.assigned, None, "a voter reads below the kind");
        let counted = reader.points(holds);
        assert_eq!(counted, [counted_as("member_unconfirmed", 2 * n)]);
    }
    assert!(reader.points(assignments).is_empty(), "nothing committed");

    reads_up_to(&cluster, voter, 1);
    let first = pass(&mut steward, &cluster, leader, &placed).await;
    assert_eq!(first.assigned, Some(cluster.id(order[0])));
    assert_eq!(first.configured, None, "a voter reads below 0x04");
    reads_up_to(&cluster, voter, Control::HIGHEST_KIND);
    let configured = pass(&mut steward, &cluster, leader, &placed).await;
    assert!(configured.configured.is_some());
    let kept = pass(&mut steward, &cluster, leader, &placed).await;
    assert_eq!(kept, Pass::default());
    let counted = reader.points(assignments);
    assert_eq!(counted, [counted_as("unassigned", 1)]);
    let counted = reader.points(holds);
    assert_eq!(counted, [counted_as("member_unconfirmed", 5)]);
    cluster.shutdown().await;
}

/// Scenario RFC0065.30 — the leader names one flusher and keeps it (B11),
/// with the patience shortened to [`PATIENCE`]: first in placement order,
/// replaced once unhealthy for the patience and not taken back, replaced
/// at once once no longer a learner, kept by a new leader for the
/// patience, and kept with none healthy. Leaving the placement is the
/// next test's arm.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0065_30_the_leader_names_one_flusher_and_keeps_it() {
    let mut cluster = Cluster::start_with_tail(3, 3, &[0]).await;
    let leader = ready_leader(&cluster).await;
    let (placed, order) = placement(&cluster, 3..6);
    let [a, b, c] = [order[0], order[1], order[2]];
    let all = strs(&placed);
    let mut steward = Steward::with_patience(PATIENCE);

    admit(&cluster, leader, &all).await;
    heard_from(&cluster, leader, &order).await;
    let first = pass(&mut steward, &cluster, leader, &all).await;
    assert_eq!(
        first.assigned,
        Some(cluster.id(a)),
        "first in placement order"
    );

    cluster.kill(a).await;
    let stopped = tokio::time::Instant::now();
    let kept = pass(&mut steward, &cluster, leader, &all).await;
    assert_eq!(kept.assigned, None, "replaced before the patience");
    let next = pass_until_assigned(&mut steward, &cluster, leader, &all).await;
    assert_eq!(next, cluster.id(b));
    assert!(
        stopped.elapsed() >= PATIENCE * 3 / 2,
        "replaced {:?} after it stopped",
        stopped.elapsed()
    );

    cluster.restart(a).await;
    heard_from(&cluster, leader, &[a, b, c]).await;
    let kept = pass(&mut steward, &cluster, leader, &all).await;
    assert_eq!(kept.assigned, None, "the recovered replica took it back");

    cluster.kill(b).await;
    let removed = tokio::time::Instant::now();
    remove_learner(&cluster, leader, b).await;
    let gone = pass(&mut steward, &cluster, leader, &all).await;
    assert_eq!(
        gone.assigned,
        Some(cluster.id(a)),
        "the flusher is no learner"
    );
    assert!(removed.elapsed() < PATIENCE, "waited for the patience");

    cluster.kill(a).await;
    cluster.kill(leader).await;
    let successor = cluster.leader(0, Some(leader)).await;
    let mut steward = Steward::with_patience(PATIENCE);
    failover_keeps_then_replaces(&mut steward, &cluster, successor, (&all, a, c)).await;

    cluster.kill(c).await;
    let none_healthy = tokio::time::Instant::now();
    while none_healthy.elapsed() < PATIENCE * 3 {
        let pass = pass(&mut steward, &cluster, successor, &all).await;
        assert_eq!(pass.assigned, None, "assigned with none healthy");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        log(&cluster, successor).controls().flusher,
        Some(cluster.id(c))
    );
    cluster.shutdown().await;
}

/// Remove node `i`'s learner from the group by a committed change.
async fn remove_learner(cluster: &Cluster, leader: usize, i: usize) {
    let raft = cluster.node(leader).raft(0);
    let planned_at = *raft
        .metrics()
        .borrow_watched()
        .committed_membership_config
        .log_id();
    let remove = Changes {
        add: BTreeMap::new(),
        remove: BTreeSet::from([cluster.id(i)]),
    };
    learners::apply(raft, &remove, planned_at)
        .await
        .expect("removed");
}

/// A new leader `successor` keeps the stopped flusher `kept` its log
/// names until it has led for the patience, then assigns `next`.
async fn failover_keeps_then_replaces(
    steward: &mut Steward,
    cluster: &Cluster,
    successor: usize,
    (placed, kept, next): (&[&str], usize, usize),
) {
    write(cluster.node(successor).raft(0), 2).await;
    let took_over = tokio::time::Instant::now();
    let first = pass(steward, cluster, successor, placed).await;
    assert_eq!(first.assigned, None, "a new leader moved the flusher");
    let named = log(cluster, successor).controls().flusher;
    assert_eq!(named, Some(cluster.id(kept)));
    let assigned = pass_until_assigned(steward, cluster, successor, placed).await;
    assert_eq!(assigned, cluster.id(next));
    assert!(took_over.elapsed() >= PATIENCE, "moved within the patience");
}

/// Scenario RFC0065.30, the placement arm: a flusher that leaves the tail
/// placement while still a learner is reassigned at once to a remaining
/// replica that has mined through the commit point, and its learner is
/// removed only once that assignment commits (§6.1's handoff).
#[test]
#[ignore = "RFC0065.30 stub — the placement arm, implemented with mining on tail replicas"]
fn rfc0065_30_a_flusher_leaving_the_placement_is_handed_off() {
    todo!(
        "RFC0065.30 — reassigned at once to a replica mined through the \
         commit point; the learner goes only once that assignment commits"
    );
}

/// A lost term or a changed membership outranks an unconfirmed member: an
/// `Identify` that failed while either happened may have failed because of
/// it, so the round is not a `member_unconfirmed` hold.
#[test]
fn a_lost_term_or_a_changed_membership_outranks_an_unconfirmed_member() {
    for confirmed in [false, true] {
        assert_eq!(verdict(false, true, confirmed), Gate::Deposed);
        assert_eq!(verdict(false, false, confirmed), Gate::Deposed);
        assert_eq!(
            verdict(true, false, confirmed),
            Gate::Held(Hold::MembershipChange)
        );
    }
    assert_eq!(
        verdict(true, true, false),
        Gate::Held(Hold::MemberUnconfirmed)
    );
    assert_eq!(verdict(true, true, true), Gate::Reads);
}
