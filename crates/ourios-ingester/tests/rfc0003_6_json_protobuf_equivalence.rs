//! RFC0003.6 — HTTP/JSON ↔ gRPC/protobuf equivalence with OTLP-JSON encoding rules.
//!
//! Two layers, per §8 and the OTLP/JSON spec:
//!
//! 1. **Equivalence proptest** — `decode_json` and `decode_protobuf`
//!    decode the same logical request to the same in-memory value.
//!    Necessary but *not* sufficient: both paths share the proto types'
//!    `with-serde` codec, so agreement alone wouldn't prove OTLP-JSON
//!    compliance.
//! 2. **Spec-asserting cases** — the encoder emits, and the decoder
//!    accepts, exactly the OTLP/JSON deviations from plain proto3-JSON:
//!    hex `traceId`/`spanId` (not base64), **integer** enums (not
//!    names), **decimal-string** 64-bit ints (accepted as number *or*
//!    string), lowerCamelCase keys, and unknown-field tolerance. These
//!    asserts *are* the verification of `with-serde`'s actual behaviour
//!    — a failure is an empirical finding, not an assumption.

mod otlp_strategy;

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::common::v1::AnyValue;
use opentelemetry_proto::tonic::common::v1::any_value::Value;
use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
use otlp_strategy::export_request_json_safe;
use ourios_ingester::receiver::{decode_json, decode_protobuf};
use proptest::prelude::*;
use prost::Message;

/// Build a one-record request with non-default values in every field
/// the encoding rules touch (so proto3-JSON can't omit them as
/// defaults).
fn one_record_request() -> ExportLogsServiceRequest {
    let record = LogRecord {
        time_unix_nano: 1_700_000_000_000_000_000,
        severity_number: 9, // SEVERITY_NUMBER_INFO
        trace_id: (1u8..=16).collect(),
        span_id: (1u8..=8).collect(),
        dropped_attributes_count: 3,
        body: Some(AnyValue {
            value: Some(Value::StringValue("hello".to_owned())),
        }),
        ..Default::default()
    };
    ExportLogsServiceRequest {
        resource_logs: vec![ResourceLogs {
            scope_logs: vec![ScopeLogs {
                log_records: vec![record],
                ..Default::default()
            }],
            ..Default::default()
        }],
    }
}

/// Scenario RFC0003.6 — HTTP/JSON ↔ gRPC/protobuf decode equivalence.
/// See `docs/rfcs/0003-otlp-receiver.md` §5.
#[test]
fn rfc0003_6_json_and_protobuf_decode_to_the_same_request() {
    // Double `AnyValue`s are excluded from this strict equivalence check
    // (`export_request_json_safe`): `with-serde`'s double formatter isn't
    // shortest-round-trippable, so arbitrary `f64` drifts by 1–2 ULP
    // through JSON. That's a precision limitation, not an OTLP/JSON syntax
    // violation (the spec constrains syntax, not binary64 round-trip
    // fidelity), and is covered by the targeted double test below.
    proptest!(|(req in export_request_json_safe())| {
        let from_protobuf = decode_protobuf(&req.encode_to_vec())
            .expect("protobuf payload decodes");
        let json = serde_json::to_vec(&req).expect("serialise OTLP/JSON");
        let from_json = decode_json(&json).expect("OTLP/JSON payload decodes");
        // Equivalence at the decoded-request (AnyValue) level. Shared
        // `with-serde` codec, so this is the necessary-not-sufficient
        // layer; the spec asserts below carry the rest.
        prop_assert_eq!(from_protobuf, from_json);
    });
}

/// Scenario RFC0003.6 — `double` `AnyValue` precision through OTLP/JSON.
/// See `docs/rfcs/0003-otlp-receiver.md` §5.
#[test]
fn rfc0003_6_nice_doubles_roundtrip_through_otlp_json() {
    // OTLP/JSON constrains double *syntax* (JSON numbers), not bit-exact
    // round-trip fidelity. `with-serde`'s double formatter is not
    // shortest-round-trippable, so ~1 in 8 arbitrary `f64` values drift by
    // 1–2 ULP through JSON — a known with-serde precision limitation
    // (tracked in #130), not a spec violation. We pin the guarantee we
    // rely on: common ("nice") doubles
    // round-trip exactly. We deliberately do NOT assert that arbitrary
    // doubles are lossy — that would be a test of a bug that breaks the
    // day upstream improves the formatter.
    for nice in [0.0_f64, 1.5, -2.5, std::f64::consts::PI, 1e15, f64::MAX] {
        let back = roundtrip_double_via_otlp_json(nice);
        assert_eq!(
            back.to_bits(),
            nice.to_bits(),
            "nice double {nice:e} must round-trip exactly via OTLP/JSON",
        );
    }
}

