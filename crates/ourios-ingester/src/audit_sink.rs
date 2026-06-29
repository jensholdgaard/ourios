//! Buffering audit sink for the ingest path (issue #302).
//!
//! The Drain miner emits a template audit event (`template_created` /
//! `template_widened` / `template_type_expanded`, RFC 0001 §6.4 / RFC 0017
//! §3.1) synchronously from `MinerCluster::ingest`, which runs on the
//! receiver's async request path. The querier's read-time template registry
//! (RFC 0017 `derive_template_registry`) folds those events to reconstruct a
//! clean row's body bit-for-bit (`CLAUDE.md` §3.3); without them a clean,
//! high-confidence row — whose `body` column is intentionally empty — renders
//! to nothing.
//!
//! [`ourios_parquet::ParquetAuditSink`] persists one Parquet file per event
//! with a blocking store write inside `emit`, so wiring it into the receiver
//! would both stall the request path on store latency and spray one tiny
//! object per template event (hazard `CLAUDE.md` §4 #4, the small-file
//! problem). This sink instead mirrors the RFC 0014 record sink
//! ([`crate::record_sink::ParquetRecordSink`]): [`AuditSink::emit`] only
//! buffers (a cheap, request-path-safe push), and the blocking store I/O
//! happens on [`SharedParquetAuditSink::flush`], which the receiver drives off
//! the async runtime at the same cadence + rotation + shutdown points it
//! flushes the record sink.
//!
//! **Flush ordering (no later than the records).** A row stamped
//! `template_version = N` needs its template event durable for the registry to
//! render it, so the receiver flushes this sink *before* the record sink at
//! each cadence point and, if this sink does not fully drain, skips the record
//! flush that cycle (and the miner snapshot): a non-empty buffer after a flush
//! means a *transient* store error (permanent errors drop, below), so the
//! record flush to the same store would fail anyway, and skipping it avoids
//! exposing a clean row before its template event is durable. A transient flush
//! failure retains the events (the WAL is the durability of record, so recovery
//! re-mines and re-emits them). The inline size trigger on the record sink
//! (RFC 0014 §3.1, fired from `emit`) is *not* paired with an audit flush — a
//! clean row flushed by that trigger may render empty in the narrow window
//! before the next audit flush; it reconstructs once the audit cadence catches
//! up, and a crash in that window re-mines from the WAL, so durability is
//! unaffected.
//!
//! **Bounded buffer.** `emit` enforces a soft event ceiling without blocking or
//! flushing inline: on reaching it, it signals a [`tokio::sync::Notify`] the
//! receiver's age-sweep selects on, so an adversarial burst of template churn
//! flushes promptly off the runtime rather than growing the buffer until OOM
//! (the miner has no template-count cap). This is signal-to-flush, never drop.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use ourios_core::audit::{AuditEvent, AuditSink};
use ourios_parquet::{AuditWriter, AuditWriterError, PartitionKey, Store, derive_audit_partition};
use tokio::sync::Notify;

use crate::metrics::{AuditSinkMetrics, FLUSH_OUTCOME_PERMANENT, FLUSH_OUTCOME_TRANSIENT};

/// A buffering [`AuditSink`] persisting the RFC 0005 §3.7 audit Parquet stream
/// through the [`Store`] (local or S3, RFC 0019).
///
/// [`AuditSink::emit`] buffers in memory; [`Self::flush`] drains the buffer,
/// groups the events by audit partition (tenant + UTC day, RFC 0005 §3.4), and
/// writes each partition's batch with a single [`AuditWriter`] — so one flush
/// produces at most one file per partition, not one per event.
///
/// A failed partition flush is classified: a transient store-I/O error retains
/// the batch for the next flush (the WAL is the durability of record), but a
/// permanent content/encode error drops it — retrying a malformed batch loops
/// forever and would block every newer good event for that partition behind it
/// (issue #302).
pub struct BufferingAuditSink {
    store: Store,
    buffer: Vec<AuditEvent>,
    /// Soft cap on buffered events. `emit` never blocks or flushes inline; on
    /// reaching the cap it signals [`Self::overflow_notify`] so the receiver's
    /// age-sweep flushes promptly off the runtime (issue #302 — the miner has
    /// no template-count cap, so adversarial template churn could otherwise
    /// grow this `Vec` unbounded between the cadence flushes).
    ceiling_events: usize,
    /// Signalled by `emit` when the buffer reaches `ceiling_events`; the
    /// receiver's age-sweep selects on it to flush eagerly — signal-to-flush,
    /// never drop, so no data is lost.
    overflow_notify: Arc<Notify>,
    flushes: u64,
    events_flushed: u64,
    flush_errors_transient: u64,
    flush_errors_permanent: u64,
    derive_errors: u64,
    /// RFC 0001 §6.8 / `CLAUDE.md` §6.3 instruments; no-op without a provider.
    metrics: AuditSinkMetrics,
}

