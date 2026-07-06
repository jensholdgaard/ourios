//! RFC 0025 §5 — the sink-owned scenarios: the permanent-encode-error
//! quarantine (`.4`) and its telemetry (`.5`). See
//! `crates/ourios-parquet/tests/rfc0025_absent_body.rs` for the
//! scenario placement map.

use std::time::Duration;

use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData, ResourceMetrics};
use ourios_core::audit::{AuditPayload, EVENT_TYPE_RECORD_QUARANTINED, SharedAuditSink};
use ourios_core::record::{BodyKind, MinedRecord, RecordSink};
use ourios_core::tenant::TenantId;
use ourios_ingester::record_sink::{FlushConfig, ParquetRecordSink};
use ourios_parquet::Store;
use tempfile::TempDir;

/// 2026-04-02T10:58:00Z.
const TS0: u64 = 1_775_127_480_000_000_000;

fn healthy(ts_offset: u64) -> MinedRecord {
    MinedRecord {
        tenant_id: TenantId::new("rfc0025"),
        template_id: 0,
        template_version: 0,
        severity_number: 9,
        severity_text: None,
        scope_name: None,
        scope_version: None,
        scope_attributes: Vec::new(),
        resource_schema_url: None,
        scope_schema_url: None,
        time_unix_nano: TS0 + ts_offset,
        observed_time_unix_nano: None,
        attributes: Vec::new(),
        dropped_attributes_count: 0,
        resource_attributes: Vec::new(),
        trace_id: None,
        span_id: None,
        flags: 0,
        event_name: None,
        body_kind: BodyKind::String,
        params: Vec::new(),
        separators: Vec::new(),
        body: Some("line".to_string()),
        confidence: 0.0,
        lossy_flag: true,
    }
}

/// A permanently-rejected record: `observed_time_unix_nano` past
/// `i64::MAX` trips the RFC 0005 §3.2 timestamp-overflow contract.
/// `time_unix_nano` stays sane so it lands in the same partition as
/// the healthy rows.
fn poisoned() -> MinedRecord {
    MinedRecord {
        observed_time_unix_nano: Some(u64::MAX),
        ..healthy(500)
    }
}

fn never_flush_config() -> FlushConfig {
    FlushConfig {
        target_bytes: usize::MAX,
        max_buffer_age: Duration::from_secs(3600),
        ceiling_bytes: usize::MAX,
    }
}

/// Scenario RFC0025.4 — the sink no longer wedges.
/// See `docs/rfcs/0025-absent-body-representation.md` §5.
#[test]
fn rfc0025_4_sink_quarantines_instead_of_wedging() {
    let bucket = TempDir::new().expect("temp dir");
    let audit = SharedAuditSink::new();
    let mut sink = ParquetRecordSink::new(
        Store::local(bucket.path()).expect("local store"),
        never_flush_config(),
    )
    .with_audit_sink(Box::new(audit.clone()));

    sink.emit(healthy(1_000));
    sink.emit(poisoned());
    sink.emit(healthy(2_000));
    sink.flush_all();

    // The healthy records persisted: exactly one data object exists
    // and holds two rows.
    let files: Vec<_> = walk_parquet(bucket.path());
    assert_eq!(files.len(), 1, "one partition object: {files:?}");
    let rows = read_rows(&files[0]);
    assert_eq!(rows.len(), 2, "the two healthy rows persisted");
    assert!(
        rows.iter().all(|r| r.observed_time_unix_nano.is_none()),
        "the poisoned record is not among them",
    );

    // The poisoned record is quarantined to the audit stream.
    let events = audit.drain();
    assert_eq!(events.len(), 1, "one quarantine event");
    match &events[0].payload {
        AuditPayload::RecordQuarantined { partition, error } => {
            assert_eq!(
                events[0].payload.event_type(),
                EVENT_TYPE_RECORD_QUARANTINED
            );
            assert!(
                partition.starts_with("year=2026/month=04/day=02/hour=10"),
                "partition key names the wedged partition: {partition}",
            );
            assert!(
                error.contains("i64::MAX"),
                "the error text explains the permanence: {error}",
            );
        }
        other => panic!("expected RecordQuarantined, got {other:?}"),
    }

    // Subsequent flushes of the partition succeed: a new record
    // buffers and publishes without error.
    let flushes_before = sink.flushes();
    sink.emit(healthy(3_000));
    sink.flush_all();
    assert_eq!(
        sink.flushes(),
        flushes_before + 1,
        "the partition flushes normally after quarantine",
    );
    assert_eq!(walk_parquet(bucket.path()).len(), 2, "second object landed");
}

