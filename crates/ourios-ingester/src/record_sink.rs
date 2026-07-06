//! Production data write path (RFC 0014): a buffering [`RecordSink`] that
//! accumulates mined records per partition and flushes each to a Parquet
//! object on the RFC 0013 [`Store`] seam.
//!
//! Flush policy (RFC 0014 §3.2, hybrid): a partition flushes when its buffered
//! bytes reach [`FlushConfig::target_bytes`] (size, evaluated on `emit`), when
//! its oldest record reaches [`FlushConfig::max_buffer_age`] (age, evaluated by
//! [`ParquetRecordSink::flush_aged`] on the batch-window tick), or when the WAL
//! segment rotates ([`ParquetRecordSink::flush_all`], force-flushing *every*
//! partition). Buffered bytes are kept under [`FlushConfig::ceiling_bytes`]
//! by flushing the largest partition inline before `emit` would exceed it
//! (RFC 0014 §3.4) — a hard ceiling whenever the store accepts writes; a flush
//! failure retains the buffer and may transiently exceed it rather than
//! stalling ingest (see [`FlushConfig::ceiling_bytes`]).
//!
//! Records reach the sink only after the WAL is durable (`CLAUDE.md` §3.4), so
//! a buffer is a bounded accelerator, never the durability of record: a crash
//! re-mines the un-flushed tail from the WAL. A flush failure therefore retains
//! the buffer (counted, retried on the next trigger) rather than dropping data.
//! Buffers are keyed by [`PartitionKey`], which carries `tenant_id`, so they
//! are tenant-scoped by construction (`CLAUDE.md` §3.7).

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use ourios_core::record::{MinedRecord, RecordSink};
use ourios_parquet::{
    DEFAULT_ZSTD_LEVEL, PartitionKey, PromotedAttributes, Store, StoreError, WriterError,
    encode_records_to_parquet_with_promoted,
};
use uuid::Uuid;

use crate::metrics::SinkMetrics;

/// Flush-policy knobs (RFC 0014 §3; RFC 0004 config at the call site).
#[derive(Debug, Clone)]
pub struct FlushConfig {
    /// Size trigger: a partition flushes once its estimated buffered bytes
    /// reach this. Production targets RFC 0005 §3.5's 256 MiB–2 GiB file band;
    /// tests use small values. (Tuning is RFC 0014 §7.)
    pub target_bytes: usize,
    /// Age trigger: a partition flushes once its oldest buffered record's age
    /// reaches this (inclusive), bounding low-volume staleness.
    pub max_buffer_age: Duration,
    /// Ceiling on total buffered bytes across all partitions; `emit` flushes
    /// inline to stay at or under it whenever the store accepts writes. If a
    /// flush fails, or a single record alone exceeds the ceiling (nothing left
    /// to flush), the buffer is retained (the WAL is the durability of record)
    /// and the ceiling may be transiently exceeded — rather than stalling
    /// ingest. (A failed flush attempt is also counted as a flush error.)
    pub ceiling_bytes: usize,
}

/// A failed flush of one partition. Non-fatal — the buffer is retained and the
/// WAL remains the durability of record. Internal: the public `emit` / tick /
/// rotation surface is infallible (errors are swallowed + counted).
#[derive(Debug)]
enum FlushError {
    /// Encoding the buffered records to Parquet failed.
    Encode(WriterError),
    /// Writing the encoded object to the store failed.
    Store(StoreError),
}

impl std::fmt::Display for FlushError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Encode(e) => write!(f, "encode buffered records: {e}"),
            Self::Store(e) => write!(f, "put Parquet object: {e}"),
        }
    }
}

impl std::error::Error for FlushError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Encode(e) => Some(e),
            Self::Store(e) => Some(e),
        }
    }
}

struct PartitionBuffer {
    records: Vec<MinedRecord>,
    est_bytes: usize,
    oldest: Instant,
}

/// The buffering Parquet record sink — the production replacement for
/// `NoOpRecordSink` (RFC 0014). See the module docs for the flush policy.
pub struct ParquetRecordSink {
    store: Store,
    config: FlushConfig,
    buffers: HashMap<PartitionKey, PartitionBuffer>,
    total_bytes: usize,
    flushes: u64,
    records_flushed: u64,
    flush_errors: u64,
    derive_errors: u64,
    /// Audit-durability barrier (issue #302 fix #2): run before any *inline*
    /// publish (size trigger / ceiling) to flush the audit sink to durability
    /// first, returning whether it fully drained. A record must not be published
    /// before its template event is durable (`CLAUDE.md` §3.3); the inline
    /// publish runs under the miner lock, so the barrier flush + the publish are
    /// atomic w.r.t. ingest. `None` (the RFC 0014 default + the record-sink unit
    /// tests) leaves the inline publish unconstrained.
    audit_barrier: Option<Box<dyn FnMut() -> bool + Send>>,
    /// RFC 0025 §3.3 quarantine destination: permanently-rejected
    /// records emit a `record_quarantined` audit event here before
    /// being dropped from the buffer (the WAL still holds them).
    /// `None` (tests / minimal wiring) quarantines silently to the
    /// counter only.
    audit: Option<Box<dyn ourios_core::audit::AuditSink + Send>>,
    /// RFC 0014 §6.3 instruments (flush throughput/latency by trigger,
    /// errors, buffer occupancy). No-op when no meter provider is installed.
    metrics: SinkMetrics,
    /// The RFC 0022 promoted attribute set every flushed file projects
    /// (`storage.promoted_attributes`, §3.2). Defaults to the implicit
    /// `service.name`-only set; set via [`Self::with_promoted_attributes`].
    promoted: PromotedAttributes,
}

