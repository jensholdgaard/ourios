// Shared across integration-test binaries: each test crate compiles
// this module independently and uses only the builder it needs
// (RFC0003.5 → `export_request`, RFC0003.6 → `export_request_json_safe`),
// so the other builder is unused *in that binary*. `dead_code` here is
// the expected shared-`tests/`-module shape, not a real dead path.
#![allow(dead_code)]

//! Shared proptest strategy over the OTLP proto value space, used by
//! the wire-decode equivalence scenarios (RFC0003.5 protobuf,
//! RFC0003.6 OTLP/JSON).
//!
//! Two builders differing only in whether `AnyValue` doubles are
//! generated:
//! - [`export_request`] — full value space (with finite doubles), for
//!   the protobuf round-trip (RFC0003.5), where every `f64` round-trips
//!   exactly.
//! - [`export_request_json_safe`] — **no doubles**, for the OTLP/JSON ↔
//!   protobuf equivalence proptest (RFC0003.6). `with-serde`'s double
//!   formatter is not shortest-round-trippable (~1 in 8 arbitrary `f64`
//!   drift by 1–2 ULP — a precision limitation, not an OTLP/JSON syntax
//!   violation; tracked separately), so doubles are excluded from the
//!   *strict* equivalence proptest and covered by a targeted test
//!   instead.
//!
//! Doubles, when generated, are **finite**: `NaN` breaks `==` and
//! `Infinity` has no plain-JSON literal.

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::common::v1::{
    AnyValue, ArrayValue, InstrumentationScope, KeyValue, KeyValueList, any_value::Value,
};
use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
use opentelemetry_proto::tonic::resource::v1::Resource;
use proptest::prelude::*;
use proptest::strategy::{BoxedStrategy, Union};

fn scalar_value(doubles: bool) -> BoxedStrategy<Value> {
    let mut options: Vec<BoxedStrategy<Value>> = vec![
        ".{0,16}".prop_map(Value::StringValue).boxed(),
        any::<bool>().prop_map(Value::BoolValue).boxed(),
        any::<i64>().prop_map(Value::IntValue).boxed(),
        prop::collection::vec(any::<u8>(), 0..16)
            .prop_map(Value::BytesValue)
            .boxed(),
    ];
    if doubles {
        options.push(
            any::<f64>()
                .prop_filter("finite: NaN breaks ==, Infinity has no JSON literal", |f| {
                    f.is_finite()
                })
                .prop_map(Value::DoubleValue)
                .boxed(),
        );
    }
    Union::new(options).boxed()
}

/// An `AnyValue` tree: scalars, plus up to two nesting levels of array
/// and kvlist (`prop_recursive` depth `2`).
fn any_value(doubles: bool) -> BoxedStrategy<AnyValue> {
    scalar_value(doubles)
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
        .boxed()
}

fn key_value(doubles: bool) -> BoxedStrategy<KeyValue> {
    (".{0,8}", any_value(doubles))
        .prop_map(|(key, v)| KeyValue {
            key,
            value: Some(v),
            ..Default::default()
        })
        .boxed()
}

fn log_record(doubles: bool) -> BoxedStrategy<LogRecord> {
    (
        any::<u64>(),
        0i32..=24,
        prop::option::of(any_value(doubles)),
        prop::collection::vec(key_value(doubles), 0..3),
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
        .boxed()
}

fn export_request_impl(doubles: bool) -> BoxedStrategy<ExportLogsServiceRequest> {
    let scope_logs = (
        ".{0,8}",
        ".{0,4}",
        prop::collection::vec(log_record(doubles), 0..3),
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
        prop::collection::vec(key_value(doubles), 0..3),
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
        .boxed()
}

/// Full value space (with finite doubles) — for the protobuf round-trip
/// (RFC0003.5), where every `f64` round-trips exactly.
pub fn export_request() -> impl Strategy<Value = ExportLogsServiceRequest> {
    export_request_impl(true)
}

/// No-doubles value space — for the OTLP/JSON ↔ protobuf equivalence
/// proptest (RFC0003.6); see the module docs for why doubles are
/// excluded from the strict equivalence check.
pub fn export_request_json_safe() -> impl Strategy<Value = ExportLogsServiceRequest> {
    export_request_impl(false)
}
