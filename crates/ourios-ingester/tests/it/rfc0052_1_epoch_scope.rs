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

use ourios_core::audit::AuditSink;
use ourios_ingester::barrier::{CaptureOutcome, CutOutcome};
use ourios_ingester::publish::PublishCoordinator;
use ourios_ingester::record_sink::{FlushConfig, ParquetRecordSink, SharedParquetSink};
use ourios_parquet::Store;

use crate::rfc0052_barrier_support::{BarrierRig, Gate, held_puts};

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

/// Scenario RFC0052.1 — the capture's drain is registered above its own cut.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0052_1_a_captures_drain_is_registered_above_its_own_cut() {
    // Given one acknowledged, buffered batch and a store that will refuse
    // the publish — the arm that puts the cut's own batch back into the
    // buffers *dated*, which is where the registration becomes visible.
    let tmp = tempfile::TempDir::new().expect("temp");
    let rig = BarrierRig::new(tmp.path());
    rig.ingest("checkout", &["user 1 logged in"]).await;
    rig.pipeline.quiesce_encodes();
    let before = rig.sink.quiesce_publishes().recorded();

    // When a cut is captured and cannot publish.
    assert_eq!(
        rig.barrier.capture(&rig.pipeline, false),
        CaptureOutcome::Filled,
    );
    let cut = rig.barrier.pending_epoch().expect("a cut is pending");
    assert_eq!(
        rig.epochs.current().get(),
        cut.get() + 1,
        "the slot holds cut E, so a publish registering from here reads E + 1",
    );
    rig.sabotage_data_store();
    assert_eq!(rig.barrier.run_pending(), CutOutcome::Retained);

    // Then the requeue dated nothing, because the batch was registered at
    // `E + 1`: the epoch `current()` already read when the capture
    // drained. `note_resettled` discards a settlement whose date is at or
    // below its registration, so a recorded one here would mean the drain
    // ran while `current()` still read `E`.
    //
    // That is what opening the cut *before* the drain buys, and it is not
    // bookkeeping: `current()` has to mean "the next cut that will drain
    // the buffers" for the whole span in which this one is captured, so a
    // pre-cut publish parking concurrently settles at `E + 1` and refuses
    // `E`. Opened after the drain, such a park reads `E`, is discarded as
    // covered, and the cut stamps over records that exist only in buffers
    // it no longer holds — the escape `PublishCoordinator::park` exists to
    // close.
    assert_eq!(
        rig.sink.quiesce_publishes().recorded(),
        before,
        "a batch registered above the cut that drained it dates no settlement",
    );
    assert!(
        rig.sink.buffered_records() > 0,
        "and its records are back in the buffers, where the next cut drains them",
    );
}

/// Scenario RFC0052.1 — the publish guard is created in `submit` and settles on the last detach.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
fn rfc0052_1_publish_guard_covers_detaches_between_capture_and_enqueue() {
    // Given a batch still sitting in the queue — the worker is held
    // inside the previous batch's emit — and a publisher that will hold
    // the queued batch's PUT, so "detached but not durable" is an
    // observed state.
    let tmp = tempfile::TempDir::new().expect("temp");
    let rig = BarrierRig::new(tmp.path());
    let detach = Gate::default();
    let put = Gate::letting_through(1);
    let sink = gated_sink(&rig, Some(&detach), &put, Quarantine::Records);
    let epochs = sink.epochs();
    let pool = ourios_ingester::encode_pool::EncodePool::new(&sink, 1);
    let _release = (detach.opened_on_drop(), put.opened_on_drop());
    pool.submit(vec![mined("zulu")]);
    detach.await_entered(1);
    pool.submit(vec![mined("alpha")]);

    // When cut E is captured after its submit and before its worker has
    // dequeued it, let alone detached anything.
    let cut = epochs.open_cut();
    detach.open();
    pool.quiesce();
    put.await_entered(2);

    // Then its detach is already in flight at E: the guard was made in
    // `submit`, so the cut waits for its PUT instead of stamping past it.
    // A guard made by the worker would read E + 1 and let this wait
    // return at once.
    let waiting = held_wait(&sink, Some(cut), "the cut waits for the batch's publish");
    assert!(
        tenant_files(&rig, "alpha").is_empty(),
        "which is not durable yet"
    );
    put.open();
    let outcomes = waiting.finish();
    assert!(outcomes.all_ok(cut), "the detach settled durably");
    assert_eq!(tenant_files(&rig, "alpha").len(), 1);
    assert_eq!(sink.publishes_in_flight(), 0);

    a_batch_that_detaches_nothing_releases_its_guard_unused();
    a_batch_detaching_several_partitions_settles_on_the_last();
}

