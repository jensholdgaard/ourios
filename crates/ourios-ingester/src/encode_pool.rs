//! PROTOTYPE (RFC 0035 red): the bounded worker pool for the
//! concurrent encode phase (§3.1 Design A).
//!
//! The ingest pipeline's ordered phase (Drain match + template-id
//! assignment under the global gate) hands each batch's mined records
//! here; N workers run the order-insensitive
//! [`SharedParquetSink::emit_concurrent`] off the gate. This module is
//! measurement scaffolding for the RFC's `red`-stage serial-fraction
//! number, not the production design:
//!
//! - Backpressure is a fixed-bound `sync_channel` — a full queue blocks
//!   `submit` (which runs under the ingest gate), the bluntest form of
//!   the §3.1 backpressure requirement.
//! - `quiesce` is a crude *global* drain (wait until the queue is empty
//!   and every worker is idle); production needs the RFC 0035 §3.1
//!   encode-drain-and-flush barrier keyed to the rotation mark.

use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;

use ourios_core::record::MinedRecord;

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

/// PROTOTYPE (RFC 0035 red): a bounded pool of OS threads draining
/// mined-record batches into [`SharedParquetSink::emit_concurrent`].
/// Dropping the pool closes the queue and joins the workers.
pub struct EncodePool {
    /// `Some` until drop; taking it closes the channel so workers exit.
    tx: Option<SyncSender<Vec<MinedRecord>>>,
    pending: Arc<Pending>,
    workers: Vec<JoinHandle<()>>,
}

impl EncodePool {
    /// Spawn `workers` (min 1) threads emitting into `sink`.
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
        let handles = (0..workers)
            .map(|_| {
                let rx = Arc::clone(&rx);
                let pending = Arc::clone(&pending);
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
                        for record in batch {
                            sink.emit_concurrent(record);
                        }
                        let mut count =
                            pending.count.lock().unwrap_or_else(PoisonError::into_inner);
                        *count = count.saturating_sub(1);
                        if *count == 0 {
                            pending.idle.notify_all();
                        }
                    }
                })
            })
            .collect();
        Self {
            tx: Some(tx),
            pending,
            workers: handles,
        }
    }

    /// Queue one batch's mined records for concurrent emit. Blocks when
    /// the queue is full (backpressure to the caller — the ingest gate).
    pub fn submit(&self, batch: Vec<MinedRecord>) {
        if batch.is_empty() {
            return;
        }
        *self
            .pending
            .count
            .lock()
            .unwrap_or_else(PoisonError::into_inner) += 1;
        let sent = self.tx.as_ref().is_some_and(|tx| tx.send(batch).is_ok());
        if !sent {
            // Workers already gone (drop in progress): undo the count so
            // a concurrent quiesce cannot wait forever on a batch nobody
            // will run.
            let mut count = self
                .pending
                .count
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.pending.idle.notify_all();
            }
        }
    }

    /// PROTOTYPE: crude global quiesce — block until the queue is empty
    /// and every worker is idle; production needs the RFC 0035 §3.1
    /// drain-and-flush barrier keyed to the rotation mark.
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
}
