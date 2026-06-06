//! RFC0003.8 — `Body::String` reaches the miner as the unwrapped `L_raw`.
//!
//! A `string_value` body is unwrapped to `Body::String(s)` with the
//! original UTF-8 verbatim (no wrapping, quoting, or escaping); an
//! absent body stays `None`.

use opentelemetry_proto::tonic::common::v1::AnyValue;
use opentelemetry_proto::tonic::common::v1::any_value::Value;
use opentelemetry_proto::tonic::logs::v1::LogRecord;
use ourios_core::otlp::Body;
use ourios_core::tenant::TenantId;
use ourios_ingester::receiver::materialize_record;

/// Scenario RFC0003.8 — `Body::String` reaches the miner as the unwrapped `L_raw`.
/// See `docs/rfcs/0003-otlp-receiver.md` §5.
#[test]
fn rfc0003_8_string_body_is_unwrapped_verbatim() {
    let raw = "user 42 logged in from 10.0.0.1".to_owned();
    let record = LogRecord {
        body: Some(AnyValue {
            value: Some(Value::StringValue(raw.clone())),
        }),
        ..Default::default()
    };
    let materialized = materialize_record(record, &[], None, TenantId::new("tenant-a"));
    assert_eq!(
        materialized.body,
        Some(Body::String(raw)),
        "a string body is unwrapped to Body::String byte-for-byte",
    );
}

/// Scenario RFC0003.8 — an absent body stays `None`.
/// See `docs/rfcs/0003-otlp-receiver.md` §5.
#[test]
fn rfc0003_8_absent_body_is_none() {
    let record = LogRecord {
        body: None,
        ..Default::default()
    };
    let materialized = materialize_record(record, &[], None, TenantId::new("tenant-a"));
    assert_eq!(
        materialized.body, None,
        "a record with no body materialises to body = None, not an empty string",
    );
}
