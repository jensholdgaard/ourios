//! RFC0003.9 — Edge OTLP fields pass through unchanged.
//!
//! `severity_number = 0` (UNSPECIFIED) is an explicit value, kept as
//! `0`; empty `scope_name`/`scope_version` and wire
//! `observed_time_unix_nano = 0` narrow to `None`; `time_unix_nano = 0`
//! (unknown) is kept as the `u64` `0`. Nothing is coalesced, substituted,
//! or downcast to a default, and inherited resource attributes pass
//! through verbatim.

use opentelemetry_proto::tonic::common::v1::any_value::Value;
use opentelemetry_proto::tonic::common::v1::{AnyValue, InstrumentationScope, KeyValue};
use opentelemetry_proto::tonic::logs::v1::LogRecord;
use ourios_core::tenant::TenantId;
use ourios_ingester::receiver::materialize_record;

/// Scenario RFC0003.9 — Edge OTLP fields pass through unchanged.
/// See `docs/rfcs/0003-otlp-receiver.md` §5.
#[test]
fn rfc0003_9_edge_fields_pass_through_without_coalescing() {
    let record = LogRecord {
        time_unix_nano: 0,          // unknown event time
        observed_time_unix_nano: 0, // unset collector observation time
        severity_number: 0,         // SEVERITY_NUMBER_UNSPECIFIED
        ..Default::default()
    };
    let scope = InstrumentationScope {
        name: String::new(),
        version: String::new(),
        ..Default::default()
    };
    let resource_attributes = vec![KeyValue {
        key: "service.name".to_owned(),
        value: Some(AnyValue {
            value: Some(Value::StringValue("checkout".to_owned())),
        }),
        ..Default::default()
    }];

    let materialized = materialize_record(
        record,
        &resource_attributes,
        Some(&scope),
        TenantId::new("tenant-a"),
    );

    assert_eq!(
        materialized.severity_number, 0,
        "UNSPECIFIED (0) is preserved, not coalesced or substituted",
    );
    assert_eq!(
        materialized.observed_time_unix_nano, None,
        "wire observed_time_unix_nano = 0 narrows to None",
    );
    assert_eq!(
        materialized.time_unix_nano, 0,
        "unknown event time is kept as the u64 0 (not narrowed to None)",
    );
    assert_eq!(
        materialized.scope_name, None,
        "empty scope name narrows to None",
    );
    assert_eq!(
        materialized.scope_version, None,
        "empty scope version narrows to None",
    );
    assert_eq!(
        materialized.body, None,
        "an absent body stays None — no substitution",
    );
    assert_eq!(
        materialized.resource_attributes, resource_attributes,
        "inherited resource attributes pass through verbatim",
    );
}
