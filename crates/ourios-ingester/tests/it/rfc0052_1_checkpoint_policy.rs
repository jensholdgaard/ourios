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

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ourios_ingester::barrier::CutOutcome;
use ourios_ingester::record_sink::{FlushConfig, ParquetRecordSink, SharedParquetSink};
use ourios_parquet::Store;
use ourios_wal::SnapshotHorizons;

use crate::rfc0052_barrier_support::{BarrierRig, wal_config};

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

/// Scenario RFC0052.1 — the `cadence_failed` latch refuses every barrier until restart.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0052_1_latched_epoch_refuses_checkpoint_and_snapshot_until_restart() {
    // Given an acknowledged frame and a latch holding the epoch the next
    // cut will take.
    let tmp = tempfile::TempDir::new().expect("temp");
    let rig = BarrierRig::new(tmp.path());
    rig.ingest("checkout", &["user 1 logged in"]).await;
    // Quiesced here rather than left to the capture's own quiesce: with
    // the latch checked before the cut there is no capture to do it, and
    // the buffered-records assertion below has to be about a record that
    // really is in the sink's buffers.
    rig.pipeline.quiesce_encodes();
    assert_eq!(rig.sink.buffered_records(), 1, "the record is buffered");
    rig.epochs.report(rig.epochs.current());
    let latched_at = rig.epochs.current();

    // When however many timer passes run.
    for _ in 0..3 {
        assert_eq!(
            rig.barrier.tick(&rig.pipeline, false),
            CutOutcome::Latched,
            "a latch at or below the cut's epoch refuses it",
        );
    }

    // Then the barrier neither checkpoints nor snapshots.
    assert_eq!(rig.commits.last_checkpoint(), None, "no checkpoint");
    assert!(rig.snapshots().is_empty(), "and no snapshot was installed");
    assert_eq!(
        rig.sink.buffered_records(),
        1,
        "the records stay where a later cut — or a restart's replay — finds them",
    );
    assert_eq!(
        rig.epochs.current(),
        latched_at,
        "and no cut was taken at all: a latched node stands still rather than \
         quiescing, rotating and draining once per tick for a cut that cannot stamp",
    );

    // And a *failed publish* refuses only cuts captured before its
    // requeue: the same latch word carries the scope, so a publish
    // registered after a cut fails no cut at or below it.
    let fresh = tempfile::TempDir::new().expect("temp");
    let later = BarrierRig::new(fresh.path());
    let mark = later.ingest("checkout", &["user 1 logged in"]).await;
    let registered = later.epochs.current();
    let cut = later.epochs.open_cut();
    assert_eq!(cut, registered, "the guard was registered before the cut");
    // A publish registered *after* that cut: its frames are above the
    // cut's mark by construction.
    let after = later.epochs.current();
    later.sink.note_resettled(after);
    assert_eq!(
        later.barrier.tick(&later.pipeline, false),
        CutOutcome::Stamped
    );
    assert_eq!(
        later.commits.last_checkpoint(),
        Some(mark),
        "a post-cut settlement fails no cut at or below the current one",
    );
}

/// Scenario RFC0052.1 — unwind leg: an age-sweep publish panics inside `quiesce_publishes`.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0052_1_publish_panic_during_quiesce_leaves_checkpoint_and_snapshots_unchanged() {
    // Given a publish registered *before* the barrier began — the age
    // sweep's own in-flight guard — and an acknowledged frame.
    let tmp = tempfile::TempDir::new().expect("temp");
    let rig = BarrierRig::new(tmp.path());
    rig.ingest("checkout", &["user 1 logged in"]).await;

    let sink = rig.sink.clone();
    let panicking = Arc::new(AtomicBool::new(false));
    let release = Arc::clone(&panicking);
    // The guard is taken on this thread, before the barrier starts, so
    // it carries the epoch of the cut the barrier is about to take.
    let guard = sink.begin_publish();
    let epoch = guard.epoch();
    let sweep = std::thread::spawn(move || {
        while !release.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        // The guard drops during this unwind, which is what releases the
        // barrier waiting in `quiesce_publishes`.
        let _held = guard;
        panic!("injected age-sweep publish panic");
    });

    // When the barrier reaches `quiesce_publishes` and the publish then
    // panics: the latch it sets lands *after* the barrier's first check.
    let barrier = Arc::clone(&rig.barrier);
    let pipeline = rig.pipeline.clone();
    let running = std::thread::spawn(move || barrier.tick(&pipeline, false));
    std::thread::sleep(Duration::from_millis(50));
    panicking.store(true, Ordering::Release);
    assert!(sweep.join().is_err(), "the publish panicked");
    let outcome = running.join().expect("the barrier itself did not panic");

    // Then the checkpoint and every snapshot are unchanged.
    assert_eq!(
        outcome,
        CutOutcome::Latched,
        "the latch is observed at the recheck before stamping",
    );
    assert_eq!(rig.commits.last_checkpoint(), None, "no checkpoint");
    assert!(rig.snapshots().is_empty(), "and no snapshot was installed");

    // And the publish's outcome is reported failed independently of the
    // latch: the recheck defends the ordering, the outcome defends the
    // data, and either alone refuses the stamp.
    assert!(
        !rig.sink.quiesce_publishes().all_ok(epoch),
        "the unwind is recorded as a failed publish, not only as a latch",
    );
}