/// Cheap per-record footprint estimate driving the size trigger + ceiling — a
/// rough heuristic over the larger variable-length fields, not the exact
/// encoded (compressed) size and not every field. Good enough to bound memory
/// and roughly right-size files; precise estimation is RFC 0014 §7.
fn estimate_bytes(r: &MinedRecord) -> usize {
    let opt = |o: &Option<String>| o.as_ref().map_or(0, String::len);
    // Fixed per-record overhead plus the variable-length payloads.
    96 + opt(&r.body)
        + opt(&r.severity_text)
        + opt(&r.scope_name)
        + opt(&r.scope_version)
        + r.params.iter().map(|p| p.value.len() + 8).sum::<usize>()
        + r.separators.iter().map(String::len).sum::<usize>()
        // Attributes are encoded as JSON; a flat per-entry estimate suffices.
        + (r.attributes.len() + r.resource_attributes.len()) * 48
}

/// `/`-delimited object key for a partition's flushed file: the RFC 0005 §3.4
/// Hive path (relative to the store root) plus a `UUIDv7` name. Mirrors
/// `ourios_parquet::Writer`'s key; object keys are `/`-delimited on every host.
/// Whether `e` condemns a specific record (RFC 0025 §3.3 quarantine
/// territory) rather than signalling an internal invariant violation.
/// `Arrow` means *our* batch-building broke — quarantining on it
/// would drop data on our own bug; those retain the buffer for retry
/// and investigation like any other non-record failure.
fn is_per_record_rejection(e: &ourios_parquet::BatchError) -> bool {
    !matches!(e, ourios_parquet::BatchError::Arrow(_))
}

/// Bisect `records` into `(encodable, permanently rejected)` using
/// the batch conversion as the probe — `BatchError` is deterministic,
/// so a failing subset always shrinks to its poison records.
/// O(k·log n) probes for k poison records.
fn split_poisoned(
    records: Vec<ourios_core::record::MinedRecord>,
    promoted: &ourios_parquet::PromotedAttributes,
) -> (
    Vec<ourios_core::record::MinedRecord>,
    Vec<(ourios_core::record::MinedRecord, ourios_parquet::BatchError)>,
) {
    match ourios_parquet::mined_records_to_batch_with_promoted(&records, promoted) {
        Ok(_) => (records, Vec::new()),
        Err(error) => {
            if records.len() == 1 {
                if !is_per_record_rejection(&error) {
                    // Internal invariant violation, not this record's
                    // fault — keep it; the flush will fail and retain.
                    return (records, Vec::new());
                }
                let record = records.into_iter().next().expect("len checked");
                return (Vec::new(), vec![(record, error)]);
            }
            let mid = records.len() / 2;
            let mut left = records;
            let right = left.split_off(mid);
            let (mut kept, mut poisoned) = split_poisoned(left, promoted);
            let (kept_r, poisoned_r) = split_poisoned(right, promoted);
            kept.extend(kept_r);
            poisoned.extend(poisoned_r);
            (kept, poisoned)
        }
    }
}

fn object_key(partition: &PartitionKey) -> String {
    let rel = partition.data_path(Path::new(""));
    format!(
        "{}/{}.parquet",
        rel.to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/"),
        Uuid::now_v7()
    )
}

impl ParquetRecordSink {
    /// Create a sink flushing to `store` under `config`.
    #[must_use]
    pub fn new(store: Store, config: FlushConfig) -> Self {
        Self {
            store,
            config,
            buffers: HashMap::new(),
            total_bytes: 0,
            flushes: 0,
            records_flushed: 0,
            flush_errors: 0,
            derive_errors: 0,
            audit_barrier: None,
            audit: None,
            metrics: SinkMetrics::new(),
            promoted: PromotedAttributes::default(),
        }
    }

    /// Set the RFC 0022 promoted attribute set flushed files project
    /// (`storage.promoted_attributes`, §3.2).
    #[must_use]
    pub fn with_promoted_attributes(mut self, promoted: PromotedAttributes) -> Self {
        self.promoted = promoted;
        self
    }

