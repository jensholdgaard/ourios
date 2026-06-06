//! RFC0003.4 — Tenant resolution failure rejects the entire batch.
//!
//! When any `ResourceLogs` group's Resource does not resolve to a tenant
//! under the configured rule, the whole export is rejected with a
//! controlled error naming the missing attribute — even if other groups
//! in the same export would have resolved. No records are accepted.

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::common::v1::any_value::Value;
use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue};
use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
use opentelemetry_proto::tonic::resource::v1::Resource;
use ourios_ingester::receiver::{TenantRule, fan_out};

fn string_attr(key: &str, value: &str) -> KeyValue {
    KeyValue {
        key: key.to_owned(),
        value: Some(AnyValue {
            value: Some(Value::StringValue(value.to_owned())),
        }),
        ..Default::default()
    }
}

fn resource_logs(attributes: Vec<KeyValue>) -> ResourceLogs {
    ResourceLogs {
        resource: Some(Resource {
            attributes,
            ..Default::default()
        }),
        scope_logs: vec![ScopeLogs {
            log_records: vec![LogRecord::default()],
            ..Default::default()
        }],
        ..Default::default()
    }
}

/// Scenario RFC0003.4 — Tenant resolution failure rejects the entire batch.
/// See `docs/rfcs/0003-otlp-receiver.md` §5.
#[test]
fn rfc0003_4_unresolved_resource_rejects_entire_batch() {
    // Arrange: a resolvable Resource followed by one with no
    // `service.name` (only `host.name`) — the second must reject the
    // whole export, not just its own group.
    let request = ExportLogsServiceRequest {
        resource_logs: vec![
            resource_logs(vec![string_attr("service.name", "checkout")]),
            resource_logs(vec![string_attr("host.name", "node-1")]),
        ],
    };

    // Act
    let result = fan_out(request, &TenantRule::service_name());

    // Assert: the whole batch is rejected and the error names the
    // missing attribute; no records are returned.
    match result {
        Err(error) => assert_eq!(
            error.attribute(),
            "service.name",
            "the error names the attribute the rule required",
        ),
        Ok(records) => panic!(
            "expected the whole batch to be rejected, got {} accepted records",
            records.len()
        ),
    }
}
