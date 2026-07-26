//! RFC 0022 — promoted attribute columns (typed classes per RFC 0042).
//!
//! A promoted key is projected at write time from its canonical-JSON
//! attribute column into a dedicated `OPTIONAL` column named literally
//! after the DSL path (`resource.<key>` / `attr.<key>`), so attribute
//! predicates can prune row groups instead of scanning JSON (RFC 0022
//! §3.1). RFC 0042 adds per-key **classes**: `string` (Utf8, the RFC
//! 0022 behaviour), `i64` (Int64) and `f64` (Float64). The JSON columns
//! remain the source of truth: a promoted cell is a query-only
//! projection, never read back into a
//! [`MinedRecord`](ourios_core::record::MinedRecord).

use arrow_schema::{DataType, Field};
use ourios_core::otlp::{KeyValue, any_value};

/// The resource key that is always promoted (RFC 0022 §3.1): the
/// `Required`, `Stable` identity attribute of the `OTel` `service`
/// resource entity, surfaced in the DSL as the bare `service` field.
pub const SERVICE_NAME_KEY: &str = "service.name";

/// Column-name prefix for promoted resource-attribute keys. Public because
/// the query-side compile (RFC 0022 §3.3) derives promoted column names from
/// the same prefixes the writer declares.
pub const RESOURCE_PREFIX: &str = "resource.";
/// Column-name prefix for promoted log-attribute keys (see
/// [`RESOURCE_PREFIX`]).
pub const ATTR_PREFIX: &str = "attr.";

/// The RFC 0042 §3.1 promotion class of a key: which `AnyValue`
/// variant(s) project into which Arrow column type. Everything outside
/// a class's row projects `NULL` — never a parse, never a coercion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PromotedClass {
    /// String `AnyValue` → `Utf8`. The RFC 0022 behaviour, unchanged.
    #[default]
    String,
    /// Int `AnyValue` → `Int64`. Doubles do **not** narrow.
    I64,
    /// Double or int `AnyValue` → `Float64` (ints widen — exact for
    /// `|v| ≤ 2^53`; a key expected to exceed that belongs in
    /// [`PromotedClass::I64`]).
    F64,
}

impl PromotedClass {
    /// The Arrow type of a column of this class (RFC 0042 §3.1 table).
    #[must_use]
    pub fn data_type(self) -> DataType {
        match self {
            Self::String => DataType::Utf8,
            Self::I64 => DataType::Int64,
            Self::F64 => DataType::Float64,
        }
    }
}

/// One promoted key with its declared class (RFC 0042 §3.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromotedKey {
    /// The attribute key, taken literally (no globbing).
    pub key: String,
    /// The projection class; [`PromotedClass::String`] for RFC 0022
    /// bare-string config entries.
    pub class: PromotedClass,
}

impl PromotedKey {
    /// A string-class key — the RFC 0022 spelling.
    #[must_use]
    pub fn string(key: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            class: PromotedClass::String,
        }
    }
}

/// The effective promoted attribute key set (RFC 0022 §3.1/§3.2, typed
/// per RFC 0042).
///
/// `service.name` is implicit, non-removable, and always
/// string-class: `resource_keys()` yields it first regardless of the
/// configured set (a configured `service.name` collapses into it, and
/// cannot re-type it — RFC 0042 §3.2). The configured keys come from
/// `storage.promoted_attributes` (an RFC 0020 schema extension) and
/// are deduplicated by key preserving first occurrence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromotedAttributes {
    resource: Vec<PromotedKey>,
    log: Vec<PromotedKey>,
}

impl Default for PromotedAttributes {
    /// The empty configured set — `service.name` only.
    fn default() -> Self {
        Self::new(std::iter::empty::<String>(), std::iter::empty::<String>())
    }
}

impl PromotedAttributes {
    /// Build the effective set from bare (string-class) resource and
    /// log keys — the RFC 0022 constructor, unchanged for existing
    /// callers.
    pub fn new(
        resource: impl IntoIterator<Item = String>,
        log: impl IntoIterator<Item = String>,
    ) -> Self {
        Self::new_typed(
            resource.into_iter().map(PromotedKey::string),
            log.into_iter().map(PromotedKey::string),
        )
    }

    /// Build the effective set from typed keys (RFC 0042 §3.2). The
    /// implicit string-class `service.name` is prepended to the
    /// resource keys; duplicate keys (including a configured
    /// `service.name`, whatever class it claims) collapse, preserving
    /// the first occurrence — so `service.name` cannot be re-typed
    /// here. Config-level rejection of duplicates is the server's job
    /// (RFC0042.6); this constructor stays total.
    pub fn new_typed(
        resource: impl IntoIterator<Item = PromotedKey>,
        log: impl IntoIterator<Item = PromotedKey>,
    ) -> Self {
        fn dedup_preserving_order(
            implicit: impl IntoIterator<Item = PromotedKey>,
            keys: impl IntoIterator<Item = PromotedKey>,
        ) -> Vec<PromotedKey> {
            let mut seen = std::collections::HashSet::new();
            implicit
                .into_iter()
                .chain(keys)
                .filter(|k| seen.insert(k.key.clone()))
                .collect()
        }
        Self {
            resource: dedup_preserving_order([PromotedKey::string(SERVICE_NAME_KEY)], resource),
            log: dedup_preserving_order([], log),
        }
    }