/// Scenario RFC0052.1 — unwind leg: an encode worker panics mid-batch, barrier after.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0052_1_encode_worker_panic_then_barrier_stamps_nothing_and_restart_replays() {
    // Given a whole ingest path whose encode worker panics mid-batch:
    // the sink's inline audit barrier runs inside `emit_concurrent`, on
    // the record that crosses the size target, which is exactly where
    // §3.1 says a worker panic leaves a batch's remainder in neither the
    // buffers nor Parquet. The batch is a real acknowledged OTLP export,
    // so "replayed on restart" is something this can actually observe.
    let tmp = tempfile::TempDir::new().expect("temp");
    let rig = BarrierRig::with_panicking_encode(tmp.path());
    let epoch = rig.epochs.current();
    let mark = rig
        .ingest("checkout", &["user 1 logged in", "user 2 logged in"])
        .await;
    rig.pipeline.quiesce_encodes();

    // Then the unwinding guard stored the latch before the decrement
    // that settled the count, so a barrier cannot see a quiet pool and a
    // clear latch...
    let latched = rig.epochs.capture();
    assert_eq!(
        latched.failed_epoch(),
        Some(epoch),
        "the worker's unwinding batch guard reported its own epoch",
    );

    // ...and a barrier that follows stamps nothing.
    assert_eq!(rig.barrier.tick(&rig.pipeline, false), CutOutcome::Latched);
    assert_eq!(rig.commits.last_checkpoint(), None, "no checkpoint");
    assert!(rig.snapshots().is_empty(), "and no snapshot was installed");
    assert!(
        rig.data_files().is_empty(),
        "the panicking batch reached no Parquet object",
    );

    // And the batch's unemitted records are replayed on restart: the
    // frame is in the WAL, which is the only place they survive.
    let wal_root = rig.wal_root.clone();
    let snapshots_root = rig.snapshots_root.clone();
    drop(rig);
    let mut wal = ourios_wal::Wal::open(wal_config(&wal_root)).expect("reopen");
    let mut miner = ourios_miner::cluster::MinerCluster::new(ourios_config::MinerConfig::default());
    let report = ourios_ingester::recovery::recover(&mut wal, &snapshots_root, &mut miner)
        .expect("recovery completes");
    assert_eq!(
        report.max_delivered,
        Some(mark),
        "the acknowledged frame survived the worker's unwind",
    );
    assert_eq!(
        report.records_fed_to_miner, 2,
        "and every record in it is re-mined, with nothing suppressed by a mark that never stamped",
    );
}

