//! The bounded worker pool for the concurrent encode phase
//! (RFC 0035 §3.1 Design A).
//!
//! The ingest pipeline's ordered phase (Drain match + template-id
//! assignment under the global gate) hands each batch's mined records
//! here; N workers append them to the record sink off the gate
//! ([`SharedParquetSink::detach_concurrent`]) and hand whatever the size
//! and ceiling triggers take to RFC 0052 §3.1's publisher, so no worker
//! is ever inside a PUT.
//!
//! - **Backpressure**: the queue is bounded — a full queue blocks
//!   [`EncodePool::submit`], which runs under the ingest gate, so an
//!   encode-bound burst throttles admission instead of growing an
//!   unbounded in-flight backlog (§3.1 / hazard #4). Depth is exported
//!   as `ourios.ingest.encode.queue_depth`.
//! - **Quiesce**: [`EncodePool::quiesce`] blocks until every submitted
//!   batch has finished its encode phase. Whole-pool (not keyed to a WAL
//!   mark) — sound because submission happens inside the gated region,
//!   so at any rotation check every in-flight encode is for a frame at
//!   or below the mark; see the §3.1 barrier reasoning in
//!   `receiver/pipeline.rs`. Rotation is per-segment (128 MiB default),
//!   so the drain's cost amortizes to ~zero.

use std::sync::{Arc, Condvar, Mutex, PoisonError};

use ourios_core::record::MinedRecord;

use crate::cadence::{BarrierEpochs, Epoch};
use crate::lane::{Lane, Serve};
use crate::metrics::EncodePoolMetrics;
use crate::publisher::{BatchCompletion, Detached, Publisher};
use crate::record_sink::SharedParquetSink;

/// Upper bound on encode workers (see `new`): thread-budget +
/// overflow safety for the queue-capacity multiplication.
const MAX_WORKERS: usize = 256;
/// Queue bound in batches per worker: deep enough to keep workers fed,
/// shallow enough that the in-flight backlog stays a few batches per
/// core (memory bound + backpressure to the gate).
const QUEUE_BATCHES_PER_WORKER: usize = 4;

/// Outstanding-batch accounting shared between submitters, workers, and
/// the quiesce waiter.
struct Pending {
    count: Mutex<usize>,
    idle: Condvar,
}

impl Pending {
    fn decrement(&self) {
        let mut count = self.count.lock().unwrap_or_else(PoisonError::into_inner);
        *count = count.saturating_sub(1);
        if *count == 0 {
            self.idle.notify_all();
        }
    }
}

/// Decrements the pending count on drop — so a batch is settled even if
/// its emit panics mid-batch. Without this, a worker panic would strand
/// `quiesce` forever, which turns one poisoned record into a wedged
/// rotation barrier (and a hung shutdown).
///
/// RFC 0052 §3.1 makes it do two more things. It is **constructed in
/// `submit`**, under the barrier exclusion, and travels with the queued
/// batch: constructed after dequeue it would read `E + 1` for a batch
/// queued before cut `E`'s capture, and a later panic in it would fail
/// the wrong cut. And an unwinding drop **reports** its epoch to the
/// cadence latch *before* the decrement that settles the count — so a
/// barrier that observes the pool's pending count at zero has already
/// observed the latch. The emit runs after the frame was acknowledged,
/// so a worker panic mid-batch leaves the batch's unemitted remainder in
/// neither the buffers nor Parquet; without the report, a barrier after
/// it would stamp across that frame.
struct BatchGuard {
    pending: Arc<Pending>,
    metrics: Arc<EncodePoolMetrics>,
    epochs: Arc<BarrierEpochs>,
    epoch: Epoch,
}

impl Drop for BatchGuard {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.epochs.report(self.epoch);
        }
        self.pending.decrement();
        self.metrics.batch_completed();
    }
}

/// One batch on its way to a worker, carrying the guards `submit`
/// created for it under the exclusion: the encode phase's, and the
/// publish completion every partition it detaches will share.
struct QueuedBatch {
    records: Vec<MinedRecord>,
    guard: BatchGuard,
    completion: Arc<BatchCompletion>,
}