    /// Install the RFC 0025 §3.3 quarantine audit destination:
    /// permanently-rejected records emit a `record_quarantined`
    /// event here before being dropped from the buffer.
    #[must_use]
    pub fn with_audit_sink(mut self, sink: Box<dyn ourios_core::audit::AuditSink + Send>) -> Self {
        self.audit = Some(sink);
        self
    }

    /// Install the audit-durability barrier (issue #302 fix #2): a closure run
    /// before any inline size/ceiling publish that flushes the audit sink and
    /// returns whether it fully drained. When it returns `false` the inline
    /// publish is skipped (the partition is retained — the WAL is the durability
    /// of record — and the coordinated cadence flush retries it once the store
    /// recovers), so a record is never published before its template event is
    /// durable.
    #[must_use]
    pub fn with_audit_barrier(mut self, barrier: Box<dyn FnMut() -> bool + Send>) -> Self {
        self.audit_barrier = Some(barrier);
        self
    }

    /// Whether an inline publish may proceed: with a barrier installed, flush
    /// the audit sink first and require it to fully drain; without one, always.
    fn inline_publish_allowed(&mut self) -> bool {
        match &mut self.audit_barrier {
            Some(barrier) => barrier(),
            None => true,
        }
    }

    /// Total estimated bytes currently buffered across all partitions.
    #[must_use]
    pub fn buffered_bytes(&self) -> usize {
        self.total_bytes
    }

    /// Number of partitions with a non-empty buffer.
    #[must_use]
    pub fn buffered_partitions(&self) -> usize {
        self.buffers.len()
    }

    /// Count of successful partition flushes.
    #[must_use]
    pub fn flushes(&self) -> u64 {
        self.flushes
    }

    /// Total records written out across all successful flushes.
    #[must_use]
    pub fn records_flushed(&self) -> u64 {
        self.records_flushed
    }

    /// Records currently buffered (not yet flushed) across all partitions.
    #[must_use]
    pub fn buffered_records(&self) -> usize {
        self.buffers.values().map(|b| b.records.len()).sum()
    }

    /// Force-flush every buffered partition — the WAL-segment-rotation trigger
    /// (RFC0014.3), including sub-threshold low-volume partitions.
    pub fn flush_all(&mut self) {
        let keys: Vec<PartitionKey> = self.buffers.keys().cloned().collect();
        for key in keys {
            self.flush_partition_swallow(&key, "rotation");
        }
    }

    /// Flush partitions whose oldest record has reached `max_buffer_age` — the
    /// age trigger (RFC0014.2), driven by the batch-window tick.
    pub fn flush_aged(&mut self) {
        let max = self.config.max_buffer_age;
        let keys: Vec<PartitionKey> = self
            .buffers
            .iter()
            .filter(|(_, b)| b.oldest.elapsed() >= max)
            .map(|(k, _)| k.clone())
            .collect();
        for key in keys {
            self.flush_partition_swallow(&key, "age");
        }
    }

    /// Encode + put one partition's buffer. On success the buffer is removed
    /// and the counters advance; the caller (via [`Self::flush_partition_swallow`])
    /// retains it on error.
    fn flush_partition(
        &mut self,
        key: &PartitionKey,
        trigger: &'static str,
    ) -> Result<(), FlushError> {
        let flush_start = Instant::now();
        let bytes = match self.buffers.get(key) {
            Some(buf) if !buf.records.is_empty() => {
                match encode_records_to_parquet_with_promoted(
                    &buf.records,
                    DEFAULT_ZSTD_LEVEL,
                    &self.promoted,
                ) {
                    Ok(bytes) => bytes,
                    Err(WriterError::Batch(e)) if is_per_record_rejection(&e) => {
                        // A permanent per-record rejection (RFC 0025
                        // §3.3): retrying the whole buffer would wedge
                        // the partition forever (#362). Quarantine the
                        // poisoned record(s) and encode the remainder;
                        // the WAL keeps the originals.
                        self.quarantine_poisoned(key);
                        match self.buffers.get(key) {
                            Some(buf) if !buf.records.is_empty() => {
                                encode_records_to_parquet_with_promoted(
                                    &buf.records,
                                    DEFAULT_ZSTD_LEVEL,
                                    &self.promoted,
                                )
                                .map_err(FlushError::Encode)?
                            }
                            _ => {
                                // Every record quarantined: drop the
                                // emptied entry and release its byte
                                // accounting, or the ceiling logic and
                                // buffer gauge drift forever.
                                self.drop_emptied_buffer(key);
                                return Ok(());
                            }
                        }
                    }
                    Err(e) => return Err(FlushError::Encode(e)),
                }
            }
            _ => return Ok(()),
        };
        self.store
            .put_blocking(&object_key(key), bytes)
            .map_err(FlushError::Store)?;
        let elapsed = flush_start.elapsed();
        if let Some(buf) = self.buffers.remove(key) {
            self.total_bytes = self.total_bytes.saturating_sub(buf.est_bytes);
            self.flushes += 1;
            self.records_flushed += buf.records.len() as u64;
            self.metrics
                .record_flush(trigger, buf.records.len(), elapsed);
            self.metrics
                .add_buffered(-i64::try_from(buf.est_bytes).unwrap_or(i64::MAX));
        }
        Ok(())
    }

