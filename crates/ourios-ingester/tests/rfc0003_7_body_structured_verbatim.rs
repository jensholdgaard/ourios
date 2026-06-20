//! RFC0003.7 — `Body::Structured` carries the decoded `AnyValue` verbatim.
//!
//! Materialisation routes every non-`string_value` `AnyValue` to
//! `Body::Structured`, carrying the decoded value with no
//! canonicalisation, reshape, or dropped fields (the §6.4 amendment).

use opentelemetry_proto::tonic::common::v1::any_value::Value;
use opentelemetry_proto::tonic::common::v1::{AnyValue, ArrayValue, KeyValue, KeyValueList};
use opentelemetry_proto::tonic::logs::v1::LogRecord;
use ourios_core::otlp::Body;
use ourios_core::tenant::TenantId;
use ourios_ingester::receiver::materialize_record;

/// Scenario RFC0003.7 — `Body::Structured` carries the decoded `AnyValue` verbatim.
/// See `docs/rfcs/0003-otlp-receiver.md` §5.
#[test]
fn rfc0003_7_structured_body_is_carried_verbatim() {
    let non_string_variants = [
        Value::BoolValue(true),
        Value::IntValue(-42),
        Value::DoubleValue(1.5),
        Value::BytesValue(vec![0xDE, 0xAD, 0xBE, 0xEF]),
        Value::ArrayValue(ArrayValue {
            values: vec![AnyValue {
                value: Some(Value::IntValue(7)),
            }],
        }),
        Value::KvlistValue(KeyValueList {
            values: vec![KeyValue {
                key: "k".to_owned(),
                value: Some(AnyValue {
                    value: Some(Value::StringValue("v".to_owned())),
                }),
                ..Default::default()
            }],
        }),
    ];

    for variant in non_string_variants {
        let any_value = AnyValue {
            value: Some(variant),
        };
        let record = LogRecord {
            body: Some(any_value.clone()),
            ..Default::default()
        };
        let materialized = materialize_record(record, &[], "", None, "", TenantId::new("tenant-a"));
        assert_eq!(
            materialized.body,
            Some(Body::Structured(any_value)),
            "a non-string AnyValue reaches the miner as Body::Structured, verbatim",
        );
    }
}
