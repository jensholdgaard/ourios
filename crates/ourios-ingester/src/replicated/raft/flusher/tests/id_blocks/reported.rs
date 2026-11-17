//! The tests the steward's own top-up was specified by, run as it now
//! works: on a report from its flusher. A report whose `mined` precedes
//! every entry counts every block the log holds, as the leader counted
//! them itself before the first checkpoint.

use openraft::vote::leader_id_std::CommittedLeaderId;

#[allow(clippy::wildcard_imports)] // the parent's fixtures, as its own tests use them
use super::*;
use crate::replicated::raft::telemetry::tests::Reader;
use crate::replicated::raft::types::LogId;

/// A report from a flusher that has mined nothing and holds no block.
fn from_the_start() -> Reports {
    let reports = Reports::default();
    reports.record(Reported {
        mined: LogId {
            leader_id: CommittedLeaderId::new(0),
            index: 0,
        },
        unstarted: 0,
    });
    reports
}

fn steward(ids: Store) -> Steward {
    super::steward(ids).with_reports(from_the_start())
}

fn counted(reader: &Reader) -> Steward {
    super::counted(reader).with_reports(from_the_start())
}

/// A fresh group gets one block a pass until it holds three, each above
/// the last, and a pass once they have committed appends none.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fresh_group_gets_three_blocks_and_then_none() {
    let cluster = Cluster::start(3, &[0]).await;
    let leader = ready_leader(&cluster).await;
    let store = seated_store(5_000);
    let mut steward = steward(store.clone());

    for n in 0..u64::from(AHEAD) {
        let topped = pass(&mut steward, &cluster, leader, &[]).await;
        let expected = block(5_000 + n * BLOCK);
        assert_eq!(topped.id_block, Some(TopUp::Appended(expected)));
    }
    let ahead = Ahead::of(group(&cluster, leader));
    assert_eq!(ahead.blocks, AHEAD);
    let again = pass(&mut steward, &cluster, leader, &[]).await;
    assert_eq!(again.id_block, None, "three are ahead");
    assert_eq!(high_water(&store).await, Some(5_000 + 3 * BLOCK));
    let follower = (0..3).find(|&i| i != leader).expect("a follower");
    let index = write(cluster.node(leader).raft(0), 1).await;
    cluster.node(follower).applied_through(0, index).await;
    assert_eq!(Ahead::of(group(&cluster, follower)), ahead);
    cluster.shutdown().await;
}

/// The store's high-water covers the block before its entry exists: the
/// reservation is held once its write lands, and the log holds no block
/// until it is let go.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_block_is_reserved_before_its_entry_exists() {
    let cluster = Cluster::start(3, &[0]).await;
    let leader = ready_leader(&cluster).await;
    let (store, mut reservation) = pausing(seated_store(0));
    let mut steward = steward(store.clone());

    let watch = async {
        let reserved_through = high_water(&store).await;
        let held = Ahead::of(group(&cluster, leader));
        (reserved_through, held)
    };
    let (topped, (reserved_through, held)) = while_reserved(
        &mut (&mut steward, &cluster, leader),
        passing,
        &mut reservation,
        watch,
    )
    .await;

    assert_eq!(held.blocks, 0, "no entry before the reservation is durable");
    assert_eq!(topped.id_block, Some(TopUp::Appended(block(0))));
    assert!(reserved_through >= Some(BLOCK), "{reserved_through:?}");
    cluster.shutdown().await;
}

/// A reservation that outlives the membership the kind gate admitted it
/// under appends nothing: while it is held, the leader admits a tail member
/// reading below `0x03`, which the gate never asked. The block's ids are
/// only skipped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reservation_outliving_its_membership_appends_nothing() {
    let cluster = Cluster::start_with_tail(3, 1, &[0]).await;
    let leader = ready_leader(&cluster).await;
    let below = 3;
    reads_up_to(&cluster, below, id_blocks::KIND - 1);
    let (store, mut reservation) = pausing(seated_store(0));
    let mut steward = steward(store.clone());

    let admit_below = async {
        let learner = cluster.id(below);
        let add = ChangeMembers::AddNodes(BTreeMap::from([(learner, name(learner))]));
        let raft = cluster.node(leader).raft(0);
        raft.change_membership(add, false).await.expect("admitted");
    };
    let (passed, ()) = while_reserved(
        &mut (&mut steward, &cluster, leader),
        passing,
        &mut reservation,
        admit_below,
    )
    .await;

    assert_eq!(passed.id_block, None, "admitted under another membership");
    held_back_after_reserving(&cluster, leader, &store).await;
    cluster.shutdown().await;
}

