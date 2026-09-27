//! RFC0052.1 — The checkpoint advances only behind a proven publication
//! barrier.
//! See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the
//! RFC is red; each names the green slice that discharges it.
//!
//! Placement note: the barrier (`flush_then_snapshot` under the ingest
//! exclusion, RFC 0052 §3.1) is pipeline code, so the policy legs live
//! beside the RFC 0035 barrier tests. The retain path is already
//! exercised by the existing skip-the-snapshot tests (§6); these legs
//! extend them rather than duplicating. The unwind legs use the seeded
//! scheduler RFC0052.14 uses, so a latch stored after the pending count
//! settles fails the test rather than passing by timing.

/// Scenario RFC0052.1 — healthy store: the mark advances to the barrier's high-water mark.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.1 stub — implemented in the barrier green slice D (checkpoint behind both sinks fully drained)"]
fn rfc0052_1_healthy_store_advances_the_checkpoint_to_the_high_water_mark() {
    todo!(
        "RFC0052.1 — a WAL with acknowledged frames and a healthy record \
         sink; the barrier completes with both sinks fully drained: \
         last_checkpoint() equals the barrier's high-water mark; with a \
         None high-water mark no checkpoint is attempted"
    );
}

/// Scenario RFC0052.1 — failing store: a retained partition leaves the mark untouched.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.1 stub — implemented in the barrier green slice D (any retained partition on either sink → no checkpoint attempted)"]
fn rfc0052_1_failing_store_leaves_the_checkpoint_unchanged() {
    todo!(
        "RFC0052.1 — a store that fails one partition write so a sink \
         retains something: no checkpoint is attempted and \
         last_checkpoint() is unchanged after the barrier"
    );
}

/// Scenario RFC0052.1 — sidecar failure: the previous mark stays usable and fail-closed.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.1 stub — implemented in the barrier green slice D (Wal::checkpoint leaves the old mark intact on a failed sidecar write)"]
fn rfc0052_1_checkpoint_write_failure_keeps_the_previous_mark_usable() {
    todo!(
        "RFC0052.1 — healthy store, the CHECKPOINT sidecar write and its \
         directory fsync made to fail: the barrier still reports \
         success, last_checkpoint() is unchanged, and the next \
         housekeeping pass reclaims nothing that was not already \
         eligible under the previous mark — the fail-closed branch a \
         partition-write failure cannot reach"
    );
}

/// Scenario RFC0052.1 — the `cadence_failed` latch refuses every barrier until restart.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.1 stub — implemented in the barrier green slice D (epoch latch at or below the cut's epoch refuses checkpoint and snapshot)"]
fn rfc0052_1_latched_epoch_refuses_checkpoint_and_snapshot_until_restart() {
    todo!(
        "RFC0052.1 — while the cadence_failed latch holds an epoch at or \
         below the cut's, the barrier neither checkpoints nor snapshots \
         however many timer passes run; a failed publish refuses only \
         cuts captured before its requeue"
    );
}

/// Scenario RFC0052.1 — unwind leg: an age-sweep publish panics inside `quiesce_publishes`.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.1 stub — implemented in the barrier green slice D (latch recheck before stamping; publish outcome reported failed independently)"]
fn rfc0052_1_publish_panic_during_quiesce_leaves_checkpoint_and_snapshots_unchanged() {
    todo!(
        "RFC0052.1 — a test sink whose age-sweep publish, registered \
         before the barrier began, panics while the barrier waits in \
         quiesce_publishes: the latch set after the barrier's first \
         check is observed at its recheck before stamping, the \
         checkpoint and every snapshot are unchanged, and the publish's \
         outcome is reported failed"
    );
}

/// Scenario RFC0052.1 — unwind leg: an encode worker panics mid-batch, barrier after.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.1 stub — implemented in the barrier green slice D (unwinding guard stores the latch; unemitted records replay on restart)"]
fn rfc0052_1_encode_worker_panic_then_barrier_stamps_nothing_and_restart_replays() {
    todo!(
        "RFC0052.1 — a record that panics the encode worker mid-batch, \
         followed by a barrier: the checkpoint and every snapshot are \
         unchanged and the batch's unemitted records are replayed on \
         restart"
    );
}