/// A worker's work: append each record to the buffers and hand whatever
/// the size and ceiling triggers detach to the publisher — never a PUT
/// of its own (RFC 0052 §3.1).
struct Encode {
    sink: SharedParquetSink,
    publisher: Publisher,
}

impl Serve<QueuedBatch> for Encode {
    fn serve(&self, batch: QueuedBatch) {
        let QueuedBatch {
            records,
            guard,
            completion,
        } = batch;
        let _settle = guard;
        for record in records {
            for (trigger, taken) in self.sink.detach_concurrent(record) {
                if !taken.is_empty() {
                    self.publisher
                        .publish(Detached::new(taken, trigger, &completion));
                }
            }
        }
    }

    /// A batch queued behind a panicking one is still encoded: its
    /// records were acknowledged, and settling it without an emit would
    /// strand them outside both the buffers and the store (issue #837).
    fn salvage(&self, batch: QueuedBatch) {
        self.serve(batch);
    }
}

/// A bounded pool of OS threads appending mined-record batches to the
/// record sink. Dedicated threads rather than the tokio blocking pool: the
/// encode is CPU-bound and its concurrency must stay fixed at the
/// configured worker count, not compete with the runtime's elastic
/// blocking pool. A worker panic retires the pool's generation — the
/// batches queued behind it are still encoded — and the next `submit`
/// starts a new one (`crate::lane`). Dropping the pool closes the queue
/// and joins the workers.
pub struct EncodePool {
    lane: Lane<QueuedBatch>,
    sink: SharedParquetSink,
    pending: Arc<Pending>,
    metrics: Arc<EncodePoolMetrics>,
    epochs: Arc<BarrierEpochs>,
    workers: usize,
}

impl EncodePool {
    /// Spawn `workers` (min 1) threads emitting into `sink`, with a
    /// publisher of their own for what the size and ceiling triggers
    /// detach. The sink must be the same sink the miner was built with,
    /// so flush triggers and the rotation/shutdown drains see one buffer.
    #[must_use]
    pub fn new(sink: &SharedParquetSink, workers: usize) -> Self {
        Self::with_publisher(&Publisher::over(sink), workers)
    }

    /// [`Self::new`], handing detached partitions to `publisher` — the
    /// publish coordinator's, in the receiver, so they feed the RFC 0047
    /// §3.3 graph like every other publish.
    #[must_use]
    pub fn with_publisher(publisher: &Publisher, workers: usize) -> Self {
        // Clamp to a sane OS-thread budget: a mistyped config value must
        // not spawn thousands of threads or overflow the queue-capacity
        // multiplication below. 256 is far above any per-node core count
        // this targets while keeping capacity arithmetic trivially safe.
        let workers = workers.clamp(1, MAX_WORKERS);
        let sink = publisher.record().clone();
        let lane = Lane::new(
            workers,
            workers * QUEUE_BATCHES_PER_WORKER,
            Arc::new(Encode {
                sink: sink.clone(),
                publisher: publisher.clone(),
            }),
        );
        lane.start();
        Self {
            lane,
            epochs: sink.epochs(),
            sink,
            pending: Arc::new(Pending {
                count: Mutex::new(0),
                idle: Condvar::new(),
            }),
            metrics: Arc::new(EncodePoolMetrics::new()),
            workers,
        }
    }

    /// The cadence state this pool's batch guards report into — the
    /// sink's, so one latch covers the pool, the publish guards and the
    /// barrier (RFC 0052 §3.1).
    #[must_use]
    pub fn epochs(&self) -> Arc<BarrierEpochs> {
        Arc::clone(&self.epochs)
    }

    /// The number of OS worker threads per generation (post-clamp).
    #[must_use]
    pub fn worker_count(&self) -> usize {
        self.workers
    }