impl BufferingAuditSink {
    /// A sink buffering events and flushing the audit stream through `store`
    /// (local or S3). `ceiling_events` bounds the in-memory buffer: reaching it
    /// signals an eager off-runtime flush (it never drops or blocks `emit`).
    #[must_use]
    pub fn new(store: Store, ceiling_events: usize) -> Self {
        Self {
            store,
            buffer: Vec::new(),
            ceiling_events,
            overflow_notify: Arc::new(Notify::new()),
            flushes: 0,
            events_flushed: 0,
            flush_errors_transient: 0,
            flush_errors_permanent: 0,
            derive_errors: 0,
            metrics: AuditSinkMetrics::new(),
        }
    }

    /// Events currently buffered (not yet flushed).
    #[must_use]
    pub fn buffered_events(&self) -> usize {
        self.buffer.len()
    }

    /// Count of successful partition flushes (one per partition per [`Self::flush`]).
    #[must_use]
    pub fn flushes(&self) -> u64 {
        self.flushes
    }

    /// Total events written out across all successful flushes.
    #[must_use]
    pub fn events_flushed(&self) -> u64 {
        self.events_flushed
    }

    /// Transient (store-I/O) flush failures — the batch was retained + retried.
    #[must_use]
    pub fn flush_errors_transient(&self) -> u64 {
        self.flush_errors_transient
    }

    /// Permanent (content/encode) flush failures — the batch was dropped.
    #[must_use]
    pub fn flush_errors_permanent(&self) -> u64 {
        self.flush_errors_permanent
    }

    /// Events dropped because their partition key could not be derived.
    #[must_use]
    pub fn derive_errors(&self) -> u64 {
        self.derive_errors
    }

    /// Buffer the `event`, updating the occupancy gauge and signalling an eager
    /// flush once the buffer reaches the ceiling. Never blocks or flushes inline
    /// — this runs on the request path.
    fn buffer_event(&mut self, event: AuditEvent) {
        self.buffer.push(event);
        self.metrics.set_buffered(self.buffer.len());
        if self.buffer.len() >= self.ceiling_events {
            // `notify_one` coalesces, so a burst past the ceiling wakes the
            // sweep once; it flushes off the runtime (signal-to-flush).
            self.overflow_notify.notify_one();
        }
    }

    /// Drain the buffer and persist it, one [`AuditWriter`] per audit partition.
    ///
    /// A partition whose write fails with a transient store-I/O error has its
    /// events retained for the next flush (the WAL is the durability of record);
    /// a permanent content/encode error drops the batch (retrying loops forever
    /// and blocks newer good events behind it). An event whose timestamp can't
    /// derive a partition (pre-epoch / overflow) is counted and dropped, since
    /// retrying it would loop forever. An empty buffer is a no-op — no writer is
    /// opened and no file is published.
    pub fn flush(&mut self) {
        if self.buffer.is_empty() {
            return;
        }
        let mut groups: HashMap<PartitionKey, Vec<AuditEvent>> = HashMap::new();
        for event in std::mem::take(&mut self.buffer) {
            let Ok(partition) = derive_audit_partition(&event) else {
                self.derive_errors = self.derive_errors.saturating_add(1);
                self.metrics.record_derive_error();
                continue;
            };
            groups.entry(partition).or_default().push(event);
        }
        for (partition, batch) in groups {
            let result = self.write_partition(&partition, &batch);
            self.handle_flush_result(&partition, batch, result);
        }
        self.metrics.set_buffered(self.buffer.len());
    }