/// Scenario RFC0052.1 — unwind leg: the barrier starts concurrently with the panic, every schedule.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.1 stub — implemented in the barrier green slice D (seeded scheduler covers the store-before-decrement order)"]
fn rfc0052_1_barrier_concurrent_with_worker_panic_observes_the_latch_under_every_schedule() {
    todo!(
        "RFC0052.1 — the same encode-worker panic with the barrier \
         started concurrently, under every seeded schedule: a barrier \
         that observes the pool's pending count at zero has observed \
         the latch, because the unwinding guard stores it before the \
         decrement that settles the count"
    );
}

/// Scenario RFC0052.1 — a panic in a cadence tick itself, and a `JoinError` at shutdown.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.1 stub — implemented in the timer green slice E (barrier-tick panic lowers failed_epoch; housekeeping-tick panic counts cadence_panic)"]
fn rfc0052_1_cadence_tick_panic_and_join_error_read_as_a_failed_cut() {
    todo!(
        "RFC0052.1 — a panic in the barrier tick outside any batch guard \
         lowers failed_epoch to that tick's epoch, invalidates the \
         pending slot, and the barrier task takes the next tick; a panic \
         in a housekeeping tick counts with error.type = cadence_panic, \
         leaves the checkpoint untouched, and the next pass re-plans the \
         uncommitted plan; a JoinError from either task at shutdown is \
         logged and read as a failed cut"
    );
}

/// Scenario RFC0052.1 — epoch is assigned in `submit`, so a straddling batch fails cut `E`.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.1 stub — implemented in the barrier green slice D (epoch stamped at submit, not at dequeue)"]
fn rfc0052_1_batch_queued_before_the_cut_carries_the_cuts_epoch() {
    todo!(
        "RFC0052.1 — a batch queued before cut E's capture and dequeued \
         after it carries epoch E, so a panic in it fails cut E and not \
         only later ones"
    );
}

/// Scenario RFC0052.1 — the publish guard is created in `submit` and settles on the last detach.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.1 stub — implemented in the barrier green slice D (guard-at-submit; shared completion across detached partitions)"]
fn rfc0052_1_publish_guard_covers_detaches_between_capture_and_enqueue() {
    todo!(
        "RFC0052.1 — a partition detached by a batch's first record with \
         the cut captured between the detach and the enqueue is covered, \
         the guard having been created in submit before the exclusion \
         was released; a batch that detaches nothing releases its guard \
         unused; a batch detaching several partitions settles its guard \
         only when the last completes, in any completion order"
    );
}

/// Scenario RFC0052.1 — publisher panic and closed-channel send park batches and respawn.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.1 stub — implemented in the barrier green slice D (bounded publisher queue; park under the sink lock before releasing the guard)"]
fn rfc0052_1_publisher_panic_parks_queued_batches_and_respawns() {
    todo!(
        "RFC0052.1 — a publisher panic with batches still queued behind \
         the failing one latches only the failing batch's epoch, parks \
         every queued batch in the buffers with its guard released, and \
         a quiesce_publishes started during the panic returns; the next \
         enqueue finds a respawned publisher; a worker whose send fails \
         on a closed channel parks that batch under the sink lock before \
         releasing its guard, so the records are covered by the next \
         drain"
    );
}

/// Scenario RFC0052.1 — a detached partition waits for the in-flight audit write it depends on.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.1 stub — implemented in the barrier green slice D (PublishItem::Detached carries audit_watermark; empty buffer is not a durable prefix)"]
fn rfc0052_1_detached_partition_waits_for_its_audit_watermark() {
    todo!(
        "RFC0052.1 — a detached partition whose audit_watermark is \
         covered by another writer's in-flight audit write is not \
         published until that write is durable: an empty audit buffer \
         is not read as a durable prefix, and when that write fails the \
         dependent partition requeues rather than landing in Parquet \
         ahead of its template events"
    );
}