    /// Split `key`'s buffer into encodable records and permanently
    /// rejected ones; emit a `record_quarantined` audit event and the
    /// `error.type`-attributed flush-error count for each rejection,
    /// then drop the poison from the buffer (RFC 0025 §3.3 — the WAL
    /// remains the durability of record; the audit event is the
    /// operator's pointer for replay after a fix).
    /// Remove `key`'s (empty) buffer entry and release its byte
    /// accounting — the flush-success bookkeeping minus the flush
    /// counters (nothing was published).
    fn drop_emptied_buffer(&mut self, key: &PartitionKey) {
        if let Some(buf) = self.buffers.remove(key) {
            debug_assert!(buf.records.is_empty(), "only for emptied buffers");
            self.total_bytes = self.total_bytes.saturating_sub(buf.est_bytes);
            self.metrics
                .add_buffered(-i64::try_from(buf.est_bytes).unwrap_or(i64::MAX));
        }
    }

    fn quarantine_poisoned(&mut self, key: &PartitionKey) {
        let records = match self.buffers.get_mut(key) {
            Some(buf) => std::mem::take(&mut buf.records),
            None => return,
        };
        let kept = self.quarantine_owned(key, records);
        if let Some(buf) = self.buffers.get_mut(key) {
            buf.records = kept;
        }
    }

    /// [`Self::quarantine_poisoned`] over an owned batch (the
    /// cadence-drain path) — returns the encodable remainder.
    fn quarantine_owned(
        &mut self,
        key: &PartitionKey,
        records: Vec<MinedRecord>,
    ) -> Vec<MinedRecord> {
        let (kept, poisoned) = split_poisoned(records, &self.promoted);
        let partition = format!(
            "year={:04}/month={:02}/day={:02}/hour={:02}",
            key.year, key.month, key.day, key.hour
        );
        for (record, error) in poisoned {
            self.metrics.record_flush_error(Some(error.error_type()));
            if let Some(audit) = &mut self.audit {
                audit.emit(ourios_core::audit::AuditEvent {
                    tenant_id: record.tenant_id.clone(),
                    timestamp: std::time::SystemTime::now(),
                    payload: ourios_core::audit::AuditPayload::RecordQuarantined {
                        partition: partition.clone(),
                        error: error.to_string(),
                    },
                });
            }
        }
        kept
    }

    /// [`Self::flush_partition`] for the infallible `emit` / tick / rotation
    /// paths: a failed flush retains the buffer (the WAL is the durability of
    /// record) and is counted for observability. `trigger` records *why* the
    /// flush happened (RFC 0014 §3.2).
    fn flush_partition_swallow(&mut self, key: &PartitionKey, trigger: &'static str) {
        if self.flush_partition(key, trigger).is_err() {
            self.flush_errors += 1;
            self.metrics.record_flush_error(None);
        }
    }

    /// Flush the largest buffered partition to reclaim memory. Returns whether
    /// a flush actually succeeded (so the ceiling loop stops if the store is
    /// unavailable rather than spinning).
    fn flush_largest(&mut self) -> bool {
        let Some(key) = self
            .buffers
            .iter()
            .filter(|(_, b)| !b.records.is_empty())
            .max_by_key(|(_, b)| b.est_bytes)
            .map(|(k, _)| k.clone())
        else {
            return false;
        };
        if self.flush_partition(&key, "ceiling").is_ok() {
            true
        } else {
            self.flush_errors += 1;
            self.metrics.record_flush_error(None);
            false
        }
    }

    /// Take every partition whose oldest record has reached `max_buffer_age`
    /// (the cadence drain) as owned batches — a cheap memory move, **no I/O**.
    /// The [`crate::publish::PublishCoordinator`] calls this under the pipeline's
    /// miner lock so the drain is atomic w.r.t. `miner.ingest` (issue #302 #1),
    /// then publishes the batches off-lock via `publish_partition`.
    pub fn drain_aged(&mut self) -> Vec<(PartitionKey, Vec<MinedRecord>)> {
        let max = self.config.max_buffer_age;
        let keys: Vec<PartitionKey> = self
            .buffers
            .iter()
            .filter(|(_, b)| b.oldest.elapsed() >= max)
            .map(|(k, _)| k.clone())
            .collect();
        self.take_partitions(keys)
    }

    /// Take **every** buffered partition as owned batches (the rotation /
    /// shutdown drain) — a cheap memory move, no I/O.
    pub fn drain_all(&mut self) -> Vec<(PartitionKey, Vec<MinedRecord>)> {
        let keys: Vec<PartitionKey> = self.buffers.keys().cloned().collect();
        self.take_partitions(keys)
    }

