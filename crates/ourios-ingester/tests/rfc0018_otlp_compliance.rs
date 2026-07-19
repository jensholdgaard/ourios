//! RFC 0018 — OTLP log-spec compliance acceptance scenarios (§5), the
//! receiver arms.
//!
//! `.1` (receiver half): the receiver materialises `InstrumentationScope`
//! attributes and the per-resource / per-scope `schema_url` from the wire.
//! `.6`: out-of-range severity is preserved + tagged via `error.type`. The
//! storage round-trip + back-compat halves of `.1`/`.2` live in
//! `ourios-parquet/tests/rfc0018_otlp_compliance.rs` (the Writer/Reader
//! harness); `.3` (retryable error mapping) in `tests/rfc0018_retryable.rs`;
//! `.6`'s monotonicity arm in `ourios-querier/tests/rfc0018_severity.rs`.
//!
//! See `docs/rfcs/0018-otlp-log-spec-compliance.md` §5/§6.

use std::time::Duration;

use opentelemetry_proto::tonic::common::v1::any_value::Value;
use opentelemetry_proto::tonic::common::v1::{AnyValue, InstrumentationScope, KeyValue};
use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
use opentelemetry_proto::tonic::resource::v1::Resource;
use opentelemetry_sdk::metrics::data::{
    AggregatedMetrics, MetricData, ResourceMetrics, SumDataPoint,
};
use ourios_core::tenant::TenantId;
use ourios_ingester::metrics::IngestMetrics;
use ourios_ingester::receiver::{materialize_record, materialize_resource_logs};
use ourios_semconv as semconv;

fn kv(key: &str, value: &str) -> KeyValue {
    KeyValue {
        key: key.to_owned(),
        value: Some(AnyValue {
            value: Some(Value::StringValue(value.to_owned())),
        }),
        ..Default::default()
    }
}

/// Scenario RFC0018.1 — scope attributes + schema URLs survive ingest
/// (receiver half): a batch whose `InstrumentationScope` carries
/// `attributes`, whose `ScopeLogs` carries a `schema_url`, and whose
/// `ResourceLogs` carries a `schema_url` materialises into an
/// `OtlpLogRecord` carrying all three (the storage round-trip is proven in
/// `ourios-parquet/tests/rfc0018_otlp_compliance.rs`).
/// See `docs/rfcs/0018-otlp-log-spec-compliance.md` §5.
#[test]
fn rfc0018_1_receiver_materialises_scope_fields() {
    let resource_logs = ResourceLogs {
        resource: Some(Resource {
            attributes: vec![kv("service.name", "checkout")],
            ..Default::default()
        }),
        schema_url: "https://opentelemetry.io/schemas/1.31.0".to_owned(),
        scope_logs: vec![ScopeLogs {
            scope: Some(InstrumentationScope {
                name: "lib.cart".to_owned(),
                version: "1.0.0".to_owned(),
                attributes: vec![kv("db.system", "postgres")],
                ..Default::default()
            }),
            schema_url: "https://opentelemetry.io/schemas/1.0.0".to_owned(),
            log_records: vec![LogRecord::default()],
        }],
    };

    let materialized = materialize_resource_logs(resource_logs, &TenantId::new("tenant-a"));

    assert_eq!(materialized.len(), 1, "one record materialised");
    let r = &materialized[0];
    assert_eq!(
        r.scope_attributes,
        vec![kv("db.system", "postgres")],
        "scope.attributes carried from the wire"
    );
    assert_eq!(
        r.scope_schema_url.as_deref(),
        Some("https://opentelemetry.io/schemas/1.0.0"),
        "ScopeLogs.schema_url carried"
    );
    assert_eq!(
        r.resource_schema_url.as_deref(),
        Some("https://opentelemetry.io/schemas/1.31.0"),
        "ResourceLogs.schema_url carried"
    );
}

/// Sum of `ourios.ingest.records` datapoints, filtered by `error.type`:
/// `None` → success points (the attribute absent); `Some(v)` → points whose
/// `error.type` equals `v`.
fn ingest_records_sum(rms: &[ResourceMetrics], error_type: Option<&str>) -> u64 {
    let data = rms
        .iter()
        .flat_map(ResourceMetrics::scope_metrics)
        .flat_map(opentelemetry_sdk::metrics::data::ScopeMetrics::metrics)
        .find(|m| m.name() == semconv::OURIOS_INGEST_RECORDS)
        .map(opentelemetry_sdk::metrics::data::Metric::data)
        .expect("ourios.ingest.records exported");
    let AggregatedMetrics::U64(MetricData::Sum(sum)) = data else {
        panic!("ourios.ingest.records should be a u64 sum");
    };
    sum.data_points()
        .filter(|dp| {
            let et = dp
                .attributes()
                .find(|kv| kv.key.as_str() == "error.type")
                .map(|kv| kv.value.as_str().into_owned());
            match error_type {
                None => et.is_none(),
                Some(v) => et.as_deref() == Some(v),
            }
        })
        .map(SumDataPoint::value)
        .sum()
}

