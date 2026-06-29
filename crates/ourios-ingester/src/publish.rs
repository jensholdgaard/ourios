//! Audit-ordered publication coordinator (issue #302 fix #1/#2).
//!
//! The cross-cutting invariant: **a mined record must never become
//! query-visible (published to the data store) before its template's audit
//! event is durable in the audit stream** (`CLAUDE.md` §3.3, strengthened to
//! hold under concurrency and the inline size trigger).
//!
//! The miner emits a line's template audit event *and* its record within one
//! `MinerCluster::ingest` call (audit event first), under the pipeline's miner
//! lock. This coordinator owns clones of both sinks and enforces the invariant
//! on the cadence path via **snapshot-then-ordered-write**:
//!
//! 1. [`PublishCoordinator::drain_aged`] takes both buffers into owned batches —
//!    a cheap memory move, **no I/O**. The caller runs it under the pipeline's
//!    miner lock (`with_miner`), so the drain is atomic w.r.t. `ingest`: every
//!    record in the drained record batch has its template event in the drained
//!    audit batch (or already durable from a prior cadence) — no record/audit
//!    pair can be split across the drain (closes the TOCTOU race, issue #302 #1).
//! 2. [`PublishCoordinator::write_ordered`] runs **off the lock**: it writes the
//!    audit batch to durability *first*, and publishes the record batch to the
//!    data store **only after** the audit write succeeds. A transient audit
//!    failure holds the records (requeued, retried next cadence); a permanent
//!    audit failure drops the audit batch and still publishes the records — the
//!    documented degraded case (those templates render retained/empty).
//!
//! Rotation / shutdown already drain audit-before-record under the miner lock
//! (the receiver's `flush_then_snapshot`), and the inline size/ceiling publish
//! is gated by the record sink's audit barrier (also under the miner lock), so
//! every publication path is audit-ordered.

use ourios_core::audit::AuditEvent;
use ourios_core::record::MinedRecord;
use ourios_parquet::PartitionKey;

use crate::audit_sink::SharedParquetAuditSink;
use crate::record_sink::SharedParquetSink;

/// An atomic snapshot of both sinks' buffers, taken under the miner lock and
/// written off-lock by [`PublishCoordinator::write_ordered`].
#[derive(Debug)]
pub struct Drained {
    audit: Vec<AuditEvent>,
    records: Vec<(PartitionKey, Vec<MinedRecord>)>,
}

impl Drained {
    /// Whether the snapshot holds nothing to write (both buffers were empty).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.audit.is_empty() && self.records.is_empty()
    }
}

/// Coordinates audit-ordered publication across the record + audit sinks
/// (issue #302). Cloneable: every clone drives the same two sinks.
#[derive(Clone)]
pub struct PublishCoordinator {
    record: SharedParquetSink,
    audit: SharedParquetAuditSink,
}

impl PublishCoordinator {
    /// Build a coordinator over the two shared sinks.
    #[must_use]
    pub fn new(record: SharedParquetSink, audit: SharedParquetAuditSink) -> Self {
        Self { record, audit }
    }

    /// Atomically take the audit buffer + the aged record partitions (the
    /// cadence drain). **Must be called under the pipeline's miner lock** so the
    /// drain is atomic w.r.t. `ingest`; it does only cheap memory moves, never
    /// I/O, so the lock is held for microseconds.
    #[must_use]
    pub fn drain_aged(&self) -> Drained {
        let audit = self.audit.take_buffer();
        let records = self.record.drain_aged();
        Drained { audit, records }
    }

    /// Atomically take the audit buffer + **all** record partitions (rotation /
    /// shutdown). Same atomicity contract as [`Self::drain_aged`].
    #[must_use]
    pub fn drain_all(&self) -> Drained {
        let audit = self.audit.take_buffer();
        let records = self.record.drain_all();
        Drained { audit, records }
    }

    /// Write a `drained` snapshot to durability **off the lock**, audit-first:
    /// the audit batch is written before any record partition is published, so
    /// a record never reaches the store before its template event is durable.
    ///
    /// A transient audit failure (events retained for retry) holds the records —
    /// they are requeued, not published — and returns `false`. A permanent audit
    /// failure (malformed content dropped, [`crate::audit_sink`]) does not block:
    /// the records publish (degraded — those templates render retained/empty).
    /// Returns whether everything was published (no transient retention on
    /// either sink) — the caller's snapshot-gating signal (no-loss, §3.4).
    #[must_use]
    pub fn write_ordered(&self, drained: Drained, trigger: &'static str) -> bool {
        let audit_durable = self.audit.write_owned(drained.audit);
        if !audit_durable {
            // The audit stream didn't fully reach durability (a transient store
            // error). Do NOT publish the records — their template events aren't
            // durable yet. Requeue them for the next cadence (the WAL is the
            // durability of record).
            self.record.requeue(drained.records);
            return false;
        }
        self.record.publish_owned(drained.records, trigger)
    }

