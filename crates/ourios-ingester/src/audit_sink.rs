//! Buffering audit sink for the ingest path (issue #302).
//!
//! The template miner emits a template audit event (`template_created` /
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
//! happens on [`SharedParquetAuditSink::flush`] / [`SharedParquetAuditSink::write_owned`].
//!
//! **`emit` never blocks on flush I/O (issue #302 fix #4).** A flush drains the
//! buffer under the lock, releases it, does the `AuditWriter` store I/O
//! **unlocked**, then re-locks only to settle counters and to requeue a
//! transient failure's events *ahead* of anything `emit` buffered meanwhile.
//! So a slow store flush never holds the mutex `emit` contends on.
//!
//! **Buffer bound (issue #302 fix #3).** `emit` **always buffers** — it never
//! drops. Under a healthy store the soft `ceiling_events` fires the `Notify` for
//! an eager off-runtime flush, which is the bound for the realistic case (a
//! working store keeps up with template churn). Under sustained
//! store-unavailability the buffer is *retained* and may transiently exceed the
//! ceiling — exactly the [`crate::record_sink::ParquetRecordSink`] posture (its
//! `FlushConfig::ceiling_bytes` doc: retain + transiently exceed rather than
//! stall or drop). Dropping here would be **data loss**, not graceful
//! degradation: a dropped event isn't counted by `buffered_events`, so the
//! no-loss snapshot gate (`flush_then_snapshot`, §3.4) would advance the miner
//! snapshot past that line's WAL position and recovery would never re-mine the
//! template event — the row becomes permanently unreconstructable (§3.3). So we
//! retain; the snapshot gate (which won't advance while the buffer is non-empty)
//! holds. OOM under a *total* sustained store outage is the same accepted
//! failure mode the record sink already carries.
//!
//! **Publication is audit-ordered (issue #302 fix #1/#2).** A mined record must
//! never be query-visible before its template event is durable. The miner emits
//! both within one `ingest` call, audit event before record, so: the
//! [`crate::publish::PublishCoordinator`] drains both buffers atomically under
//! the pipeline's miner lock and writes audit-before-record; and the record
//! sink's inline size/ceiling publish first calls an audit barrier
//! ([`crate::record_sink::ParquetRecordSink::with_audit_barrier`]) that flushes
//! this sink to durability (race-free — that publish runs under the miner lock).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use ourios_core::audit::{AuditEvent, AuditSink};
use ourios_parquet::{AuditWriter, AuditWriterError, PartitionKey, Store, derive_audit_partition};
use tokio::sync::Notify;

use crate::metrics::{AuditSinkMetrics, FLUSH_OUTCOME_PERMANENT, FLUSH_OUTCOME_TRANSIENT};

/// Counts accumulated by one off-lock write pass, settled back into the sink
/// under the lock afterwards.
#[derive(Debug, Default)]
struct FlushSummary {
    flushed_partitions: u64,
    flushed_events: u64,
    transient_errors: u64,
    permanent_errors: u64,
    derive_errors: u64,
}

/// Classify a failed audit partition flush (issue #302 fix #2): a store-I/O
/// error is **transient** (the object store was unreachable — retain + retry),
/// every other error is **permanent** (a bad batch, partition mismatch, encode
/// failure, or poisoned writer — content that will never succeed, so drop it
/// rather than wedge the partition behind it forever).
fn is_transient_flush_error(err: &AuditWriterError) -> bool {
    matches!(err, AuditWriterError::Io { .. })
}