    /// Remove `keys` from the buffer map, returning their non-empty record
    /// batches and decrementing the byte accounting + occupancy gauge.
    fn take_partitions(
        &mut self,
        keys: Vec<PartitionKey>,
    ) -> Vec<(PartitionKey, Vec<MinedRecord>)> {
        let mut out = Vec::new();
        for key in keys {
            if let Some(buf) = self.buffers.remove(&key) {
                self.total_bytes = self.total_bytes.saturating_sub(buf.est_bytes);
                self.metrics
                    .add_buffered(-i64::try_from(buf.est_bytes).unwrap_or(i64::MAX));
                if !buf.records.is_empty() {
                    out.push((key, buf.records));
                }
            }
        }
        out
    }

    /// Re-buffer `batches` whose off-lock publish failed (transient): the WAL is
    /// the durability of record, so retain + retry. Retained records go *ahead*
    /// of anything `emit` buffered for the same partition meanwhile, and the
    /// byte accounting + gauge are restored.
    pub fn requeue(&mut self, batches: Vec<(PartitionKey, Vec<MinedRecord>)>) {
        // These records were drained because they had aged; keep the partition
        // aged so the retry isn't deferred behind newer records that arrived
        // during the off-lock publish (which carry a newer `oldest`). Pin
        // `oldest` to the age threshold (or older), so the next sweep re-drains.
        let aged = Instant::now()
            .checked_sub(self.config.max_buffer_age)
            .unwrap_or_else(Instant::now);
        for (key, records) in batches {
            let est: usize = records.iter().map(estimate_bytes).sum();
            let buf = self.buffers.entry(key).or_insert_with(|| PartitionBuffer {
                records: Vec::new(),
                est_bytes: 0,
                oldest: Instant::now(),
            });
            let mut combined = records;
            combined.append(&mut buf.records);
            buf.records = combined;
            buf.est_bytes = buf.est_bytes.saturating_add(est);
            buf.oldest = buf.oldest.min(aged);
            self.total_bytes = self.total_bytes.saturating_add(est);
            self.metrics
                .add_buffered(i64::try_from(est).unwrap_or(i64::MAX));
        }
    }

    /// Settle counters + metrics for a successful off-lock publish of one
    /// partition's `records`, caused by `trigger`, taking `elapsed`.
    pub fn note_published(&mut self, records: usize, elapsed: Duration, trigger: &'static str) {
        self.flushes += 1;
        self.records_flushed += records as u64;
        self.metrics.record_flush(trigger, records, elapsed);
    }

    /// Settle counters + metrics for a failed off-lock publish (the partition is
    /// requeued by the caller).
    pub fn note_flush_error(&mut self) {
        self.flush_errors += 1;
        self.metrics.record_flush_error(None);
    }

    /// A clone of the data store, for the coordinator's off-lock publish.
    #[must_use]
    pub fn store(&self) -> Store {
        self.store.clone()
    }
}

/// Encode + put one partition's records to the data store, holding **no sink
/// lock** — the coordinator's off-lock publish step (issue #302). Mirrors
/// [`ParquetRecordSink::flush_partition`]'s encode+put without the buffer
/// bookkeeping (the caller settles via [`ParquetRecordSink::note_published`] /
/// [`ParquetRecordSink::requeue`]).
fn publish_partition(
    store: &Store,
    key: &PartitionKey,
    records: &[MinedRecord],
    promoted: &PromotedAttributes,
) -> Result<(), FlushError> {
    let bytes = encode_records_to_parquet_with_promoted(records, DEFAULT_ZSTD_LEVEL, promoted)
        .map_err(FlushError::Encode)?;
    store
        .put_blocking(&object_key(key), bytes)
        .map_err(FlushError::Store)?;
    Ok(())
}

/// A cloneable handle to one shared [`ParquetRecordSink`].
///
/// The ingest path has two writers to the same sink: the miner `emit`s mined
/// records through its `Box<dyn RecordSink>`, while the pipeline drives the
/// flush triggers the sink itself can't observe — [`Self::flush_all`] on WAL
/// segment rotation (RFC0014.3) and [`Self::flush_aged`] on the batch-window
/// tick (RFC0014.2). `Clone` yields another handle to the *same* sink: hand
/// one to `MinerCluster::with_record_sink` and keep another to drive the
/// triggers (same pattern as `SharedRecordSink` / `SharedAuditSink`).
///
/// All access serializes on one mutex. `emit` is a short critical section, but
/// the flush triggers are **not**: `flush_all` / `flush_aged` hold the lock
/// across `encode_records_to_parquet_with_promoted` + `Store::put_blocking` (see
/// [`ParquetRecordSink::flush_all`]), so a flush against a slow store blocks
/// every concurrent `emit` and trigger for the duration of the I/O. Callers
/// must treat them as blocking sections (the server runs them via
/// `block_in_place` / `spawn_blocking`). With the local backend a flush is
/// sub-millisecond, so this is benign; the encode+put is worth moving outside
/// the lock (drain under the lock, do I/O unlocked, re-lock to settle counters)
/// when the S3 backend lands (RFC 0014 §7 / RFC 0013), where PUTs are slow.
///
/// The only lock order is miner → sink (the pipeline holds the miner lock while
/// it `emit`s and while the rotation hook flushes); the tick takes the sink
/// alone, so there is no cycle.
#[derive(Clone)]
pub struct SharedParquetSink {
    inner: Arc<Mutex<ParquetRecordSink>>,
}