    /// Queue one batch's mined records for concurrent emit. Blocks when
    /// the queue is full — the backpressure to the caller (the ingest
    /// gate).
    pub fn submit(&self, batch: Vec<MinedRecord>) {
        if batch.is_empty() {
            return;
        }
        *self
            .pending
            .count
            .lock()
            .unwrap_or_else(PoisonError::into_inner) += 1;
        self.metrics.batch_submitted();
        // RFC 0052 §3.1: the guards and their epoch are assigned here,
        // under the caller's barrier exclusion, and travel with the
        // batch. A batch queued before cut `E`'s capture and dequeued
        // after it therefore still carries `E`, and every partition it
        // detaches is in flight from the instant it leaves the buffers.
        let completion = BatchCompletion::begin(&self.sink);
        let guard = BatchGuard {
            pending: Arc::clone(&self.pending),
            metrics: Arc::clone(&self.metrics),
            epochs: Arc::clone(&self.epochs),
            epoch: completion.epoch(),
        };
        self.lane.send(QueuedBatch {
            records: batch,
            guard,
            completion,
        });
    }

    /// Block until every submitted batch has finished its **encode
    /// phase** — each record appended to the buffers or detached into a
    /// registered publish — the drain half of the RFC 0035 §3.1
    /// encode-drain-and-flush barrier. It does not wait for the
    /// publisher's PUTs: those settle under `quiesce_publishes`, outside
    /// the exclusion a barrier holds here (RFC 0052 §3.1).
    pub fn quiesce(&self) {
        let mut count = self
            .pending
            .count
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        while *count > 0 {
            count = self
                .pending
                .idle
                .wait(count)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ourios_core::audit::ParamType;
    use ourios_core::record::{BodyKind, Param};
    use ourios_core::tenant::TenantId;
    use ourios_parquet::Store;

    use super::*;
    use crate::record_sink::{FlushConfig, ParquetRecordSink};

    fn rec(tenant: &str) -> MinedRecord {
        MinedRecord {
            tenant_id: TenantId::new(tenant),
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
            body_kind: BodyKind::String,
            params: vec![Param {
                type_tag: ParamType::Num,
                value: "1".to_string(),
            }],
            separators: vec![String::new(), String::new()],
            body: None,
            confidence: 1.0,
            lossy_flag: false,
        }
    }

    #[test]
    fn quiesce_waits_for_every_submitted_record_to_land() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let store = Store::local(dir.path()).expect("local store");
        let sink = SharedParquetSink::new(ParquetRecordSink::new(
            store,
            FlushConfig {
                target_bytes: usize::MAX,
                max_buffer_age: Duration::from_secs(86_400),
                ceiling_bytes: usize::MAX,
            },
        ));
        let pool = EncodePool::new(&sink, 4);
        for _ in 0..8 {
            pool.submit(vec![rec("tenant-a"), rec("tenant-b")]);
        }
        pool.quiesce();
        assert_eq!(
            sink.buffered_records(),
            16,
            "after quiesce every submitted record has reached the sink",
        );
    }

    #[test]
    fn size_trigger_publishes_off_lock_under_concurrency() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let store = Store::local(dir.path()).expect("local store");
        let sink = SharedParquetSink::new(ParquetRecordSink::new(
            store,
            FlushConfig {
                target_bytes: 16, // every emit crosses the target
                max_buffer_age: Duration::from_secs(86_400),
                ceiling_bytes: usize::MAX,
            },
        ));
        let pool = EncodePool::new(&sink, 4);
        for _ in 0..8 {
            pool.submit(vec![rec("tenant-a")]);
        }
        pool.quiesce();
        // RFC 0052 §3.1: the quiesce covers the encode phase only; the
        // publisher's PUTs settle under `quiesce_publishes`.
        let _outcomes = sink.quiesce_publishes();
        assert_eq!(sink.buffered_records(), 0, "everything published");
        assert_eq!(sink.flushes(), 8, "one size-triggered publish per emit");
    }

