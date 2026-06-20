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
    DEFAULT_ZSTD_LEVEL, PartitionKey, Store, StoreError, WriterError, encode_records_to_parquet,
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
    /// RFC 0014 §6.3 instruments (flush throughput/latency by trigger,
    /// errors, buffer occupancy). No-op when no meter provider is installed.
    metrics: SinkMetrics,
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
            metrics: SinkMetrics::new(),
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
                encode_records_to_parquet(&buf.records, DEFAULT_ZSTD_LEVEL)
                    .map_err(FlushError::Encode)?
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

    /// [`Self::flush_partition`] for the infallible `emit` / tick / rotation
    /// paths: a failed flush retains the buffer (the WAL is the durability of
    /// record) and is counted for observability. `trigger` records *why* the
    /// flush happened (RFC 0014 §3.2).
    fn flush_partition_swallow(&mut self, key: &PartitionKey, trigger: &'static str) {
        if self.flush_partition(key, trigger).is_err() {
            self.flush_errors += 1;
            self.metrics.record_flush_error();
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
            self.metrics.record_flush_error();
            false
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
/// across `encode_records_to_parquet` + `Store::put_blocking` (see
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
        while self.total_bytes.saturating_add(est) > self.config.ceiling_bytes
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

        // Size trigger (RFC0014.1): the emit that crosses the target flushes.
        if over_target {
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
}