/// A reservation that outlives the term the kind gate admitted it under
/// appends nothing, even when the same node leads the next term.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reservation_outliving_its_term_appends_nothing() {
    let cluster = Cluster::start(3, &[0]).await;
    let leader = ready_leader(&cluster).await;
    let (store, mut reservation) = pausing(seated_store(0));
    let mut steward = steward(store.clone());

    let next_term = async {
        let term = cluster
            .node(leader)
            .raft(0)
            .metrics()
            .borrow_watched()
            .current_term;
        lead_a_later_term(&cluster, leader, term).await;
    };
    let (passed, ()) = while_reserved(
        &mut (&mut steward, &cluster, leader),
        passing,
        &mut reservation,
        next_term,
    )
    .await;

    assert_eq!(passed.id_block, None, "admitted under another term");
    held_back_after_reserving(&cluster, leader, &store).await;
    cluster.shutdown().await;
}

/// A reservation that fails appends nothing and says why; so does a store
/// without the high-water, which a reservation never creates.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_or_absent_reservation_appends_nothing() {
    let cluster = Cluster::start(3, &[0]).await;
    let leader = ready_leader(&cluster).await;
    let malformed = Store::in_memory();
    malformed
        .put_blocking(HIGH_WATER_KEY, b"not json".to_vec())
        .expect("put");
    let cases = [
        (malformed, Refusal::Failed("malformed")),
        (Store::in_memory(), Refusal::Absent),
    ];
    for (store, refusal) in cases {
        let mut steward = steward(store.clone());
        let refused = pass(&mut steward, &cluster, leader, &[]).await;
        assert_eq!(refused.id_block, Some(TopUp::Refused(refusal)));
        assert_eq!(Ahead::of(group(&cluster, leader)).blocks, 0);
    }
    assert!(
        log(&cluster, leader).controls().highest_kind > id_blocks::KIND,
        "the miner configuration was appended"
    );
    cluster.shutdown().await;
}

/// While a member reads below `0x03` the leader reserves nothing and
/// appends no block; once every member reads it, a block is appended even
/// though the miner configuration, `0x04`, is still held.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_kind_gate_holds_the_block_back() {
    let cluster = Cluster::start(3, &[0]).await;
    let leader = ready_leader(&cluster).await;
    let voter = (0..3).find(|&i| i != leader).expect("a follower");
    let store = seated_store(0);
    let mut steward = steward(store.clone());

    reads_up_to(&cluster, voter, id_blocks::KIND - 1);
    let held = pass(&mut steward, &cluster, leader, &[]).await;
    assert_eq!(held.id_block, None, "a voter reads below the kind");
    assert_eq!(log(&cluster, leader).controls().highest_kind, 0);
    assert_eq!(high_water(&store).await, Some(0), "nothing reserved");

    reads_up_to(&cluster, voter, id_blocks::KIND);
    let appended = pass(&mut steward, &cluster, leader, &[]).await;
    assert_eq!(appended.configured, None, "0x04 is still held");
    assert_eq!(appended.id_block, Some(TopUp::Appended(block(0))));
    assert_eq!(
        log(&cluster, leader).controls().highest_kind,
        id_blocks::KIND
    );
    cluster.shutdown().await;
}

/// A leader cut off after appending a block it reserved never commits it,
/// and the next leader reserves above it: its ids are only skipped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deposed_leaders_block_is_only_skipped_ids() {
    let mut cluster = Cluster::start(3, &[0]).await;
    let first = ready_leader(&cluster).await;
    let store = seated_store(0);
    let mut deposed = steward(store.clone());
    let committed = pass(&mut deposed, &cluster, first, &[]).await;
    assert_eq!(committed.id_block, Some(TopUp::Appended(block(0))));

    for other in (0..3).filter(|&i| i != first) {
        cluster.cut(first, other);
    }
    let cut_off = deposed.pass(group(&cluster, first), super::super::tail(&[], &cluster));
    let timed_out = tokio::time::timeout(Duration::from_secs(3), cut_off).await;
    assert!(timed_out.is_err(), "no quorum commits the block");
    let uncommitted = Ahead::of(group(&cluster, first));
    assert_eq!(
        uncommitted.latest,
        Some(block(BLOCK)),
        "appended, uncommitted"
    );
    assert_eq!(high_water(&store).await, Some(2 * BLOCK));

    cluster.kill(first).await;
    let second = cluster.leader(0, Some(first)).await;
    write(cluster.node(second).raft(0), 1).await;
    let mut successor = steward(store.clone());
    let next = pass(&mut successor, &cluster, second, &[]).await;
    assert_eq!(next.id_block, Some(TopUp::Appended(block(2 * BLOCK))));
    let ahead = Ahead::of(group(&cluster, second));
    assert_eq!(ahead.blocks, 2, "the deposed leader's block is not there");
    cluster.shutdown().await;
}

