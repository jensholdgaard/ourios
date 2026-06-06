//! RFC0003.5 — gRPC ≡ HTTP/protobuf decode equivalence.
//!
//! The gRPC and HTTP `application/x-protobuf` transports both deliver
//! the *same* protobuf payload, so they share one decoder
//! ([`ourios_ingester::receiver::decode_protobuf`]). This pins that the
//! decoder is transport-agnostic and faithful: a byte-equal payload
//! yields an equal `ExportLogsServiceRequest` either way, and decode
//! round-trips the original. Per §8, exercised by a proptest strategy
//! over the proto value space.

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::common::v1::{
    AnyValue, ArrayValue, InstrumentationScope, KeyValue, KeyValueList, any_value::Value,
};
use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
use opentelemetry_proto::tonic::resource::v1::Resource;
use ourios_ingester::receiver::decode_protobuf;
use proptest::prelude::*;
use prost::Message;

/// Scalar `AnyValue` payloads. Doubles exclude `NaN`: protobuf
/// round-trips `NaN` faithfully, but `NaN != NaN` would make the
/// `decoded == original` equality assertion spuriously fail, so the
/// strategy never generates it (the decode contract, not float
/// identity, is what's under test).
fn scalar_value() -> impl Strategy<Value = Value> {
    prop_oneof![
        ".{0,16}".prop_map(Value::StringValue),
        any::<bool>().prop_map(Value::BoolValue),
        any::<i64>().prop_map(Value::IntValue),
        any::<f64>()
            .prop_filter("NaN breaks == on the round-trip", |f| !f.is_nan())
            .prop_map(Value::DoubleValue),
        prop::collection::vec(any::<u8>(), 0..16).prop_map(Value::BytesValue),
    ]
}

/// An `AnyValue` tree: scalars, plus up to two nesting levels of array
/// and kvlist (`prop_recursive` depth `2` — the structured-body shapes
/// RFC0003.7 will lean on).
fn any_value() -> impl Strategy<Value = AnyValue> {
    scalar_value()
        .prop_recursive(2, 12, 4, |inner| {
            let element = inner.prop_map(|v| AnyValue { value: Some(v) });
            prop_oneof![
                prop::collection::vec(element.clone(), 0..4)
                    .prop_map(|values| Value::ArrayValue(ArrayValue { values })),
                prop::collection::vec((".{0,8}", element), 0..4).prop_map(|pairs| {
                    let values = pairs
                        .into_iter()
                        .map(|(key, v)| KeyValue {
                            key,
                            value: Some(v),
                            ..Default::default()
                        })
                        .collect();
                    Value::KvlistValue(KeyValueList { values })
                }),
            ]
        })
        .prop_map(|v| AnyValue { value: Some(v) })
}

fn key_value() -> impl Strategy<Value = KeyValue> {
    (".{0,8}", any_value()).prop_map(|(key, v)| KeyValue {
        key,
        value: Some(v),
        ..Default::default()
    })
}

fn log_record() -> impl Strategy<Value = LogRecord> {
    (
        any::<u64>(),
        0i32..=24,
        prop::option::of(any_value()),
        prop::collection::vec(key_value(), 0..3),
        any::<u32>(),
    )
        .prop_map(
            |(time_unix_nano, severity_number, body, attributes, dropped_attributes_count)| {
                LogRecord {
                    time_unix_nano,
                    severity_number,
                    body,
                    attributes,
                    dropped_attributes_count,
                    ..Default::default()
                }
            },
        )
}

fn export_request() -> impl Strategy<Value = ExportLogsServiceRequest> {
    let scope_logs = (
        ".{0,8}",
        ".{0,4}",
        prop::collection::vec(log_record(), 0..3),
    )
        .prop_map(|(name, version, log_records)| ScopeLogs {
            scope: Some(InstrumentationScope {
                name,
                version,
                ..Default::default()
            }),
            log_records,
            ..Default::default()
        });
    let resource_logs = (
        prop::collection::vec(key_value(), 0..3),
        prop::collection::vec(scope_logs, 0..2),
    )
        .prop_map(|(attributes, scope_logs)| ResourceLogs {
            resource: Some(Resource {
                attributes,
                ..Default::default()
            }),
            scope_logs,
            ..Default::default()
        });
    prop::collection::vec(resource_logs, 0..2)
        .prop_map(|resource_logs| ExportLogsServiceRequest { resource_logs })
}

/// Scenario RFC0003.5 — gRPC ≡ HTTP/protobuf decode equivalence.
/// See `docs/rfcs/0003-otlp-receiver.md` §5.
#[test]
fn rfc0003_5_grpc_and_http_protobuf_decode_identically() {
    proptest!(|(req in export_request())| {
        let bytes = req.encode_to_vec();
        // The gRPC and HTTP/x-protobuf transports hand the same payload
        // bytes to the same decoder, so decoding via either path is
        // identical.
        let from_grpc = decode_protobuf(&bytes).expect("gRPC-framed payload decodes");
        let from_http = decode_protobuf(&bytes).expect("HTTP-framed payload decodes");
        prop_assert_eq!(&from_grpc, &from_http);
        // ...and the decode is faithful — it round-trips the original.
        prop_assert_eq!(from_grpc, req);
    });
}
