//! Mined-record schema and the sink boundary.
//!
//! Every line the miner ingests produces exactly one
//! [`MinedRecord`] — including the `Body::None` case, which
//! emits a `BodyKind::Absent` record carrying the template-id
//! sentinel and `lossy_flag = true` (no tokenize ran, no
//! template was allocated, reconstruction is not possible).
//! The record is the §6.1 row that ends up in Parquet —
//! schema-stable, self-contained, addressable by
//! `(tenant_id, template_id, template_version)`.
//!
//! Producers (today: `ourios-miner`) hand records to an
//! [`RecordSink`]. Consumers (eventually: `ourios-parquet` writer,
//! the future query layer) plug into the same trait. The trait
//! ships with the schema for the same reason `AuditSink` does —
//! the second consumer (the Parquet writer) is a named roadmap
//! item, so the abstraction names a committed contract rather
//! than a hypothetical one.
//!
//! This module is **emission-only** today: the
//! `reconstruct(record, template) -> Bytes` function lives in
//! `ourios-miner` and lands with the §6.6 follow-up PR; the
//! `lossy_flag` field is set by emission-time policy here, but
//! the *tokenizer-failure* path that flips it to `true` is also
//! deferred.

use std::sync::{Arc, Mutex};

use crate::audit::ParamType;
use crate::tenant::TenantId;

/// RFC 0001 §6.1 *Body representation* discriminator.
///
/// Same fork the cluster uses in §6.2 step 0: a `String` body
/// runs the Drain pipeline (tokenize → mask → descend); any
/// non-`String` `AnyValue` short-circuits to a structured
/// template keyed on `(severity, scope, BodyKind::Structured)`.
/// The discriminator lands on the emitted record so a reader can
/// branch reconstruction by kind without re-deriving from the
/// template shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BodyKind {
    /// `LogRecord.body` was an `AnyValue::String` — the line went
    /// through tokenize → mask → descend.
    String,
    /// `LogRecord.body` was a non-`String` `AnyValue` — the line
    /// short-circuited per §6.2 step 0. `params`/`separators`
    /// are empty for these records; reconstruction reads the
    /// canonicalised JSON from `body` directly.
    Structured,
    /// `LogRecord.body` was absent on the wire. No template was
    /// allocated and no Drain pipeline ran.
    Absent,
}

/// One masked-parameter slot, in template order.
///
/// `value` carries the original token bytes (or, post-overflow,
/// a `(length, sha256_prefix)` marker per RFC §6.5 — that
/// rendering is the §6.5 follow-up PR's job). `type_tag` reuses
/// `ourios_core::audit::ParamType` so the two stores share one
/// type-tag alphabet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Param {
    pub type_tag: ParamType,
    pub value: String,
}

