//! RFC0052.1 — The checkpoint mark, and what each store outcome does to
//! it.
//! See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
//!
//! Placement note: the barrier (`flush_then_snapshot` under the ingest
//! exclusion, RFC 0052 §3.1) is pipeline code, so the policy legs live
//! beside the RFC 0035 barrier tests. The retain path is already
//! exercised by the existing skip-the-snapshot tests (§6); these legs
//! extend them rather than duplicating.
//!
//! RFC0052.1's other three claims are one file each, because each is a
//! different thing to be right about: [`crate::rfc0052_1_latch_policy`]
//! (what a latched node refuses and settles),
//! [`crate::rfc0052_1_unwind_policy`] (what an unwind leaves behind) and
//! [`crate::rfc0052_1_epoch_scope`] (where the epoch is stamped, and
//! what the publish guard covers).

use ourios_ingester::barrier::CutOutcome;
use ourios_wal::SnapshotHorizons;

use crate::rfc0052_barrier_support::BarrierRig;

/// Scenario RFC0052.1 — healthy store: the mark advances to the barrier's high-water mark.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0052_1_healthy_store_advances_the_checkpoint_to_the_high_water_mark() {
    // Given a WAL with acknowledged frames and a healthy record sink.
    let tmp = tempfile::TempDir::new().expect("temp");
    let rig = BarrierRig::new(tmp.path());
    let mark = rig.ingest("checkout", &["user 1 logged in"]).await;
    assert_eq!(
        rig.commits.last_checkpoint(),
        None,
        "nothing has stamped yet",
    );

    // When the barrier completes with both sinks fully drained.
    let outcome = rig.barrier.tick(&rig.pipeline, false);

    // Then the journal's checkpoint is advanced to that mark.
    assert_eq!(outcome, CutOutcome::Stamped);
    assert_eq!(
        rig.commits.last_checkpoint(),
        Some(mark),
        "the checkpoint is the barrier's high-water mark",
    );
    assert_eq!(rig.sink.buffered_records(), 0, "the cut drained the sink");
    assert!(
        !rig.data_files().is_empty(),
        "and the cut's own batch reached the store before the stamp",
    );

    // And with a `None` high-water mark, no checkpoint is attempted: a
    // node that has acknowledged nothing has nothing to declare
    // reclaimable.
    let idle_tmp = tempfile::TempDir::new().expect("temp");
    let idle = BarrierRig::new(idle_tmp.path());
    assert_eq!(idle.pipeline.last_durable(), None, "no acked frame");
    assert_eq!(
        idle.barrier.tick(&idle.pipeline, false),
        CutOutcome::Stamped
    );
    assert_eq!(
        idle.commits.last_checkpoint(),
        None,
        "no mark, so no checkpoint was attempted",
    );
}

/// Scenario RFC0052.1 — failing store: a retained partition leaves the mark untouched.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0052_1_failing_store_leaves_the_checkpoint_unchanged() {
    // Given a healthy first cut, so there is a previous mark to be
    // "unchanged" against.
    let tmp = tempfile::TempDir::new().expect("temp");
    let rig = BarrierRig::new(tmp.path());
    let first = rig.ingest("checkout", &["user 1 logged in"]).await;
    assert_eq!(rig.barrier.tick(&rig.pipeline, false), CutOutcome::Stamped);
    assert_eq!(rig.commits.last_checkpoint(), Some(first));

    // Given a store that fails the partition write, so a sink retains.
    let second = rig.ingest("checkout", &["user 2 logged in"]).await;
    assert_ne!(second, first, "the second frame is above the first mark");
    rig.sabotage_data_store();

    // When the barrier runs.
    let outcome = rig.barrier.tick(&rig.pipeline, false);

    // Then no checkpoint is attempted and the mark is unchanged.
    assert_eq!(outcome, CutOutcome::Retained);
    assert_eq!(
        rig.commits.last_checkpoint(),
        Some(first),
        "a retained partition leaves the previous mark exactly where it was",
    );
    assert_eq!(
        rig.sink.buffered_records(),
        1,
        "and the records are requeued, not lost (the WAL is the durability of record)",
    );
}

/// Scenario RFC0052.1 — sidecar failure: the previous mark stays usable and fail-closed.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0052_1_checkpoint_write_failure_keeps_the_previous_mark_usable() {
    // Given a healthy store and a first, successful stamp.
    let tmp = tempfile::TempDir::new().expect("temp");
    let rig = BarrierRig::new(tmp.path());
    let first = rig.ingest("checkout", &["user 1 logged in"]).await;
    assert_eq!(rig.barrier.tick(&rig.pipeline, false), CutOutcome::Stamped);
    assert_eq!(rig.commits.last_checkpoint(), Some(first));

    // Given the `CHECKPOINT` sidecar write and its directory fsync made
    // to fail.
    rig.sabotage_checkpoint();
    let second = rig.ingest("checkout", &["user 2 logged in"]).await;
    assert_ne!(second, first);

    // When the barrier runs.
    let outcome = rig.barrier.tick(&rig.pipeline, false);

    // Then the barrier still reports success — the data is in the store
    // either way, and refusing to ack over a reclamation error would turn
    // a disk-space problem into an availability one.
    assert_eq!(outcome, CutOutcome::Stamped);
    assert_eq!(
        rig.commits.last_checkpoint(),
        Some(first),
        "`Wal::checkpoint` leaves the previous mark intact on a failed write",
    );
    assert!(
        rig.data_files().len() >= 2,
        "the cut's own records did reach the store",
    );

    // And the next housekeeping pass reclaims nothing that was not
    // already eligible under the previous mark. Forbidding all
    // reclamation would reject the fail-closed behaviour §3.1 specifies
    // rather than test it, so the assertion is that the pass runs and
    // stays bounded by `first`.
    let progress = rig
        .commits
        .maintain(
            &SnapshotHorizons::NoConsumer,
            usize::try_from(ourios_wal::DEFAULT_MAX_UNLINKS_PER_PASS).expect("the cap fits"),
        )
        .expect("the pass runs against the previous mark");
    assert_eq!(
        rig.commits.last_checkpoint(),
        Some(first),
        "the pass did not move the mark either",
    );
    assert_eq!(
        progress.removed_segments, 0,
        "nothing above the previous mark became eligible (the only segment is the current one)",
    );
}