/// Scenario RFC0018.6 — out-of-range `SeverityNumber` is preserved, not clamped:
/// the receiver preserves `25` / `200` verbatim (non-`u8` → `0`), and the
/// `ourios.ingest.records` counter records out-of-range records with
/// `error.type = severity_out_of_range` (in-range ones carry no `error.type`).
/// Monotonicity (`severity >= ERROR` still matches the preserved `25`) is the
/// querier's `SeverityNumber` comparison — covered in
/// `ourios-querier/tests/rfc0018_severity.rs`.
/// See `docs/rfcs/0018-otlp-log-spec-compliance.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn rfc0018_6_out_of_range_severity_preserved() {
    // Receiver: preserve the wire value (non-u8 extremes narrow to 0).
    for (wire, expected) in [(25i32, 25u8), (200, 200), (1000, 0), (-5, 0)] {
        let m = materialize_record(
            LogRecord {
                severity_number: wire,
                ..Default::default()
            },
            &[],
            "",
            None,
            "",
            TenantId::new("tenant-a"),
        );
        assert_eq!(
            m.severity_number, expected,
            "severity {wire} preserved as {expected} (non-u8 → 0)",
        );
    }

    // Metric: a 4-record batch, 2 out-of-range, splits onto the records
    // counter via error.type (OTel "recording errors on metrics" convention).
    let (guard, exporter) = ourios_telemetry::init_in_memory("ourios-test-rfc0018-6");
    let ingest = IngestMetrics::new();
    ingest.record_batch(4, 2, false, Duration::from_millis(1));
    guard.force_flush().expect("force_flush");
    let rms = exporter.get_finished_metrics().expect("metrics exported");

    assert_eq!(
        ingest_records_sum(&rms, None),
        2,
        "in-range records carry no error.type",
    );
    assert_eq!(
        ingest_records_sum(&rms, Some("severity_out_of_range")),
        2,
        "out-of-range records tagged error.type = severity_out_of_range",
    );

    // A lenient-JSON batch (ourios#549) lands on the batches counter
    // with the registry's `ourios.ingest.json.lenient` attribute — one
    // instrument, path on an attribute, the same shape as error.type.
    let (guard, exporter) = ourios_telemetry::init_in_memory("ourios-test-rfc0018-6b");
    let ingest = IngestMetrics::new();
    ingest.record_batch(1, 0, true, Duration::from_millis(1));
    ingest.record_batch(1, 0, false, Duration::from_millis(1));
    guard.force_flush().expect("force_flush");
    let rms = exporter.get_finished_metrics().expect("metrics exported");
    let batches = rms
        .iter()
        .flat_map(ResourceMetrics::scope_metrics)
        .flat_map(opentelemetry_sdk::metrics::data::ScopeMetrics::metrics)
        .find(|m| m.name() == semconv::OURIOS_INGEST_BATCHES)
        .map(opentelemetry_sdk::metrics::data::Metric::data)
        .expect("ourios.ingest.batches exported");
    let AggregatedMetrics::U64(MetricData::Sum(sum)) = batches else {
        panic!("ourios.ingest.batches should be a u64 sum");
    };
    let lenient_total: u64 = sum
        .data_points()
        .filter(|dp| {
            dp.attributes()
                .any(|kv| kv.key.as_str() == semconv::OURIOS_INGEST_JSON_LENIENT)
        })
        .map(SumDataPoint::value)
        .sum();
    let direct_total: u64 = sum
        .data_points()
        .filter(|dp| {
            !dp.attributes()
                .any(|kv| kv.key.as_str() == semconv::OURIOS_INGEST_JSON_LENIENT)
        })
        .map(SumDataPoint::value)
        .sum();
    assert_eq!(lenient_total, 1, "the lenient batch carries the attribute");
    assert_eq!(direct_total, 1, "the direct batch stays attribute-free");
}
