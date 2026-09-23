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
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use ourios_core::record::{MinedRecord, RecordSink};
use ourios_parquet::{
    DEFAULT_ZSTD_LEVEL, PartitionKey, PromotedAttributes, Store, StoreError, WriterError,
    encode_records_to_parquet_with_promoted,
};
use uuid::Uuid;

use crate::cadence::{BarrierEpochs, Epoch};
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
    /// RFC 0052 §3.1's `ready` mark: set when a partition that had
    /// already left the buffers is **parked** back into them — a full
    /// publisher queue, the publisher's unwind drain, or a send that
    /// found the channel closed.
    ///
    /// It keeps the partition's `audit_watermark`, because parking must
    /// not strip the dependency that makes the records publishable, and
    /// it excludes the partition from the size trigger: it is already
    /// past it, so re-firing would publish it inline instead of letting
    /// the publisher or the next drain take it oldest-first.
    ready: Option<u64>,
}

impl PartitionBuffer {
    fn empty() -> Self {
        Self {
            records: Vec::new(),
            est_bytes: 0,
            oldest: Instant::now(),
            ready: None,
        }
    }
}

/// Partitions taken out of the buffers by one drain, with the audit
/// position their publish depends on (RFC 0052 §3.1).
///
/// The watermark is the **maximum** over the `ready` partitions taken —
/// each carrying the position its park preserved — and nothing else
/// contributes to it yet. An ordinary partition's own position is the
/// audit sink's count at drain time, which only the publish coordinator
/// can read (it owns both sinks); until §3.1's publisher lands and
/// supplies it, an ordinary drain reports `0`.
///
/// **So this value is not yet a sufficient gate.** Every current
/// consumer is audit-ordered by other means: `write_ordered` writes the
/// audit batch first, and the inline size / ceiling publish runs behind
/// the sink's audit barrier, which is strictly stronger (it requires the
/// whole buffer durable). A publisher that gated on this alone would
/// publish records ahead of their template events.
///
/// Accessors rather than public fields — a caller that could rebuild
/// this value could defeat the dependency parking preserves.
#[derive(Debug, Default)]
pub struct TakenPartitions {
    partitions: Vec<(PartitionKey, Vec<MinedRecord>)>,
    audit_watermark: u64,
}

impl TakenPartitions {
    /// Whether the drain took nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.partitions.is_empty()
    }

    /// The audit position that must be durable before these records are
    /// written.
    #[must_use]
    pub fn audit_watermark(&self) -> u64 {
        self.audit_watermark
    }

    /// The partitions, borrowed.
    #[must_use]
    pub fn partitions(&self) -> &[(PartitionKey, Vec<MinedRecord>)] {
        &self.partitions
    }

    /// The partitions, owned — the publish path's consuming form.
    #[must_use]
    pub fn into_partitions(self) -> Vec<(PartitionKey, Vec<MinedRecord>)> {
        self.partitions
    }

    /// Total estimated bytes, for the barrier's coalescing bound
    /// (§3.1's `ceiling_bytes`).
    #[must_use]
    pub fn estimated_bytes(&self) -> usize {
        self.partitions
            .iter()
            .flat_map(|(_, records)| records.iter())
            .map(estimate_bytes)
            .sum()
    }

    /// Fold `other`'s partitions into this drain, keeping the higher
    /// watermark — the barrier's coalescing step (§3.1). The two drains
    /// are disjoint by construction, each having emptied the buffers.
    pub fn absorb(&mut self, other: Self) {
        self.partitions.extend(other.partitions);
        self.audit_watermark = self.audit_watermark.max(other.audit_watermark);
    }
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