/// RFC0065.29: with a membership change in flight the leader appends no
/// new kind, the assignment, the miner configuration nor an id block, and
/// counts each hold; once it commits, all three are appended.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_membership_change_in_flight_holds_back_every_new_kind() {
    let cluster = Cluster::start_with_tail(3, 2, &[0]).await;
    let leader = ready_leader(&cluster).await;
    let (placed, order) = placement(&cluster, 3..5);
    let first = [placed[0].as_str()];
    admit(&cluster, leader, &first).await;
    heard_from(&cluster, leader, &order[..1]).await;
    let second = (cluster.id(order[1]), placed[1].clone());
    let (membership, followers) = membership_change_in_flight(&cluster, leader, second).await;

    let reader = Reader::new();
    let mut steward = counted(&reader).reserving(seated_store(0));
    let held = pass(&mut steward, &cluster, leader, &first).await;
    assert_eq!(
        held,
        Pass::default(),
        "a new kind beside a membership change"
    );
    assert_eq!(log(&cluster, leader).controls().highest_kind, 0);
    let holds = reader.points(semconv::OURIOS_WAL_REPLICATION_CONTROL_HOLDS);
    assert_eq!(holds, [counted_as("membership_change", 3)], "one per kind");

    for &f in &followers {
        cluster.node(f).gate.open();
    }
    membership_committed(membership, &cluster, leader).await;
    let both = [placed[0].as_str(), placed[1].as_str()];
    let appended = pass(&mut steward, &cluster, leader, &both).await;
    assert_eq!(appended.assigned, Some(cluster.id(order[0])));
    assert!(appended.configured.is_some(), "the configuration held back");
    assert!(
        matches!(appended.id_block, Some(TopUp::Appended(_))),
        "the id block held back: {appended:?}"
    );
    cluster.shutdown().await;
}

/// The leader counts each kind its gate holds back in a pass for a member
/// reading below it, the flusher assignment's, the miner configuration's
/// and the id block's, and the assignment once it commits, each by group
/// and reason; a pass that holds back nothing counts nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_held_kind_and_committed_assignment_is_counted() {
    let cluster = Cluster::start_with_tail(3, 2, &[0]).await;
    let leader = ready_leader(&cluster).await;
    let (placed, order) = placement(&cluster, 3..5);
    let placed = strs(&placed);
    let voter = (0..3).find(|&i| i != leader).expect("a follower");
    let reader = Reader::new();
    let mut steward = counted(&reader).reserving(seated_store(0));
    let holds = semconv::OURIOS_WAL_REPLICATION_CONTROL_HOLDS;
    let assignments = semconv::OURIOS_WAL_REPLICATION_FLUSHER_ASSIGNMENTS;

    admit(&cluster, leader, &placed).await;
    heard_from(&cluster, leader, &order).await;
    reads_up_to(&cluster, voter, 0);
    for n in 1..=2 {
        let held = pass(&mut steward, &cluster, leader, &placed).await;
        assert_eq!(held.assigned, None, "a voter reads below the kind");
        let counted = reader.points(holds);
        assert_eq!(counted, [counted_as("member_unconfirmed", 3 * n)]);
    }
    assert!(reader.points(assignments).is_empty(), "nothing committed");

    reads_up_to(&cluster, voter, 1);
    let first = pass(&mut steward, &cluster, leader, &placed).await;
    assert_eq!(first.assigned, Some(cluster.id(order[0])));
    assert_eq!(first.configured, None, "a voter reads below 0x04");
    assert_eq!(first.id_block, None, "a voter reads below 0x03");
    reads_up_to(&cluster, voter, Control::HIGHEST_KIND);
    let configured = pass(&mut steward, &cluster, leader, &placed).await;
    assert!(configured.configured.is_some());
    assert!(matches!(configured.id_block, Some(TopUp::Appended(_))));
    for _ in 1..AHEAD {
        let topped = pass(&mut steward, &cluster, leader, &placed).await;
        assert!(matches!(topped.id_block, Some(TopUp::Appended(_))));
    }
    let kept = pass(&mut steward, &cluster, leader, &placed).await;
    assert_eq!(kept, Pass::default());
    let counted = reader.points(assignments);
    assert_eq!(counted, [counted_as("unassigned", 1)]);
    let counted = reader.points(holds);
    assert_eq!(counted, [counted_as("member_unconfirmed", 8)]);
    cluster.shutdown().await;
}
