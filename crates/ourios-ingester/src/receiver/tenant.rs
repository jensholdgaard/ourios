//! Tenant derivation + fan-out (RFC 0003 §6.3; RFC 0001 §6.1
//! *Tenant derivation*).
//!
//! `tenant_id` is derived **once per `ResourceLogs` group** from its
//! `Resource.attributes`, so one OTLP export can route records to
//! several tenants. The default rule reads `service.name` — the
//! OTel-canonical "what application emitted this", which maps onto
//! Ourios's per-tenant template-tree partitioning (`[§3.7]`); the
//! operator may configure a composite of several keys (RFC 0045). If any
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
/// `ResourceLogs`' `Resource.attributes` (RFC 0045 §3.1): an ordered,
/// non-empty list of resource-attribute keys, every one required.
///
/// A single-key rule (the default `[service.name]`) derives the
/// attribute's string value verbatim. A composite rule (two or more keys)
/// percent-escapes `%` and `/` in each value and joins the values with
/// `/`, so distinct value tuples never collide (RFC 0045 §3.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TenantRule {
    keys: Vec<String>,
}

impl TenantRule {
    /// The default rule: `tenant_id` is the string value of the
    /// `service.name` resource attribute.
    #[must_use]
    pub fn service_name() -> Self {
        Self::by_attribute("service.name")
    }

    /// A single-key rule reading an operator-chosen resource attribute.
    pub fn by_attribute(key: impl Into<String>) -> Self {
        Self {
            keys: vec![key.into()],
        }
    }

    /// An ordered rule over `keys` (RFC0045.1).
    ///
    /// # Errors
    ///
    /// [`TenantRuleError::Empty`] for no keys; [`TenantRuleError::Duplicate`]
    /// naming the first repeated key.
    pub fn from_keys<I, K>(keys: I) -> Result<Self, TenantRuleError>
    where
        I: IntoIterator<Item = K>,
        K: Into<String>,
    {
        let keys: Vec<String> = keys.into_iter().map(Into::into).collect();
        if keys.is_empty() {
            return Err(TenantRuleError::Empty);
        }
        let mut seen = std::collections::HashSet::new();
        if let Some(duplicate) = keys.iter().find(|key| !seen.insert(key.as_str())) {
            return Err(TenantRuleError::Duplicate {
                key: duplicate.clone(),
            });
        }
        Ok(Self { keys })
    }

    /// The resource attribute keys this rule reads, in join order.
    #[must_use]
    pub fn keys(&self) -> &[String] {
        &self.keys
    }

    /// Whether `key` is one of the rule's keys.
    #[must_use]
    pub fn contains(&self, key: &str) -> bool {
        self.keys.iter().any(|k| k == key)
    }

    /// Derive the tenant for one Resource from its `attributes`.
    ///
    /// Resolves when every key is present with a non-empty string value.
    ///
    /// # Errors
    ///
    /// [`TenantResolutionError`] naming the first key that is absent, not a
    /// string, or an empty string — the receiver never invents a tenant the
    /// operator hasn't declared, and never joins a partial tuple.
    pub fn derive(
        &self,
        resource_attributes: &[KeyValue],
    ) -> Result<TenantId, TenantResolutionError> {
        let mut values = Vec::with_capacity(self.keys.len());
        for key in &self.keys {
            let value = string_attribute(resource_attributes, key).ok_or_else(|| {
                TenantResolutionError {
                    attribute: key.clone(),
                    resource_index: None,
                }
            })?;
            values.push(value);
        }
        Ok(TenantId::new(join_components(&values)))
    }
}

/// The non-empty string value of `key` in `attributes`, if any.
fn string_attribute<'a>(attributes: &'a [KeyValue], key: &str) -> Option<&'a str> {
    attributes
        .iter()
        .find(|kv| kv.key == key)
        .and_then(|kv| kv.value.as_ref())
        .and_then(|value| match value.value.as_ref() {
            Some(Value::StringValue(s)) if !s.is_empty() => Some(s.as_str()),
            _ => None,
        })
}

/// RFC 0045 §3.2: one component is the value verbatim; two or more are
/// `%`/`/`-escaped and `/`-joined.
fn join_components(values: &[&str]) -> String {
    match values {
        [single] => (*single).to_owned(),
        many => {
            let mut out = String::new();
            for (i, value) in many.iter().enumerate() {
                if i > 0 {
                    out.push('/');
                }
                for c in value.chars() {
                    match c {
                        '%' => out.push_str("%25"),
                        '/' => out.push_str("%2F"),
                        other => out.push(other),
                    }
                }
            }
            out
        }
    }
}