/// `/`-delimited object key for a partition's flushed file: the RFC 0005 §3.4
/// Hive path (relative to the store root) plus a `UUIDv7` name. Mirrors
/// `ourios_parquet::Writer`'s key; object keys are `/`-delimited on every host.
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

    /// Split `key`'s buffer into encodable records and permanently
    /// rejected ones; emit a `record_quarantined` audit event and the
    /// `error.type`-attributed flush-error count for each rejection,
    /// then drop the poison from the buffer and release its byte
    /// accounting (RFC 0025 §3.3 — the WAL remains the durability of
    /// record; the audit event is the operator's pointer for replay
    /// after a fix).
    fn quarantine_poisoned(&mut self, key: &PartitionKey) {
        let records = match self.buffers.get_mut(key) {
            Some(buf) => std::mem::take(&mut buf.records),
            None => return,
        };
        let before: usize = records.iter().map(estimate_bytes).sum();
        let kept = self.quarantine_owned(key, records);
        let after: usize = kept.iter().map(estimate_bytes).sum();
        // Release the dropped records' share of the byte accounting
        // (same estimator as emit) so ceiling triggers and the buffer
        // gauge track what is actually buffered.
        let freed = before.saturating_sub(after);
        self.total_bytes = self.total_bytes.saturating_sub(freed);
        self.metrics
            .add_buffered(-i64::try_from(freed).unwrap_or(i64::MAX));
        if let Some(buf) = self.buffers.get_mut(key) {
            buf.est_bytes = buf.est_bytes.saturating_sub(freed);
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
    pub fn drain_aged(&mut self) -> TakenPartitions {
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
    pub fn drain_all(&mut self) -> TakenPartitions {
        let keys: Vec<PartitionKey> = self.buffers.keys().cloned().collect();
        self.take_partitions(keys)
    }

    /// Remove `keys` from the buffer map, returning their non-empty record
    /// batches and decrementing the byte accounting + occupancy gauge.
    ///
    /// A `ready` partition (RFC 0052 §3.1, parked back into the buffers)
    /// contributes its stored `audit_watermark` to the drain's, so the
    /// dependency parking preserved travels with the records.
    fn take_partitions(&mut self, keys: Vec<PartitionKey>) -> TakenPartitions {
        let mut out = TakenPartitions::default();
        for key in keys {
            if let Some(buf) = self.buffers.remove(&key) {
                self.total_bytes = self.total_bytes.saturating_sub(buf.est_bytes);
                self.metrics
                    .add_buffered(-i64::try_from(buf.est_bytes).unwrap_or(i64::MAX));
                out.audit_watermark = out.audit_watermark.max(buf.ready.unwrap_or(0));
                if !buf.records.is_empty() {
                    out.partitions.push((key, buf.records));
                }
            }
        }
        out
    }

    /// RFC 0052 §3.1's **park**: put partitions that had already left
    /// the buffers back into them as `ready`, keeping the
    /// `audit_watermark` their publish depends on.
    ///
    /// Three sites take it — a full publisher queue, the publisher's
    /// unwind drain, and a send that found the channel closed — and all
    /// three must be a settlement with a date on it, which the caller
    /// records on the in-flight accounting.
    pub fn park_ready(
        &mut self,
        batches: Vec<(PartitionKey, Vec<MinedRecord>)>,
        audit_watermark: u64,
    ) {
        let keys: Vec<PartitionKey> = batches.iter().map(|(key, _)| key.clone()).collect();
        self.requeue(batches);
        for key in keys {
            if let Some(buf) = self.buffers.get_mut(&key) {
                buf.ready = Some(buf.ready.unwrap_or(0).max(audit_watermark));
            }
        }
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
            let buf = self
                .buffers
                .entry(key)
                .or_insert_with(PartitionBuffer::empty);
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

    /// The under-lock half of [`SharedParquetSink::emit_concurrent`]
    /// (RFC 0035 §3.1): buffer append + trigger checks only. The caller
    /// derived the partition key and byte estimate off-lock, and any
    /// partitions this returns — `(ceiling-taken, size-taken)` — are taken
    /// out of the buffer for the caller to encode + publish **off-lock**
    /// (via [`SharedParquetSink::publish_owned`]). Unlike [`RecordSink::emit`],
    /// which encodes under the sink lock on the size / ceiling triggers,
    /// this keeps the locked section to a memory move so concurrent pool
    /// workers never serialize on one worker's Parquet encode.
    #[allow(clippy::type_complexity)] // the two trigger classes of one drain, not a nameable domain type
    fn append_off_lock(
        &mut self,
        key: PartitionKey,
        est: usize,
        record: MinedRecord,
    ) -> (TakenPartitions, TakenPartitions) {
        // Ceiling (RFC0014.4), off-lock form: take the largest partition
        // out for the caller to publish instead of flushing inline. The
        // byte accounting drops at take time, so the loop terminates once
        // the buffers are drained even if the off-lock publish later
        // fails (a failure requeues and the ceiling is transiently
        // exceeded — same posture as `emit`).
        let mut ceiling_taken = TakenPartitions::default();
        while self.total_bytes.saturating_add(est) > self.config.ceiling_bytes {
            if !self.inline_publish_allowed() {
                break;
            }
            let Some(largest) = self
                .buffers
                .iter()
                .filter(|(_, b)| !b.records.is_empty())
                .max_by_key(|(_, b)| b.est_bytes)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            ceiling_taken.absorb(self.take_partitions(vec![largest]));
        }

        let buf = self
            .buffers
            .entry(key.clone())
            .or_insert_with(PartitionBuffer::empty);
        buf.records.push(record);
        buf.est_bytes = buf.est_bytes.saturating_add(est);
        // RFC 0052 §3.1: a parked `ready` partition is already past the
        // size trigger, so re-firing it would publish inline instead of
        // letting the publisher or the next drain take it oldest-first.
        let over_target = buf.ready.is_none() && buf.est_bytes >= self.config.target_bytes;
        self.total_bytes = self.total_bytes.saturating_add(est);
        self.metrics
            .add_buffered(i64::try_from(est).unwrap_or(i64::MAX));

        // Size trigger (RFC0014.1), audit-ordered exactly like `emit`
        // (issue #302 fix #2): the partition is taken only after the
        // barrier confirms the audit sink is durable.
        let mut size_taken = TakenPartitions::default();
        if over_target && self.inline_publish_allowed() {
            size_taken = self.take_partitions(vec![key]);
        }
        (ceiling_taken, size_taken)
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

/// Outstanding off-lock publish accounting (issue #578), shared by every
/// clone of one [`SharedParquetSink`]: how many drained-out-of-the-buffers
/// snapshots ([`crate::publish::Drained`]) have not yet settled their
/// off-lock write. Living in the sink's shared state — rather than on the
/// [`crate::publish::PublishCoordinator`] — makes the count structurally
/// unsplittable: any coordinator built over this sink, and any clone of it,
/// feeds the same counter the stamping paths quiesce. Same shape as the
/// encode pool's pending counter.
#[derive(Debug)]
struct InFlightPublishes {
    count: Mutex<usize>,
    settled: Condvar,
    /// RFC 0052 §3.1's dated settlements: for each publish whose records
    /// re-entered the buffers — a requeue after a transient failure, or
    /// one of the three park sites — the epoch it was **registered**
    /// under and the `barrier_epoch` current when they came back.
    ///
    /// Both halves are needed. A cut `E` is refused only by a publish
    /// *registered before its capture* (`registered <= E`) that came
    /// back *after* it (`E < at`): such a cut's drain could not have
    /// seen those records. A publish registered after `E` holds only
    /// frames above `E`'s mark, so its failure fails no cut at or below
    /// `E`; and a cut captured after the return drained them itself. The
    /// `at` alone — one monotone maximum — would refuse both of those.
    settlements: Mutex<Vec<Settlement>>,
    /// The cadence state every guard reports into.
    epochs: Arc<BarrierEpochs>,
}

impl InFlightPublishes {
    fn record(&self, settlement: Settlement) {
        self.settlements
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(settlement);
    }
}

/// How one registered publish ended up somewhere other than the store
/// (RFC 0052 §3.1). Two states, not one with a flag: they refuse
/// different sets of cuts, and a single variant could not say which.
#[derive(Clone, Copy, Debug)]
enum Settlement {
    /// The records went back into the buffers — a transient failure's
    /// requeue, or one of the three park sites. Only a cut captured
    /// before the return, by a publish registered before that cut, is
    /// refused: a cut captured after it drained them itself.
    Returned { registered: Epoch, at: Epoch },
    /// The publish unwound: its records are in neither the buffers nor
    /// the store, so every cut whose mark could cover them is refused
    /// until a restart re-mines them from the WAL.
    Unwound { registered: Epoch },
}

impl Settlement {
    /// Whether this settlement refuses a cut of `epoch`.
    fn refuses(self, epoch: Epoch) -> bool {
        match self {
            Self::Returned { registered, at } => registered <= epoch && epoch < at,
            Self::Unwound { registered } => registered <= epoch,
        }
    }

    /// Whether a cut of `epoch` has put this settlement permanently
    /// behind it.
    fn spent(self, epoch: Epoch) -> bool {
        match self {
            Self::Returned { at, .. } => at <= epoch,
            // Stage 1 never clears an unwind: the records exist only in
            // the WAL until a restart re-mines them.
            Self::Unwound { .. } => false,
        }
    }
}

/// The outcome of [`SharedParquetSink::quiesce_publishes`]: every
/// registered publish has settled, and this says whether any of them
/// settled in a way that refuses a given cut.
///
/// A value rather than a bare wait, because §3.1 needs both halves —
/// the recheck defends the ordering, the outcome defends the data, and
/// either alone refuses the stamp.
#[derive(Clone, Debug, Default)]
pub struct PublishOutcomes {
    settlements: Vec<Settlement>,
}

impl PublishOutcomes {
    /// Whether a cut of `epoch` may stamp: false when a publish
    /// registered before its capture put records back into the buffers
    /// after it.
    #[must_use]
    pub fn all_ok(&self, epoch: Epoch) -> bool {
        !self
            .settlements
            .iter()
            .any(|settlement| settlement.refuses(epoch))
    }
}

/// Settles one in-flight publish on drop — including during unwinding, so a
/// panic mid-publish cannot strand [`SharedParquetSink::quiesce_publishes`],
/// which waits under the pipeline's miner lock at rotation, where a wedge
/// would halt all ingest (the same posture as the encode pool's
/// `BatchGuard`).
///
/// RFC 0052 §3.1: the guard carries the epoch it was **registered**
/// under, and an unwinding drop *reports* that epoch to the cadence
/// latch **before** the decrement that wakes a waiting barrier — so
/// there is no schedule in which the count settles and the latch is
/// still clear.
#[derive(Debug)]
pub struct PublishGuard {
    in_flight: Arc<InFlightPublishes>,
    epoch: Epoch,
}

impl PublishGuard {
    /// The epoch this publish was registered under.
    #[must_use]
    pub fn epoch(&self) -> Epoch {
        self.epoch
    }
}

impl Drop for PublishGuard {
    fn drop(&mut self) {
        if std::thread::panicking() {
            // Two records of the same unwind, because §3.1 needs both:
            // the latch defends the *ordering* (a cut's recheck sees it),
            // and the settlement defends the *data* (`all_ok` is false
            // for it independently). Either alone refuses the stamp.
            self.in_flight.epochs.report(self.epoch);
            self.in_flight.record(Settlement::Unwound {
                registered: self.epoch,
            });
        }
        let mut count = self
            .in_flight
            .count
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *count = count.saturating_sub(1);
        if *count == 0 {
            self.in_flight.settled.notify_all();
        }
    }
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
    in_flight: Arc<InFlightPublishes>,
}

impl SharedParquetSink {
    /// Wrap `sink` in a shared, cloneable handle.
    ///
    /// The handle owns the process's [`BarrierEpochs`]: it is the one
    /// object every party to a cut already holds — the encode pool, the
    /// publish coordinator, the barrier task — so making it the root
    /// removes any way for two of them to end up on different latches.
    #[must_use]
    pub fn new(sink: ParquetRecordSink) -> Self {
        Self::with_cadence(sink, Arc::new(BarrierEpochs::new()))
    }

    /// [`Self::new`] over an existing cadence state — for a test that
    /// drives the latch directly.
    #[must_use]
    pub fn with_cadence(sink: ParquetRecordSink, epochs: Arc<BarrierEpochs>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(sink)),
            in_flight: Arc::new(InFlightPublishes {
                count: Mutex::new(0),
                settled: Condvar::new(),
                settlements: Mutex::new(Vec::new()),
                epochs,
            }),
        }
    }

    /// The cadence state this sink's guards report into.
    #[must_use]
    pub fn epochs(&self) -> Arc<BarrierEpochs> {
        Arc::clone(&self.in_flight.epochs)
    }

    /// Register one off-lock publish of records about to be drained out of
    /// this sink's buffers (issue #578): the returned guard marks them in
    /// flight until it drops. **Acquire before the drain, under the
    /// barrier exclusion** — the same exclusion every cut capture takes —
    /// so there is no instant at which drained records exist outside both
    /// the buffers and the in-flight count, and the epoch the guard reads
    /// is ordered against the capture (RFC 0052 §3.1).
    #[must_use]
    pub fn begin_publish(&self) -> PublishGuard {
        *self
            .in_flight
            .count
            .lock()
            .unwrap_or_else(PoisonError::into_inner) += 1;
        PublishGuard {
            epoch: self.in_flight.epochs.current(),
            in_flight: Arc::clone(&self.in_flight),
        }
    }

    /// Record that a publish registered at `registered` put its records
    /// back into the buffers now — a requeue or one of §3.1's three park
    /// sites.
    ///
    /// A return at the epoch the publish was registered under refuses no
    /// cut (the records never left a cut's reach), so it is not stored.
    pub fn note_resettled(&self, registered: Epoch) {
        let at = self.in_flight.epochs.current();
        if at <= registered {
            return;
        }
        self.in_flight
            .record(Settlement::Returned { registered, at });
    }

    /// Block until no drained-but-unsettled off-lock publish is in flight —
    /// the publish half of the RFC 0035 §3.1 barrier (issue #578).
    ///
    /// Every `wal_high_water` stamping path MUST call this (after quiescing
    /// any encode pool, before the flush) with exclusive access to the
    /// miner — the rotation hook and shutdown hold the pipeline's miner
    /// lock; the post-recovery stamp runs before the pipeline (and its
    /// mutex) exists, so exclusivity is by construction and no drain can
    /// race it. Records the age sweep drained out of the buffers are neither
    /// buffered (so `flush_all` cannot cover them) nor durable until their
    /// off-lock `write_ordered` completes, and a mark stamped across that
    /// window makes recovery skip the WAL frames that are their only
    /// surviving copy — acked-data loss (`CLAUDE.md` §3.4). When this
    /// returns, every such snapshot is either durable in the store or
    /// requeued into the buffers (where the caller's flush covers it), and
    /// the caller's miner exclusivity keeps the count at zero until the
    /// stamp: every drain acquires its guard under the barrier exclusion.
    ///
    /// RFC 0052 §3.1 makes it **report** rather than merely wait: the
    /// returned [`PublishOutcomes`] says whether any settled publish put
    /// records back into the buffers at an epoch above a given cut's, in
    /// which case that cut must not stamp even though its own flush
    /// succeeded.
    #[must_use]
    pub fn quiesce_publishes(&self) -> PublishOutcomes {
        let mut count = self
            .in_flight
            .count
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        while *count > 0 {
            count = self
                .in_flight
                .settled
                .wait(count)
                .unwrap_or_else(PoisonError::into_inner);
        }
        drop(count);
        PublishOutcomes {
            settlements: self
                .in_flight
                .settlements
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone(),
        }
    }

    /// Discard the settlements a cut of `epoch` has now put permanently
    /// behind it — called once that cut's outcome is known, so the list
    /// cannot grow with the process. An unwind is never discarded: stage
    /// 1 has no way to clear it short of a restart.
    pub fn settle_cut(&self, epoch: Epoch) {
        self.in_flight
            .settlements
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|settlement| !settlement.spent(epoch));
    }

    /// Lock the sink, recovering a poisoned mutex. A poison means a past panic
    /// while a flush was in flight; the buffer + counters remain structurally
    /// consistent (the WAL is the durability of record), so recovering the
    /// inner sink is safer than panicking the ingest path (`CLAUDE.md` §3.4,
    /// and the same posture `receiver` takes on the miner mutex).
    fn lock(&self) -> std::sync::MutexGuard<'_, ParquetRecordSink> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Count a cadence sweep step that panicked.
    ///
    /// Tagged onto the existing flush-error counter rather than given a
    /// metric of its own, per the project's `error.type` convention. The
    /// lock recovers from poisoning, so a panic taken while the sink was
    /// locked still gets counted.
    pub fn record_cadence_panic(&self) {
        self.lock()
            .metrics
            .record_flush_error(Some(crate::metrics::CADENCE_PANIC));
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
    pub fn drain_aged(&self) -> TakenPartitions {
        self.lock().drain_aged()
    }

    /// Atomically take **every** buffered partition as owned batches.
    #[must_use]
    pub fn drain_all(&self) -> TakenPartitions {
        self.lock().drain_all()
    }

    /// Re-buffer owned `batches` (a transient publish failure, or the
    /// coordinator holding records because the audit write failed). The WAL is
    /// the durability of record, so retain + retry on the next cadence.
    ///
    /// The re-entry is dated against the epoch the publish was
    /// `registered` under (RFC 0052 §3.1): a cut captured before it —
    /// one whose drain could not have seen these records — is refused,
    /// and a cut captured after it is not.
    pub fn requeue(&self, batches: Vec<(PartitionKey, Vec<MinedRecord>)>, registered: Epoch) {
        if batches.is_empty() {
            return;
        }
        self.lock().requeue(batches);
        self.note_resettled(registered);
    }

    /// RFC 0052 §3.1's **park**: re-buffer `batches` as `ready`, keeping
    /// the `audit_watermark` their publish depends on, and date the
    /// re-entry like any other settlement.
    pub fn park_ready(
        &self,
        batches: Vec<(PartitionKey, Vec<MinedRecord>)>,
        audit_watermark: u64,
        registered: Epoch,
    ) {
        if batches.is_empty() {
            return;
        }
        self.lock().park_ready(batches, audit_watermark);
        self.note_resettled(registered);
    }

    /// Publish owned `batches` to the data store **off the lock** (the encode +
    /// put runs unlocked; the sink is locked only to read the store handle and
    /// to settle / requeue). A partition whose put fails is requeued (the WAL is
    /// the durability of record). Returns whether every partition was published.
    /// `trigger` labels the flush metric.
    ///
    /// `registered` is the cut epoch the publish holding these records
    /// was registered under, so a requeue here is dated the way RFC 0052
    /// §3.1 requires.
    #[must_use]
    pub fn publish_owned(
        &self,
        batches: Vec<(PartitionKey, Vec<MinedRecord>)>,
        trigger: &'static str,
        registered: Epoch,
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
        self.requeue(requeue, registered);
        all_published
    }

    /// The concurrent-phase emit (RFC 0035 §3.1). Safe to call from many
    /// encode-pool workers at once: the partition key and byte estimate
    /// are derived outside the sink lock, the buffer append is a short
    /// locked section (`ParquetRecordSink::append_off_lock`), and any
    /// size / ceiling-triggered partitions are encoded + published **off
    /// the lock** via [`Self::publish_owned`] (which settles counters,
    /// quarantines poison records, and requeues on transient failure) —
    /// so one worker's Parquet encode never blocks the others' appends.
    pub fn emit_concurrent(&self, record: MinedRecord, registered: Epoch) {
        for (trigger, taken) in self.detach_concurrent(record) {
            if !taken.is_empty() {
                let _ = self.publish_owned(taken.into_partitions(), trigger, registered);
            }
        }
    }

    /// [`Self::emit_concurrent`]'s under-lock half alone: append the
    /// record and return whatever the ceiling and size triggers took,
    /// **unpublished**.
    ///
    /// RFC 0052 §3.1 moves the store I/O off the encode worker, so the
    /// worker hands these to the publisher instead of writing them
    /// itself. The returned partitions are out of the buffers and out of
    /// the byte accounting, so the caller owes them a settlement — a
    /// durable write, a requeue, or a park.
    #[must_use]
    pub fn detach_concurrent(&self, record: MinedRecord) -> [(&'static str, TakenPartitions); 2] {
        let Ok(key) = PartitionKey::derive(&record) else {
            let mut sink = self.lock();
            sink.derive_errors += 1;
            sink.metrics.record_derive_error();
            return [
                ("ceiling", TakenPartitions::default()),
                ("size", TakenPartitions::default()),
            ];
        };
        let est = estimate_bytes(&record);
        let (ceiling_taken, size_taken) = self.lock().append_off_lock(key, est, record);
        [("ceiling", ceiling_taken), ("size", size_taken)]
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
            .or_insert_with(PartitionBuffer::empty);
        buf.records.push(record);
        // Saturating (matching `saturating_sub` on flush) so the byte counters
        // stay monotonic and the triggers can't be corrupted by wraparound
        // under prolonged retention (e.g. a store outage).
        buf.est_bytes = buf.est_bytes.saturating_add(est);
        // RFC 0052 §3.1: a parked `ready` partition is already past the
        // size trigger, so re-firing it would publish inline instead of
        // letting the publisher or the next drain take it oldest-first.
        let over_target = buf.ready.is_none() && buf.est_bytes >= self.config.target_bytes;
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
        assert_eq!(batch.partitions().len(), 1, "one partition drained");

        // A newer record arrives for the same partition during the off-lock
        // publish; on its own it is not aged.
        sink.emit(rec("checkout"));
        assert!(
            sink.drain_aged().is_empty(),
            "the fresh record alone is not aged",
        );

        sink.requeue(batch.into_partitions());

        let retry = sink.drain_aged();
        assert_eq!(
            retry.partitions().len(),
            1,
            "the requeued partition is aged again and re-drains on the next sweep",
        );
        assert_eq!(
            retry.partitions()[0].1.len(),
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

    // --- RFC 0035 §3.1: the concurrent-phase emit. ---

    #[test]
    fn emit_concurrent_size_trigger_publishes_off_lock() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let store = Store::local(dir.path()).expect("local store");
        let sink = SharedParquetSink::new(ParquetRecordSink::new(store, size_trigger_config()));

        for _ in 0..4 {
            sink.emit_concurrent(rec("tenant-a"), sink.epochs().current());
        }

        assert_eq!(sink.buffered_records(), 0, "every emit crossed the target");
        assert_eq!(sink.flushes(), 4, "one size-triggered publish per emit");
    }

    #[test]
    fn emit_concurrent_respects_the_audit_barrier() {
        // The concurrent path must uphold the same issue #302 ordering as
        // `emit`: no partition is published before the audit sink is
        // durable. A failing barrier retains the record.
        let tmp = tempfile::TempDir::new().expect("temp");
        let data_root = tmp.path().join("data");
        std::fs::create_dir_all(&data_root).expect("data root");
        let sink = SharedParquetSink::new(
            ParquetRecordSink::new(
                Store::local(&data_root).expect("data store"),
                size_trigger_config(),
            )
            .with_audit_barrier(Box::new(|| false)),
        );

        sink.emit_concurrent(rec("tenant-a"), sink.epochs().current());

        assert_eq!(
            sink.flushes(),
            0,
            "the size trigger is skipped while the audit sink isn't durable",
        );
        assert_eq!(
            sink.buffered_records(),
            1,
            "the record is retained (the WAL is the durability of record)",
        );
    }
}