fn a_batch_that_detaches_nothing_releases_its_guard_unused() {
    // And a batch that detaches nothing releases its guard unused, at the
    // end of its own encode phase.
    let tmp = tempfile::TempDir::new().expect("temp");
    let rig = BarrierRig::new(tmp.path());
    let buffering = SharedParquetSink::new(ParquetRecordSink::new(
        Store::local(&rig.data_root).expect("store"),
        crate::rfc0052_barrier_support::never_flush(),
    ));
    let pool = ourios_ingester::encode_pool::EncodePool::new(&buffering, 1);
    pool.submit(vec![mined("alpha"), mined("bravo")]);
    pool.quiesce();
    assert_eq!(buffering.buffered_records(), 2, "both records are buffered");
    assert_eq!(
        buffering.publishes_in_flight(),
        0,
        "and the batch's guard settled with its encode phase",
    );
}

fn a_batch_detaching_several_partitions_settles_on_the_last() {
    // And a batch detaching several partitions settles its guard only
    // when the last of them completes: with the second one's PUT held,
    // the first being durable does not release the cut.
    let tmp = tempfile::TempDir::new().expect("temp");
    let rig = BarrierRig::new(tmp.path());
    let put = Gate::letting_through(1);
    let sink = gated_sink(&rig, None, &put, Quarantine::Records);
    let epochs = sink.epochs();
    let pool = ourios_ingester::encode_pool::EncodePool::new(&sink, 1);
    let _release = put.opened_on_drop();
    pool.submit(vec![mined("alpha"), mined("bravo")]);
    put.await_entered(2);
    pool.quiesce();
    let cut = epochs.open_cut();
    assert_eq!(
        tenant_files(&rig, "alpha").len(),
        1,
        "the first partition is durable"
    );
    assert_eq!(
        sink.publishes_in_flight(),
        1,
        "and the batch's one guard is still held for the second",
    );
    let waiting = held_wait(&sink, Some(cut), "so the cut still waits");
    put.open();
    assert!(waiting.finish().all_ok(cut));
    assert_eq!(tenant_files(&rig, "bravo").len(), 1);
    assert_eq!(sink.publishes_in_flight(), 0);
}

/// Scenario RFC0052.1 — publisher panic and closed-channel send park batches and respawn.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
fn rfc0052_1_publisher_panic_parks_queued_batches_and_respawns() {
    // Given a publisher held on one batch's PUT, with a batch behind it
    // whose publish will panic (a permanently rejected record, whose
    // quarantine write panics) and two more batches behind that — each
    // batch under its own cut, so the epochs tell them apart.
    let tmp = tempfile::TempDir::new().expect("temp");
    let rig = BarrierRig::new(tmp.path());
    let put = Gate::default();
    let sink = gated_sink(&rig, None, &put, Quarantine::Panics);
    let epochs = sink.epochs();
    let pool = ourios_ingester::encode_pool::EncodePool::new(&sink, 1);
    let _release = put.opened_on_drop();
    pool.submit(vec![mined("alpha")]);
    put.await_entered(1);
    let _alpha = epochs.open_cut();
    let failing = epochs.current();
    pool.submit(vec![poisoned("papa")]);
    let _papa = epochs.open_cut();
    pool.submit(vec![mined("bravo")]);
    pool.submit(vec![mined("charlie")]);
    pool.quiesce();
    assert_eq!(sink.buffered_records(), 0, "every batch is detached");

    // When the held PUT is released and the next publish panics, with a
    // `quiesce_publishes` already waiting.
    let waiting = held_wait(&sink, None, "the wait is held by the publisher");
    put.open();

    // Then the wait returns, only the failing batch's epoch is latched —
    // not the durable batch's before it, nor the parked ones' after — and
    // every queued batch is back in the buffers with its guard released.
    let _outcomes = waiting.finish();
    assert_eq!(epochs.capture().failed_epoch(), Some(failing));
    assert_eq!(
        tenant_files(&rig, "alpha").len(),
        1,
        "the batch ahead landed"
    );
    assert_eq!(sink.buffered_records(), 2, "bravo and charlie were parked");
    assert_eq!(sink.publishes_in_flight(), 0);

    // And the first enqueue after it finds the retired publisher: that
    // batch is parked under the sink lock before its guard is released,
    // so the next drain covers it — and the publisher is restarted.
    pool.submit(vec![mined("delta")]);
    pool.quiesce();
    assert_eq!(sink.buffered_records(), 3, "delta was parked, not dropped");
    assert_eq!(
        sink.publishes_in_flight(),
        0,
        "and its guard released after the park"
    );

    // And the enqueue after that is written by the new publisher.
    pool.submit(vec![mined("echo")]);
    pool.quiesce();
    let _outcomes = sink.quiesce_publishes();
    assert_eq!(
        tenant_files(&rig, "echo").len(),
        1,
        "the respawned publisher wrote it"
    );
    assert_eq!(
        sink.buffered_records(),
        3,
        "and the parked batches wait for a drain"
    );
    sink.flush_all();
    assert_eq!(
        sink.buffered_records(),
        0,
        "which publishes every one of them"
    );
    for tenant in ["bravo", "charlie", "delta"] {
        assert_eq!(
            tenant_files(&rig, tenant).len(),
            1,
            "{tenant} reached the store"
        );
    }
}