/// Serialize a one-record request whose body is `DoubleValue(x)` to
/// OTLP/JSON, decode it back, and return the recovered double.
fn roundtrip_double_via_otlp_json(x: f64) -> f64 {
    let req = ExportLogsServiceRequest {
        resource_logs: vec![ResourceLogs {
            scope_logs: vec![ScopeLogs {
                log_records: vec![LogRecord {
                    body: Some(AnyValue {
                        value: Some(Value::DoubleValue(x)),
                    }),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }],
    };
    let json = serde_json::to_vec(&req).expect("serialise OTLP/JSON");
    let back = decode_json(&json).expect("OTLP/JSON decodes");
    match back.resource_logs[0].scope_logs[0].log_records[0]
        .body
        .as_ref()
        .and_then(|b| b.value.as_ref())
    {
        Some(Value::DoubleValue(y)) => *y,
        other => panic!("expected a double body, got {other:?}"),
    }
}

/// Scenario RFC0003.6 — the encoder emits the OTLP-JSON deviations.
/// See `docs/rfcs/0003-otlp-receiver.md` §5.
#[test]
fn rfc0003_6_json_encoder_emits_otlp_json_deviations() {
    let json = serde_json::to_value(one_record_request()).expect("serialise OTLP/JSON");
    let rec = &json["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0];

    // traceId / spanId: case-insensitive hex strings, NOT base64.
    assert_eq!(
        rec["traceId"]
            .as_str()
            .expect("traceId is a string")
            .to_lowercase(),
        "0102030405060708090a0b0c0d0e0f10",
        "traceId is hex-encoded, not base64",
    );
    assert_eq!(
        rec["spanId"]
            .as_str()
            .expect("spanId is a string")
            .to_lowercase(),
        "0102030405060708",
        "spanId is hex-encoded, not base64",
    );
    // Enums: integer values, not enum-name strings.
    assert!(
        rec["severityNumber"].is_number(),
        "severityNumber is an integer, not an enum name: {:?}",
        rec["severityNumber"],
    );
    assert_eq!(rec["severityNumber"], serde_json::json!(9));
    // 64-bit ints: decimal strings.
    assert!(
        rec["timeUnixNano"].is_string(),
        "timeUnixNano is a decimal string: {:?}",
        rec["timeUnixNano"],
    );
    assert_eq!(
        rec["timeUnixNano"],
        serde_json::json!("1700000000000000000")
    );
    // Keys: lowerCamelCase, not snake_case.
    assert!(
        rec.get("droppedAttributesCount").is_some(),
        "keys are lowerCamelCase",
    );
    assert!(
        rec.get("dropped_attributes_count").is_none(),
        "keys are not snake_case",
    );
}

/// Scenario RFC0003.6 — the decoder accepts a compliant OTLP/JSON body.
/// See `docs/rfcs/0003-otlp-receiver.md` §5.
#[test]
fn rfc0003_6_decoder_accepts_compliant_otlp_json() {
    let compliant = br#"{"resourceLogs":[{"scopeLogs":[{"logRecords":[{
        "timeUnixNano":"1700000000000000000",
        "severityNumber":9,
        "traceId":"0102030405060708090a0b0c0d0e0f10",
        "spanId":"0102030405060708",
        "body":{"stringValue":"hello"}
    }]}]}]}"#;
    let req = decode_json(compliant).expect("compliant OTLP/JSON decodes");
    let rec = &req.resource_logs[0].scope_logs[0].log_records[0];

    assert_eq!(rec.time_unix_nano, 1_700_000_000_000_000_000);
    assert_eq!(rec.severity_number, 9);
    assert_eq!(rec.trace_id, (1u8..=16).collect::<Vec<u8>>());
    assert_eq!(rec.span_id, (1u8..=8).collect::<Vec<u8>>());
    match rec.body.as_ref().and_then(|b| b.value.as_ref()) {
        Some(Value::StringValue(s)) => assert_eq!(s, "hello"),
        other => panic!("expected a string body, got {other:?}"),
    }
}

/// Scenario RFC0003.6 — the decoder accepts a 64-bit int as a JSON number.
/// See `docs/rfcs/0003-otlp-receiver.md` §5.
#[test]
fn rfc0003_6_decoder_accepts_64bit_int_as_number_or_string() {
    // The spec: decoders MUST accept both numbers and strings for
    // 64-bit ints. The compliant-decode test covers the string form;
    // this covers the number form.
    let numeric = br#"{"resourceLogs":[{"scopeLogs":[{"logRecords":[{
        "timeUnixNano":1700000000000000000
    }]}]}]}"#;
    let req = decode_json(numeric).expect("a 64-bit int as a JSON number is accepted");
    assert_eq!(
        req.resource_logs[0].scope_logs[0].log_records[0].time_unix_nano,
        1_700_000_000_000_000_000,
    );
}

/// Scenario RFC0003.6 — the decoder ignores unknown fields.
/// See `docs/rfcs/0003-otlp-receiver.md` §5.
#[test]
fn rfc0003_6_decoder_ignores_unknown_fields() {
    let unknown = br#"{"resourceLogs":[{"scopeLogs":[{"logRecords":[{
        "timeUnixNano":"7",
        "thisFieldIsNotInTheSchema":{"nested":[1,2,3]}
    }]}]}]}"#;
    let req = decode_json(unknown).expect("unknown fields are ignored, not rejected");
    assert_eq!(
        req.resource_logs[0].scope_logs[0].log_records[0].time_unix_nano,
        7,
    );
}
