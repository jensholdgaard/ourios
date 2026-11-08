//! Learner reconciliation: the plan for each membership and answer, and
//! the passes on the in-process cluster.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use openraft::async_runtime::WatchReceiver as _;

use super::{Changes, Group, Members, Tail, TailPool, plan, reconcile};
use crate::replicated::raft::cluster::{Cluster, WAIT, name, write};
use crate::replicated::raft::transport::Identity;
use crate::replicated::raft::types::{Membership, NodeId};

const PATIENCE: Duration = Duration::from_secs(2);

fn id(n: u64) -> NodeId {
    NodeId::from(n)
}

fn members(voters: &[u64], learners: &[(u64, &str)]) -> Members {
    Members {
        voters: voters.iter().map(|&n| id(n)).collect(),
        learners: learners
            .iter()
            .map(|&(n, node_id)| (id(n), node_id.to_owned()))
            .collect(),
    }
}

fn answers(answered: &[(&str, u64, u8)]) -> BTreeMap<String, Identity> {
    answered
        .iter()
        .map(|&(node_id, n, kind)| {
            let identity = Identity {
                node_id: node_id.to_owned(),
                replica: id(n),
                highest_control_kind: kind,
            };
            (node_id.to_owned(), identity)
        })
        .collect()
}

fn changes(add: &[(u64, &str)], remove: &[u64]) -> Changes {
    Changes {
        add: add
            .iter()
            .map(|&(n, node_id)| (id(n), node_id.to_owned()))
            .collect(),
        remove: remove.iter().map(|&n| id(n)).collect::<BTreeSet<_>>(),
    }
}

#[test]
fn placed_peers_are_added_under_the_uuid_they_answer_with() {
    let planned = plan(
        &members(&[1, 2, 3], &[]),
        &["t1", "t2"],
        &answers(&[("t1", 11, 0), ("t2", 12, 0)]),
        0,
    );
    assert_eq!(planned, changes(&[(11, "t1"), (12, "t2")], &[]));
}

#[test]
fn a_learner_that_left_the_placement_is_removed_and_one_that_stayed_is_kept() {
    let planned = plan(
        &members(&[1, 2, 3], &[(11, "t1"), (12, "t2")]),
        &["t1"],
        &answers(&[("t1", 11, 0)]),
        0,
    );
    assert_eq!(planned, changes(&[], &[12]));
}

/// A tail node back on an empty `wal_root` answers under a new UUID with
/// its `node_id` unchanged: the new UUID is added and the old one removed.
#[test]
fn a_wiped_tail_node_is_replaced_under_its_new_uuid() {
    let planned = plan(
        &members(&[1, 2, 3], &[(11, "t1")]),
        &["t1"],
        &answers(&[("t1", 21, 0)]),
        0,
    );
    assert_eq!(planned, changes(&[(21, "t1")], &[11]));
}

/// A placed peer that did not answer keeps the learner it has, and is not
/// added if it has none.
#[test]
fn an_unanswered_peer_changes_nothing() {
    let planned = plan(
        &members(&[1, 2, 3], &[(11, "t1")]),
        &["t1", "t2"],
        &answers(&[]),
        0,
    );
    assert_eq!(planned, Changes::default());
}

/// A voter is never added as a learner, even one that reads every kind
/// the log holds; a tail peer that reads fewer is held back (§6.8), and
/// one that reads enough is added.
#[test]
fn a_voter_or_a_peer_that_cannot_read_the_log_is_not_added() {
    let planned = plan(
        &members(&[1, 2, 3], &[]),
        &["v1", "t1", "t2"],
        &answers(&[("v1", 1, 1), ("t1", 11, 0), ("t2", 12, 1)]),
        1,
    );
    assert_eq!(planned, changes(&[(12, "t2")], &[]));
}

/// A joint membership is never edited: a learner change there would
/// collapse the voters to one configuration, so neither an addition nor a
/// removal is planned until the joint change finishes. A uniform one is.
#[test]
fn a_joint_membership_is_left_alone() {
    let nodes = |ids: &[u64]| {
        ids.iter()
            .map(|&n| (id(n), format!("n{n}")))
            .collect::<BTreeMap<_, _>>()
    };
    let set = |ids: &[u64]| ids.iter().map(|&n| id(n)).collect::<BTreeSet<_>>();
    let joint = Membership::new(
        vec![set(&[1, 2, 3]), set(&[1, 2, 4])],
        nodes(&[1, 2, 3, 4, 11]),
    )
    .expect("joint membership");
    assert!(
        super::stable(&joint).is_none(),
        "a joint membership is not planned"
    );
    let uniform =
        Membership::new(vec![set(&[1, 2, 3])], nodes(&[1, 2, 3, 11])).expect("uniform membership");
    let members = super::stable(&uniform).expect("a uniform membership is planned");
    assert_eq!(members.voters, set(&[1, 2, 3]));
    assert_eq!(
        members.learners,
        BTreeMap::from([(id(11), "n11".to_owned())])
    );
}