/// Scenario RFC0052.1 — a detached partition waits for the in-flight audit write it depends on.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
fn rfc0052_1_detached_partition_waits_for_its_audit_watermark() {
    // Given the production wiring — the record sink's inline barrier is
    // the audit sink's own `barrier` — and a template event taken out of
    // the audit buffer by an age drain whose write has not finished: the
    // buffer is empty, and the event is not durable.
    let tmp = tempfile::TempDir::new().expect("temp");
    let rig = BarrierRig::new(tmp.path());
    let barrier_audit = rig.audit.clone();
    let sink = SharedParquetSink::new(
        ParquetRecordSink::new(
            Store::local(&rig.data_root).expect("store"),
            FlushConfig {
                target_bytes: 1, // every emit crosses the target
                max_buffer_age: Duration::ZERO,
                ceiling_bytes: usize::MAX,
            },
        )
        .with_audit_barrier(Box::new(move || barrier_audit.barrier())),
    );
    let coordinator = PublishCoordinator::new(sink.clone(), rig.audit.clone());
    let pool = ourios_ingester::encode_pool::EncodePool::with_publisher(coordinator.publisher(), 1);
    let mut events = rig.audit.clone();
    events.emit(template_event("alpha"));
    let in_flight = coordinator.drain_aged();
    assert_eq!(rig.audit.buffered_events(), 0, "the audit buffer is empty");

    // When a record that depends on it crosses the size target.
    pool.submit(vec![mined("alpha")]);
    pool.quiesce();

    // Then it is not published: the empty buffer is not read as a durable
    // prefix, so the partition is never detached and stays in the buffers.
    assert!(
        rig.data_files().is_empty(),
        "no record ahead of its template event"
    );
    assert_eq!(sink.buffered_records(), 1);
    assert_eq!(
        sink.publishes_in_flight(),
        1,
        "the only publish in flight is the age drain's own",
    );

    // And when that in-flight write fails, the dependent partition still
    // does not land: the event is back in the buffer and the store
    // refuses it, so the next trigger is refused too.
    let audit_root = rig.audit_root.clone();
    std::fs::remove_dir_all(&audit_root).expect("remove audit root");
    std::fs::write(&audit_root, b"not a directory").expect("sabotage audit store");
    assert!(
        !coordinator.write_ordered(in_flight, "age"),
        "the in-flight audit write failed",
    );
    assert_eq!(rig.audit.buffered_events(), 1, "its event was requeued");
    pool.submit(vec![mined("alpha")]);
    pool.quiesce();
    assert!(
        rig.data_files().is_empty(),
        "still nothing ahead of the event"
    );
    assert_eq!(
        sink.buffered_records(),
        2,
        "the partition waits in the buffers"
    );

    // And once the store recovers, the next trigger writes the event and
    // then the records it gates.
    std::fs::remove_file(&audit_root).expect("unsabotage");
    std::fs::create_dir_all(&audit_root).expect("audit root");
    pool.submit(vec![mined("alpha")]);
    pool.quiesce();
    let _outcomes = sink.quiesce_publishes();
    assert_eq!(rig.audit.buffered_events(), 0, "the event is durable");
    assert!(
        !crate::rfc0052_barrier_support::parquet_files(&audit_root).is_empty(),
        "in the audit store",
    );
    assert_eq!(sink.buffered_records(), 0, "and every record followed it");
    assert!(!rig.data_files().is_empty());
}

