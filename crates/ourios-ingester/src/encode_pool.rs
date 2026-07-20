//! The bounded worker pool for the concurrent encode phase
//! (RFC 0035 §3.1 Design A).
//!
//! The ingest pipeline's ordered phase (Drain match + template-id
//! assignment under the global gate) hands each batch's mined records
//! here; N workers run the order-insensitive
//! [`SharedParquetSink::emit_concurrent`] off the gate.
//!
//! - **Backpressure**: the queue is a fixed-bound `sync_channel` — a
//!   full queue blocks [`EncodePool::submit`], which runs under the
//!   ingest gate, so an encode-bound burst throttles admission instead
//!   of growing an unbounded in-flight backlog (§3.1 / hazard #4). Depth
//!   is exported as `ourios.ingest.encode.queue_depth`.
//! - **Quiesce**: [`EncodePool::quiesce`] blocks until every submitted
//!   batch has completed its sink emit. Whole-pool (not keyed to a WAL
//!   mark) — sound because submission happens inside the gated region,
//!   so at any rotation check every in-flight encode is for a frame at
//!   or below the mark; see the §3.1 barrier reasoning in
//!   `receiver/pipeline.rs`. Rotation is per-segment (128 MiB default),
//!   so the drain's cost amortizes to ~zero.

use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;

use ourios_core::record::MinedRecord;

use crate::metrics::EncodePoolMetrics;
use crate::record_sink::SharedParquetSink;

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
/// `emit_concurrent` panics mid-batch. Without this, a worker panic
/// would strand `quiesce` forever, which turns one poisoned record into
/// a wedged rotation barrier (and a hung shutdown).
struct BatchGuard<'a> {
    pending: &'a Pending,
    metrics: &'a EncodePoolMetrics,
}

impl Drop for BatchGuard<'_> {
    fn drop(&mut self) {
        self.pending.decrement();
        self.metrics.batch_completed();
    }
}

/// A bounded pool of OS threads draining mined-record batches into
/// [`SharedParquetSink::emit_concurrent`]. Dedicated threads rather than
/// the tokio blocking pool: the encode is CPU-bound and its concurrency
/// must stay fixed at the configured worker count, not compete with the
/// runtime's elastic blocking pool. Dropping the pool closes the queue
/// and joins the workers.
pub struct EncodePool {
    /// `Some` until drop; taking it closes the channel so workers exit.
    tx: Option<SyncSender<Vec<MinedRecord>>>,
    pending: Arc<Pending>,
    metrics: Arc<EncodePoolMetrics>,
    workers: Vec<JoinHandle<()>>,
}

impl EncodePool {
    /// Spawn `workers` (min 1) threads emitting into `sink`. The sink
    /// must be the same sink the miner was built with, so flush triggers
    /// and the rotation/shutdown drains see one buffer.
    #[must_use]
    pub fn new(sink: &SharedParquetSink, workers: usize) -> Self {
        let workers = workers.max(1);
        let (tx, rx) = sync_channel::<Vec<MinedRecord>>(workers * QUEUE_BATCHES_PER_WORKER);
        // `mpsc::Receiver` is single-consumer; the mutex turns it into a
        // shared work queue — pickup serializes, the emit work does not.
        let rx = Arc::new(Mutex::new(rx));
        let pending = Arc::new(Pending {
            count: Mutex::new(0),
            idle: Condvar::new(),
        });
        let metrics = Arc::new(EncodePoolMetrics::new());
        let handles = (0..workers)
            .map(|_| {
                let rx = Arc::clone(&rx);
                let pending = Arc::clone(&pending);
                let metrics = Arc::clone(&metrics);
                let sink = sink.clone();
                std::thread::spawn(move || {
                    loop {
                        let batch = {
                            let rx = rx.lock().unwrap_or_else(PoisonError::into_inner);
                            rx.recv()
                        };
                        let Ok(batch) = batch else {
                            return; // channel closed: pool dropped
                        };
                        let _settle = BatchGuard {
                            pending: &pending,
                            metrics: &metrics,
                        };
                        for record in batch {
                            sink.emit_concurrent(record);
                        }
                    }
                })
            })
            .collect();
        Self {
            tx: Some(tx),
            pending,
            metrics,
            workers: handles,
        }
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
        let sent = self.tx.as_ref().is_some_and(|tx| tx.send(batch).is_ok());
        if !sent {
            // Workers already gone (drop in progress): undo the count so
            // a concurrent quiesce cannot wait forever on a batch nobody
            // will run.
            self.pending.decrement();
            self.metrics.batch_completed();
        }
    }

    /// Block until the queue is empty and every worker has finished its
    /// batch — the drain half of the RFC 0035 §3.1 encode-drain-and-flush
    /// barrier. The caller owns the flush half (and the reasoning for why
    /// a whole-pool drain covers the WAL mark lives at the call sites in
    /// `receiver/pipeline.rs`).
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

impl Drop for EncodePool {
    fn drop(&mut self) {
        // Closing the channel ends each worker's `recv` loop; joining
        // guarantees no worker still holds a sink handle after drop.
        drop(self.tx.take());
        for handle in self.workers.drain(..) {
            drop(handle.join());
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
        assert_eq!(sink.buffered_records(), 0, "everything published");
        assert_eq!(sink.flushes(), 8, "one size-triggered publish per emit");
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
        let worker_pending = Arc::clone(&pending);
        let worker = std::thread::spawn(move || {
            let _settle = BatchGuard {
                pending: &worker_pending,
                metrics: &metrics,
            };
            panic!("injected emit panic");
        });
        assert!(worker.join().is_err(), "the worker panicked");
        assert_eq!(
            *pending.count.lock().unwrap_or_else(PoisonError::into_inner),
            0,
            "the guard settled the batch during unwinding",
        );
    }
}
