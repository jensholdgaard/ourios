//! The leader's miner configuration entry on the in-process cluster
//! (RFC 0065 §6.5): appended when none is in force or the leader's own
//! differs outside the tree's shape, in the shape in force, under the kind
//! gate, and over a snapshot whose checkpoint purged every one, RFC0065.31's
//! leader arm.

use ourios_config::MinerConfig;
use ourios_wal::{Control, MinerConfigV1};

use super::{
    PATIENCE, checkpoint_purging, group, log, pass, pass_until_assigned, placement, reads_up_to,
    ready_leader, strs,
};
use crate::replicated::raft::cluster::{Cluster, write};
use crate::replicated::raft::flusher::Steward;
use crate::replicated::raft::miner_config::body_of;
use crate::replicated::raft::snapshot_record::GroupMinerConfig;

fn own(threshold: f32, prefix_depth: u8) -> MinerConfig {
    MinerConfig::try_new(threshold, 256)
        .and_then(|c| c.with_prefix_depth(prefix_depth))
        .expect("valid")
}

fn steward(miner: MinerConfig) -> Steward {
    Steward::with_patience(PATIENCE).configured(miner)
}

/// The configuration node `i` applied last.
fn applied(cluster: &Cluster, i: usize) -> Option<MinerConfigV1> {
    let sm = cluster.node(i).host.state_machine(0).expect("hosted");
    sm.miner_config().map(GroupMinerConfig::body)
}

/// Wait until node `i` has applied everything `leader` committed so far.
async fn caught_up(cluster: &Cluster, leader: usize, i: usize) {
    let index = write(cluster.node(leader).raft(0), 9).await;
    cluster.node(i).applied_through(0, index).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn with_neither_log_nor_snapshot_holding_one_the_leader_appends_its_own() {
    let cluster = Cluster::start(3, &[0]).await;
    let leader = ready_leader(&cluster).await;
    let mine = own(0.8, 3);
    let mut steward = steward(mine);

    let first = pass(&mut steward, &cluster, leader, &[]).await;
    assert_eq!(first.configured, Some(body_of(&mine)));
    assert_eq!(
        group(&cluster, leader).controls().miner_config,
        Some(body_of(&mine))
    );
    let again = pass(&mut steward, &cluster, leader, &[]).await;
    assert_eq!(again.configured, None, "appended once in force");
    let follower = (0..3).find(|&i| i != leader).expect("a follower");
    caught_up(&cluster, leader, follower).await;
    assert_eq!(applied(&cluster, follower), Some(body_of(&mine)));
    cluster.shutdown().await;
}

/// The kind gate holds the first configuration entry back while a member
/// reads below `0x04`, and appends it once every member reads it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_kind_gate_holds_the_configuration_back() {
    let cluster = Cluster::start(3, &[0]).await;
    let leader = ready_leader(&cluster).await;
    let voter = (0..3).find(|&i| i != leader).expect("a follower");
    let mut steward = steward(MinerConfig::default());

    reads_up_to(&cluster, voter, Control::HIGHEST_KIND - 1);
    let held = pass(&mut steward, &cluster, leader, &[]).await;
    assert_eq!(held.configured, None, "a voter reads below the kind");
    assert_eq!(log(&cluster, leader).controls().highest_kind, 0);

    reads_up_to(&cluster, voter, Control::HIGHEST_KIND);
    let appended = pass(&mut steward, &cluster, leader, &[]).await;
    assert_eq!(appended.configured, Some(body_of(&MinerConfig::default())));
    cluster.shutdown().await;
}

/// Two leaders in turn with different thresholds: the second appends its
/// own in the shape the first put in force, whatever its own shape, so
/// the leader's wins; a leader differing only in shape appends nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_leaders_configuration_wins_in_the_shape_in_force() {
    let mut cluster = Cluster::start(3, &[0]).await;
    let first = ready_leader(&cluster).await;
    let mut steward_a = steward(own(0.8, 2));
    let put = pass(&mut steward_a, &cluster, first, &[]).await;
    assert_eq!(put.configured, Some(body_of(&own(0.8, 2))));

    cluster.kill(first).await;
    let second = cluster.leader(0, Some(first)).await;
    write(cluster.node(second).raft(0), 1).await;
    let mut deeper = steward(own(0.8, 5));
    let shape_only = pass(&mut deeper, &cluster, second, &[]).await;
    assert_eq!(shape_only.configured, None, "only the shape differs");

    let mut steward_b = steward(own(0.9, 5));
    let wins = pass(&mut steward_b, &cluster, second, &[]).await;
    let expected = MinerConfigV1 {
        prefix_depth: 2,
        ..body_of(&own(0.9, 5))
    };
    assert_eq!(wins.configured, Some(expected), "in the shape in force");
    let follower = (0..3)
        .find(|&i| i != first && i != second)
        .expect("a third");
    caught_up(&cluster, second, follower).await;
    assert_eq!(applied(&cluster, follower), Some(expected));
    cluster.shutdown().await;
}

/// Scenario RFC0065.31, the leader arm: a leader elected after a checkpoint
/// purged every configuration entry takes the snapshot's as in force,
/// appends nothing when its own differs only in `prefix_depth`, and
/// otherwise appends its own with the snapshot's `prefix_depth`, which
/// every replica then applies.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0065_31_a_leader_after_a_purging_checkpoint_appends_the_snapshots_shape() {
    let mut cluster = Cluster::start_with_tail(3, 2, &[0]).await;
    let leader = ready_leader(&cluster).await;
    let (placed, _) = placement(&cluster, 3..5);
    let placed = strs(&placed);
    let mut steward_a = steward(own(0.7, 2));
    let flusher = pass_until_assigned(&mut steward_a, &cluster, leader, &placed).await;
    let in_force = body_of(&own(0.7, 2));
    assert_eq!(
        group(&cluster, leader).controls().miner_config,
        Some(in_force)
    );

    let voters: Vec<_> = (0..3).collect();
    checkpoint_purging(&cluster, (leader, flusher), &voters).await;
    cluster.kill(leader).await;
    let successor = cluster.leader(0, Some(leader)).await;
    write(cluster.node(successor).raft(0), 1).await;
    assert_eq!(
        log(&cluster, successor).controls().miner_config,
        None,
        "purged"
    );
    let controls = group(&cluster, successor).controls();
    assert_eq!(controls.miner_config, Some(in_force), "the snapshot's");

    let mut deeper = steward(own(0.7, 5));
    let shape_only = pass(&mut deeper, &cluster, successor, &placed).await;
    assert_eq!(shape_only.configured, None, "only prefix_depth differs");

    let mut raised = steward(own(0.9, 5));
    let appended = pass(&mut raised, &cluster, successor, &placed).await;
    let expected = MinerConfigV1 {
        prefix_depth: 2,
        ..body_of(&own(0.9, 5))
    };
    assert_eq!(appended.configured, Some(expected), "the snapshot's shape");
    let follower = (0..3)
        .find(|&i| i != leader && i != successor)
        .expect("a third");
    caught_up(&cluster, successor, follower).await;
    assert_eq!(applied(&cluster, follower), Some(expected));
    cluster.shutdown().await;
}
