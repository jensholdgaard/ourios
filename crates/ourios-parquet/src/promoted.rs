//! RFC 0022 — promoted attribute columns.
//!
//! A promoted key is projected at write time from its canonical-JSON
//! attribute column into a dedicated `OPTIONAL` Utf8 column named
//! literally after the DSL path (`resource.<key>` / `attr.<key>`), so
//! attribute predicates can prune row groups instead of scanning JSON
//! (RFC 0022 §3.1). The JSON columns remain the source of truth: a
//! promoted cell is a query-only projection, never read back into a
//! [`MinedRecord`](ourios_core::record::MinedRecord).

use arrow_schema::{DataType, Field};
use ourios_core::otlp::{KeyValue, any_value};

/// The resource key that is always promoted (RFC 0022 §3.1): the
/// `Required`, `Stable` identity attribute of the OTel `service`
/// resource entity, surfaced in the DSL as the bare `service` field.
pub const SERVICE_NAME_KEY: &str = "service.name";

/// Column-name prefix for promoted resource-attribute keys.
const RESOURCE_PREFIX: &str = "resource.";
/// Column-name prefix for promoted log-attribute keys.
const ATTR_PREFIX: &str = "attr.";

/// The effective promoted attribute key set (RFC 0022 §3.1/§3.2).
///
/// `service.name` is implicit and non-removable: `resource_keys()`
/// always yields it first, regardless of the configured set. The
/// configured keys come from `storage.promoted_attributes` (an RFC
/// 0020 schema extension) and are deduplicated preserving first
/// occurrence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromotedAttributes {
    resource: Vec<String>,
    log: Vec<String>,
}

impl Default for PromotedAttributes {
    /// The empty configured set — `service.name` only.
    fn default() -> Self {
        Self::new(std::iter::empty::<String>(), std::iter::empty::<String>())
    }
}

impl PromotedAttributes {
    /// Build the effective set from the configured resource and log
    /// keys. The implicit `service.name` is prepended to the resource
    /// keys; duplicates (including a configured `service.name`)
    /// collapse, preserving first occurrence.
    pub fn new(
        resource: impl IntoIterator<Item = String>,
        log: impl IntoIterator<Item = String>,
    ) -> Self {
        let mut resource_keys = vec![SERVICE_NAME_KEY.to_string()];
        for key in resource {
            if !resource_keys.contains(&key) {
                resource_keys.push(key);
            }
        }
        let mut log_keys: Vec<String> = Vec::new();
        for key in log {
            if !log_keys.contains(&key) {
                log_keys.push(key);
            }
        }
        Self {
            resource: resource_keys,
            log: log_keys,
        }
    }

    /// The promoted resource-attribute keys, `service.name` first.
    #[must_use]
    pub fn resource_keys(&self) -> &[String] {
        &self.resource
    }

    /// The promoted log-attribute keys (configured only).
    #[must_use]
    pub fn log_keys(&self) -> &[String] {
        &self.log
    }

    /// The promoted column names in schema order: `resource.<key>`
    /// columns first (`resource.service.name` leading), then
    /// `attr.<key>` columns.
    pub fn column_names(&self) -> impl Iterator<Item = String> + '_ {
        self.resource
            .iter()
            .map(|k| format!("{RESOURCE_PREFIX}{k}"))
            .chain(self.log.iter().map(|k| format!("{ATTR_PREFIX}{k}")))
    }

    /// The promoted columns as Arrow fields (RFC 0022 §3.1: `OPTIONAL`
    /// Utf8 — Parquet `STRING` logical type over `BYTE_ARRAY`), in
    /// [`Self::column_names`] order.
    #[must_use]
    pub fn fields(&self) -> Vec<Field> {
        self.column_names()
            .map(|name| Field::new(name, DataType::Utf8, true))
            .collect()
    }
}

/// Project one promoted key out of an attribute list (RFC 0022 §3.1):
/// the value **iff** the key is present with a string `AnyValue`;
/// `None` (a `NULL` cell) when the key is absent or its value is any
/// other `AnyValue` variant. First occurrence wins, mirroring the
/// first-match semantics of the query-side JSON `LIKE` arm.
#[must_use]
pub fn project_string_value<'a>(attrs: &'a [KeyValue], key: &str) -> Option<&'a str> {
    attrs.iter().find(|kv| kv.key == key).and_then(|kv| {
        match kv.value.as_ref().and_then(|v| v.value.as_ref()) {
            Some(any_value::Value::StringValue(s)) => Some(s.as_str()),
            _ => None,
        }
    })
}

#[cfg(test)]
mod tests {
    use ourios_core::otlp::AnyValue;

    use super::*;

    fn kv_str(key: &str, value: &str) -> KeyValue {
        KeyValue {
            key: key.to_string(),
            value: Some(AnyValue {
                value: Some(any_value::Value::StringValue(value.to_string())),
            }),
            ..Default::default()
        }
    }

    fn kv_int(key: &str, value: i64) -> KeyValue {
        KeyValue {
            key: key.to_string(),
            value: Some(AnyValue {
                value: Some(any_value::Value::IntValue(value)),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn service_name_is_implicit_first_and_deduplicated() {
        let p = PromotedAttributes::new(
            ["service.name".to_string(), "k8s.namespace.name".to_string()],
            ["http.route".to_string(), "http.route".to_string()],
        );
        assert_eq!(p.resource_keys(), ["service.name", "k8s.namespace.name"]);
        assert_eq!(p.log_keys(), ["http.route"]);
        assert_eq!(
            p.column_names().collect::<Vec<_>>(),
            [
                "resource.service.name",
                "resource.k8s.namespace.name",
                "attr.http.route"
            ]
        );
    }

    #[test]
    fn default_set_is_service_name_only() {
        let p = PromotedAttributes::default();
        assert_eq!(p.resource_keys(), [SERVICE_NAME_KEY]);
        assert!(p.log_keys().is_empty());
    }

    #[test]
    fn fields_are_optional_utf8() {
        for f in PromotedAttributes::default().fields() {
            assert_eq!(*f.data_type(), DataType::Utf8);
            assert!(f.is_nullable());
        }
    }

    #[test]
    fn projection_is_string_only_first_match() {
        let attrs = [
            kv_int("http.status_code", 500),
            kv_str("service.name", "api"),
            kv_str("service.name", "shadowed"),
        ];
        assert_eq!(project_string_value(&attrs, "service.name"), Some("api"));
        assert_eq!(project_string_value(&attrs, "http.status_code"), None);
        assert_eq!(project_string_value(&attrs, "absent"), None);
    }
}