/// The resolved `receiver.tenant` section (RFC 0045 §3.1): the derivation
/// rule plus the divergence-watch keys and state bound (§3.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TenantDerivation {
    pub rule: TenantRule,
    /// Keys watched for divergence; a key also in `rule` is skipped.
    pub watch: Vec<String>,
    /// Upper bound on remembered (tenant, key) pairs.
    pub watch_capacity: usize,
}

impl TenantDerivation {
    /// The RFC 0045 §3.4 default watch key.
    pub const DEFAULT_WATCH: &'static str = "k8s.cluster.name";
    /// The RFC 0045 §3.4 default state bound.
    pub const DEFAULT_WATCH_CAPACITY: usize = 10_000;
}

impl Default for TenantDerivation {
    fn default() -> Self {
        Self {
            rule: TenantRule::service_name(),
            watch: vec![Self::DEFAULT_WATCH.to_owned()],
            watch_capacity: Self::DEFAULT_WATCH_CAPACITY,
        }
    }
}

/// A `receiver.tenant.rule` that cannot be a rule (RFC0045.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TenantRuleError {
    /// No keys at all.
    Empty,
    /// The same key listed twice.
    Duplicate { key: String },
}

impl std::fmt::Display for TenantRuleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(
                f,
                "tenant rule must list at least one resource attribute key"
            ),
            Self::Duplicate { key } => {
                write!(
                    f,
                    "tenant rule lists resource attribute key `{key}` more than once"
                )
            }
        }
    }
}

impl std::error::Error for TenantRuleError {}

impl Default for TenantRule {
    fn default() -> Self {
        Self::service_name()
    }
}

/// A `ResourceLogs` group's `Resource` did not resolve to a tenant under
/// the configured rule. Per RFC 0003 §6.3 the **whole** export is
/// rejected; the error names the failing `ResourceLogs` index and the
/// attribute the rule required (RFC0003.4) so the sender can fix the
/// offending emitter or deployment.
///
/// `resource_index` is `None` for a bare [`TenantRule::derive`] (which
/// sees one Resource with no batch context) and `Some(i)` once
/// [`fan_out`] attaches the group's position in the export.
#[derive(Debug)]
pub struct TenantResolutionError {
    attribute: String,
    resource_index: Option<usize>,
}

impl TenantResolutionError {
    /// The resource attribute the rule required but could not resolve.
    #[must_use]
    pub fn attribute(&self) -> &str {
        &self.attribute
    }

    /// The position of the failing `ResourceLogs` group in the export,
    /// once known (`fan_out` attaches it; a bare `derive` leaves `None`).
    #[must_use]
    pub fn resource_index(&self) -> Option<usize> {
        self.resource_index
    }

    /// Attach the failing group's index (called by [`derive_for_group`],
    /// which both [`fan_out`] and the RFC 0026 binding check go through).
    #[must_use]
    fn at_resource(mut self, index: usize) -> Self {
        self.resource_index = Some(index);
        self
    }

    /// Build an instance for in-crate unit tests (the real constructor is
    /// driven by the resolution path).
    #[cfg(test)]
    pub(crate) fn for_test(attribute: &str) -> Self {
        Self {
            attribute: attribute.to_owned(),
            resource_index: None,
        }
    }
}

impl std::fmt::Display for TenantResolutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.resource_index {
            Some(index) => write!(
                f,
                "tenant resolution failed: ResourceLogs[{index}] is missing the `{}` attribute (or it is not a non-empty string)",
                self.attribute
            ),
            None => write!(
                f,
                "tenant resolution failed: Resource is missing the `{}` attribute (or it is not a non-empty string)",
                self.attribute
            ),
        }
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
    for (index, resource_logs) in request.resource_logs.into_iter().enumerate() {
        // Derived before `resource_logs` is moved into
        // `materialize_resource_logs`.
        let tenant_id = derive_for_group(&resource_logs, index, rule)?;
        records.extend(materialize_resource_logs(resource_logs, &tenant_id));
    }
    Ok(records)
}

/// Derive one `ResourceLogs` group's tenant (RFC 0003 §6.3), attaching the
/// group's index to a failure so the error names the failing Resource
/// (RFC0003.4). The single derivation used by [`fan_out`] and the RFC 0026
/// binding check — one source of truth, so the two walks cannot drift.
pub(crate) fn derive_for_group(
    resource_logs: &opentelemetry_proto::tonic::logs::v1::ResourceLogs,
    index: usize,
    rule: &TenantRule,
) -> Result<ourios_core::tenant::TenantId, TenantResolutionError> {
    let resource_attributes = resource_logs
        .resource
        .as_ref()
        .map(|resource| resource.attributes.as_slice())
        .unwrap_or_default();
    rule.derive(resource_attributes)
        .map_err(|error| error.at_resource(index))
}

