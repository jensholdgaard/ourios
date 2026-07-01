//! The RFC 0020 YAML configuration schema and its environment-substitution walk.
//!
//! [`parse`] turns config-file text into a [`FileConfig`]: it parses the YAML
//! into a node tree, substitutes `${env:…}` references in the scalar **values**
//! ([`env_subst`](super::env_subst)), then deserialises the substituted tree
//! into the schema. Mapping onto the resolved server config (via the existing
//! `build_*` validators — the single validation path, RFC 0020 §3.1) is the
//! `--config` wiring layer in the binary; this module stops at a validated,
//! substituted view of the file.
//!
//! **Substitution operates on parsed scalar values, never raw text** (RFC 0020
//! §3.3): the walk descends the node tree and rewrites only scalar *values*, so
//! mapping keys are left verbatim (rule 4) and a substituted value is inserted
//! as-is and never re-parsed into YAML structure (rule 5 — the security
//! boundary). It is not recursive: [`env_subst::resolve`](super::env_subst::resolve)
//! emits the resolved value without re-scanning it.
//!
//! **Type after substitution** (rule 7) is resolved at the typed boundary rather
//! than by re-tagging the node tree. `serde_yaml`'s `Value` does not preserve a
//! scalar's quoting style, so a literal "re-interpret the substituted scalar by
//! YAML's type rules" pass cannot tell a quoted string from a bare one and would
//! wrongly coerce `"01"` to an integer. Instead every leaf is carried as its
//! string form (a bare `3600` and a substituted `${env:W}`→`3600` both become
//! the string `"3600"`) and the final type is resolved when that string flows
//! through the existing `build_*` validators — the same path the environment
//! values take (§3.1). The observable result is identical for the bounded
//! schema, and a quoted scalar can never be corrupted into a number.
//!
//! See `docs/rfcs/0020-configuration-file.md` §3.3–§3.4.

use std::fmt;

use serde::Deserialize;
use serde_yaml::Value;

use super::env_subst::{self, MalformedReference};

/// A failure loading a configuration file.
///
/// Both variants name only structural locators — a YAML key path or a
/// non-conforming `${…}` reference — never a resolved value, so the error is
/// safe to surface even when a sibling scalar holds a secret (RFC 0020 §3.5 /
/// RFC 0019 §3.4).
#[derive(Debug)]
pub enum FileConfigError {
    /// A `${…}` reference that does not conform to the substitution grammar.
    Substitution(MalformedReference),
    /// A YAML syntax error, an unknown key (`deny_unknown_fields`), or a value
    /// whose shape does not match the schema.
    Schema(serde_yaml::Error),
}

impl fmt::Display for FileConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Substitution(e) => write!(f, "configuration file: {e}"),
            Self::Schema(e) => write!(f, "configuration file: {e}"),
        }
    }
}

impl std::error::Error for FileConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Substitution(e) => Some(e),
            Self::Schema(e) => Some(e),
        }
    }
}

/// The parsed, substituted configuration file (RFC 0020 §3.4).
///
/// Every leaf is an already-substituted scalar in string form; the binary maps
/// these onto the resolved `ServerConfig` through the existing `build_*`
/// validators (RFC 0020 §3.1). Absent sections and fields are the type default
/// (`None` / an empty section), matching an unset environment variable.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FileConfig {
    /// Data + audit store backend (`storage.*`, RFC 0019).
    pub storage: StorageSection,
    /// OTLP receiver role (`receiver.*`, RFC 0003).
    pub receiver: ReceiverSection,
    /// Query role (`querier.*`, RFC 0016).
    pub querier: QuerierSection,
    /// Background compaction (`compaction.*`, RFC 0009).
    pub compaction: CompactionSection,
}

/// `storage.*` — the data + audit store backend selection (RFC 0019 §3.1/§3.2).
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StorageSection {
    /// `local` (default) or `s3`.
    #[serde(deserialize_with = "scalar_opt")]
    pub backend: Option<String>,
    /// S3 addressing + credentials (`storage.s3.*`).
    pub s3: S3Section,
    /// Local-backend store root (`storage.local.*`).
    pub local: LocalSection,
}

