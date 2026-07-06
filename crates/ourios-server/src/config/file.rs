//! The RFC 0020 YAML configuration schema and its environment substitution.
//!
//! [`parse`] turns config-file text into a [`FileConfig`]: it deserialises the
//! YAML into the schema, then substitutes `${env:…}` references in the scalar
//! **values** ([`env_subst`]). Mapping onto the resolved server config (via the
//! existing `build_*` validators — the single validation path, RFC 0020 §3.1) is
//! the `--config` wiring layer in the binary; this module stops at a validated,
//! substituted view of the file.
//!
//! **Order: validate the schema, then substitute.** Deserialisation runs on the
//! *raw* (pre-substitution) YAML text, so a shape or unknown-key error references
//! the file's own text — a bare `${env:SECRET}` written where a section is
//! expected is reported as `invalid type: string "${env:SECRET}", …`, naming the
//! reference, never a resolved secret (RFC 0020 §3.5 / RFC 0019 §3.4). `serde`
//! never sees a substituted value. Substitution then rewrites the typed scalar
//! leaves in place — the parsed *values* only, so mapping keys (which became
//! field names) are never candidates (rule 4), and a substituted value stays in
//! its `Option<String>` field, never re-parsed into YAML structure (rule 5, the
//! security boundary). It is not recursive: [`env_subst::resolve`] emits the
//! resolved value without re-scanning it.
//!
//! **Type after substitution** (rule 7) is resolved at the typed boundary rather
//! than by re-tagging a node tree. `serde_yaml`'s `Value` does not preserve a
//! scalar's quoting style, so a literal "re-interpret the substituted scalar by
//! YAML's type rules" pass cannot tell a quoted string from a bare one and would
//! wrongly coerce `"01"` to an integer. Instead every leaf is captured as its
//! string form (a bare `3600` and a substituted `${env:W}`→`3600` both become
//! the string `"3600"`) and the final type is resolved when that string flows
//! through the existing `build_*` validators — the same path the environment
//! values take (§3.1). The observable result is identical for the bounded
//! schema, and a quoted scalar can never be corrupted into a number.
//!
//! See `docs/rfcs/0020-configuration-file.md` §3.3–§3.4.

use std::fmt;

use serde::Deserialize;

use super::env_subst::{self, MalformedReference};

/// Substitute `${env:…}` in one optional scalar leaf in place (RFC 0020 §3.3).
fn substitute(
    field: &mut Option<String>,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<(), MalformedReference> {
    if let Some(value) = field {
        *value = env_subst::resolve(value, lookup)?;
    }
    Ok(())
}

/// A failure loading a configuration file.
///
/// Every variant names only structural locators — a YAML key path or a
/// non-conforming `${…}` reference — never a resolved value, so the error is
/// safe to surface even when a sibling scalar holds a secret (RFC 0020 §3.5 /
/// RFC 0019 §3.4).
///
/// `Display` forwards to the underlying error where there is one; the caller
/// supplies the file context (e.g. `config file <path>: <this>`), so the two do
/// not stack.
///
/// `#[non_exhaustive]` — this enum has grown variants across the RFC 0020 green
/// slices; forcing a wildcard arm keeps further additions non-breaking for
/// downstream matches (the codebase's public-error-enum convention, e.g.
/// `ourios_miner::tokenize::TokenizeError`).
#[derive(Debug)]
#[non_exhaustive]
pub enum FileConfigError {
    /// A `${…}` reference that does not conform to the substitution grammar.
    Substitution(MalformedReference),
    /// A YAML syntax error, an unknown key (`deny_unknown_fields`), or a value
    /// whose shape does not match the schema.
    Schema(serde_yaml::Error),
    /// A `storage.s3.*` credential holds an inline literal instead of an
    /// `${env:…}` reference (RFC 0020 §3.5). Names the offending key only, never
    /// the value.
    InlineCredential {
        /// The offending `storage.s3.*` credential field name.
        key: &'static str,
    },
    /// An `auth.tokens[…].token` holds an inline literal instead of an
    /// `${env:…}` reference (RFC 0026 §3.1). Names the entry's position only,
    /// never the value.
    InlineToken {
        /// The offending entry's index in `auth.tokens`.
        index: usize,
    },
}

impl fmt::Display for FileConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Substitution(e) => e.fmt(f),
            Self::Schema(e) => e.fmt(f),
            Self::InlineCredential { key } => write!(
                f,
                "storage.s3.{key} must be an ${{env:…}} reference, not an inline \
                 literal (RFC 0020 §3.5)"
            ),
            Self::InlineToken { index } => write!(
                f,
                "auth.tokens[{index}].token must be an ${{env:…}} reference, not \
                 an inline literal (RFC 0026 §3.1)"
            ),
        }
    }
}