    /// The record sink handle (for the receiver's existing flush/snapshot paths).
    #[must_use]
    pub fn record(&self) -> &SharedParquetSink {
        &self.record
    }

    /// The audit sink handle.
    #[must_use]
    pub fn audit(&self) -> &SharedParquetAuditSink {
        &self.audit
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::{Duration, UNIX_EPOCH};

    use ourios_core::audit::{
        AuditEvent, AuditPayload, AuditSink, ParamType, TemplateChange, hash_triggering_line,
    };
    use ourios_core::record::{BodyKind, MinedRecord, Param, RecordSink};
    use ourios_core::tenant::TenantId;
    use ourios_parquet::Store;

    use super::PublishCoordinator;
    use crate::audit_sink::{BufferingAuditSink, SharedParquetAuditSink};
    use crate::record_sink::{FlushConfig, ParquetRecordSink, SharedParquetSink};

    fn never_age() -> FlushConfig {
        FlushConfig {
            target_bytes: usize::MAX,
            max_buffer_age: Duration::ZERO, // every partition is "aged" → drained
            ceiling_bytes: usize::MAX,
        }
    }

    fn mined(tenant: &str) -> MinedRecord {
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

    fn created_event(tenant: &str) -> AuditEvent {
        AuditEvent {
            tenant_id: TenantId::new(tenant),
            timestamp: UNIX_EPOCH + Duration::from_secs(1_775_127_480),
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

    fn data_files(root: &Path) -> Vec<std::path::PathBuf> {
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

    #[test]
    fn write_ordered_holds_records_when_audit_write_fails() {
        // The invariant under a transient audit-store failure: the record
        // partition is NOT published even though the record store is healthy,
        // because its template event could not reach durability.
        let tmp = tempfile::TempDir::new().expect("temp");
        let data_root = tmp.path().join("data");
        let audit_root = tmp.path().join("audit");
        std::fs::create_dir_all(&data_root).expect("data root");
        std::fs::create_dir_all(&audit_root).expect("audit root");

        let records = SharedParquetSink::new(ParquetRecordSink::new(
            Store::local(&data_root).expect("data store"),
            never_age(),
        ));
        let audit = SharedParquetAuditSink::new(BufferingAuditSink::new(
            Store::local(&audit_root).expect("audit store"),
            1024,
            4096,
        ));
        let coord = PublishCoordinator::new(records.clone(), audit.clone());

        // Buffer one record + its template event.
        records.clone().emit(mined("checkout"));
        audit.clone().emit(created_event("checkout"));

        // Sabotage the AUDIT store so its write fails transiently; the data
        // store stays healthy.
        std::fs::remove_dir_all(&audit_root).expect("remove audit dir");
        std::fs::write(&audit_root, b"not a directory").expect("sabotage audit");

        // Drain atomically, then ordered write.
        let drained = coord.drain_aged();
        assert!(!drained.is_empty(), "the snapshot captured both buffers");
        let published = coord.write_ordered(drained, "age");

        assert!(!published, "a transient audit failure holds the records");
        assert!(
            data_files(&data_root).is_empty(),
            "NO record partition was published while the audit event isn't durable (issue #302)",
        );
        assert_eq!(
            records.buffered_records(),
            1,
            "the record is requeued (the WAL is the durability of record)",
        );
        assert_eq!(
            audit.buffered_events(),
            1,
            "the audit event is retained too"
        );
    }

    #[test]
    fn write_ordered_publishes_audit_then_records_when_healthy() {
        let tmp = tempfile::TempDir::new().expect("temp");
        let data_root = tmp.path().join("data");
        let audit_root = tmp.path().join("audit");
        std::fs::create_dir_all(&data_root).expect("data root");
        std::fs::create_dir_all(&audit_root).expect("audit root");

        let records = SharedParquetSink::new(ParquetRecordSink::new(
            Store::local(&data_root).expect("data store"),
            never_age(),
        ));
        let audit = SharedParquetAuditSink::new(BufferingAuditSink::new(
            Store::local(&audit_root).expect("audit store"),
            1024,
            4096,
        ));
        let coord = PublishCoordinator::new(records.clone(), audit.clone());

        records.clone().emit(mined("checkout"));
        audit.clone().emit(created_event("checkout"));

        let published = coord.write_ordered(coord.drain_aged(), "age");

        assert!(published, "a healthy store publishes everything");
        assert_eq!(records.buffered_records(), 0, "records drained");
        assert_eq!(audit.buffered_events(), 0, "audit drained");
        assert!(
            !data_files(&data_root).is_empty(),
            "the record partition is published once its audit event is durable",
        );
    }
}