fn group(cluster: &Cluster, i: usize) -> Group<'_> {
    Group {
        raft: cluster.node(i).raft(0),
        log: cluster.node(i).host.log(0).expect("hosted"),
        sm: cluster.node(i).host.state_machine(0).expect("hosted"),
    }
}

fn reaching<'a>(placed: &'a [&'a str], cluster: &'a Cluster) -> Tail<'a> {
    Tail {
        placed,
        peers: cluster.peers(),
        patience: PATIENCE,
    }
}

/// The leader of group 0, once it has committed an entry of its term.
async fn ready_leader(cluster: &Cluster) -> usize {
    let leader = cluster.leader(0, None).await;
    write(cluster.node(leader).raft(0), 0).await;
    leader
}

fn learners_of(cluster: &Cluster, i: usize) -> BTreeSet<NodeId> {
    let metrics = cluster.node(i).raft(0).metrics().borrow_watched().clone();
    metrics
        .membership_config
        .membership()
        .non_write_voter_ids()
        .collect()
}

/// The leader adds the placed tail node as a learner, which then applies
/// the group's entries; a follower's pass changes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_leader_adds_the_placed_tail_node_and_it_catches_up() {
    let cluster = Cluster::start_with_tail(3, 2, &[0]).await;
    let leader = ready_leader(&cluster).await;
    let pool = TailPool::new(vec![name(cluster.id(3)), name(cluster.id(4))], 1);
    let placed = pool.placed(0);
    let tail = (3..5)
        .find(|&i| name(cluster.id(i)) == placed[0])
        .expect("a placed tail node");
    let follower = (0..3).find(|&i| i != leader).expect("a follower");

    let none = reconcile(group(&cluster, follower), reaching(&placed, &cluster)).await;
    assert_eq!(none, Ok(Changes::default()), "only the leader reconciles");
    let added = reconcile(group(&cluster, leader), reaching(&placed, &cluster)).await;
    let expected = Changes {
        add: BTreeMap::from([(cluster.id(tail), placed[0].to_owned())]),
        remove: BTreeSet::new(),
    };
    assert_eq!(added, Ok(expected));
    assert_eq!(
        learners_of(&cluster, leader),
        BTreeSet::from([cluster.id(tail)])
    );

    let index = write(cluster.node(leader).raft(0), 1).await;
    cluster.node(tail).applied_through(0, index).await;
    let again = reconcile(group(&cluster, leader), reaching(&placed, &cluster)).await;
    assert_eq!(
        again,
        Ok(Changes::default()),
        "a second pass changes nothing"
    );
    cluster.shutdown().await;
}

/// A learner never counts toward the write quorum: with both voting
/// followers' fsyncs held, a write the learner has fsynced and matched is
/// still not committed, and commits once a voting follower fsyncs it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_learner_never_makes_a_quorum() {
    let cluster = Cluster::start_with_tail(3, 1, &[0]).await;
    let leader = ready_leader(&cluster).await;
    let placed = [name(cluster.id(3))];
    let placed: Vec<&str> = placed.iter().map(String::as_str).collect();
    reconcile(group(&cluster, leader), reaching(&placed, &cluster))
        .await
        .expect("added");
    let followers: Vec<_> = (0..3).filter(|&i| i != leader).collect();
    for &f in &followers {
        cluster.node(f).gate.close();
    }
    let raft = cluster.node(leader).raft(0).clone();
    let target = raft.metrics().borrow_watched().last_log_index.unwrap_or(0) + 1;
    let writing = tokio::spawn(async move {
        raft.client_write(crate::replicated::raft::cluster::batch(9).into())
            .await
    });
    let learner = cluster.id(3);
    cluster
        .node(leader)
        .raft(0)
        .wait(WAIT)
        .metrics(
            |m| {
                let matched = m.replication.as_ref().and_then(|r| r.get(&learner));
                matched
                    .copied()
                    .flatten()
                    .is_some_and(|l| l.index >= target)
            },
            "the learner matches the write",
        )
        .await
        .expect("the learner caught up");
    let committed = cluster
        .node(leader)
        .raft(0)
        .metrics()
        .borrow_watched()
        .cluster_committed;
    assert!(
        committed.is_none_or(|c| c.index < target),
        "committed on the leader and a learner alone: {committed:?}"
    );
    assert!(
        !writing.is_finished(),
        "acknowledged without a voting follower"
    );
    for &f in &followers {
        cluster.node(f).gate.open();
    }
    writing
        .await
        .expect("join")
        .expect("committed once a voter fsyncs");
    cluster.shutdown().await;
}