impl std::error::Error for FileConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Substitution(e) => Some(e),
            Self::Schema(e) => Some(e),
            Self::InlineCredential { .. } | Self::InlineToken { .. } => None,
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
    /// Bearer-token authentication (`auth.*`, RFC 0026). `Option` because
    /// presence is meaningful: an absent section is open mode, a present one
    /// enables enforcement (and an empty token list inside it is a startup
    /// error) — see RFC 0026 §3.1.
    pub auth: Option<AuthSection>,
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
    /// Promoted attribute key sets (`storage.promoted_attributes.*`,
    /// RFC 0022 §3.2 — an RFC 0020 schema extension).
    pub promoted_attributes: PromotedAttributesSection,
}

/// `storage.promoted_attributes.*` — the RFC 0022 §3.2 promoted attribute
/// key sets. Keys are plain attribute-key strings, taken literally (no
/// globbing); the implicit `service.name` promotion never needs listing.
/// Defaults: empty — promotion beyond `service.name` is opt-in.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PromotedAttributesSection {
    /// Resource-attribute keys to promote (`resource.<key>` columns).
    #[serde(deserialize_with = "scalar_vec")]
    pub resource: Vec<String>,
    /// Log-attribute keys to promote (`attr.<key>` columns).
    #[serde(deserialize_with = "scalar_vec")]
    pub log: Vec<String>,
}

/// `storage.s3.*` — S3 addressing and (env-only) credentials (RFC 0019 §3.4).
///
/// The credential fields are **secret**: the manual [`fmt::Debug`] impl redacts
/// their values (showing only presence), mirroring `ourios_parquet::S3Config` so
/// a `Debug` rendering never leaks a key (RFC 0020 §3.5 / RFC 0019 §3.4).
#[derive(Default, Deserialize)]
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

impl fmt::Debug for S3Section {
    /// Redacts the credential fields — a `Debug` rendering shows only whether a
    /// credential is present, never its value (RFC 0020 §3.5 / RFC 0019 §3.4).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let redact = |v: &Option<String>| v.as_ref().map(|_| "<redacted>");
        f.debug_struct("S3Section")
            .field("bucket", &self.bucket)
            .field("endpoint", &self.endpoint)
            .field("region", &self.region)
            .field("prefix", &self.prefix)
            .field("access_key_id", &redact(&self.access_key_id))
            .field("secret_access_key", &redact(&self.secret_access_key))
            .field("session_token", &redact(&self.session_token))
            .finish()
    }
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
    /// The RFC 0027 MCP surface (`querier.mcp.*`).
    pub mcp: McpSection,
}

/// `querier.mcp.*` — the RFC 0027 MCP surface (§3.1; default off).
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpSection {
    #[serde(deserialize_with = "scalar_opt")]
    pub enabled: Option<String>,
}

/// `auth.*` — bearer-token authentication and tenant binding (RFC 0026 §3.1).
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuthSection {
    /// The configured tokens. Empty (or absent inside a present `auth`
    /// section) is a startup configuration error — a locked-out server is
    /// never the intent (RFC 0026 §3.1).
    pub tokens: Vec<TokenEntry>,
}