/// `storage.s3.*` — S3 addressing and (env-only) credentials (RFC 0019 §3.4).
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct S3Section {
    #[serde(deserialize_with = "scalar_opt")]
    pub bucket: Option<String>,
    #[serde(deserialize_with = "scalar_opt")]
    pub endpoint: Option<String>,
    #[serde(deserialize_with = "scalar_opt")]
    pub region: Option<String>,
    #[serde(deserialize_with = "scalar_opt")]
    pub prefix: Option<String>,
    #[serde(deserialize_with = "scalar_opt")]
    pub access_key_id: Option<String>,
    #[serde(deserialize_with = "scalar_opt")]
    pub secret_access_key: Option<String>,
    #[serde(deserialize_with = "scalar_opt")]
    pub session_token: Option<String>,
}

/// `storage.local.*` — the local store root (RFC 0019 §3.1).
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LocalSection {
    #[serde(deserialize_with = "scalar_opt")]
    pub bucket_root: Option<String>,
}

/// `receiver.*` — the OTLP receiver role (RFC 0003 §6.2).
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReceiverSection {
    #[serde(deserialize_with = "scalar_opt")]
    pub enabled: Option<String>,
    #[serde(deserialize_with = "scalar_opt")]
    pub grpc_addr: Option<String>,
    #[serde(deserialize_with = "scalar_opt")]
    pub http_addr: Option<String>,
    #[serde(deserialize_with = "scalar_opt")]
    pub wal_root: Option<String>,
}

/// `querier.*` — the query role (RFC 0016 §3.2).
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct QuerierSection {
    #[serde(deserialize_with = "scalar_opt")]
    pub enabled: Option<String>,
    #[serde(deserialize_with = "scalar_opt")]
    pub http_addr: Option<String>,
    #[serde(deserialize_with = "scalar_opt")]
    pub default_window_secs: Option<String>,
}

/// `compaction.*` — the background compaction sweep (RFC 0009 §3.2).
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CompactionSection {
    #[serde(deserialize_with = "scalar_opt")]
    pub enabled: Option<String>,
    #[serde(deserialize_with = "scalar_opt")]
    pub interval_secs: Option<String>,
}

