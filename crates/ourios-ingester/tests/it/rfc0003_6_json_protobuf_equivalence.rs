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

use crate::otlp_strategy::export_request_json_safe;
use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::common::v1::AnyValue;
use opentelemetry_proto::tonic::common::v1::any_value::Value;
use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
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
        let (from_json, _) = decode_json(&json).expect("OTLP/JSON payload decodes");
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
    let (back, _) = decode_json(&json).expect("OTLP/JSON decodes");
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
    let (req, lenient) = decode_json(compliant).expect("compliant OTLP/JSON decodes");
    assert!(
        !lenient,
        "a fully-valid payload stays on the direct parse path"
    );
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
    let (req, _) = decode_json(numeric).expect("a 64-bit int as a JSON number is accepted");
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
    let (req, _) = decode_json(unknown).expect("unknown fields are ignored, not rejected");
    assert_eq!(
        req.resource_logs[0].scope_logs[0].log_records[0].time_unix_nano,
        7,
    );
}

/// Scenario RFC0003.6 follow-on (ourios#549) — an UNSET `AnyValue` is
/// spec-valid OTLP/JSON (`{}` per proto3-JSON's empty-message
/// encoding; real exporters emit it for empty-body events, and
/// `with-serde`'s own serializer emits `null` for the same state) and
/// must decode to the SAME materialized record the protobuf transport
/// produces for a wire-present-but-unset `AnyValue`.
///
/// This is the empirical-finding layer this file's header promises:
/// upstream `with-serde` rejects both encodings ("Invalid data for
/// Value, no known keys found"), so `decode_json` parses through
/// `ourios_core::otlp::lenient_json`. On the JSON path the unset value
/// re-parses as the ABSENT field (`None`) where protobuf yields
/// `Some(AnyValue { value: None })` — this test pins that the
/// difference is invisible at the [`Body`]/downstream level, which is
/// the level Ourios stores.
#[test]
fn rfc0003_6_unset_any_value_decodes_equivalently_across_transports() {
    use ourios_core::otlp::Body;

    // Protobuf: body PRESENT but unset (an empty AnyValue message on
    // the wire), one attribute with an unset value.
    let record = LogRecord {
        severity_number: 9,
        body: Some(AnyValue { value: None }),
        attributes: vec![opentelemetry_proto::tonic::common::v1::KeyValue {
            key: "feature_flag_key".to_owned(),
            value: Some(AnyValue { value: None }),
            ..Default::default()
        }],
        event_name: "shipping.feature_flag.evaluated".to_owned(),
        ..Default::default()
    };
    let request = ExportLogsServiceRequest {
        resource_logs: vec![ResourceLogs {
            scope_logs: vec![ScopeLogs {
                log_records: vec![record],
                ..Default::default()
            }],
            ..Default::default()
        }],
    };
    let from_protobuf = decode_protobuf(&request.encode_to_vec()).expect("protobuf decodes");

    // JSON: the demo-corpus shape — `"body": {}`, `"value": {}`.
    let json = br#"{"resourceLogs":[{"scopeLogs":[{"logRecords":[{"severityNumber":9,"body":{},"attributes":[{"key":"feature_flag_key","value":{}}],"eventName":"shipping.feature_flag.evaluated"}]}]}]}"#;
    let (from_json, lenient) = decode_json(json).expect("spec-valid unset AnyValue must decode");
    assert!(
        lenient,
        "the unset encodings only parse via the lenient retry today — this flip is \
         ALSO an upstream-fix signal (direct parse succeeding makes this false)",
    );

    let pb_rec = &from_protobuf.resource_logs[0].scope_logs[0].log_records[0];
    let js_rec = &from_json.resource_logs[0].scope_logs[0].log_records[0];

    // The raw decode differs BY DESIGN (Some(empty) vs None) …
    assert_eq!(pb_rec.body, Some(AnyValue { value: None }));
    assert_eq!(js_rec.body, None);
    // … and collapses to the identical stored state at the Body level:
    assert_eq!(
        pb_rec.body.clone().and_then(Body::from_any_value),
        js_rec.body.clone().and_then(Body::from_any_value),
        "unset body must be storage-equivalent across transports",
    );
    // Attribute values: the INTERIM contract, pinned exactly. The
    // protobuf transport preserves the wire's present-but-unset value
    // (`Some(AnyValue { value: None })` — the RFC 0018 fidelity rule,
    // and the canonical codec round-trips it, see
    // `attributes_with_absent_and_empty_values_round_trip`). The JSON
    // transport CANNOT currently deliver that state — upstream
    // `with-serde` rejects its only encodings (`{}` / `null`,
    // ourios#549) — so the lenient shim's strip-to-absent is a
    // BOUNDED, DOCUMENTED fidelity concession: the presence bit of an
    // EMPTY value is downgraded, nothing else. When the upstream fix
    // ships, the direct parse succeeds, the shim goes dormant, and the
    // JSON path regains Some(empty) — flipping the assert below is the
    // signal that full fidelity is restored.
    assert_eq!(pb_rec.attributes[0].value, Some(AnyValue { value: None }));
    assert_eq!(js_rec.attributes[0].value, None, "interim: see ourios#549");
    // Both states store and read back faithfully through the canonical
    // codec — each transport's delivered form survives its own round
    // trip (no silent convergence, no read failure on either form).
    let canon = ourios_core::otlp::canonical::encode_attributes;
    let decode = ourios_core::otlp::canonical::decode_attributes;
    for rec in [pb_rec, js_rec] {
        let stored = canon(&rec.attributes).expect("canonical encode");
        let read = decode(&stored).expect("stored attributes must decode");
        assert_eq!(
            read, rec.attributes,
            "canonical round trip preserves the form"
        );
    }
    assert_eq!(pb_rec.event_name, js_rec.event_name);
}

/// Scenario RFC0003.6 — a `null`-valued `AnyValue` field decodes via
/// the lenient shim. See `docs/rfcs/0003-otlp-receiver.md` §5.
///
/// `{"intValue":null}` / `{"doubleValue":null}` is proto3-JSON's
/// encoding of a field default (for a `oneof`, unset). Real exporters
/// emit it: the Vercel AI SDK writes `doubleValue: null` for non-finite
/// token counts (upstream opentelemetry-rust#3603) — exactly the
/// AI-agent traffic RFC 0037 targets. The production JSON receive path
/// (`decode_json`) must accept it via the lenient shim rather than
/// reject the whole batch, decoding it to the same absent state as `{}`.
/// This is also the null-field arm of the shim-retirement flip signal:
/// removal needs BOTH #3595 (`{}`) and #3603 (the null field) released.
#[test]
fn rfc0003_6_null_valued_anyvalue_field_decodes_via_lenient_shim() {
    let json = br#"{"resourceLogs":[{"scopeLogs":[{"logRecords":[{"severityNumber":9,"body":{"intValue":null},"attributes":[{"key":"gen_ai.usage.output_tokens","value":{"doubleValue":null}}],"eventName":"gen_ai.client.inference.operation.details"}]}]}]}"#;
    let (decoded, lenient) =
        decode_json(json).expect("a null-field unset AnyValue must decode, not reject the batch");
    assert!(
        lenient,
        "the null field only parses via the lenient retry today — the null-field arm of \
         the flip signal (direct parse succeeding makes this false; needs upstream #3603)",
    );
    let rec = &decoded.resource_logs[0].scope_logs[0].log_records[0];
    assert_eq!(
        rec.body, None,
        "a null intValue field decodes to an absent body"
    );
    assert_eq!(
        rec.attributes[0].value, None,
        "a null doubleValue field decodes to an absent value",
    );
}
