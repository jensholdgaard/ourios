//! One export, one tenant (RFC 0046 §3.2).
//!
//! The tenant is selected out of band on the request
//! ([`super::selector`]) — never derived from the payload — and every
//! record in the export carries it. Resource attributes such as
//! `service.name` are what `OTel` says they are: descriptions of the
//! producer, promoted and queryable inside the tenant, never a partition
//! key. Nothing about the payload can reject the export on tenancy grounds
//! any more; that decision is made before this module runs.

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use ourios_core::otlp::OtlpLogRecord;
use ourios_core::tenant::TenantId;

use crate::receiver::materialize::materialize_resource_logs;

/// Materialise every record of `request` under `tenant`, in wire order.
#[must_use]
pub fn assign(request: ExportLogsServiceRequest, tenant: &TenantId) -> Vec<OtlpLogRecord> {
    request
        .resource_logs
        .into_iter()
        .flat_map(|resource_logs| materialize_resource_logs(resource_logs, tenant))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::assign;
    use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
    use opentelemetry_proto::tonic::common::v1::any_value::Value;
    use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue};
    use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
    use opentelemetry_proto::tonic::resource::v1::Resource;
    use ourios_core::tenant::TenantId;

    fn group(service: Option<&str>, body: &str) -> ResourceLogs {
        ResourceLogs {
            resource: Some(Resource {
                attributes: service
                    .map(|s| KeyValue {
                        key: "service.name".to_owned(),
                        value: Some(AnyValue {
                            value: Some(Value::StringValue(s.to_owned())),
                        }),
                        ..Default::default()
                    })
                    .into_iter()
                    .collect(),
                ..Default::default()
            }),
            scope_logs: vec![ScopeLogs {
                log_records: vec![LogRecord {
                    body: Some(AnyValue {
                        value: Some(Value::StringValue(body.to_owned())),
                    }),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    // RFC0046.3 — different service.names, and none at all, all land in the
    // selected tenant; nothing is rejected.
    #[test]
    fn every_group_lands_in_the_selected_tenant() {
        let request = ExportLogsServiceRequest {
            resource_logs: vec![
                group(Some("fluxcd"), "a"),
                group(Some("checkout"), "b"),
                group(None, "c"),
            ],
        };
        let records = assign(request, &TenantId::new("acme"));
        assert_eq!(records.len(), 3);
        assert!(records.iter().all(|r| r.tenant_id.as_str() == "acme"));
    }
}