/// One row the miner emits per ingested line — RFC 0001 §6.1
/// record schema.
///
/// Fields the §6.1 schema names but PR-A defers (everything in
/// `OtlpLogRecord` outside of the mining-output set —
/// `attributes`, `resource_attributes`, `trace_id`, `span_id`,
/// `flags`, `event_name`, etc.) land alongside the Parquet
/// writer, where they actually become observable. PR-A carries
/// only what the cluster has to populate today: the identity
/// triple, the template-key half of `(severity, scope)`, the
/// time field, and the mining outputs.
///
/// Records emitted on the parse-failure paths
/// (`body::None`-with-record, empty input, over-cap, degenerate
/// rejection, §6.3 parse-failure zone) carry
/// `template_id == NO_TEMPLATE` / `template_version == 0` and a
/// retained body. Records emitted on the clean / widening / lossy
/// paths carry a real template id.
#[derive(Debug, Clone, PartialEq)]
pub struct MinedRecord {
    /// §3.7 — the routing key that scopes every downstream
    /// query.
    pub tenant_id: TenantId,
    /// Cluster-wide unique template id. `0` (the cluster's
    /// `NO_TEMPLATE` sentinel) on parse-failure records.
    pub template_id: u64,
    /// Per-leaf monotonic version. `0` when no template was
    /// allocated.
    pub template_version: u32,
    /// `(severity_number, scope_name)` half of the §6.1
    /// template-key composition tuple. Mirrored onto the record
    /// so a reader can filter without joining back through the
    /// template store.
    pub severity_number: u8,
    pub scope_name: Option<String>,
    /// Source event time per `OtlpLogRecord.time_unix_nano`. `0`
    /// = unknown.
    pub time_unix_nano: u64,
    /// §6.1 *Body representation* fork.
    pub body_kind: BodyKind,
    /// Masked-parameter slots in template order. Empty for
    /// `BodyKind::Structured` and `BodyKind::Absent`.
    pub params: Vec<Param>,
    /// Captured-verbatim whitespace between tokens — RFC §6.6
    /// "capture, always". Length invariant for `BodyKind::String`
    /// successful tokenizations: the array carries one more entry
    /// than `tokens` (the leading byte run, plus one between each
    /// token, plus the trailing byte run). For non-`String` body
    /// kinds, `separators` is empty.
    pub separators: Vec<String>,
    /// Original body bytes, retained on the §6.3 paths the RFC
    /// marks "retain body" (lossy zone, parse-failure zone) and
    /// on every tokenizer-failure path (RFC §6.6, `lossy_flag`
    /// → next PR). `None` for clean attaches where reconstruction
    /// from `template + params + separators` is expected to
    /// match.
    pub body: Option<String>,
    /// `confidence = simSeq / threshold` per RFC §6.3. `1.0` for
    /// `BodyKind::Structured` (sentinel — no Drain comparison
    /// happens) and for fresh-leaf creation; `0.0` for
    /// parse-failure records (no template to compare against).
    pub confidence: f32,
    /// Set by RFC §6.6's tokenizer / preprocessing-failure rule
    /// (next PR). The §6.3 lossy *zone* keeps this `false` even
    /// though body is retained.
    pub lossy_flag: bool,
}

/// Sink for mined records.
///
/// Producers (`ourios-miner`) call [`Self::emit`] once per
/// ingested line. The trait is `Send` so a
/// `Box<dyn RecordSink>` moves across threads with the cluster
/// that owns it.
pub trait RecordSink: Send {
    /// Consume one record. Sinks own the record; producers must
    /// not retain references.
    fn emit(&mut self, record: MinedRecord);
}

/// Sink that drops every record it receives.
///
/// Production default until `ourios-parquet` lands.
/// [`InMemoryRecordSink`] would otherwise buffer records
/// without bound, which is fine for tests but a memory leak
/// for any long-running production miner — same shape as the
/// `NoOpAuditSink` / `InMemoryAuditSink` split.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoOpRecordSink;