impl SharedParquetSink {
    /// Wrap `sink` in a shared, cloneable handle.
    #[must_use]
    pub fn new(sink: ParquetRecordSink) -> Self {
        Self {
            inner: Arc::new(Mutex::new(sink)),
        }
    }

    /// Lock the sink, recovering a poisoned mutex. A poison means a past panic
    /// while a flush was in flight; the buffer + counters remain structurally
    /// consistent (the WAL is the durability of record), so recovering the
    /// inner sink is safer than panicking the ingest path (`CLAUDE.md` §3.4,
    /// and the same posture `receiver` takes on the miner mutex).
    fn lock(&self) -> std::sync::MutexGuard<'_, ParquetRecordSink> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Force-flush every buffered partition — the WAL-segment-rotation trigger
    /// (RFC0014.3) and the graceful-shutdown drain.
    pub fn flush_all(&self) {
        self.lock().flush_all();
    }

    /// Flush partitions past `max_buffer_age` — the batch-window tick
    /// (RFC0014.2).
    pub fn flush_aged(&self) {
        self.lock().flush_aged();
    }

    /// Successful partition flushes so far (observability + tests).
    #[must_use]
    pub fn flushes(&self) -> u64 {
        self.lock().flushes()
    }

    /// Records currently buffered (not yet flushed) across all partitions.
    #[must_use]
    pub fn buffered_records(&self) -> usize {
        self.lock().buffered_records()
    }

    /// Atomically take the aged partitions as owned batches (issue #302). The
    /// [`crate::publish::PublishCoordinator`] calls this under the pipeline's
    /// miner lock so the drain is atomic w.r.t. `miner.ingest`, then publishes
    /// off-lock via [`Self::publish_owned`].
    #[must_use]
    pub fn drain_aged(&self) -> Vec<(PartitionKey, Vec<MinedRecord>)> {
        self.lock().drain_aged()
    }

    /// Atomically take **every** buffered partition as owned batches.
    #[must_use]
    pub fn drain_all(&self) -> Vec<(PartitionKey, Vec<MinedRecord>)> {
        self.lock().drain_all()
    }

    /// Re-buffer owned `batches` (a transient publish failure, or the
    /// coordinator holding records because the audit write failed). The WAL is
    /// the durability of record, so retain + retry on the next cadence.
    pub fn requeue(&self, batches: Vec<(PartitionKey, Vec<MinedRecord>)>) {
        self.lock().requeue(batches);
    }

    /// Publish owned `batches` to the data store **off the lock** (the encode +
    /// put runs unlocked; the sink is locked only to read the store handle and
    /// to settle / requeue). A partition whose put fails is requeued (the WAL is
    /// the durability of record). Returns whether every partition was published.
    /// `trigger` labels the flush metric.
    #[must_use]
    pub fn publish_owned(
        &self,
        batches: Vec<(PartitionKey, Vec<MinedRecord>)>,
        trigger: &'static str,
    ) -> bool {
        if batches.is_empty() {
            return true;
        }
        let (store, promoted) = {
            let sink = self.lock();
            (sink.store(), sink.promoted.clone())
        };
        let mut requeue = Vec::new();
        let mut all_published = true;
        for (key, records) in batches {
            let start = Instant::now();
            match publish_partition(&store, &key, &records, &promoted) {
                Ok(()) => {
                    self.lock()
                        .note_published(records.len(), start.elapsed(), trigger);
                }
                Err(FlushError::Encode(WriterError::Batch(e))) if is_per_record_rejection(&e) => {
                    // Permanent per-record rejection: requeueing would
                    // re-fail forever (#362 via the cadence path).
                    // Quarantine and publish the remainder once.
                    let kept = self.lock().quarantine_owned(&key, records);
                    if kept.is_empty() {
                        continue;
                    }
                    if publish_partition(&store, &key, &kept, &promoted).is_ok() {
                        self.lock()
                            .note_published(kept.len(), start.elapsed(), trigger);
                    } else {
                        self.lock().note_flush_error();
                        requeue.push((key, kept));
                        all_published = false;
                    }
                }
                Err(_) => {
                    self.lock().note_flush_error();
                    requeue.push((key, records));
                    all_published = false;
                }
            }
        }
        if !requeue.is_empty() {
            self.lock().requeue(requeue);
        }
        all_published
    }
}