/// One `auth.tokens[…]` entry (RFC 0026 §3.1).
///
/// The `token` field is **secret**: the manual [`fmt::Debug`] impl redacts its
/// value (showing only presence), mirroring [`S3Section`], and [`parse`]
/// rejects an inline literal — the file may hold it only as an `${env:…}`
/// reference.
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TokenEntry {
    /// Audit/metric label for this token — never secret (RFC 0026 §3.4).
    #[serde(deserialize_with = "scalar_opt")]
    pub name: Option<String>,
    /// The bearer token value (**secret**; `${env:…}` reference only).
    #[serde(deserialize_with = "scalar_opt")]
    pub token: Option<String>,
    /// The allowed tenant set: exact tenant ids, or the single wildcard `"*"`.
    #[serde(deserialize_with = "scalar_vec")]
    pub tenants: Vec<String>,
}

impl fmt::Debug for TokenEntry {
    /// Redacts the token value — a `Debug` rendering shows only whether it is
    /// present, never its value (RFC 0026 §3.1 / RFC 0020 §3.5).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenEntry")
            .field("name", &self.name)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("tenants", &self.tenants)
            .finish()
    }
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
/// is deserialised into the schema — a strict pass (unknown keys are rejected,
/// RFC 0020 §3.4) — on the **raw** tree, so a schema error references the file's
/// own text rather than a resolved value; substitution then runs on the typed
/// scalar leaves (see the module docs).
///
/// # Errors
///
/// Returns [`FileConfigError::Schema`] for a YAML syntax error, an unknown key,
/// or a value that does not fit the schema; [`FileConfigError::InlineCredential`]
/// for an object-store credential given as an inline literal rather than an
/// `${env:…}` reference (RFC 0020 §3.5); or [`FileConfigError::Substitution`] for
/// a malformed `${…}` reference in a scalar value (RFC0020.5). Resolution is
/// all-or-nothing: on error no partial configuration is produced.
pub fn parse(
    yaml: &str,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<FileConfig, FileConfigError> {
    // Deserialise straight from the text (not via an intermediate `serde_yaml::
    // Value`) so a schema error keeps its source location. Validation runs on the
    // raw (pre-substitution) text, so any shape / unknown-key error names the
    // file's own text, never a resolved secret (RFC 0020 §3.5). `Option` lets an
    // empty / null document resolve to an all-default config (`None`) rather than
    // fail the `null`-into-struct type check.
    let mut config: FileConfig = serde_yaml::from_str::<Option<FileConfig>>(yaml)
        .map_err(FileConfigError::Schema)?
        .unwrap_or_default();
    // Enforce §3.5 on the *raw* credential values — after substitution a
    // reference is indistinguishable from a literal.
    check_credentials_are_references(&config.storage.s3)?;
    if let Some(auth) = &config.auth {
        check_tokens_are_references(auth)?;
    }
    config
        .substitute(lookup)
        .map_err(FileConfigError::Substitution)?;
    Ok(config)
}

/// Enforce RFC 0020 §3.5: object-store credentials must be `${env:…}` references,
/// never inline literals. Runs on the **raw** (pre-substitution) values. An
/// absent or empty field is not a literal and is allowed (it reads as "unset",
/// falling back to the AWS credential chain).
fn check_credentials_are_references(s3: &S3Section) -> Result<(), FileConfigError> {
    for (key, value) in [
        ("access_key_id", &s3.access_key_id),
        ("secret_access_key", &s3.secret_access_key),
        ("session_token", &s3.session_token),
    ] {
        if let Some(raw) = value
            && !raw.is_empty()
            && !is_env_reference(raw)
        {
            return Err(FileConfigError::InlineCredential { key });
        }
    }
    Ok(())
}

/// Enforce RFC 0026 §3.1: bearer-token values must be `${env:…}` references,
/// never inline literals, so config files stay committable. Runs on the **raw**
/// (pre-substitution) values; the [`check_credentials_are_references`] rule,
/// applied to `auth.tokens`. Unlike an S3 credential, an **empty** token is a
/// literal like any other — there is no unset-with-fallback reading for a
/// bearer token. Only an *absent* token (no `token` key) is deferred to the
/// token-store validation, which can name the entry.
fn check_tokens_are_references(auth: &AuthSection) -> Result<(), FileConfigError> {
    for (index, entry) in auth.tokens.iter().enumerate() {
        if let Some(raw) = &entry.token
            && !is_env_reference(raw)
        {
            return Err(FileConfigError::InlineToken { index });
        }
    }
    Ok(())
}

/// Whether `raw` is a single `${env:NAME}` / `${NAME}` substitution reference
/// spanning the whole value, with no default or an **empty** default
/// (`${env:NAME:-}`). A literal, a partial (`foo-${…}`), two references, or a
/// **non-empty** default (which would itself embed a literal secret) are all
/// rejected. The reference's name is validated later by substitution.
fn is_env_reference(raw: &str) -> bool {
    let Some(body) = raw.strip_prefix("${").and_then(|s| s.strip_suffix('}')) else {
        return false;
    };
    if body.contains('}') {
        return false; // a second `}` ⇒ more than one reference / trailing text
    }
    let body = body.strip_prefix("env:").unwrap_or(body);
    match body.split_once(":-") {
        Some((_name, default)) => default.is_empty(),
        None => true,
    }
}

impl FileConfig {
    /// Substitute `${env:…}` in every scalar leaf (RFC 0020 §3.3), in place.
    fn substitute(
        &mut self,
        lookup: &dyn Fn(&str) -> Option<String>,
    ) -> Result<(), MalformedReference> {
        self.storage.substitute(lookup)?;
        self.receiver.substitute(lookup)?;
        self.querier.substitute(lookup)?;
        self.compaction.substitute(lookup)?;
        if let Some(auth) = &mut self.auth {
            auth.substitute(lookup)?;
        }
        Ok(())
    }
}

impl AuthSection {
    fn substitute(
        &mut self,
        lookup: &dyn Fn(&str) -> Option<String>,
    ) -> Result<(), MalformedReference> {
        for entry in &mut self.tokens {
            substitute(&mut entry.name, lookup)?;
            substitute(&mut entry.token, lookup)?;
            for tenant in &mut entry.tenants {
                *tenant = env_subst::resolve(tenant, lookup)?;
            }
        }
        Ok(())
    }
}

impl StorageSection {
    fn substitute(
        &mut self,
        lookup: &dyn Fn(&str) -> Option<String>,
    ) -> Result<(), MalformedReference> {
        substitute(&mut self.backend, lookup)?;
        self.s3.substitute(lookup)?;
        self.local.substitute(lookup)?;
        self.promoted_attributes.substitute(lookup)
    }
}

impl PromotedAttributesSection {
    fn substitute(
        &mut self,
        lookup: &dyn Fn(&str) -> Option<String>,
    ) -> Result<(), MalformedReference> {
        for key in self.resource.iter_mut().chain(self.log.iter_mut()) {
            *key = env_subst::resolve(key, lookup)?;
        }
        Ok(())
    }
}

impl S3Section {
    fn substitute(
        &mut self,
        lookup: &dyn Fn(&str) -> Option<String>,
    ) -> Result<(), MalformedReference> {
        substitute(&mut self.bucket, lookup)?;
        substitute(&mut self.endpoint, lookup)?;
        substitute(&mut self.region, lookup)?;
        substitute(&mut self.prefix, lookup)?;
        substitute(&mut self.access_key_id, lookup)?;
        substitute(&mut self.secret_access_key, lookup)?;
        substitute(&mut self.session_token, lookup)
    }
}

impl LocalSection {
    fn substitute(
        &mut self,
        lookup: &dyn Fn(&str) -> Option<String>,
    ) -> Result<(), MalformedReference> {
        substitute(&mut self.bucket_root, lookup)
    }
}

impl ReceiverSection {
    fn substitute(
        &mut self,
        lookup: &dyn Fn(&str) -> Option<String>,
    ) -> Result<(), MalformedReference> {
        substitute(&mut self.enabled, lookup)?;
        substitute(&mut self.grpc_addr, lookup)?;
        substitute(&mut self.http_addr, lookup)?;
        substitute(&mut self.wal_root, lookup)
    }
}

impl QuerierSection {
    fn substitute(
        &mut self,
        lookup: &dyn Fn(&str) -> Option<String>,
    ) -> Result<(), MalformedReference> {
        substitute(&mut self.enabled, lookup)?;
        substitute(&mut self.http_addr, lookup)?;
        substitute(&mut self.default_window_secs, lookup)?;
        substitute(&mut self.mcp.enabled, lookup)
    }
}

impl CompactionSection {
    fn substitute(
        &mut self,
        lookup: &dyn Fn(&str) -> Option<String>,
    ) -> Result<(), MalformedReference> {
        substitute(&mut self.enabled, lookup)?;
        substitute(&mut self.interval_secs, lookup)
    }
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

/// A YAML sequence of scalars, each captured as its string form — the
/// [`scalar_opt`] model applied per element (RFC 0020 §3.3 rule 7). A
/// mapping or sequence where an element scalar is expected is a schema
/// error, as is a bare scalar where the sequence is expected.
fn scalar_vec<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Vec::<Scalar>::deserialize(deserializer)?
        .into_iter()
        .map(|s| s.0)
        .collect())
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

    use super::{FileConfigError, parse};

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
    endpoint: ${env:MISSING}
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

    /// RFC 0022 §3.2 — `storage.promoted_attributes.{resource,log}` parse as
    /// key lists, each element getting the scalar treatment: `${env:…}`
    /// substitution applies per element, and the sub-section stays strict
    /// (unknown keys inside it are rejected elsewhere in this suite).
    #[test]
    fn promoted_attribute_keys_parse_and_substitute_per_element() {
        let lookup = env(&[("NS_KEY", "k8s.namespace.name")]);
        // Block style: a `${env:…}` reference is not a valid *flow*-sequence
        // plain scalar (the `:` inside the braces ends the flow entry).
        let yaml = "
storage:
  promoted_attributes:
    resource:
      - ${env:NS_KEY}
      - cloud.region
    log: [http.route]
";
        let cfg = parse(yaml, &lookup).expect("valid");
        assert_eq!(
            cfg.storage.promoted_attributes.resource,
            ["k8s.namespace.name", "cloud.region"]
        );
        assert_eq!(cfg.storage.promoted_attributes.log, ["http.route"]);
    }

    /// RFC 0022 §3.2 — the section defaults to empty key sets when omitted
    /// (promotion beyond the implicit `service.name` is opt-in), and an
    /// unknown key inside it is rejected by the strict schema.
    #[test]
    fn promoted_attributes_default_empty_and_stay_strict() {
        let lookup = env(&[]);
        let cfg = parse("storage:\n  backend: local\n", &lookup).expect("valid");
        assert!(cfg.storage.promoted_attributes.resource.is_empty());
        assert!(cfg.storage.promoted_attributes.log.is_empty());

        let err = parse(
            "storage:\n  promoted_attributes:\n    resources: [a]\n",
            &lookup,
        )
        .expect_err("unknown key inside promoted_attributes");
        assert!(matches!(err, FileConfigError::Schema(_)), "got {err:?}");
    }

    /// RFC 0022 §3.2 — a bare scalar where a key *list* is expected is a
    /// schema error (the `scalar_vec` shape rule), mirroring the
    /// scalar-where-structure rule the other leaves enforce.
    #[test]
    fn promoted_attributes_reject_a_scalar_where_a_list_is_expected() {
        let lookup = env(&[]);
        let err = parse(
            "storage:\n  promoted_attributes:\n    resource: k8s.namespace.name\n",
            &lookup,
        )
        .expect_err("scalar where a sequence is expected");
        assert!(matches!(err, FileConfigError::Schema(_)), "got {err:?}");
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

    /// RFC0020.2 rule 4 — a `${…}` in a mapping **key** position is never a
    /// substitution candidate. `X` resolves to a *valid* section name, so if keys
    /// were substituted the file would parse; it must not — keys deserialise as
    /// field names and are left verbatim, so the reference-shaped key is rejected
    /// as unknown.
    #[test]
    fn a_reference_in_key_position_is_left_verbatim() {
        let lookup = env(&[("X", "storage")]);
        let err = parse("${env:X}:\n  backend: s3\n", &lookup).expect_err("verbatim key");
        assert!(matches!(err, FileConfigError::Schema(_)), "got {err:?}");
    }

    /// RFC0020.6 (schema-error hygiene) — a reference placed where a whole
    /// section is expected fails on the **raw** tree, so the error names the
    /// reference text, never the resolved secret value (RFC 0020 §3.5): `serde`
    /// never sees a substituted value.
    #[test]
    fn schema_error_never_echoes_a_resolved_value() {
        const SECRET: &str = "SUPER-SECRET-TOKEN";
        let lookup = env(&[("SECRET", SECRET)]);
        let err = parse("storage: ${env:SECRET}\n", &lookup).expect_err("shape mismatch");
        assert!(matches!(err, FileConfigError::Schema(_)), "got {err:?}");
        let msg = err.to_string();
        assert!(
            !msg.contains(SECRET),
            "the resolved secret must not leak: {msg}"
        );
        assert!(
            msg.contains("${env:SECRET}"),
            "names the reference instead: {msg}",
        );
    }

    /// The S3 credential fields are redacted in `Debug` — presence only, never
    /// the value (RFC 0020 §3.5 / RFC 0019 §3.4), mirroring `S3Config`.
    #[test]
    fn s3_credentials_are_redacted_in_debug() {
        let lookup = env(&[("KEY", "AKIAEXAMPLE"), ("SECRET", "s3cr3t-value")]);
        let cfg = parse(
            "storage:\n  s3:\n    bucket: b\n    access_key_id: ${env:KEY}\n    secret_access_key: ${env:SECRET}\n",
            &lookup,
        )
        .expect("valid");
        let rendered = format!("{:?}", cfg.storage.s3);
        assert!(
            rendered.contains("bucket"),
            "non-secret fields stay visible"
        );
        assert!(
            !rendered.contains("AKIAEXAMPLE"),
            "access key id redacted: {rendered}",
        );
        assert!(
            !rendered.contains("s3cr3t-value"),
            "secret access key redacted: {rendered}",
        );
        assert!(
            rendered.contains("<redacted>"),
            "shows presence: {rendered}"
        );
    }

    /// RFC0020.6 (§3.5 enforcement) — an object-store credential given as an
    /// inline literal is rejected, and the error names the offending key, never
    /// the value. Bare `${env:…}` references (with an optional empty default) are
    /// allowed; a non-empty default (an embedded literal) is not.
    #[test]
    fn inline_credential_literal_is_rejected_naming_the_key() {
        let lookup = env(&[]);

        // A literal secret is rejected — the error names the key, not the value.
        let err = parse(
            "storage:\n  s3:\n    secret_access_key: AKIAHARDCODEDSECRET\n",
            &lookup,
        )
        .expect_err("inline literal");
        assert!(
            matches!(err, FileConfigError::InlineCredential { key } if key == "secret_access_key"),
            "got {err:?}",
        );
        let msg = err.to_string();
        assert!(msg.contains("secret_access_key"), "names the key: {msg}");
        assert!(
            !msg.contains("AKIAHARDCODEDSECRET"),
            "never the value: {msg}",
        );

        // A reference with a non-empty default embeds a literal — also rejected.
        assert!(
            parse(
                "storage:\n  s3:\n    access_key_id: ${env:K:-AKIAFALLBACK}\n",
                &lookup,
            )
            .is_err(),
            "a non-empty default is an embedded literal",
        );
        // A partial reference (surrounding literal text) is rejected.
        assert!(
            parse(
                "storage:\n  s3:\n    session_token: tok-${env:T}\n",
                &lookup,
            )
            .is_err(),
        );

        // Bare references, and a reference with an empty default, are allowed.
        for ok in [
            "${env:OURIOS_S3_SECRET_ACCESS_KEY}",
            "${OURIOS_S3_SECRET_ACCESS_KEY}",
            "${env:OURIOS_S3_SECRET_ACCESS_KEY:-}",
        ] {
            let yaml = format!("storage:\n  s3:\n    secret_access_key: {ok}\n");
            parse(&yaml, &lookup).unwrap_or_else(|e| panic!("{ok} should be allowed: {e}"));
        }

        // An absent or empty credential is not a literal (reads as unset).
        parse("storage:\n  s3:\n    secret_access_key: \"\"\n", &lookup)
            .expect("an empty credential is allowed (unset)");
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

    /// RFC0026.1 (schema) — `auth.tokens` parses with per-entry `${env:…}`
    /// substitution on name, token, and tenant elements; a file with no
    /// `auth` section parses to `None` (open mode), distinguishable from a
    /// present-but-empty section.
    #[test]
    fn auth_tokens_parse_and_substitute() {
        let lookup = env(&[("TOK_EDGE", "s3cr3t-edge"), ("TENANT", "acme")]);
        let yaml = "
auth:
  tokens:
    - name: edge-collector
      token: ${env:TOK_EDGE}
      tenants:
        - ${env:TENANT}
        - globex
    - name: admin-cli
      token: ${env:TOK_ADMIN}
      tenants: [\"*\"]
";
        let cfg = parse(yaml, &lookup).expect("valid");
        let auth = cfg.auth.expect("auth section present");
        assert_eq!(auth.tokens.len(), 2);
        assert_eq!(auth.tokens[0].name.as_deref(), Some("edge-collector"));
        assert_eq!(auth.tokens[0].token.as_deref(), Some("s3cr3t-edge"));
        assert_eq!(auth.tokens[0].tenants, ["acme", "globex"]);
        assert_eq!(auth.tokens[1].token.as_deref(), Some("")); // undefined, no default
        assert_eq!(auth.tokens[1].tenants, ["*"]);

        assert!(
            parse("storage:\n  backend: local\n", &lookup)
                .expect("valid")
                .auth
                .is_none(),
            "no auth section parses to None (open mode)",
        );
        let empty = parse("auth:\n  tokens: []\n", &lookup).expect("valid");
        assert!(
            empty.auth.expect("present").tokens.is_empty(),
            "a present-but-empty section is distinguishable from an absent one",
        );
    }

    /// RFC0026.1 (secret hygiene) — an inline-literal token is rejected with
    /// an error naming the entry's index, never the value; and a resolved
    /// token is redacted in the entry's `Debug` (the [`S3Section`] rules,
    /// applied to `auth.tokens`).
    #[test]
    fn inline_token_literal_is_rejected_and_debug_redacts() {
        let lookup = env(&[("TOK", "s3cr3t-token-value")]);

        let err = parse(
            "auth:\n  tokens:\n    - name: a\n      token: ${env:TOK}\n      tenants: [x]\n    - name: b\n      token: hardcoded-secret\n      tenants: [y]\n",
            &lookup,
        )
        .expect_err("inline literal");
        assert!(
            matches!(err, FileConfigError::InlineToken { index: 1 }),
            "got {err:?}",
        );
        let msg = err.to_string();
        assert!(
            msg.contains("auth.tokens[1].token"),
            "names the entry: {msg}"
        );
        assert!(!msg.contains("hardcoded-secret"), "never the value: {msg}");

        // An empty string is a literal like any other — a bearer token has no
        // unset-with-fallback reading (unlike an S3 credential).
        let err = parse(
            "auth:\n  tokens:\n    - name: a\n      token: \"\"\n      tenants: [x]\n",
            &lookup,
        )
        .expect_err("empty literal");
        assert!(
            matches!(err, FileConfigError::InlineToken { index: 0 }),
            "got {err:?}",
        );

        let cfg = parse(
            "auth:\n  tokens:\n    - name: a\n      token: ${env:TOK}\n      tenants: [x]\n",
            &lookup,
        )
        .expect("valid");
        let rendered = format!("{:?}", cfg.auth.expect("present").tokens[0]);
        assert!(rendered.contains("\"a\""), "the name stays visible");
        assert!(
            !rendered.contains("s3cr3t-token-value"),
            "token redacted: {rendered}",
        );
        assert!(
            rendered.contains("<redacted>"),
            "shows presence: {rendered}"
        );
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