/// The cadence-drain path (RFC 0025 §3.3's "both routes"): a poison
/// record in an owned batch quarantines instead of requeueing forever
/// — the #362 wedge through the second door.
#[test]
fn cadence_drain_publish_quarantines_instead_of_requeueing() {
    use ourios_ingester::record_sink::SharedParquetSink;

    let bucket = TempDir::new().expect("temp dir");
    let audit = SharedAuditSink::new();
    let shared = SharedParquetSink::new(
        ParquetRecordSink::new(
            Store::local(bucket.path()).expect("local store"),
            never_flush_config(),
        )
        .with_audit_sink(Box::new(audit.clone())),
    );

    let key = ourios_parquet::PartitionKey::derive(&healthy(1_000)).expect("derive");
    let all_published = shared.publish_owned(
        vec![(key, vec![healthy(1_000), poisoned(), healthy(2_000)])],
        "cadence",
    );

    assert!(all_published, "the remainder publishes — nothing requeues");
    assert_eq!(
        shared.buffered_records(),
        0,
        "no requeue: the poison is quarantined, not re-buffered",
    );
    let files = walk_parquet(bucket.path());
    assert_eq!(files.len(), 1, "one object landed");
    assert_eq!(read_rows(&files[0]).len(), 2, "the two healthy rows");
    let events = audit.drain();
    assert_eq!(events.len(), 1, "one quarantine event via the cadence path");
    assert!(matches!(
        events[0].payload,
        AuditPayload::RecordQuarantined { .. }
    ));
}

/// The all-poison edge: quarantining every record must release the
/// buffer entry and its byte accounting (no permanent gauge drift),
/// and the partition must keep working afterward.
#[test]
fn all_poison_buffer_releases_its_accounting() {
    let bucket = TempDir::new().expect("temp dir");
    let audit = SharedAuditSink::new();
    let mut sink = ParquetRecordSink::new(
        Store::local(bucket.path()).expect("local store"),
        never_flush_config(),
    )
    .with_audit_sink(Box::new(audit.clone()));

    sink.emit(poisoned());
    sink.flush_all();

    assert_eq!(audit.drain().len(), 1, "the record quarantined");
    assert!(walk_parquet(bucket.path()).is_empty(), "nothing published");
    assert_eq!(sink.buffered_partitions(), 0, "the emptied entry is gone");
    assert_eq!(sink.buffered_bytes(), 0, "byte accounting released");

    sink.emit(healthy(1_000));
    sink.flush_all();
    assert_eq!(
        walk_parquet(bucket.path()).len(),
        1,
        "partition works after"
    );
}

/// Scenario RFC0025.5 — quarantine telemetry.
/// See `docs/rfcs/0025-absent-body-representation.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn rfc0025_5_quarantine_telemetry() {
    // The in-memory provider must exist before the sink builds its
    // instruments.
    let (guard, exporter) = ourios_telemetry::init_in_memory("ourios-test");
    let bucket = TempDir::new().expect("temp dir");
    let mut sink = ParquetRecordSink::new(
        Store::local(bucket.path()).expect("local store"),
        never_flush_config(),
    );

    sink.emit(poisoned());
    sink.flush_all();
    guard.force_flush().expect("force_flush");

    // The existing flush-error counter carries the rejection with
    // `error.type` — no new metric name (OTel recording-errors).
    let rms = exporter.get_finished_metrics().expect("metrics exported");
    let counted: u64 = rms
        .iter()
        .flat_map(ResourceMetrics::scope_metrics)
        .flat_map(opentelemetry_sdk::metrics::data::ScopeMetrics::metrics)
        .filter(|m| m.name() == ourios_semconv::OURIOS_SINK_FLUSH_ERRORS)
        .filter_map(|m| match m.data() {
            AggregatedMetrics::U64(MetricData::Sum(sum)) => Some(
                sum.data_points()
                    .filter(|dp| {
                        dp.attributes().any(|kv| {
                            kv.key.as_str() == "error.type"
                                && kv.value.as_str() == "timestamp_overflow"
                        })
                    })
                    .map(opentelemetry_sdk::metrics::data::SumDataPoint::value)
                    .sum::<u64>(),
            ),
            _ => None,
        })
        .sum();
    assert!(
        counted >= 1,
        "flush-error counter must carry error.type=timestamp_overflow",
    );
}

fn walk_parquet(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("read_dir") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "parquet") {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

fn read_rows(file: &std::path::Path) -> Vec<MinedRecord> {
    let partition = ourios_parquet::PartitionKey::derive(&healthy(1_000)).expect("derive");
    ourios_parquet::Reader::open_partition(file, partition)
        .expect("open_partition")
        .read_all()
        .expect("read_all")
}
