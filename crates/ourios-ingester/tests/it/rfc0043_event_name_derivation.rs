//! RFC0043.1/.2/.3/.4/.7 — `event_name` derived from the legacy
//! `event.name` attribute at the materialisation boundary.
//!
//! The wire field wins when non-empty; an unset-or-empty field derives
//! from a non-empty string `event.name` attribute; the attribute is
//! preserved verbatim either way (derivation, never correction); empty
//! is never a value in either encoding, and the protobuf and RFC0003.6
//! JSON paths cannot diverge because both funnel through
//! `materialize_record`.

use opentelemetry_proto::tonic::common::v1::any_value::Value;
use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue};
use opentelemetry_proto::tonic::logs::v1::LogRecord;
use ourios_core::otlp::OtlpLogRecord;
use ourios_core::tenant::TenantId;
use ourios_ingester::receiver::{decode_json, materialize_record};

fn attr(key: &str, value: Option<Value>) -> KeyValue {
    KeyValue {
        key: key.to_owned(),
        value: Some(AnyValue { value }),
        ..Default::default()
    }
}

fn materialize(record: LogRecord) -> OtlpLogRecord {
    materialize_record(record, &[], "", None, "", TenantId::new("tenant-a"))
}

/// Scenario RFC0043.1 — the wire field wins.
/// See `docs/rfcs/0043-event-name-attribute-ingest.md` §5.
#[test]
fn rfc0043_1_wire_field_wins_and_attribute_is_preserved() {
    let record = LogRecord {
        event_name: "from.the.wire".to_owned(),
        attributes: vec![attr(
            "event.name",
            Some(Value::StringValue("from.the.attribute".to_owned())),
        )],
        ..Default::default()
    };
    let materialized = materialize(record);
    assert_eq!(
        materialized.event_name.as_deref(),
        Some("from.the.wire"),
        "a non-empty wire event_name is never overridden",
    );
    assert_eq!(
        materialized.attributes[0],
        attr(
            "event.name",
            Some(Value::StringValue("from.the.attribute".to_owned()))
        ),
        "the mismatching attribute is source telemetry, preserved unflagged",
    );
}

/// Scenario RFC0043.2 — derivation from the attribute, attribute intact.
#[test]
fn rfc0043_2_derives_from_attribute_and_preserves_it() {
    let record = LogRecord {
        attributes: vec![attr(
            "event.name",
            Some(Value::StringValue("claude_code.api_request".to_owned())),
        )],
        ..Default::default()
    };
    let materialized = materialize(record);
    assert_eq!(
        materialized.event_name.as_deref(),
        Some("claude_code.api_request"),
    );
    assert_eq!(
        materialized.attributes[0],
        attr(
            "event.name",
            Some(Value::StringValue("claude_code.api_request".to_owned()))
        ),
        "derivation, not a move: the attribute stays byte-identical",
    );
}

/// Scenario RFC0043.3 — both encodings derive identically. The JSON
/// payload spells the record per RFC0003.6 (lowerCamelCase, no wire
/// `eventName`); decoding yields the same proto structs the protobuf
/// path produces, so one materialisation covers both.
#[test]
fn rfc0043_3_json_and_protobuf_paths_agree() {
    let json = br#"{"resourceLogs":[{"scopeLogs":[{"logRecords":[{"severityNumber":9,"body":{"stringValue":"api_request"},"attributes":[{"key":"event.name","value":{"stringValue":"claude_code.api_request"}}]}]}]}]}"#;
    let (decoded, _) = decode_json(json).expect("RFC0003.6 payload decodes");
    let record = decoded.resource_logs[0].scope_logs[0].log_records[0].clone();
    let via_json = materialize(record);

    let via_proto = materialize(LogRecord {
        severity_number: 9,
        body: Some(AnyValue {
            value: Some(Value::StringValue("api_request".to_owned())),
        }),
        attributes: vec![attr(
            "event.name",
            Some(Value::StringValue("claude_code.api_request".to_owned())),
        )],
        ..Default::default()
    });

    assert_eq!(
        via_json.event_name.as_deref(),
        Some("claude_code.api_request")
    );
    assert_eq!(via_json.event_name, via_proto.event_name);
    assert_eq!(via_json.attributes, via_proto.attributes);
}

/// Scenario RFC0043.4 — a non-string `event.name` derives nothing.
#[test]
fn rfc0043_4_non_string_attribute_derives_nothing() {
    let record = LogRecord {
        attributes: vec![attr("event.name", Some(Value::IntValue(7)))],
        ..Default::default()
    };
    let materialized = materialize(record);
    assert_eq!(materialized.event_name, None);
    assert_eq!(
        materialized.attributes[0],
        attr("event.name", Some(Value::IntValue(7))),
        "the non-string attribute is preserved verbatim",
    );
}

/// Scenario RFC0043.7 — empty is never a value, in either position:
/// (a) an empty wire field is unset (proto3 cannot distinguish absent
/// from empty), so the attribute derives; (b) an empty-string attribute
/// derives nothing; (c) a JSON `null` value derives nothing.
#[test]
fn rfc0043_7_empty_is_never_a_value() {
    // (a) empty wire field + attribute → derives.
    let a = materialize(LogRecord {
        event_name: String::new(),
        attributes: vec![attr(
            "event.name",
            Some(Value::StringValue("derived.anyway".to_owned())),
        )],
        ..Default::default()
    });
    assert_eq!(a.event_name.as_deref(), Some("derived.anyway"));

    // (b) empty-string attribute → nothing.
    let b = materialize(LogRecord {
        attributes: vec![attr("event.name", Some(Value::StringValue(String::new())))],
        ..Default::default()
    });
    assert_eq!(b.event_name, None);

    // (c) JSON null value (an unset AnyValue) → nothing, via the real
    // JSON decoder so the encoding-equivalence claim is exercised.
    let json = br#"{"resourceLogs":[{"scopeLogs":[{"logRecords":[{"attributes":[{"key":"event.name","value":null}]}]}]}]}"#;
    let (decoded, _) = decode_json(json).expect("null attribute value decodes");
    let record = decoded.resource_logs[0].scope_logs[0].log_records[0].clone();
    let c = materialize(record);
    assert_eq!(c.event_name, None);
}
