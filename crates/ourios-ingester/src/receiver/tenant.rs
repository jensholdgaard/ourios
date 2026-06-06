//! Tenant derivation + fan-out (RFC 0003 §6.3; RFC 0001 §6.1
//! *Tenant derivation*).
//!
//! `tenant_id` is derived **once per `ResourceLogs` group** from its
//! `Resource.attributes`, so one OTLP export can route records to
//! several tenants. The default rule reads `service.name` — the
//! OTel-canonical "what application emitted this", which maps onto
//! Ourios's per-tenant template-tree partitioning (`[§3.7]`). If any
//! group's Resource resolves to no tenant, the **entire** export is
//! rejected (RFC0003.4) — no silent default tenant, no per-Resource
//! partial acceptance.

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::common::v1::KeyValue;
use opentelemetry_proto::tonic::common::v1::any_value::Value;
use ourios_core::otlp::OtlpLogRecord;
use ourios_core::tenant::TenantId;

use crate::receiver::materialize::materialize_resource_logs;

/// The operator-configured rule that derives a `tenant_id` from a
/// `ResourceLogs`' `Resource.attributes`.
///
/// Today the rule reads a single string-valued resource attribute (the
/// default key is `service.name`). RFC 0001 §6.1 reserves richer
/// operator models (per-namespace, composite of several attributes) for
/// when they're actually configured; this stays a single key until then.
#[derive(Debug, Clone)]
pub struct TenantRule {
    attribute_key: String,
}

impl TenantRule {
    /// The default rule: `tenant_id` is the string value of the
    /// `service.name` resource attribute.
    #[must_use]
    pub fn service_name() -> Self {
        Self::by_attribute("service.name")
    }

    /// A rule reading an operator-chosen resource attribute key.
    pub fn by_attribute(key: impl Into<String>) -> Self {
        Self {
            attribute_key: key.into(),
        }
    }

    /// The resource attribute key this rule reads.
    #[must_use]
    pub fn attribute_key(&self) -> &str {
        &self.attribute_key
    }

    /// Derive the tenant for one Resource from its `attributes`.
    ///
    /// Resolves to the rule's attribute when it is present with a
    /// non-empty string value.
    ///
    /// # Errors
    ///
    /// [`TenantResolutionError`] (naming the attribute) when the
    /// attribute is absent, not a string, or an empty string — the
    /// receiver never invents a tenant the operator hasn't declared.
    pub fn derive(
        &self,
        resource_attributes: &[KeyValue],
    ) -> Result<TenantId, TenantResolutionError> {
        resource_attributes
            .iter()
            .find(|kv| kv.key == self.attribute_key)
            .and_then(|kv| kv.value.as_ref())
            .and_then(|value| match value.value.as_ref() {
                Some(Value::StringValue(s)) if !s.is_empty() => Some(TenantId::new(s.clone())),
                _ => None,
            })
            .ok_or_else(|| TenantResolutionError {
                attribute: self.attribute_key.clone(),
            })
    }
}

impl Default for TenantRule {
    fn default() -> Self {
        Self::service_name()
    }
}

/// A `ResourceLogs` group's `Resource` did not resolve to a tenant under
/// the configured rule. Per RFC 0003 §6.3 the **whole** export is
/// rejected; the error names the attribute the rule required so the
/// sender can fix its emitter or deployment.
#[derive(Debug)]
pub struct TenantResolutionError {
    attribute: String,
}

impl TenantResolutionError {
    /// The resource attribute the rule required but could not resolve.
    #[must_use]
    pub fn attribute(&self) -> &str {
        &self.attribute
    }
}

impl std::fmt::Display for TenantResolutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "tenant resolution failed: Resource is missing the `{}` attribute (or it is not a non-empty string)",
            self.attribute
        )
    }
}

impl std::error::Error for TenantResolutionError {}

/// Fan a decoded export out into per-tenant `OtlpLogRecord`s
/// (RFC0003.3). The tenant is derived once per `ResourceLogs` via `rule`
/// and applied to every record under it; each record carries its
/// `tenant_id`, so the miner's per-tenant routing keeps streams
/// separate with no cross-contamination.
///
/// # Errors
///
/// If **any** `ResourceLogs` fails to resolve, the entire export is
/// rejected with [`TenantResolutionError`] (RFC0003.4) — the error
/// short-circuits before any records are returned, so partial batches
/// are never accepted.
pub fn fan_out(
    request: ExportLogsServiceRequest,
    rule: &TenantRule,
) -> Result<Vec<OtlpLogRecord>, TenantResolutionError> {
    let mut records = Vec::new();
    for resource_logs in request.resource_logs {
        // Scope the borrow of `resource_logs` so it can be moved into
        // `materialize_resource_logs` afterwards.
        let tenant_id = {
            let resource_attributes = resource_logs
                .resource
                .as_ref()
                .map(|resource| resource.attributes.as_slice())
                .unwrap_or_default();
            rule.derive(resource_attributes)?
        };
        records.extend(materialize_resource_logs(resource_logs, &tenant_id));
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::{TenantRule, Value};
    use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue};

    fn string_attr(key: &str, value: &str) -> KeyValue {
        KeyValue {
            key: key.to_owned(),
            value: Some(AnyValue {
                value: Some(Value::StringValue(value.to_owned())),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn default_rule_resolves_service_name() {
        // Arrange
        let attrs = [string_attr("service.name", "checkout")];
        // Act
        let tenant = TenantRule::service_name().derive(&attrs).expect("resolves");
        // Assert
        assert_eq!(tenant.as_str(), "checkout");
    }

    #[test]
    fn missing_attribute_errors_naming_the_attribute() {
        // Arrange
        let attrs = [string_attr("host.name", "node-1")];
        // Act
        let err = TenantRule::service_name().derive(&attrs).unwrap_err();
        // Assert
        assert_eq!(err.attribute(), "service.name");
    }

    #[test]
    fn non_string_or_empty_attribute_does_not_resolve() {
        // Arrange: present but an empty string, and present but a
        // non-string value — neither is a usable tenant.
        let empty = [string_attr("service.name", "")];
        let non_string = [KeyValue {
            key: "service.name".to_owned(),
            value: Some(AnyValue {
                value: Some(Value::IntValue(7)),
            }),
            ..Default::default()
        }];
        let rule = TenantRule::service_name();
        // Act + Assert
        assert!(rule.derive(&empty).is_err(), "empty string is not a tenant");
        assert!(
            rule.derive(&non_string).is_err(),
            "a non-string attribute is not a tenant",
        );
    }

    #[test]
    fn operator_configured_attribute_key_is_used() {
        // Arrange
        let attrs = [string_attr("tenant.id", "acme")];
        // Act
        let tenant = TenantRule::by_attribute("tenant.id")
            .derive(&attrs)
            .expect("resolves under the custom key");
        // Assert
        assert_eq!(tenant.as_str(), "acme");
    }
}