    /// Write one partition's batch through a single [`AuditWriter`]. On success
    /// the counters advance; on error the caller classifies + handles it.
    fn write_partition(
        &mut self,
        partition: &PartitionKey,
        batch: &[AuditEvent],
    ) -> Result<(), AuditWriterError> {
        let mut writer = AuditWriter::open_in(&self.store, partition.clone())?;
        writer.append_events(batch)?;
        writer.close()?;
        self.flushes = self.flushes.saturating_add(1);
        self.events_flushed = self.events_flushed.saturating_add(batch.len() as u64);
        self.metrics.record_flush(batch.len());
        Ok(())
    }

    /// Apply a partition write's outcome (issue #302 fix #2). A transient
    /// store-I/O error retains `batch` for the next flush (the WAL is the
    /// durability of record); a permanent content/encode error drops it —
    /// retrying a malformed batch loops forever and wedges every newer good
    /// event for that partition behind it.
    fn handle_flush_result(
        &mut self,
        partition: &PartitionKey,
        batch: Vec<AuditEvent>,
        result: Result<(), AuditWriterError>,
    ) {
        let Err(err) = result else {
            return;
        };
        if is_transient_flush_error(&err) {
            self.flush_errors_transient = self.flush_errors_transient.saturating_add(1);
            self.metrics.record_flush_error(FLUSH_OUTCOME_TRANSIENT);
            self.buffer.extend(batch);
        } else {
            self.flush_errors_permanent = self.flush_errors_permanent.saturating_add(1);
            self.metrics.record_flush_error(FLUSH_OUTCOME_PERMANENT);
            eprintln!(
                "audit sink: dropping {} event(s) for tenant {:?} {:04}-{:02}-{:02} — \
                 permanent write error (not persisted): {err}",
                batch.len(),
                partition.tenant_id,
                partition.year,
                partition.month,
                partition.day,
            );
        }
    }
}

/// Classify a failed audit partition flush (issue #302 fix #2): a store-I/O
/// error is **transient** (the object store was unreachable — retain + retry),
/// every other error is **permanent** (a bad batch, partition mismatch, encode
/// failure, or poisoned writer — content that will never succeed, so drop it
/// rather than wedge the partition).
fn is_transient_flush_error(err: &AuditWriterError) -> bool {
    matches!(err, AuditWriterError::Io { .. })
}

/// A cloneable handle to one shared [`BufferingAuditSink`].
///
/// Mirrors [`crate::record_sink::SharedParquetSink`]: the ingest path has two
/// holders of the same sink — the miner `emit`s template events through its
/// `Box<dyn AuditSink>`, while the receiver drives [`Self::flush`] off the async
/// runtime at the cadence / rotation / shutdown points. `Clone` yields another
/// handle to the *same* sink.
///
/// All access serializes on one mutex. `emit` is a short critical section;
/// `flush` is **not** — it holds the lock across `AuditWriter` encode +
/// `Store` put (an S3 PUT on the s3 backend), so the receiver runs it via
/// `block_in_place` / `spawn_blocking`, the same posture the record sink takes.
#[derive(Clone)]
pub struct SharedParquetAuditSink {
    inner: Arc<Mutex<BufferingAuditSink>>,
}

impl SharedParquetAuditSink {
    /// Wrap `sink` in a shared, cloneable handle.
    #[must_use]
    pub fn new(sink: BufferingAuditSink) -> Self {
        Self {
            inner: Arc::new(Mutex::new(sink)),
        }
    }

    /// Lock the sink, recovering a poisoned mutex. A poison means a past panic
    /// while a flush was in flight; the buffer + counters remain structurally
    /// consistent (the WAL is the durability of record), so recovering the
    /// inner sink is safer than panicking the ingest path (the same posture
    /// [`crate::record_sink::SharedParquetSink`] takes).
    fn lock(&self) -> std::sync::MutexGuard<'_, BufferingAuditSink> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Drain the buffer to the audit stream — the cadence / rotation / shutdown
    /// trigger, run off the async runtime by the receiver.
    pub fn flush(&self) {
        self.lock().flush();
    }

    /// The eager-flush signal `emit` raises when the buffer reaches its ceiling
    /// (issue #302). The receiver's age-sweep selects on this alongside its tick
    /// so a burst flushes promptly off the runtime, bounding the buffer without
    /// blocking `emit` or dropping events.
    #[must_use]
    pub fn overflow_notify(&self) -> Arc<Notify> {
        Arc::clone(&self.lock().overflow_notify)
    }