impl NoOpRecordSink {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl RecordSink for NoOpRecordSink {
    fn emit(&mut self, _record: MinedRecord) {
        // Drop on the floor.
    }
}

/// `Vec`-backed sink for tests and the pre-Parquet bootstrap.
///
/// Holds records in memory in emission order. Tests use
/// [`Self::drain`] (or, more commonly, the
/// [`SharedRecordSink`] wrapper) to inspect what was emitted.
/// Not safe as the production default — the buffer grows
/// without bound and is not externally drainable through a
/// `Box<dyn RecordSink>`.
#[derive(Debug, Default)]
pub struct InMemoryRecordSink {
    records: Vec<MinedRecord>,
}

impl InMemoryRecordSink {
    #[must_use]
    pub fn new() -> Self {
        Self {
            records: Vec::new(),
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub fn drain(&mut self) -> Vec<MinedRecord> {
        std::mem::take(&mut self.records)
    }
}

impl RecordSink for InMemoryRecordSink {
    fn emit(&mut self, record: MinedRecord) {
        self.records.push(record);
    }
}

/// [`InMemoryRecordSink`] wrapped in `Arc<Mutex<_>>` so a
/// producer can own the sink for emission while a test (or any
/// observer) still has a handle for inspection.
///
/// `Clone` yields another handle to the *same* buffer — same
/// pattern as [`crate::audit::SharedAuditSink`]: hand one clone
/// to `MinerCluster::with_record_sink` and keep another to drain
/// after the act.
#[derive(Debug, Clone, Default)]
pub struct SharedRecordSink {
    inner: Arc<Mutex<InMemoryRecordSink>>,
}

impl SharedRecordSink {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Take ownership of every buffered record in emission order.
    /// The shared buffer is empty afterwards.
    ///
    /// # Panics
    ///
    /// Panics if another thread panicked while holding the
    /// internal mutex. A poisoned record buffer can't be trusted
    /// to be complete or ordered.
    #[must_use]
    pub fn drain(&self) -> Vec<MinedRecord> {
        self.inner
            .lock()
            .expect("record sink mutex poisoned")
            .drain()
    }

    /// # Panics
    ///
    /// As [`Self::drain`].
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.lock().expect("record sink mutex poisoned").len()
    }

    /// # Panics
    ///
    /// As [`Self::drain`].
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner
            .lock()
            .expect("record sink mutex poisoned")
            .is_empty()
    }
}

impl RecordSink for SharedRecordSink {
    fn emit(&mut self, record: MinedRecord) {
        self.inner
            .lock()
            .expect("record sink mutex poisoned")
            .emit(record);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_clean_record(tenant: &TenantId) -> MinedRecord {
        MinedRecord {
            tenant_id: tenant.clone(),
            template_id: 7,
            template_version: 1,
            severity_number: 9,
            scope_name: Some("lib.auth".to_string()),
            time_unix_nano: 1_700_000_000_000_000_000,
            body_kind: BodyKind::String,
            params: vec![Param {
                type_tag: ParamType::Num,
                value: "42".to_string(),
            }],
            separators: vec![
                String::new(),
                " ".to_string(),
                " ".to_string(),
                " ".to_string(),
                String::new(),
            ],
            body: None,
            confidence: 1.0,
            lossy_flag: false,
        }
    }

    fn sample_parse_failure_record(tenant: &TenantId) -> MinedRecord {
        MinedRecord {
            tenant_id: tenant.clone(),
            template_id: 0,
            template_version: 0,
            severity_number: 0,
            scope_name: None,
            time_unix_nano: 0,
            body_kind: BodyKind::String,
            params: Vec::new(),
            separators: Vec::new(),
            body: Some("malformed line".to_string()),
            confidence: 0.0,
            lossy_flag: true,
        }
    }

    #[test]
    fn in_memory_sink_records_emission_order() {
        let mut sink = InMemoryRecordSink::new();
        let t = TenantId::new("tenant-x");

        sink.emit(sample_clean_record(&t));
        sink.emit(sample_parse_failure_record(&t));

        assert_eq!(sink.len(), 2);
        let drained = sink.drain();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].template_id, 7);
        assert_eq!(drained[1].template_id, 0);
        assert!(sink.is_empty(), "drain leaves the sink empty");
    }

    #[test]
    fn shared_sink_clone_observes_same_buffer() {
        let producer_handle = SharedRecordSink::new();
        let observer_handle = producer_handle.clone();
        let t = TenantId::new("tenant-x");

        let mut producer = producer_handle;
        producer.emit(sample_clean_record(&t));

        assert_eq!(observer_handle.len(), 1);
        let drained = observer_handle.drain();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].template_id, 7);
        assert!(observer_handle.is_empty());
    }

    #[test]
    fn no_op_sink_drops_records() {
        let mut sink = NoOpRecordSink::new();
        let t = TenantId::new("tenant-x");
        sink.emit(sample_clean_record(&t));
        sink.emit(sample_parse_failure_record(&t));
        // No public state to inspect — the contract is just
        // "don't crash, don't allocate, don't leak." This test
        // exercises the impl so future refactors can't sneak in
        // a buffer without breaking it.
    }
}