/// When the placement shrinks, the learner that left it is removed and
/// the one that stayed is kept.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shrunk_placement_removes_the_learner_that_left() {
    let cluster = Cluster::start_with_tail(3, 2, &[0]).await;
    let leader = ready_leader(&cluster).await;
    let pool = vec![name(cluster.id(3)), name(cluster.id(4))];
    let both = TailPool::new(pool.clone(), 2);
    reconcile(group(&cluster, leader), reaching(&both.placed(0), &cluster))
        .await
        .expect("both added");
    assert_eq!(learners_of(&cluster, leader).len(), 2);

    let one = TailPool::new(pool, 1);
    let kept = one.placed(0)[0].to_owned();
    let removed = reconcile(group(&cluster, leader), reaching(&one.placed(0), &cluster))
        .await
        .expect("one removed");
    assert_eq!(removed.remove.len(), 1);
    let left: Vec<String> = learners_of(&cluster, leader)
        .iter()
        .map(|id| name(*id))
        .collect();
    assert_eq!(left, vec![kept]);
    cluster.shutdown().await;
}

/// Each edit is guarded by the membership it was planned from: once
/// another change has committed meanwhile, as a joint change would during
/// a pass's `Identify` calls, the stale edit is refused and changes
/// nothing; planned from the current membership, it commits.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_edit_planned_before_another_change_committed_is_refused() {
    let cluster = Cluster::start_with_tail(3, 2, &[0]).await;
    let leader = ready_leader(&cluster).await;
    let raft = cluster.node(leader).raft(0);
    let planned_at = *raft
        .metrics()
        .borrow_watched()
        .committed_membership_config
        .log_id();
    let (first, second) = (cluster.id(3), cluster.id(4));
    let add = |replica: NodeId| Changes {
        add: BTreeMap::from([(replica, name(replica))]),
        remove: BTreeSet::new(),
    };
    super::apply(raft, &add(first), planned_at)
        .await
        .expect("planned from the current membership");

    let stale = super::apply(raft, &add(second), planned_at).await;
    assert!(
        stale.is_err(),
        "an edit planned before the change committed"
    );
    assert_eq!(learners_of(&cluster, leader), BTreeSet::from([first]));
    cluster.shutdown().await;
}

/// A pass that adds and removes, as replacing a wiped tail node's UUID
/// does, guards its second edit by the membership its first committed:
/// both commit, and the old learner is gone from the committed membership.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_add_and_a_remove_in_one_pass_both_commit() {
    let cluster = Cluster::start_with_tail(3, 2, &[0]).await;
    let leader = ready_leader(&cluster).await;
    let raft = cluster.node(leader).raft(0);
    let committed_at = || {
        *raft
            .metrics()
            .borrow_watched()
            .committed_membership_config
            .log_id()
    };
    let (old, new) = (cluster.id(3), cluster.id(4));
    let add_old = Changes {
        add: BTreeMap::from([(old, name(old))]),
        remove: BTreeSet::new(),
    };
    super::apply(raft, &add_old, committed_at())
        .await
        .expect("the old learner added");

    let replace = Changes {
        add: BTreeMap::from([(new, name(new))]),
        remove: BTreeSet::from([old]),
    };
    super::apply(raft, &replace, committed_at())
        .await
        .expect("both edits commit");
    assert_eq!(learners_of(&cluster, leader), BTreeSet::from([new]));
    // The metrics watch can trail the commit `apply` has already seen.
    raft.wait(WAIT)
        .metrics(
            |m| {
                m.committed_membership_config
                    .membership()
                    .non_write_voter_ids()
                    .eq([new])
            },
            "the old UUID is gone",
        )
        .await
        .expect("the old UUID is gone");
    cluster.shutdown().await;
}