/// Apply one partition write's outcome to the pass accumulators: success
/// counts, a transient error retains `batch` for retry, a permanent error drops
/// it (counted + logged once here).
fn route_partition_result(
    summary: &mut FlushSummary,
    retained: &mut Vec<AuditEvent>,
    partition: &PartitionKey,
    batch: Vec<AuditEvent>,
    result: Result<(), AuditWriterError>,
) {
    match result {
        Ok(()) => {
            summary.flushed_partitions = summary.flushed_partitions.saturating_add(1);
            summary.flushed_events = summary.flushed_events.saturating_add(batch.len() as u64);
        }
        Err(err) if is_transient_flush_error(&err) => {
            summary.transient_errors = summary.transient_errors.saturating_add(1);
            retained.extend(batch);
        }
        Err(err) => {
            summary.permanent_errors = summary.permanent_errors.saturating_add(1);
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

/// Write `events` to `store` grouped by audit partition, with **no sink lock
/// held** (issue #302 fix #4). Returns the events to retain (transient
/// failures) and the pass counts. Pure w.r.t. the sink — the caller settles.
fn write_events(store: &Store, events: Vec<AuditEvent>) -> (Vec<AuditEvent>, FlushSummary) {
    let mut summary = FlushSummary::default();
    let mut groups: HashMap<PartitionKey, Vec<AuditEvent>> = HashMap::new();
    for event in events {
        let Ok(partition) = derive_audit_partition(&event) else {
            summary.derive_errors = summary.derive_errors.saturating_add(1);
            continue;
        };
        groups.entry(partition).or_default().push(event);
    }
    let mut retained = Vec::new();
    for (partition, batch) in groups {
        let result = write_one_partition(store, &partition, &batch);
        route_partition_result(&mut summary, &mut retained, &partition, batch, result);
    }
    (retained, summary)
}

/// Encode + put one partition's batch through a single [`AuditWriter`].
fn write_one_partition(
    store: &Store,
    partition: &PartitionKey,
    batch: &[AuditEvent],
) -> Result<(), AuditWriterError> {
    let mut writer = AuditWriter::open_in(store, partition.clone())?;
    writer.append_events(batch)?;
    writer.close()?;
    Ok(())
}

/// A buffering [`AuditSink`] persisting the RFC 0005 §3.7 audit Parquet stream
/// through the [`Store`] (local or S3, RFC 0019). See the module docs.
pub struct BufferingAuditSink {
    store: Store,
    buffer: Vec<AuditEvent>,
    /// Soft cap: reaching it signals an eager off-runtime flush (it never blocks
    /// or drops — `emit` always buffers). Under sustained store-unavailability
    /// the buffer is retained and may transiently exceed this, like the record
    /// sink, rather than dropping (which would be data loss — see the module
    /// docs).
    ceiling_events: usize,
    /// Signalled by `emit` at `ceiling_events`; the receiver's age-sweep selects
    /// on it to flush eagerly — signal-to-flush, never drop.
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
    /// (local or S3). `ceiling_events` signals an eager off-runtime flush; the
    /// sink never drops — under store-unavailability the buffer is retained and
    /// may transiently exceed the ceiling (see the module docs).
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

    /// Count of successful partition flushes.
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

    /// Buffer `event` — always; never drops (issue #302: dropping is data loss,
    /// not graceful degradation — see the module docs). Updates the occupancy
    /// gauge and signals an eager off-runtime flush once the buffer reaches the
    /// soft ceiling. Never blocks or flushes inline — this runs on the request
    /// path.
    fn buffer_event(&mut self, event: AuditEvent) {
        self.buffer.push(event);
        self.metrics.set_buffered(self.buffer.len());
        if self.buffer.len() >= self.ceiling_events {
            // `notify_one` coalesces, so a burst past the ceiling wakes the
            // sweep once; it flushes off the runtime (signal-to-flush). The
            // buffer is retained (may transiently exceed) until the store
            // accepts the flush — never dropped.
            self.overflow_notify.notify_one();
        }
    }

    /// Settle one off-lock write pass back into the sink's counters + metrics.
    fn settle(&mut self, summary: &FlushSummary) {
        self.flushes = self.flushes.saturating_add(summary.flushed_partitions);
        self.events_flushed = self.events_flushed.saturating_add(summary.flushed_events);
        self.flush_errors_transient = self
            .flush_errors_transient
            .saturating_add(summary.transient_errors);
        self.flush_errors_permanent = self
            .flush_errors_permanent
            .saturating_add(summary.permanent_errors);
        self.derive_errors = self.derive_errors.saturating_add(summary.derive_errors);
        self.metrics
            .record_flush(summary.flushed_partitions, summary.flushed_events);
        for _ in 0..summary.transient_errors {
            self.metrics.record_flush_error(FLUSH_OUTCOME_TRANSIENT);
        }
        for _ in 0..summary.permanent_errors {
            self.metrics.record_flush_error(FLUSH_OUTCOME_PERMANENT);
        }
        for _ in 0..summary.derive_errors {
            self.metrics.record_derive_error();
        }
    }

    /// Requeue a transient failure's `retained` events *ahead* of whatever
    /// `emit` buffered during the unlocked I/O, then refresh the gauge. Never
    /// drops — the buffer is retained for retry (the WAL is the durability of
    /// record).
    fn requeue_ahead(&mut self, mut retained: Vec<AuditEvent>) {
        if !retained.is_empty() {
            retained.append(&mut self.buffer);
            self.buffer = retained;
        }
        self.metrics.set_buffered(self.buffer.len());
    }
}

impl AuditSink for BufferingAuditSink {
    fn emit(&mut self, event: AuditEvent) {
        self.buffer_event(event);
    }
}

/// A cloneable handle to one shared [`BufferingAuditSink`].
///
/// Mirrors [`crate::record_sink::SharedParquetSink`]: the miner `emit`s template
/// events through its `Box<dyn AuditSink>`, while the receiver drives the
/// flushes off the async runtime. `Clone` yields another handle to the *same*
/// sink.
///
/// `emit` is a short critical section. The flushes ([`Self::flush`] /
/// [`Self::write_owned`]) take the lock only to drain the buffer and to settle
/// afterwards — the `AuditWriter` store I/O runs **unlocked** in between (issue
/// #302 fix #4), so a slow store flush never blocks a concurrent `emit`.
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

    /// The eager-flush signal `emit` raises when the buffer reaches its ceiling
    /// (issue #302 fix #3). The receiver's age-sweep selects on this alongside
    /// its tick so a burst flushes promptly off the runtime.
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

    /// Atomically take the whole buffer (a cheap memory move, no I/O). The
    /// [`crate::publish::PublishCoordinator`] calls this under the pipeline's
    /// miner lock so the drain is atomic w.r.t. `miner.ingest`.
    #[must_use]
    pub fn take_buffer(&self) -> Vec<AuditEvent> {
        let mut guard = self.lock();
        let events = std::mem::take(&mut guard.buffer);
        guard.metrics.set_buffered(0);
        events
    }

    /// Write an owned `events` batch to durability, holding the lock only to
    /// read the store handle and to settle afterwards (the store I/O runs
    /// unlocked). Transient failures are requeued ahead of newly-buffered
    /// events; permanent failures drop. Returns whether every event reached
    /// durability or was permanently dropped — i.e. **nothing is retained for
    /// retry** (the audit-before-record gate the publisher needs).
    #[must_use]
    pub fn write_owned(&self, events: Vec<AuditEvent>) -> bool {
        if events.is_empty() {
            return true;
        }
        let store = self.lock().store.clone();
        let (retained, summary) = write_events(&store, events);
        let fully_durable = retained.is_empty();
        let mut guard = self.lock();
        guard.settle(&summary);
        guard.requeue_ahead(retained);
        fully_durable
    }

    /// Drain the internal buffer to durability — the size-trigger audit barrier
    /// and standalone cadence/shutdown flush. Returns whether the buffer was
    /// fully drained (nothing retained for retry).
    #[must_use]
    pub fn flush(&self) -> bool {
        let events = self.take_buffer();
        self.write_owned(events)
    }
}

impl AuditSink for SharedParquetAuditSink {
    fn emit(&mut self, event: AuditEvent) {
        self.lock().buffer_event(event);
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
    use ourios_parquet::{AuditBatchError, AuditReader, AuditWriterError, Store};

    use super::{
        BufferingAuditSink, FlushSummary, SharedParquetAuditSink, is_transient_flush_error,
        route_partition_result,
    };

    /// A generous ceiling so the eager-flush signal stays out of the tests not
    /// exercising it.
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

        assert!(handle.flush(), "a healthy store fully drains");
        assert_eq!(handle.buffered_events(), 0, "flush drained the buffer");
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

        assert!(handle.flush(), "an empty flush is vacuously fully drained");
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
        let good = created_event("alpha", 1, "user <*> logged in");
        let bad = created_event("alpha", 2, "GET <*>");
        let partition = ourios_parquet::derive_audit_partition(&good).expect("derive");

        let mut summary = FlushSummary::default();
        let mut retained = Vec::new();

        // Permanent: dropped, counted, NOT retained.
        route_partition_result(
            &mut summary,
            &mut retained,
            &partition,
            vec![bad],
            Err(AuditWriterError::Batch(AuditBatchError::PreEpochTimestamp)),
        );
        assert_eq!(summary.permanent_errors, 1);
        assert!(retained.is_empty(), "a permanent error drops the batch");

        // Transient: retained for retry, counted.
        route_partition_result(
            &mut summary,
            &mut retained,
            &partition,
            vec![good.clone()],
            Err(io_error()),
        );
        assert_eq!(summary.transient_errors, 1);
        assert_eq!(retained, vec![good], "a transient error retains the batch");
        assert_eq!(
            summary.permanent_errors, 1,
            "unchanged by the transient path"
        );
    }

    #[test]
    fn transient_store_failure_retains_for_retry() {
        // End-to-end: a sabotaged store makes `write_owned` fail transiently;
        // the events are retained (not durable, not dropped) and the flush
        // reports "not fully drained" so the publisher holds the records.
        let dir = tempfile::TempDir::new().expect("temp");
        let store_root = dir.path().join("store");
        std::fs::create_dir_all(&store_root).expect("create store root");
        let store = Store::local(&store_root).expect("store");
        let handle = SharedParquetAuditSink::new(BufferingAuditSink::new(store, TEST_CEILING));
        let mut producer = handle.clone();
        producer.emit(created_event("alpha", 1, "user <*> logged in"));

        std::fs::remove_dir_all(&store_root).expect("remove store dir");
        std::fs::write(&store_root, b"not a directory").expect("sabotage");

        assert!(
            !handle.flush(),
            "a transient store error is not fully drained"
        );
        assert_eq!(
            handle.buffered_events(),
            1,
            "the event is retained for retry"
        );
    }

    #[test]
    fn persistent_store_failure_retains_every_event_never_drops() {
        // issue #302 (round 5): under a *persistently* failing store the sink
        // RETAINS — it never drops. Dropping would lose template events the WAL
        // can no longer re-mine past the snapshot gate (§3.3). The buffer may
        // transiently exceed the ceiling, exactly like the record sink.
        let dir = tempfile::TempDir::new().expect("temp");
        let store_root = dir.path().join("store");
        std::fs::create_dir_all(&store_root).expect("create store root");
        let store = Store::local(&store_root).expect("store");
        // A tiny ceiling so repeated emit would have tripped the old hard cap.
        let handle = SharedParquetAuditSink::new(BufferingAuditSink::new(store, 2));
        let mut producer = handle.clone();

        // The store is down for the whole run.
        std::fs::remove_dir_all(&store_root).expect("remove store dir");
        std::fs::write(&store_root, b"not a directory").expect("sabotage");

        // Interleave emit + flush repeatedly; every flush fails transiently and
        // retains, and emit keeps buffering past the ceiling — nothing is lost.
        for i in 0..20 {
            producer.emit(created_event("alpha", i, "user <*> logged in"));
            assert!(!handle.flush(), "the down store never fully drains");
        }
        assert_eq!(
            handle.buffered_events(),
            20,
            "every emitted event is retained for retry — none dropped",
        );
        assert_eq!(handle.flushes(), 0, "nothing reached durability");
    }

    #[tokio::test]
    async fn emit_past_ceiling_signals_an_eager_flush() {
        // issue #302 fix #3: `emit` never flushes inline; reaching the ceiling
        // signals the receiver's age-sweep to flush off the runtime.
        let dir = tempfile::TempDir::new().expect("temp");
        let store = Store::local(dir.path()).expect("store");
        let mut handle = SharedParquetAuditSink::new(BufferingAuditSink::new(store, 2));
        let notify = handle.overflow_notify();

        handle.emit(created_event("alpha", 1, "user <*> logged in"));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), notify.notified())
                .await
                .is_err(),
            "below the ceiling, no eager-flush signal is raised",
        );

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