/// Start `quiesce_publishes_through(cut)` — or `quiesce_publishes()` for
/// `None` — on its own thread, and assert it is still waiting.
fn held_wait(
    sink: &SharedParquetSink,
    cut: Option<ourios_ingester::cadence::Epoch>,
    why: &str,
) -> HeldWait {
    let (done, settled) = std::sync::mpsc::channel();
    let sink = sink.clone();
    std::thread::spawn(move || {
        let outcomes = match cut {
            Some(cut) => sink.quiesce_publishes_through(cut),
            None => sink.quiesce_publishes(),
        };
        let _ = done.send(outcomes);
    });
    for _ in 0..256 {
        std::thread::yield_now();
    }
    assert!(
        matches!(
            settled.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ),
        "{why}",
    );
    HeldWait { settled }
}

/// A publish wait started by [`held_wait`].
struct HeldWait {
    settled: std::sync::mpsc::Receiver<ourios_ingester::record_sink::PublishOutcomes>,
}

impl HeldWait {
    /// The wait's outcome — failing, not hanging, when a stranded guard
    /// means it never returns.
    fn finish(self) -> ourios_ingester::record_sink::PublishOutcomes {
        self.settled
            .recv_timeout(Duration::from_secs(30))
            .expect("the publish wait returned")
    }
}

/// What the record sink's quarantine write — the RFC 0025 §3.3 audit
/// emit a permanently rejected record takes on the publisher's thread —
/// does in a leg.
#[derive(Clone, Copy)]
enum Quarantine {
    /// Records the event and returns, as production does.
    Records,
    /// Panics: the injected publisher panic.
    Panics,
}

impl AuditSink for Quarantine {
    fn emit(&mut self, _event: ourios_core::audit::AuditEvent) {
        assert!(matches!(self, Self::Records), "injected publisher panic");
    }
}

/// A sink whose every emit crosses the size target, whose inline audit
/// barrier waits at `detach` (when given) on its first call, whose data
/// store holds its PUTs at `put`, and whose quarantine write is
/// `quarantine`.
fn gated_sink(
    rig: &BarrierRig,
    detach: Option<&Gate>,
    put: &Gate,
    quarantine: Quarantine,
) -> SharedParquetSink {
    let detach = detach.cloned();
    let first = Arc::new(AtomicBool::new(true));
    SharedParquetSink::new(
        ParquetRecordSink::new(
            held_puts(Store::local(&rig.data_root).expect("store"), put),
            FlushConfig {
                target_bytes: 1,
                max_buffer_age: Duration::from_secs(86_400),
                ceiling_bytes: usize::MAX,
            },
        )
        .with_audit_barrier(Box::new(move || {
            if first.swap(false, Ordering::AcqRel)
                && let Some(detach) = &detach
            {
                detach.wait_here();
            }
            true
        }))
        .with_audit_sink(Box::new(quarantine)),
    )
}

/// The data files written for `tenant`.
fn tenant_files(rig: &BarrierRig, tenant: &str) -> Vec<std::path::PathBuf> {
    rig.data_files()
        .into_iter()
        .filter(|path| path.to_string_lossy().contains(tenant))
        .collect()
}

/// A permanently rejected record: `observed_time_unix_nano` past
/// `i64::MAX` trips RFC 0005 §3.2's timestamp contract at encode, so its
/// publish goes through the quarantine write.
fn poisoned(tenant: &str) -> ourios_core::record::MinedRecord {
    ourios_core::record::MinedRecord {
        observed_time_unix_nano: Some(u64::MAX),
        ..mined(tenant)
    }
}

fn template_event(tenant: &str) -> ourios_core::audit::AuditEvent {
    ourios_core::audit::AuditEvent {
        tenant_id: ourios_core::tenant::TenantId::new(tenant),
        timestamp: std::time::UNIX_EPOCH + Duration::from_secs(1_775_127_480),
        payload: ourios_core::audit::AuditPayload::Template {
            template_id: 1,
            triggering_line_hash: ourios_core::audit::hash_triggering_line(b"user 1 logged in"),
            triggering_line_sample: Some("user 1 logged in".to_owned()),
            change: ourios_core::audit::TemplateChange::Created {
                new_template: "user <*> logged in".to_owned(),
            },
        },
    }
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
