//! RFC0052.1 — Where the cut's epoch is stamped, and what the publish
//! guard covers.
//! See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
//!
//! The legs here run below the barrier, against an encode pool over a
//! sink whose inline audit barrier is the seam a worker really runs
//! inside `emit_concurrent`: the claims are about `submit` versus
//! dequeue, and about the guard's span, not about what a tick stamps.
//! The barrier-level unwinds are [`crate::rfc0052_1_unwind_policy`].
//! Stubs are `#[ignore]`d so the default run stays green while the RFC
//! is red; each names the green slice that discharges it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ourios_ingester::record_sink::{FlushConfig, ParquetRecordSink, SharedParquetSink};
use ourios_parquet::Store;

use crate::rfc0052_barrier_support::BarrierRig;

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