    /// Events currently buffered (not yet flushed) — observability + tests.
    #[must_use]
    pub fn buffered_events(&self) -> usize {
        self.lock().buffered_events()
    }

    /// Successful partition flushes so far — observability + tests.
    #[must_use]
    pub fn flushes(&self) -> u64 {
        self.lock().flushes()
    }
}

impl AuditSink for SharedParquetAuditSink {
    fn emit(&mut self, event: AuditEvent) {
        self.lock().buffer_event(event);
    }
}

impl AuditSink for BufferingAuditSink {
    fn emit(&mut self, event: AuditEvent) {
        self.buffer_event(event);
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::{Duration, UNIX_EPOCH};

    use ourios_core::audit::{
        AuditEvent, AuditPayload, AuditSink, TemplateChange, hash_triggering_line,
    };
    use ourios_core::tenant::TenantId;
    use ourios_parquet::{
        AuditBatchError, AuditReader, AuditWriterError, Store, derive_audit_partition,
    };

    use super::{BufferingAuditSink, SharedParquetAuditSink, is_transient_flush_error};

    /// A generous ceiling so the bounding signal doesn't fire in the tests that
    /// aren't exercising it.
    const TEST_CEILING: usize = 1024;

    fn created_event(tenant: &str, template_id: u64, template: &str) -> AuditEvent {
        AuditEvent {
            tenant_id: TenantId::new(tenant),
            // 2026-04-02T10:58:00Z — a fixed, in-window day.
            timestamp: UNIX_EPOCH + Duration::from_secs(1_775_127_480),
            payload: AuditPayload::Template {
                template_id,
                triggering_line_hash: hash_triggering_line(template.as_bytes()),
                triggering_line_sample: Some(template.to_owned()),
                change: TemplateChange::Created {
                    new_template: template.to_owned(),
                },
            },
        }
    }

    /// Every `*.parquet` audit file under `root`, recursively.
    fn parquet_files(root: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|x| x == "parquet") {
                    out.push(path);
                }
            }
        }
        out
    }

    /// Every audit event published under `root`, read back through the reader,
    /// paired with the file it came from.
    fn read_back(root: &Path) -> Vec<(PathBuf, AuditEvent)> {
        let mut out = Vec::new();
        for file in parquet_files(root) {
            let events = AuditReader::open_file(&file)
                .expect("open_file")
                .read_all()
                .expect("read_all");
            for event in events {
                out.push((file.clone(), event));
            }
        }
        out
    }

    #[test]
    fn flush_batches_per_partition_and_round_trips() {
        let dir = tempfile::TempDir::new().expect("temp");
        let store = Store::local(dir.path()).expect("store");
        let handle = SharedParquetAuditSink::new(BufferingAuditSink::new(store, TEST_CEILING));

        // Three events across two tenants → two audit partitions.
        let events = [
            created_event("alpha", 1, "user <*> logged in"),
            created_event("alpha", 2, "GET <*>"),
            created_event("bravo", 1, "order <*> shipped"),
        ];
        let mut producer = handle.clone();
        for event in &events {
            producer.emit(event.clone());
        }
        assert_eq!(handle.buffered_events(), 3, "clones share one buffer");
        assert_eq!(handle.flushes(), 0, "no trigger fired yet");

        handle.flush();
        assert_eq!(handle.buffered_events(), 0, "flush drained the buffer");
        // One file per partition, not one per event (the small-file guard).
        assert_eq!(handle.flushes(), 2, "two partitions → two files");

        let read = read_back(dir.path());
        let distinct_files: std::collections::HashSet<&PathBuf> =
            read.iter().map(|(k, _)| k).collect();
        assert_eq!(distinct_files.len(), 2, "exactly two audit files written");
        let mut got: Vec<AuditEvent> = read.into_iter().map(|(_, e)| e).collect();
        let mut want: Vec<AuditEvent> = events.to_vec();
        let sort = |v: &mut Vec<AuditEvent>| {
            v.sort_by_key(|e| match &e.payload {
                AuditPayload::Template { template_id, .. } => {
                    (e.tenant_id.as_str().to_owned(), *template_id)
                }
                _ => (e.tenant_id.as_str().to_owned(), 0),
            });
        };
        sort(&mut got);
        sort(&mut want);
        assert_eq!(
            got, want,
            "every emitted event round-trips out of the stream"
        );
    }

    #[test]
    fn empty_buffer_flush_is_a_no_op() {
        let dir = tempfile::TempDir::new().expect("temp");
        let store = Store::local(dir.path()).expect("store");
        let handle =
            SharedParquetAuditSink::new(BufferingAuditSink::new(store.clone(), TEST_CEILING));

        handle.flush();

        assert_eq!(handle.flushes(), 0, "no flush happened");
        assert!(
            store.list_blocking(None).expect("list").is_empty(),
            "no object is published on an empty-buffer flush",
        );
    }

    fn io_error() -> AuditWriterError {
        AuditWriterError::Io {
            op: "put",
            path: PathBuf::from("audit/x.parquet"),
            source_path: None,
            source: std::io::Error::other("store unreachable"),
        }
    }

    #[test]
    fn flush_error_classification() {
        // Only a store-I/O error is transient (retry); every content/encode
        // variant is permanent (drop).
        assert!(is_transient_flush_error(&io_error()), "Io is transient");
        assert!(
            !is_transient_flush_error(&AuditWriterError::Batch(AuditBatchError::PreEpochTimestamp)),
            "Batch is permanent",
        );
        assert!(
            !is_transient_flush_error(&AuditWriterError::Poisoned),
            "Poisoned is permanent",
        );
    }

    #[test]
    fn poison_pill_permanent_drops_transient_retains() {
        // issue #302 fix #2: a permanent (Batch) error must DROP the batch (not
        // requeue it — that loops forever and wedges newer good events); a
        // transient (Io) error must RETAIN it for retry.
        let dir = tempfile::TempDir::new().expect("temp");
        let store = Store::local(dir.path()).expect("store");
        let mut sink = BufferingAuditSink::new(store, TEST_CEILING);

        let good = created_event("alpha", 1, "user <*> logged in");
        let partition = derive_audit_partition(&good).expect("derive");

        // Permanent: dropped, counted, NOT requeued.
        sink.handle_flush_result(
            &partition,
            vec![created_event("alpha", 2, "GET <*>")],
            Err(AuditWriterError::Batch(AuditBatchError::PreEpochTimestamp)),
        );
        assert_eq!(
            sink.buffered_events(),
            0,
            "a permanent error drops the batch"
        );
        assert_eq!(sink.flush_errors_permanent(), 1);
        assert_eq!(sink.flush_errors_transient(), 0);

        // Transient: retained for retry, counted.
        sink.handle_flush_result(&partition, vec![good], Err(io_error()));
        assert_eq!(
            sink.buffered_events(),
            1,
            "a transient error retains the batch"
        );
        assert_eq!(sink.flush_errors_transient(), 1);
        assert_eq!(
            sink.flush_errors_permanent(),
            1,
            "unchanged by the transient path"
        );
    }

    #[tokio::test]
    async fn emit_past_ceiling_signals_an_eager_flush() {
        // issue #302 fix #4: `emit` never flushes inline; reaching the ceiling
        // signals the receiver's age-sweep to flush off the runtime.
        let dir = tempfile::TempDir::new().expect("temp");
        let store = Store::local(dir.path()).expect("store");
        let mut handle = SharedParquetAuditSink::new(BufferingAuditSink::new(store, 2));
        let notify = handle.overflow_notify();

        // One event (below the ceiling of 2): no signal — the future stays pending.
        handle.emit(created_event("alpha", 1, "user <*> logged in"));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), notify.notified())
                .await
                .is_err(),
            "below the ceiling, no eager-flush signal is raised",
        );

        // Reaching the ceiling raises the signal.
        handle.emit(created_event("alpha", 2, "GET <*>"));
        assert!(
            tokio::time::timeout(Duration::from_millis(500), notify.notified())
                .await
                .is_ok(),
            "reaching the ceiling signals an eager flush",
        );
        assert_eq!(
            handle.buffered_events(),
            2,
            "emit buffers, never flushes or drops inline",
        );
    }
}