    #[test]
    fn worker_count_is_clamped_to_the_thread_budget() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let store = Store::local(dir.path()).expect("local store");
        let sink = SharedParquetSink::new(ParquetRecordSink::new(
            store,
            FlushConfig {
                target_bytes: usize::MAX,
                max_buffer_age: Duration::from_secs(86_400),
                ceiling_bytes: usize::MAX,
            },
        ));
        let pool = EncodePool::new(&sink, usize::MAX);
        assert_eq!(pool.worker_count(), MAX_WORKERS);
        let one = EncodePool::new(&sink, 0);
        assert_eq!(one.worker_count(), 1);
    }

    #[test]
    fn a_panicking_emit_still_settles_its_batch() {
        // If a worker's emit panics mid-batch, the batch must still be
        // settled (the `BatchGuard`), or `quiesce` — the rotation barrier
        // and the shutdown drain — would hang forever on one poison
        // record. No sink emit path panics today; the guard is the
        // defence if one ever does.
        let pending = Arc::new(Pending {
            count: Mutex::new(1),
            idle: Condvar::new(),
        });
        let metrics = Arc::new(EncodePoolMetrics::new());
        let epochs = Arc::new(BarrierEpochs::new());
        let worker_pending = Arc::clone(&pending);
        let worker_epochs = Arc::clone(&epochs);
        let epoch = epochs.current();
        let worker = std::thread::spawn(move || {
            let _settle = BatchGuard {
                pending: worker_pending,
                metrics,
                epochs: worker_epochs,
                epoch,
            };
            panic!("injected emit panic");
        });
        assert!(worker.join().is_err(), "the worker panicked");
        assert_eq!(
            *pending.count.lock().unwrap_or_else(PoisonError::into_inner),
            0,
            "the guard settled the batch during unwinding",
        );
        assert_eq!(
            epochs.capture().failed_epoch(),
            Some(epoch),
            "and reported its epoch before the decrement (RFC 0052 §3.1)",
        );
    }

    /// Issue #837: a worker that panics mid-batch must neither wedge
    /// `quiesce` nor take the pool down with it. Before the lane, the
    /// last worker's unwind dropped the queue's receiver: every batch
    /// queued behind the panic, and every batch submitted after it, was
    /// discarded without an emit — `quiesce` returned, but acknowledged
    /// records reached neither the buffers nor the store until a restart
    /// replayed them.
    #[test]
    fn a_panicked_worker_leaves_a_pool_that_still_encodes_every_later_batch() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        let dir = tempfile::TempDir::new().expect("temp dir");
        let store = Store::local(dir.path()).expect("local store");
        let calls = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(AtomicBool::new(false));
        let seen = Arc::clone(&calls);
        let gate = Arc::clone(&release);
        // The inline audit barrier is the seam a worker really runs
        // inside its emit: the first call holds, then panics.
        let sink = SharedParquetSink::new(
            ParquetRecordSink::new(
                store,
                FlushConfig {
                    target_bytes: 1, // every emit crosses the target
                    max_buffer_age: Duration::from_secs(86_400),
                    ceiling_bytes: usize::MAX,
                },
            )
            .with_audit_barrier(Box::new(move || {
                if seen.fetch_add(1, Ordering::AcqRel) == 0 {
                    while !gate.load(Ordering::Acquire) {
                        std::thread::yield_now();
                    }
                    panic!("injected encode-worker panic");
                }
                true
            })),
        );
        let epochs = sink.epochs();
        let failing = epochs.current();
        let pool = Arc::new(EncodePool::new(&sink, 1));

        // Given a batch queued behind one whose worker is about to panic,
        pool.submit(vec![rec("tenant-a")]);
        while calls.load(Ordering::Acquire) == 0 {
            std::thread::yield_now();
        }
        pool.submit(vec![rec("tenant-b")]);
        release.store(true, Ordering::Release);
        // and one submitted once the panic has retired the worker.
        while calls.load(Ordering::Acquire) < 2 {
            std::thread::yield_now();
        }
        pool.submit(vec![rec("tenant-c")]);

        // When the pool is quiesced, it returns.
        let (done, quiesced) = std::sync::mpsc::channel();
        let waiter = Arc::clone(&pool);
        std::thread::spawn(move || {
            waiter.quiesce();
            let _ = done.send(());
        });
        assert!(
            quiesced.recv_timeout(Duration::from_secs(30)).is_ok(),
            "quiesce returned after the worker panic",
        );
        let _outcomes = sink.quiesce_publishes();

        // Then both later batches were encoded and published, and only
        // the panicking batch's own record is outside the store — in the
        // buffers, where it was appended before the trigger ran.
        assert_eq!(sink.flushes(), 2, "every batch after the panic published");
        assert_eq!(sink.buffered_records(), 1);
        assert_eq!(
            epochs.capture().failed_epoch(),
            Some(failing),
            "and the panic latched its own batch's epoch",
        );
    }
}