    /// The promoted resource-attribute keys, `service.name` first.
    #[must_use]
    pub fn resource_keys(&self) -> &[PromotedKey] {
        &self.resource
    }

    /// The promoted log-attribute keys (configured only).
    #[must_use]
    pub fn log_keys(&self) -> &[PromotedKey] {
        &self.log
    }

    /// The promoted column names in schema order: `resource.<key>`
    /// columns first (`resource.service.name` leading), then
    /// `attr.<key>` columns.
    pub fn column_names(&self) -> impl Iterator<Item = String> + '_ {
        self.resource
            .iter()
            .map(|k| format!("{RESOURCE_PREFIX}{}", k.key))
            .chain(self.log.iter().map(|k| format!("{ATTR_PREFIX}{}", k.key)))
    }

    /// The promoted columns as Arrow fields — `OPTIONAL`, typed by
    /// each key's class (RFC 0022 §3.1 / RFC 0042 §3.1) — in
    /// [`Self::column_names`] order.
    #[must_use]
    pub fn fields(&self) -> Vec<Field> {
        self.resource
            .iter()
            .chain(self.log.iter())
            .zip(self.column_names())
            .map(|(k, name)| Field::new(name, k.class.data_type(), true))
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
    attrs.iter().find(|kv| kv.key == key).and_then(string_value)
}

/// The §3.1 value projection of a single attribute: the payload **iff**
/// the `AnyValue` is a string, `None` for any other variant (or none).
#[must_use]
pub fn string_value(kv: &KeyValue) -> Option<&str> {
    match kv.value.as_ref().and_then(|v| v.value.as_ref()) {
        Some(any_value::Value::StringValue(s)) => Some(s.as_str()),
        _ => None,
    }
}

/// The RFC 0042 §3.1 `i64`-class projection: the payload **iff** the
/// `AnyValue` is an int. Doubles do not narrow, strings do not parse.
#[must_use]
pub fn i64_value(kv: &KeyValue) -> Option<i64> {
    match kv.value.as_ref().and_then(|v| v.value.as_ref()) {
        Some(any_value::Value::IntValue(v)) => Some(*v),
        _ => None,
    }
}

/// The RFC 0042 §3.1 `f64`-class projection: the payload for a double
/// `AnyValue`, an int widened for an int `AnyValue` (exact for
/// `|v| ≤ 2^53`). Strings do not parse.
#[must_use]
pub fn f64_value(kv: &KeyValue) -> Option<f64> {
    match kv.value.as_ref().and_then(|v| v.value.as_ref()) {
        Some(any_value::Value::DoubleValue(v)) => Some(*v),
        #[allow(clippy::cast_precision_loss)] // §3.1: widening is the documented contract
        Some(any_value::Value::IntValue(v)) => Some(*v as f64),
        _ => None,
    }
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

    fn kv_double(key: &str, value: f64) -> KeyValue {
        KeyValue {
            key: key.to_string(),
            value: Some(AnyValue {
                value: Some(any_value::Value::DoubleValue(value)),
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
        assert_eq!(
            p.resource_keys().iter().map(|k| &k.key).collect::<Vec<_>>(),
            ["service.name", "k8s.namespace.name"]
        );
        assert_eq!(
            p.log_keys().iter().map(|k| &k.key).collect::<Vec<_>>(),
            ["http.route"]
        );
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
        assert_eq!(p.resource_keys().len(), 1);
        assert_eq!(p.resource_keys()[0].key, SERVICE_NAME_KEY);
        assert_eq!(p.resource_keys()[0].class, PromotedClass::String);
        assert!(p.log_keys().is_empty());
    }

    #[test]
    fn fields_are_optional_and_class_typed() {
        let p = PromotedAttributes::new_typed(
            [],
            [
                PromotedKey::string("model"),
                PromotedKey {
                    key: "cost_usd".into(),
                    class: PromotedClass::F64,
                },
                PromotedKey {
                    key: "input_tokens".into(),
                    class: PromotedClass::I64,
                },
            ],
        );
        let fields = p.fields();
        let by_name: Vec<(&str, &DataType, bool)> = fields
            .iter()
            .map(|f| (f.name().as_str(), f.data_type(), f.is_nullable()))
            .collect();
        assert_eq!(
            by_name,
            [
                ("resource.service.name", &DataType::Utf8, true),
                ("attr.model", &DataType::Utf8, true),
                ("attr.cost_usd", &DataType::Float64, true),
                ("attr.input_tokens", &DataType::Int64, true),
            ]
        );
    }

    #[test]
    fn service_name_cannot_be_retyped_via_new_typed() {
        let p = PromotedAttributes::new_typed(
            [PromotedKey {
                key: SERVICE_NAME_KEY.into(),
                class: PromotedClass::I64,
            }],
            [],
        );
        assert_eq!(p.resource_keys()[0].class, PromotedClass::String);
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

    #[test]
    fn i64_projects_ints_only() {
        assert_eq!(i64_value(&kv_int("k", 42)), Some(42));
        assert_eq!(i64_value(&kv_double("k", 42.0)), None);
        assert_eq!(i64_value(&kv_str("k", "42")), None);
    }

    #[test]
    fn f64_projects_doubles_and_widens_ints_never_parses() {
        assert_eq!(f64_value(&kv_double("k", 0.188_796)), Some(0.188_796));
        assert_eq!(f64_value(&kv_int("k", 42)), Some(42.0));
        assert_eq!(f64_value(&kv_str("k", "0.5")), None);
    }
}