impl RecordSink for SharedParquetSink {
    fn emit(&mut self, record: MinedRecord) {
        self.lock().emit(record);
    }
}

impl RecordSink for ParquetRecordSink {
    fn emit(&mut self, record: MinedRecord) {
        let Ok(key) = PartitionKey::derive(&record) else {
            // Un-partitionable (timestamp overflow, §3.4 fallback exhausted):
            // can't route it. The WAL still holds it; count and drop here.
            self.derive_errors += 1;
            self.metrics.record_derive_error();
            return;
        };
        let est = estimate_bytes(&record);

        // Ceiling (RFC0014.4): flush the largest partition inline to make room
        // before appending, so buffered bytes stay at or under the ceiling
        // whenever the store accepts writes. If a flush fails (store
        // unavailable) or nothing more can be flushed (a single oversized
        // buffer), the loop stops rather than spinning — the record is still
        // retained below (the WAL is the durability of record), and the
        // ceiling may be transiently exceeded (counted via `flush_errors`)
        // instead of deadlocking the ingest path.
        // The audit barrier (issue #302 fix #2) runs before each inline publish
        // so a partition is never put to the store before the audit sink is
        // durable; if it can't drain (transient store error), stop rather than
        // publish, leaving the record buffered (the WAL is the durability of
        // record) and the ceiling transiently exceeded.
        while self.total_bytes.saturating_add(est) > self.config.ceiling_bytes
            && self.inline_publish_allowed()
            && self.flush_largest()
        {}

        let buf = self
            .buffers
            .entry(key.clone())
            .or_insert_with(|| PartitionBuffer {
                records: Vec::new(),
                est_bytes: 0,
                oldest: Instant::now(),
            });
        buf.records.push(record);
        // Saturating (matching `saturating_sub` on flush) so the byte counters
        // stay monotonic and the triggers can't be corrupted by wraparound
        // under prolonged retention (e.g. a store outage).
        buf.est_bytes = buf.est_bytes.saturating_add(est);
        let over_target = buf.est_bytes >= self.config.target_bytes;
        self.total_bytes = self.total_bytes.saturating_add(est);
        self.metrics
            .add_buffered(i64::try_from(est).unwrap_or(i64::MAX));

        // Size trigger (RFC0014.1): the emit that crosses the target flushes —
        // but only after the audit barrier confirms the audit sink is durable
        // (issue #302 fix #2). If it can't drain, retain the partition (the WAL
        // is the durability of record); the coordinated cadence flush publishes
        // it once the store recovers, never before its template event is durable.
        if over_target && self.inline_publish_allowed() {
            self.flush_partition_swallow(&key, "size");
        }
    }
}

#[cfg(test)]
mod tests {
    use ourios_core::audit::ParamType;
    use ourios_core::record::{BodyKind, Param};
    use ourios_core::tenant::TenantId;

    use super::*;

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

    fn never_flush() -> FlushConfig {
        FlushConfig {
            target_bytes: usize::MAX,
            max_buffer_age: Duration::from_secs(86_400),
            ceiling_bytes: usize::MAX,
        }
    }

    #[test]
    fn shared_handle_emits_and_flushes_one_buffer() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let store = Store::local(dir.path()).expect("local store");
        let handle = SharedParquetSink::new(ParquetRecordSink::new(store, never_flush()));

        // The miner's clone emits; the pipeline's clone observes + drives the
        // flush trigger — same underlying sink.
        let mut producer = handle.clone();
        producer.emit(rec("tenant-a"));
        producer.emit(rec("tenant-a"));
        assert_eq!(handle.buffered_records(), 2, "clones share one buffer");
        assert_eq!(handle.flushes(), 0, "no trigger fired yet");

