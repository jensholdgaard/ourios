//! RFC0003.3 — Tenant fan-out.
//!
//! One export with two `ResourceLogs` from different services fans out
//! into two distinct per-tenant streams: every record is tagged with its
//! own Resource's `tenant_id`, and no record from Resource A appears
//! under tenant B.

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::common::v1::any_value::Value;
use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue};
use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
use opentelemetry_proto::tonic::resource::v1::Resource;
use ourios_core::otlp::{Body, OtlpLogRecord};
use ourios_core::tenant::TenantId;
use ourios_ingester::receiver::{TenantRule, fan_out};

fn string_value(s: &str) -> AnyValue {
    AnyValue {
        value: Some(Value::StringValue(s.to_owned())),
    }
}

/// A `ResourceLogs` for `service_name` carrying one record whose body is
/// `body_marker` (so we can trace which Resource a record came from).
fn resource_logs(service_name: &str, body_marker: &str) -> ResourceLogs {
    ResourceLogs {
        resource: Some(Resource {
            attributes: vec![KeyValue {
                key: "service.name".to_owned(),
                value: Some(string_value(service_name)),
                ..Default::default()
            }],
            ..Default::default()
        }),
        scope_logs: vec![ScopeLogs {
            log_records: vec![LogRecord {
                body: Some(string_value(body_marker)),
                ..Default::default()
            }],
            ..Default::default()
        }],
        ..Default::default()
    }
}

fn body_text(record: &OtlpLogRecord) -> &str {
    match record.body.as_ref() {
        Some(Body::String(s)) => s,
        other => panic!("expected a string body, got {other:?}"),
    }
}

/// Scenario RFC0003.3 — Tenant fan-out.
/// See `docs/rfcs/0003-otlp-receiver.md` §5.
#[test]
fn rfc0003_3_distinct_resources_fan_out_without_cross_contamination() {
    // Arrange: one export, two ResourceLogs from different services.
    let request = ExportLogsServiceRequest {
        resource_logs: vec![
            resource_logs("checkout", "from-checkout"),
            resource_logs("payments", "from-payments"),
        ],
    };

    // Act
    let records = fan_out(request, &TenantRule::service_name()).expect("both Resources resolve");

    // Assert: two records, each tagged with its own Resource's tenant.
    assert_eq!(records.len(), 2, "one record per Resource");
    let checkout: Vec<&OtlpLogRecord> = records
        .iter()
        .filter(|r| r.tenant_id == TenantId::new("checkout"))
        .collect();
    let payments: Vec<&OtlpLogRecord> = records
        .iter()
        .filter(|r| r.tenant_id == TenantId::new("payments"))
        .collect();
    assert_eq!(checkout.len(), 1, "one record under tenant `checkout`");
    assert_eq!(payments.len(), 1, "one record under tenant `payments`");
    // No cross-contamination: each tenant's record carries that
    // Resource's body marker, not the other's.
    assert_eq!(body_text(checkout[0]), "from-checkout");
    assert_eq!(body_text(payments[0]), "from-payments");
}
