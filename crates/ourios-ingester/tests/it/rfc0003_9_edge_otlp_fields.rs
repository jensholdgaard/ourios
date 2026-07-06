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
        "",
        Some(&scope),
        "",
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

/// Scenario RFC0003.9 — out-of-range `severity_number` is **preserved**, not
/// narrowed (RFC 0018 §3.5 supersedes the prior clamp-to-UNSPECIFIED: the
/// receiver is a faithful witness — §3.0 — so out-of-named-range values pass
/// through, more consistent with RFC0003.9's own "edge fields pass through
/// unchanged" theme). Only the extremes a `u8` cannot hold (negative, `>255`)
/// narrow to `0`, where the storage invariant wins.
/// See `docs/rfcs/0003-otlp-receiver.md` §5; `docs/rfcs/0018-otlp-log-spec-compliance.md` §3.5.
#[test]
fn rfc0003_9_out_of_range_severity_is_preserved() {
    for (wire, expected) in [
        (0i32, 0u8),
        (24, 24),
        (25, 25),   // out of the named range, but u8-storable → preserved
        (200, 200), // preserved
        (1000, 0),  // not u8-storable → storage-invariant narrow to 0
        (-5, 0),    // not u8-storable → 0
    ] {
        let record = LogRecord {
            severity_number: wire,
            ..Default::default()
        };
        let materialized = materialize_record(record, &[], "", None, "", TenantId::new("tenant-a"));
        assert_eq!(
            materialized.severity_number, expected,
            "severity_number {wire} → {expected} (preserve verbatim; non-u8 → 0)",
        );
    }
}