        handle.flush_all(); // the rotation trigger, via the pipeline's handle
        assert_eq!(handle.flushes(), 1);
        assert_eq!(handle.buffered_records(), 0, "flush drained the buffer");
    }

    #[test]
    fn requeue_keeps_the_partition_aged_for_prompt_retry() {
        // A transient-failed publish requeues already-aged records. If a newer
        // record arrived for the same partition during the off-lock publish, the
        // partition's `oldest` reflects that fresh record and the requeued (aged)
        // records would miss the next age-sweep. `requeue` pins `oldest` to the
        // age threshold so the retry re-drains promptly (issue #302).
        let dir = tempfile::TempDir::new().expect("temp dir");
        let store = Store::local(dir.path()).expect("local store");
        // `never_flush` has a 1-day max age, so a freshly-emitted record is not
        // aged on its own — the partition only ages via the requeue pin.
        let mut sink = ParquetRecordSink::new(store, never_flush());

        // Drain a batch to stand in for an aged drain whose publish then fails.
        sink.emit(rec("checkout"));
        let batch = sink.drain_all();
        assert_eq!(batch.len(), 1, "one partition drained");

        // A newer record arrives for the same partition during the off-lock
        // publish; on its own it is not aged.
        sink.emit(rec("checkout"));
        assert!(
            sink.drain_aged().is_empty(),
            "the fresh record alone is not aged",
        );

        sink.requeue(batch);

        let retry = sink.drain_aged();
        assert_eq!(
            retry.len(),
            1,
            "the requeued partition is aged again and re-drains on the next sweep",
        );
        assert_eq!(
            retry[0].1.len(),
            2,
            "both the requeued record and the newer one drain together",
        );
    }

    // --- issue #302 fix #2: the inline size trigger is audit-ordered. ---

    use crate::audit_sink::{BufferingAuditSink, SharedParquetAuditSink};
    use ourios_core::audit::{
        AuditEvent, AuditPayload, AuditSink, TemplateChange, hash_triggering_line,
    };

    /// A record sink whose per-emit estimate crosses `target_bytes` on the first
    /// record, so a single `emit` fires the inline size trigger.
    fn size_trigger_config() -> FlushConfig {
        FlushConfig {
            target_bytes: 16,
            max_buffer_age: Duration::from_secs(86_400),
            ceiling_bytes: usize::MAX,
        }
    }

    fn created_event(tenant: &str) -> AuditEvent {
        AuditEvent {
            tenant_id: TenantId::new(tenant),
            timestamp: std::time::UNIX_EPOCH + Duration::from_secs(1_775_127_480),
            payload: AuditPayload::Template {
                template_id: 1,
                triggering_line_hash: hash_triggering_line(b"user 1 logged in"),
                triggering_line_sample: Some("user 1 logged in".to_owned()),
                change: TemplateChange::Created {
                    new_template: "user <*> logged in".to_owned(),
                },
            },
        }
    }

    #[test]
    fn size_trigger_flushes_audit_before_publishing() {
        // The inline size trigger publishes the partition only after the audit
        // barrier drives the audit sink to durability — so a clean row is never
        // query-visible before its template event is durable (issue #302 §3.3).
        let tmp = tempfile::TempDir::new().expect("temp");
        let audit_root = tmp.path().join("audit");
        std::fs::create_dir_all(&audit_root).expect("audit root");
        let audit = SharedParquetAuditSink::new(BufferingAuditSink::new(
            Store::local(&audit_root).expect("audit store"),
            1024,
        ));

        let data_root = tmp.path().join("data");
        std::fs::create_dir_all(&data_root).expect("data root");
        let barrier_audit = audit.clone();
        let records = SharedParquetSink::new(
            ParquetRecordSink::new(
                Store::local(&data_root).expect("data store"),
                size_trigger_config(),
            )
            .with_audit_barrier(Box::new(move || barrier_audit.flush())),
        );

        // A template event is buffered (as the miner would, before the record).
        audit.clone().emit(created_event("tenant-a"));
        // The record whose emit crosses the size target.
        records.clone().emit(rec("tenant-a"));

        assert_eq!(
            records.flushes(),
            1,
            "the size trigger published the partition"
        );
        assert_eq!(records.buffered_records(), 0, "the partition was drained");
        assert_eq!(
            audit.buffered_events(),
            0,
            "the audit barrier flushed the template event to durability first",
        );
        assert!(audit.flushes() >= 1, "an audit partition was written");
    }

    #[test]
    fn size_trigger_is_skipped_when_audit_cannot_drain() {
        // If the audit barrier can't reach durability (transient store error),
        // the size trigger must NOT publish — the record is retained (the WAL is
        // the durability of record), never exposed before its template event.
        let tmp = tempfile::TempDir::new().expect("temp");
        let audit_root = tmp.path().join("audit");
        std::fs::create_dir_all(&audit_root).expect("audit root");
        let audit = SharedParquetAuditSink::new(BufferingAuditSink::new(
            Store::local(&audit_root).expect("audit store"),
            1024,
        ));
        let data_root = tmp.path().join("data");
        std::fs::create_dir_all(&data_root).expect("data root");
        let barrier_audit = audit.clone();
        let records = SharedParquetSink::new(
            ParquetRecordSink::new(
                Store::local(&data_root).expect("data store"),
                size_trigger_config(),
            )
            .with_audit_barrier(Box::new(move || barrier_audit.flush())),
        );

        audit.clone().emit(created_event("tenant-a"));
        // Sabotage the audit store so the barrier flush fails transiently.
        std::fs::remove_dir_all(&audit_root).expect("remove audit dir");
        std::fs::write(&audit_root, b"not a directory").expect("sabotage audit");

        records.clone().emit(rec("tenant-a"));

        assert_eq!(
            records.flushes(),
            0,
            "the size trigger is skipped while the template event isn't durable",
        );
        assert_eq!(
            records.buffered_records(),
            1,
            "the record is retained (the WAL is the durability of record)",
        );
    }
}