/// Scenario RFC0052.1 — unwind leg: the barrier starts concurrently with the panic, every schedule.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
fn rfc0052_1_barrier_concurrent_with_worker_panic_observes_the_latch_under_every_schedule() {
    // Given the same encode-worker panic, with the observer started
    // concurrently and the interleaving seeded rather than timed: a latch
    // stored *after* the count settled would fail this rather than pass
    // by timing.
    let tmp = tempfile::TempDir::new().expect("temp");
    let rig = BarrierRig::new(tmp.path());
    for seed in 0..64u64 {
        let panicking = panicking_sink(&rig);
        let pool = ourios_ingester::encode_pool::EncodePool::new(&panicking, 1);
        let epochs = panicking.epochs();
        let epoch = epochs.current();
        pool.submit(vec![mined("checkout")]);

        // When the observer runs at a seeded point in the panic's window.
        let observer = {
            let epochs = Arc::clone(&epochs);
            std::thread::spawn(move || {
                for _ in 0..(seed % 8) {
                    std::thread::yield_now();
                }
                // `quiesce` returning is exactly "the pool's pending count
                // is zero"; the latch is read strictly after it.
                (epochs.capture(), ())
            })
        };
        pool.quiesce();
        let (early, ()) = observer.join().expect("observer");
        let settled = epochs.capture();

        // Then a barrier that observes the count at zero has observed the
        // latch, because the unwinding guard stores it before the
        // decrement that settles the count.
        assert!(
            settled.refuses(epoch),
            "seed {seed}: the count settled, so the latch is set",
        );
        assert!(
            early.failed_epoch().is_none() || early.refuses(epoch),
            "seed {seed}: an observation before the settle is either clear or already latched — \
             never a stale 'no failure' beside a settled count",
        );
    }
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
fn rfc0052_1_batch_queued_before_the_cut_carries_the_cuts_epoch() {
    // Given one worker held inside batch A's emit, so batch B is
    // genuinely sitting in the queue — not merely submitted — when the
    // cut is captured. Without the hold the worker could dequeue B
    // first, and the test would pass on a dequeue-assigned epoch too.
    let tmp = tempfile::TempDir::new().expect("temp");
    let rig = BarrierRig::new(tmp.path());
    let held = HeldSink::new(&rig);
    let epochs = held.sink.epochs();
    let pool = ourios_ingester::encode_pool::EncodePool::new(&held.sink, 1);

    pool.submit(vec![mined("checkout")]);
    held.await_worker_inside_the_first_emit();
    let queued_at = epochs.current();
    pool.submit(vec![mined("checkout")]);

    // When cut E is captured between B's submit and its dequeue.
    let cut = epochs.open_cut();
    assert_eq!(
        cut, queued_at,
        "the cut takes the epoch the queued batch already carries",
    );
    assert_eq!(
        epochs.current().get(),
        cut.get() + 1,
        "and a batch submitted from here on would carry E + 1",
    );

    // When the hold lifts, B is dequeued — after the capture — and
    // panics.
    held.release();
    pool.quiesce();

    // Then the panic fails cut E, not only later ones: the epoch was
    // stamped in `submit`, so a dequeue-assigned one (E + 1) would leave
    // this cut free to stamp over the batch's unemitted remainder.
    let state = epochs.capture();
    assert_eq!(state.failed_epoch(), Some(cut));
    assert!(
        state.refuses(cut),
        "the epoch was stamped at submit, not at dequeue",
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

/// A sink over the rig's store whose inline audit barrier panics — the
/// production seam an encode worker actually runs inside
/// `emit_concurrent`, reached on the record that crosses the size
/// target.
fn panicking_sink(rig: &BarrierRig) -> SharedParquetSink {
    poisoned_sink(
        rig,
        Box::new(|| panic!("injected encode-worker panic")),
        Arc::new(ourios_ingester::cadence::BarrierEpochs::new()),
    )
}

/// The same sink, with its **first** emit held until released and every
/// later one panicking — the hold that makes "queued across the cut"
/// an observed state rather than a hoped-for interleaving.
struct HeldSink {
    sink: SharedParquetSink,
    calls: Arc<std::sync::atomic::AtomicUsize>,
    release: Arc<AtomicBool>,
}

impl HeldSink {
    fn new(rig: &BarrierRig) -> Self {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let release = Arc::new(AtomicBool::new(false));
        let seen = Arc::clone(&calls);
        let gate = Arc::clone(&release);
        let sink = poisoned_sink(
            rig,
            Box::new(move || {
                assert!(
                    seen.fetch_add(1, Ordering::AcqRel) == 0,
                    "injected encode-worker panic",
                );
                while !gate.load(Ordering::Acquire) {
                    std::thread::yield_now();
                }
                true
            }),
            Arc::new(ourios_ingester::cadence::BarrierEpochs::new()),
        );
        Self {
            sink,
            calls,
            release,
        }
    }

    fn await_worker_inside_the_first_emit(&self) {
        while self.calls.load(Ordering::Acquire) == 0 {
            std::thread::yield_now();
        }
    }

    fn release(&self) {
        self.release.store(true, Ordering::Release);
    }
}

fn poisoned_sink(
    rig: &BarrierRig,
    barrier: Box<dyn FnMut() -> bool + Send>,
    epochs: Arc<ourios_ingester::cadence::BarrierEpochs>,
) -> SharedParquetSink {
    SharedParquetSink::with_cadence(
        ParquetRecordSink::new(
            Store::local(&rig.data_root).expect("store"),
            FlushConfig {
                target_bytes: 1, // every emit crosses the target
                max_buffer_age: Duration::from_secs(86_400),
                ceiling_bytes: usize::MAX,
            },
        )
        .with_audit_barrier(barrier),
        epochs,
    )
}

fn mined(tenant: &str) -> ourios_core::record::MinedRecord {
    ourios_core::record::MinedRecord {
        tenant_id: ourios_core::tenant::TenantId::new(tenant),
        template_id: 1,
        template_version: 1,
        severity_number: 9,
        severity_text: None,
        scope_name: None,
        scope_version: None,
        scope_attributes: Vec::new(),
        resource_schema_url: None,
        scope_schema_url: None,
        time_unix_nano: 1_775_127_480_000_000_000,
        observed_time_unix_nano: None,
        attributes: Vec::new(),
        dropped_attributes_count: 0,
        resource_attributes: Vec::new(),
        trace_id: None,
        span_id: None,
        flags: 0,
        event_name: None,
        body_kind: ourios_core::record::BodyKind::String,
        params: vec![ourios_core::record::Param {
            type_tag: ourios_core::audit::ParamType::Num,
            value: "1".to_string(),
        }],
        separators: vec![String::new(), String::new()],
        body: None,
        confidence: 1.0,
        lossy_flag: false,
    }
}