#[cfg(test)]
mod tests {
    use super::{TenantRule, TenantRuleError, Value};
    use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue};
    use proptest::strategy::Strategy;

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
        // Assert: names the attribute; index is unknown at the
        // single-Resource `derive` level (fan_out attaches it).
        assert_eq!(err.attribute(), "service.name");
        assert_eq!(err.resource_index(), None);
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

    // RFC0045.6 — the single-key path never escapes.
    #[test]
    fn single_key_rule_is_verbatim_even_with_slash_and_percent() {
        for raw in ["a/b", "100%", "a%2Fb"] {
            let attrs = [string_attr("service.name", raw)];
            let tenant = TenantRule::service_name().derive(&attrs).expect("resolves");
            assert_eq!(tenant.as_str(), raw);
        }
    }

    // RFC0045.2 — composite join.
    #[test]
    fn composite_rule_joins_in_key_order() {
        let rule = TenantRule::from_keys(["k8s.cluster.name", "service.name"]).expect("valid");
        let attrs = [
            string_attr("service.name", "fluxcd"),
            string_attr("k8s.cluster.name", "cluster1"),
        ];
        let tenant = rule.derive(&attrs).expect("resolves");
        assert_eq!(tenant.as_str(), "cluster1/fluxcd");
    }

    // RFC0045.4 — the two canonical colliding tuples stay apart.
    #[test]
    fn composite_join_escapes_separator_and_escape_char() {
        let rule = TenantRule::from_keys(["a", "b"]).expect("valid");
        let left = rule
            .derive(&[string_attr("a", "a"), string_attr("b", "b/c")])
            .expect("resolves");
        let right = rule
            .derive(&[string_attr("a", "a/b"), string_attr("b", "c")])
            .expect("resolves");
        assert_eq!(left.as_str(), "a/b%2Fc");
        assert_eq!(right.as_str(), "a%2Fb/c");
        let pct = rule
            .derive(&[string_attr("a", "50%"), string_attr("b", "x")])
            .expect("resolves");
        assert_eq!(pct.as_str(), "50%25/x");
    }

    // RFC0045.3 — every rule key is required; the error names the missing one.
    #[test]
    fn composite_rule_rejects_missing_empty_or_non_string_component() {
        let rule = TenantRule::from_keys(["k8s.cluster.name", "service.name"]).expect("valid");
        let missing = [string_attr("service.name", "fluxcd")];
        let err = rule.derive(&missing).unwrap_err();
        assert_eq!(err.attribute(), "k8s.cluster.name");

        let empty = [
            string_attr("k8s.cluster.name", ""),
            string_attr("service.name", "fluxcd"),
        ];
        assert_eq!(
            rule.derive(&empty).unwrap_err().attribute(),
            "k8s.cluster.name"
        );

        let non_string = [
            KeyValue {
                key: "k8s.cluster.name".to_owned(),
                value: Some(AnyValue {
                    value: Some(Value::IntValue(1)),
                }),
                ..Default::default()
            },
            string_attr("service.name", "fluxcd"),
        ];
        assert_eq!(
            rule.derive(&non_string).unwrap_err().attribute(),
            "k8s.cluster.name"
        );
    }

    // RFC0045.1 — rule validation.
    #[test]
    fn from_keys_rejects_empty_and_duplicate() {
        assert_eq!(
            TenantRule::from_keys(Vec::<String>::new()).unwrap_err(),
            TenantRuleError::Empty
        );
        assert_eq!(
            TenantRule::from_keys(["service.name", "service.name"]).unwrap_err(),
            TenantRuleError::Duplicate {
                key: "service.name".to_owned()
            }
        );
        assert_eq!(
            TenantRule::from_keys(["service.name"]).expect("valid"),
            TenantRule::service_name()
        );
    }

    // RFC0045.4 in property form: for a fixed composite rule, distinct
    // value tuples derive distinct tenant ids.
    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(512))]
        #[test]
        fn composite_join_is_injective(
            (left, right) in (2usize..=3).prop_flat_map(|arity| (
                proptest::collection::vec("[a-c/%]{1,4}", arity),
                proptest::collection::vec("[a-c/%]{1,4}", arity),
            )),
        ) {
            let keys: Vec<String> = (0..left.len()).map(|i| format!("k{i}")).collect();
            let rule = TenantRule::from_keys(keys.clone()).expect("valid");
            let attrs = |values: &[String]| -> Vec<KeyValue> {
                keys.iter().zip(values).map(|(k, v)| string_attr(k, v)).collect()
            };
            let l = rule.derive(&attrs(&left)).expect("resolves");
            let r = rule.derive(&attrs(&right)).expect("resolves");
            proptest::prop_assert_eq!(left == right, l == r);
        }
    }
}
