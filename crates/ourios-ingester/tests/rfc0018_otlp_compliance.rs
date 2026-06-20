//! RFC 0018 — OTLP log-spec compliance acceptance scenarios (§5), the
//! receiver arms.
//!
//! `.1` (receiver half): the receiver materialises `InstrumentationScope`
//! attributes and the per-resource / per-scope `schema_url` from the wire.
//! The storage round-trip + back-compat halves of `.1`/`.2` live in
//! `ourios-parquet/tests/rfc0018_otlp_compliance.rs` (the Writer/Reader
//! harness). `.3` (retryable error mapping) and `.6` (severity preserve)
//! remain `#[ignore]`d stubs until those receiver changes land (`green`).
//!
//! See `docs/rfcs/0018-otlp-log-spec-compliance.md` §5/§6.

use opentelemetry_proto::tonic::common::v1::any_value::Value;
use opentelemetry_proto::tonic::common::v1::{AnyValue, InstrumentationScope, KeyValue};
use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
use opentelemetry_proto::tonic::resource::v1::Resource;
use ourios_core::tenant::TenantId;
use ourios_ingester::receiver::materialize_resource_logs;

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

/// Scenario RFC0018.3 — transient ingest failure is reported retryable: a WAL
/// append/fsync failure yields a retryable gRPC code (UNAVAILABLE, or
/// `RESOURCE_EXHAUSTED` + `RetryInfo`) and HTTP 503/429 — never `INTERNAL`/500; a
/// permanent failure still maps to `INVALID_ARGUMENT`/400.
/// See `docs/rfcs/0018-otlp-log-spec-compliance.md` §5.
#[test]
#[ignore = "RFC0018.3 — red until the transient-vs-permanent error mapping lands (green)"]
fn rfc0018_3_transient_failure_is_retryable() {
    todo!("RFC0018.3: transient -> retryable code; permanent -> INVALID_ARGUMENT/400")
}

/// Scenario RFC0018.6 — out-of-range `SeverityNumber` is preserved, not clamped:
/// `severity_number` 25 / 200 are stored verbatim (never silently clamped to 0),
/// the `ingest.severity_out_of_range` metric increments, a `severity >= ERROR`
/// query still matches the preserved 25 / 200 (monotonicity), and a value a
/// `u8` cannot hold (negative, > 255) maps to 0 + the same anomaly count.
/// See `docs/rfcs/0018-otlp-log-spec-compliance.md` §5.
#[test]
#[ignore = "RFC0018.6 — red until severity preserve+flag replaces the clamp-to-0 (green)"]
fn rfc0018_6_out_of_range_severity_preserved() {
    todo!(
        "RFC0018.6: 25/200 preserved + anomaly metric; severity >= ERROR still \
         matches them (monotonicity); non-u8 -> 0 (storage invariant)"
    )
}