/// Parse configuration-file text into a validated, substituted [`FileConfig`].
///
/// `lookup` resolves an environment-variable name for `${env:…}` substitution
/// (`None` when unset); the binary passes `|n| std::env::var(n).ok()`. The file
/// is parsed into a node tree, scalar values are substituted, and the result is
/// deserialised into the schema — a strict pass (unknown keys are rejected,
/// RFC 0020 §3.4).
///
/// # Errors
///
/// Returns [`FileConfigError::Substitution`] for a malformed `${…}` reference in
/// a scalar value (RFC0020.5), or [`FileConfigError::Schema`] for a YAML syntax
/// error, an unknown key, or a value that does not fit the schema. Resolution is
/// all-or-nothing: on error no partial configuration is produced.
pub fn parse(
    yaml: &str,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<FileConfig, FileConfigError> {
    let mut tree: Value = serde_yaml::from_str(yaml).map_err(FileConfigError::Schema)?;
    // An empty document parses to `Null`; treat it as an all-default config
    // rather than a type error (deserialising `Null` into a struct fails).
    if tree.is_null() {
        return Ok(FileConfig::default());
    }
    substitute_tree(&mut tree, lookup).map_err(FileConfigError::Substitution)?;
    serde_yaml::from_value(tree).map_err(FileConfigError::Schema)
}

/// Substitute `${env:…}` references in every scalar **value** of the tree.
///
/// Mapping keys are never visited (rule 4 — a `${…}` in a key position is left
/// verbatim), and a substituted value is a `String` that is never re-parsed into
/// YAML structure (rule 5). Non-string scalars (`Number`/`Bool`/`Null`) hold no
/// `$` and are left untouched.
fn substitute_tree(
    value: &mut Value,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<(), MalformedReference> {
    match value {
        Value::String(s) => {
            *s = env_subst::resolve(s, lookup)?;
        }
        Value::Mapping(map) => {
            for (_key, val) in map.iter_mut() {
                substitute_tree(val, lookup)?;
            }
        }
        Value::Sequence(seq) => {
            for val in seq {
                substitute_tree(val, lookup)?;
            }
        }
        Value::Tagged(tagged) => substitute_tree(&mut tagged.value, lookup)?,
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
    Ok(())
}

/// Deserialise an optional YAML scalar into its string form.
///
/// A scalar of any type (string, boolean, number) is rendered as text so a bare
/// `interval_secs: 300` and a substituted `${env:I}` both reach the `build_*`
/// validators as `"300"` (the type-after-substitution model, RFC 0020 §3.3
/// rule 7 — see the module docs). A mapping or sequence where a scalar is
/// expected is a schema error.
fn scalar_opt<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<Scalar>::deserialize(deserializer)?.map(|s| s.0))
}

/// A YAML scalar captured as its string form (see [`scalar_opt`]).
struct Scalar(String);

impl<'de> Deserialize<'de> for Scalar {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct ScalarVisitor;

        impl serde::de::Visitor<'_> for ScalarVisitor {
            type Value = Scalar;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a scalar value (string, boolean, or number)")
            }

            fn visit_str<E>(self, v: &str) -> Result<Scalar, E> {
                Ok(Scalar(v.to_owned()))
            }

            fn visit_string<E>(self, v: String) -> Result<Scalar, E> {
                Ok(Scalar(v))
            }

            fn visit_bool<E>(self, v: bool) -> Result<Scalar, E> {
                Ok(Scalar(v.to_string()))
            }

            fn visit_i64<E>(self, v: i64) -> Result<Scalar, E> {
                Ok(Scalar(v.to_string()))
            }

            fn visit_u64<E>(self, v: u64) -> Result<Scalar, E> {
                Ok(Scalar(v.to_string()))
            }

            fn visit_f64<E>(self, v: f64) -> Result<Scalar, E> {
                Ok(Scalar(v.to_string()))
            }
        }

        deserializer.deserialize_any(ScalarVisitor)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{FileConfigError, parse, substitute_tree};
    use serde_yaml::Value;

    /// A lookup over a fixed environment map.
    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |name: &str| map.get(name).cloned()
    }

    /// RFC0020.2 — `${env:NAME}`/`${NAME}` in scalar values resolve; `:-default`
    /// applies on unset/empty; an undefined ref with no default becomes empty;
    /// `$$` is a literal `$`. Type-after-substitution is exercised where a
    /// numeric field carries a reference (`default_window_secs`).
    #[test]
    fn scalar_values_are_substituted() {
        let lookup = env(&[
            ("BUCKET", "logs"),
            ("REGION", "eu-west-1"),
            ("WINDOW", "1800"),
        ]);
        let yaml = "
storage:
  backend: ${env:BACKEND:-s3}
  s3:
    bucket: ${BUCKET}
    region: ${env:REGION}
    endpoint: ${env:MISSING:-}
    prefix: a$$b
querier:
  enabled: ${env:QUERIER_ON:-true}
  default_window_secs: ${env:WINDOW}
";
        let cfg = parse(yaml, &lookup).expect("valid");
        assert_eq!(cfg.storage.backend.as_deref(), Some("s3")); // default applied
        assert_eq!(cfg.storage.s3.bucket.as_deref(), Some("logs"));
        assert_eq!(cfg.storage.s3.region.as_deref(), Some("eu-west-1"));
        assert_eq!(cfg.storage.s3.endpoint.as_deref(), Some("")); // undefined, no default → empty
        assert_eq!(cfg.storage.s3.prefix.as_deref(), Some("a$b")); // $$ → literal $
        assert_eq!(cfg.querier.enabled.as_deref(), Some("true"));
        assert_eq!(cfg.querier.default_window_secs.as_deref(), Some("1800"));
    }

    /// RFC0020.2 — native YAML scalars (a bare integer / boolean, no reference)
    /// reach the schema as their string form, so a literal and a substituted
    /// value are indistinguishable downstream (type-after-substitution).
    #[test]
    fn native_scalars_become_their_string_form() {
        let lookup = env(&[]);
        let yaml = "
querier:
  enabled: true
  default_window_secs: 3600
compaction:
  interval_secs: 300
";
        let cfg = parse(yaml, &lookup).expect("valid");
        assert_eq!(cfg.querier.enabled.as_deref(), Some("true"));
        assert_eq!(cfg.querier.default_window_secs.as_deref(), Some("3600"));
        assert_eq!(cfg.compaction.interval_secs.as_deref(), Some("300"));
    }

    /// RFC0020.2 rule 4 — substitution never touches a mapping **key**; a
    /// `${…}` in key position is left verbatim (asserted at the tree level, as
    /// the typed schema has no free-form-key field to carry one).
    #[test]
    fn mapping_keys_are_left_verbatim() {
        let lookup = env(&[("K", "resolved"), ("V", "resolved")]);
        let mut tree: Value = serde_yaml::from_str("${env:K}: ${env:V}").expect("yaml");
        substitute_tree(&mut tree, &lookup).expect("ok");
        let map = tree.as_mapping().expect("a mapping");
        let (key, value) = map.iter().next().expect("one entry");
        assert_eq!(key.as_str(), Some("${env:K}"), "the key is verbatim");
        assert_eq!(value.as_str(), Some("resolved"), "the value is substituted");
    }

    /// RFC0020.2 rule 5 — a substituted value is inserted as-is and never
    /// re-parsed into YAML structure: a value that itself looks like a mapping
    /// stays a single scalar string, injecting no keys.
    #[test]
    fn substituted_values_do_not_inject_structure() {
        let lookup = env(&[("INJECT", "evil: true\nkey: value")]);
        let cfg = parse("storage:\n  backend: ${env:INJECT}\n", &lookup).expect("valid");
        assert_eq!(
            cfg.storage.backend.as_deref(),
            Some("evil: true\nkey: value"),
            "the value is a scalar string, not a parsed mapping",
        );
    }

    /// RFC0020.5 (partial) — a malformed `${…}` reference in a scalar value is a
    /// whole-file error naming the reference, never a resolved value.
    #[test]
    fn malformed_reference_is_an_error() {
        let lookup = env(&[]);
        let err = parse("storage:\n  backend: ${1BAD}\n", &lookup).expect_err("malformed");
        assert!(
            matches!(err, FileConfigError::Substitution(_)),
            "got {err:?}",
        );
        assert!(err.to_string().contains("${1BAD}"), "names the reference");
    }

    /// RFC0020.5 (partial) — an unknown key is a schema error (strict parse,
    /// `deny_unknown_fields`), naming the offending key.
    #[test]
    fn unknown_key_is_rejected() {
        let lookup = env(&[]);
        let err = parse("storage:\n  backsend: s3\n", &lookup).expect_err("typo");
        assert!(matches!(err, FileConfigError::Schema(_)), "got {err:?}");
        assert!(err.to_string().contains("backend"), "suggests the schema");
    }

    /// An unknown **top-level** section is likewise rejected.
    #[test]
    fn unknown_top_level_section_is_rejected() {
        let lookup = env(&[]);
        let err = parse("queriar:\n  enabled: true\n", &lookup).expect_err("typo");
        assert!(matches!(err, FileConfigError::Schema(_)), "got {err:?}");
    }

    /// A structure where a scalar is expected (a mapping under a scalar field)
    /// is a schema error, not a silent stringification.
    #[test]
    fn a_structure_where_a_scalar_is_expected_errors() {
        let lookup = env(&[]);
        let err =
            parse("storage:\n  backend:\n    nested: true\n", &lookup).expect_err("not a scalar");
        assert!(matches!(err, FileConfigError::Schema(_)), "got {err:?}");
    }

    /// An empty document is an all-default config (every role unset), not an
    /// error — the equivalent of an empty environment.
    #[test]
    fn empty_document_is_all_default() {
        let lookup = env(&[]);
        let cfg = parse("", &lookup).expect("empty is valid");
        assert!(cfg.storage.backend.is_none());
        assert!(cfg.receiver.enabled.is_none());
        assert!(cfg.querier.enabled.is_none());
        assert!(cfg.compaction.enabled.is_none());
    }

    /// An omitted section leaves its fields unset (`None`), matching an unset
    /// environment variable — the schema does not require every section.
    #[test]
    fn omitted_sections_default_to_unset() {
        let lookup = env(&[("ROOT", "/var/lib/ourios")]);
        let cfg = parse(
            "storage:\n  local:\n    bucket_root: ${env:ROOT}\n",
            &lookup,
        )
        .expect("valid");
        assert_eq!(
            cfg.storage.local.bucket_root.as_deref(),
            Some("/var/lib/ourios")
        );
        assert!(cfg.receiver.enabled.is_none());
        assert!(cfg.querier.enabled.is_none());
    }
}
