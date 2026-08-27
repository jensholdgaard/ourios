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
//! than by re-tagging a node tree. `serde_yaml_ng`'s `Value` does not preserve a
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
    Schema(serde_yaml_ng::Error),
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
    /// `auth.openfga.api_token` holds an inline literal instead of an
    /// `${env:…}` reference (RFC 0047 §3.1, the RFC 0026 rule).
    InlineOpenFgaToken,
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
            Self::InlineOpenFgaToken => write!(
                f,
                "auth.openfga.api_token must be an ${{env:…}} reference, not an \
                 inline literal (RFC 0047 §3.1)"
            ),
        }
    }
}

impl std::error::Error for FileConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Substitution(e) => Some(e),
            Self::Schema(e) => Some(e),
            Self::InlineCredential { .. } | Self::InlineToken { .. } | Self::InlineOpenFgaToken => {
                None
            }
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
#[cfg_attr(test, derive(serde::Serialize))]
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
    /// Upstream-template handling (`miner.*`, RFC 0050 §3.2 — an RFC 0020
    /// schema extension). Only the RFC 0050 dial is file-configurable; the
    /// miner's other tunables stay code-default per RFC 0020's deliberate
    /// exclusion of the miner surface.
    pub miner: MinerSection,
}

/// `miner.*` — the RFC 0050 §3.2 upstream-template dial.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(test, derive(serde::Serialize))]
pub struct MinerSection {
    /// `ignore` (default) / `observe` / `adopt`.
    #[serde(deserialize_with = "scalar_opt")]
    pub upstream_templates: Option<String>,
    /// UTF-8 byte cap on an inbound `log.record.template` value
    /// (default `8192`; `0` disables all upstream-template handling).
    #[serde(deserialize_with = "scalar_opt")]
    pub upstream_template_byte_limit: Option<String>,
    /// Per-template bound on stored upstream-string associations
    /// (default `4`; overflow is counted, not stored).
    #[serde(deserialize_with = "scalar_opt")]
    pub upstream_association_limit: Option<String>,
}

impl MinerSection {
    fn substitute(
        &mut self,
        lookup: &dyn Fn(&str) -> Option<String>,
    ) -> Result<(), MalformedReference> {
        substitute(&mut self.upstream_templates, lookup)?;
        substitute(&mut self.upstream_template_byte_limit, lookup)?;
        substitute(&mut self.upstream_association_limit, lookup)
    }
}

/// `storage.*` — the data + audit store backend selection (RFC 0019 §3.1/§3.2).
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(test, derive(serde::Serialize))]
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
/// key sets, with RFC 0042 §3.2 typed entries. Keys are plain
/// attribute-key strings, taken literally (no globbing); the implicit
/// `service.name` promotion never needs listing. Defaults: empty —
/// promotion beyond `service.name` is opt-in.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(test, derive(serde::Serialize))]
pub struct PromotedAttributesSection {
    /// Resource-attribute keys to promote (`resource.<key>` columns).
    pub resource: Vec<PromotedEntry>,
    /// Log-attribute keys to promote (`attr.<key>` columns).
    pub log: Vec<PromotedEntry>,
}

/// One `storage.promoted_attributes` list entry: the RFC 0022 bare key
/// (string class) or the RFC 0042 §3.2 typed mapping `{ key, type }`.
/// The class stays its raw string at this layer — the RFC 0020 file
/// model is string-typed throughout (the `Scalar` capture) — and is validated
/// into a `PromotedClass` at startup alongside the key rules
/// (RFC0042.6), so an unknown `type` fails loudly there, after
/// `${env:…}` substitution has run on both fields.
#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(test, derive(serde::Serialize))]
pub struct PromotedEntry {
    /// The attribute key.
    pub key: String,
    /// The declared class token (`string` / `i64` / `f64`); `None` for
    /// the bare spelling, which is the string class.
    pub class: Option<String>,
}

impl<'de> Deserialize<'de> for PromotedEntry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        /// The mapping spelling, strict: `key` required, `type`
        /// optional (absent = the bare spelling's string class),
        /// anything else rejected.
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct TypedEntry {
            key: Scalar,
            #[serde(default, rename = "type")]
            r#type: Option<Scalar>,
        }

        struct EntryVisitor;

        impl<'de> serde::de::Visitor<'de> for EntryVisitor {
            type Value = PromotedEntry;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an attribute key, or a `{ key, type }` mapping (RFC 0042 §3.2)")
            }

            // The bare spelling takes the full scalar treatment (RFC
            // 0020 §3.3 rule 7, as `Scalar` renders): a bare boolean
            // or numeric key is captured as its string form, exactly
            // as the pre-RFC-0042 `scalar_vec` model did.
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<PromotedEntry, E> {
                Ok(PromotedEntry {
                    key: v.to_owned(),
                    class: None,
                })
            }

            fn visit_string<E: serde::de::Error>(self, v: String) -> Result<PromotedEntry, E> {
                Ok(PromotedEntry {
                    key: v,
                    class: None,
                })
            }

            fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<PromotedEntry, E> {
                self.visit_string(v.to_string())
            }

            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<PromotedEntry, E> {
                self.visit_string(v.to_string())
            }

            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<PromotedEntry, E> {
                self.visit_string(v.to_string())
            }

            fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<PromotedEntry, E> {
                self.visit_string(v.to_string())
            }

            fn visit_map<A>(self, map: A) -> Result<PromotedEntry, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let typed =
                    TypedEntry::deserialize(serde::de::value::MapAccessDeserializer::new(map))?;
                Ok(PromotedEntry {
                    key: typed.key.0,
                    class: typed.r#type.map(|s| s.0),
                })
            }
        }

        deserializer.deserialize_any(EntryVisitor)
    }
}

/// `storage.s3.*` — S3 addressing and (env-only) credentials (RFC 0019 §3.4).
///
/// The credential fields are **secret**: the manual [`fmt::Debug`] impl redacts
/// their values (showing only presence), mirroring `ourios_parquet::S3Config` so
/// a `Debug` rendering never leaks a key (RFC 0020 §3.5 / RFC 0019 §3.4).
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(test, derive(serde::Serialize))]
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
#[cfg_attr(test, derive(serde::Serialize))]
pub struct LocalSection {
    #[serde(deserialize_with = "scalar_opt")]
    pub bucket_root: Option<String>,
}

/// `receiver.*` — the OTLP receiver role (RFC 0003 §6.2).
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(test, derive(serde::Serialize))]
pub struct ReceiverSection {
    #[serde(deserialize_with = "scalar_opt")]
    pub enabled: Option<String>,
    #[serde(deserialize_with = "scalar_opt")]
    pub grpc_addr: Option<String>,
    /// RFC 0030 §3.1 — TLS on the gRPC listener (`receiver.grpc_tls`).
    pub grpc_tls: TlsSection,
    #[serde(deserialize_with = "scalar_opt")]
    pub http_addr: Option<String>,
    /// RFC 0030 §3.1 — TLS on the HTTP listener (`receiver.http_tls`).
    pub http_tls: TlsSection,
    #[serde(deserialize_with = "scalar_opt")]
    pub wal_root: Option<String>,
    /// RFC 0035 §3.1 — worker count for the concurrent encode pool
    /// (`receiver.encode_workers`; default: the host's available cores).
    #[serde(deserialize_with = "scalar_opt")]
    pub encode_workers: Option<String>,
}

/// One `*_tls` block (RFC 0030 §3.1). Raw string leaves — the §3.1
/// rules live in `TlsSettings::from_parts` (the single validation
/// path); paths may ride `${env:…}` like any other value, the file
/// contents never appear in config.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(test, derive(serde::Serialize))]
pub struct TlsSection {
    #[serde(deserialize_with = "scalar_opt")]
    pub cert_file: Option<String>,
    #[serde(deserialize_with = "scalar_opt")]
    pub key_file: Option<String>,
    #[serde(deserialize_with = "scalar_opt")]
    pub client_ca_file: Option<String>,
    #[serde(deserialize_with = "scalar_opt")]
    pub min_version: Option<String>,
    #[serde(deserialize_with = "scalar_opt")]
    pub reload_interval_secs: Option<String>,
}

impl TlsSection {
    fn substitute(
        &mut self,
        lookup: &dyn Fn(&str) -> Option<String>,
    ) -> Result<(), MalformedReference> {
        substitute(&mut self.cert_file, lookup)?;
        substitute(&mut self.key_file, lookup)?;
        substitute(&mut self.client_ca_file, lookup)?;
        substitute(&mut self.min_version, lookup)?;
        substitute(&mut self.reload_interval_secs, lookup)
    }
}

/// `querier.*` — the query role (RFC 0016 §3.2).
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(test, derive(serde::Serialize))]
pub struct QuerierSection {
    #[serde(deserialize_with = "scalar_opt")]
    pub enabled: Option<String>,
    #[serde(deserialize_with = "scalar_opt")]
    pub http_addr: Option<String>,
    /// RFC 0030 §3.1 — TLS on the querier listener (`querier.http_tls`),
    /// covering `/mcp`.
    pub http_tls: TlsSection,
    #[serde(deserialize_with = "scalar_opt")]
    pub default_window_secs: Option<String>,
    /// The RFC 0027 MCP surface (`querier.mcp.*`).
    pub mcp: McpSection,
}

/// `querier.mcp.*` — the RFC 0027 MCP surface (§3.1; default off).
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(test, derive(serde::Serialize))]
pub struct McpSection {
    #[serde(deserialize_with = "scalar_opt")]
    pub enabled: Option<String>,
}

/// `auth.*` — bearer-token authentication and tenant binding (RFC 0026
/// §3.1), plus the OIDC layer (RFC 0029 §3.1).
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(test, derive(serde::Serialize))]
pub struct AuthSection {
    /// The configured static tokens. `Option` because absent and explicitly
    /// empty differ (RFC 0029 §3.1): omitting `tokens` is how an oidc-only
    /// section disables the static half, while an explicit `tokens: []` is
    /// **always** a startup configuration error — a locked-out static store
    /// is never the intent (RFC 0026 §3.1).
    pub tokens: Option<Vec<TokenEntry>>,
    /// The OIDC layer (RFC 0029 §3.1). Nothing in it is secret.
    pub oidc: Option<OidcSection>,
    /// The `OpenFGA` resolver (RFC 0047 §3.1): binds the tenants of whatever
    /// the two halves above authenticate.
    pub openfga: Option<OpenFgaSection>,
}

/// `auth.oidc.*` — the OIDC bearer layer (RFC 0029 §3.1). Issuer, audience,
/// and claim names are deployment topology, not credentials — plain `Debug`.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(test, derive(serde::Serialize))]
pub struct OidcSection {
    /// The OIDC discovery root.
    #[serde(deserialize_with = "scalar_opt")]
    pub issuer: Option<String>,
    /// The required `aud` value.
    #[serde(deserialize_with = "scalar_opt")]
    pub audience: Option<String>,
    /// The claim carrying the tenant list.
    #[serde(deserialize_with = "scalar_opt")]
    pub tenant_claim: Option<String>,
    /// The claim feeding the audit/metric label (default `sub`).
    #[serde(deserialize_with = "scalar_opt")]
    pub name_claim: Option<String>,
    /// `exp`/`nbf` clock-skew allowance in seconds (default 60).
    #[serde(deserialize_with = "scalar_opt")]
    pub clock_skew_secs: Option<String>,
    /// `<claim>=<value>` marking an `agent:` principal (RFC 0047 §3.1).
    #[serde(deserialize_with = "scalar_opt")]
    pub agent_claim: Option<String>,
    /// The claim carrying the subject's groups (RFC 0047 §3.1).
    #[serde(deserialize_with = "scalar_opt")]
    pub groups_claim: Option<String>,
}

/// `auth.openfga.*` — the RFC 0047 §3.1 resolver. `api_token` is **secret**:
/// the manual [`fmt::Debug`] redacts it and [`parse`] rejects an inline
/// literal (the RFC 0026 rule).
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(test, derive(serde::Serialize))]
pub struct OpenFgaSection {
    /// The `OpenFGA` HTTP API root.
    #[serde(deserialize_with = "scalar_opt")]
    pub api_url: Option<String>,
    /// The store id.
    #[serde(deserialize_with = "scalar_opt")]
    pub store_id: Option<String>,
    /// The pinned authorization model id (absent = latest).
    #[serde(deserialize_with = "scalar_opt")]
    pub authorization_model_id: Option<String>,
    /// The API bearer token (**secret**; `${env:…}` reference only).
    #[serde(deserialize_with = "scalar_opt")]
    pub api_token: Option<String>,
    /// Per-credential binding cache lifetime in seconds (default 60).
    #[serde(deserialize_with = "scalar_opt")]
    pub session_ttl_secs: Option<String>,
    /// `minimize_latency` (default) or `higher_consistency`.
    #[serde(deserialize_with = "scalar_opt")]
    pub consistency: Option<String>,
    /// Per-call request timeout in seconds (default 5).
    #[serde(deserialize_with = "scalar_opt")]
    pub request_timeout_secs: Option<String>,
    /// The layer-2 visibility section (RFC 0047 §3.4).
    pub visibility: VisibilitySection,
    /// The `OpenFGA` server's `OPENFGA_LIST_OBJECTS_DEADLINE` in
    /// milliseconds (default 3000); `visibility.list_timeout_ms` must stay
    /// strictly below it.
    #[serde(deserialize_with = "scalar_opt")]
    pub server_list_objects_deadline_ms: Option<String>,
}

/// `auth.openfga.visibility.identities.*` — RFC 0048 §3.2.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(test, derive(serde::Serialize))]
pub struct IdentitiesSection {
    /// Promoted columns whose values become `user:` principals.
    #[serde(default, deserialize_with = "scalar_vec_opt")]
    pub user_columns: Option<Vec<String>>,
    /// Promoted columns whose values become `agent:` principals.
    #[serde(default, deserialize_with = "scalar_vec_opt")]
    pub agent_columns: Option<Vec<String>>,
}

/// `auth.openfga.visibility.*` — RFC 0047 §3.4. Nothing here is secret.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(test, derive(serde::Serialize))]
pub struct VisibilitySection {
    /// Object type → promoted column bindings (v1: `conversation` only).
    pub objects: Vec<VisibilityObjectSection>,
    /// Which promoted columns carry the principals in a conversation
    /// (RFC 0048 §3.2). Unset lists take the semantic-convention defaults.
    pub identities: IdentitiesSection,
    /// The promoted column compared to a `user:` principal's subject.
    #[serde(deserialize_with = "scalar_opt")]
    pub self_principal_column: Option<String>,
    /// The content columns a metadata-only reader may not read. `None` =
    /// the `GenAI` default set; an explicit list **replaces** it and must
    /// not be empty (masking is never disabled — validated at startup).
    #[serde(default, deserialize_with = "scalar_vec_opt")]
    pub content_columns: Option<Vec<String>>,
    /// The per-tenant enumeration bound (default 10 000).
    #[serde(deserialize_with = "scalar_opt")]
    pub max_objects: Option<String>,
    /// The client-side enumeration timeout in milliseconds (default 2000).
    #[serde(deserialize_with = "scalar_opt")]
    pub list_timeout_ms: Option<String>,
}

/// One `auth.openfga.visibility.objects[]` entry.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(test, derive(serde::Serialize))]
pub struct VisibilityObjectSection {
    /// The `OpenFGA` object type (`conversation`).
    #[serde(rename = "type", deserialize_with = "scalar_opt")]
    pub object_type: Option<String>,
    /// The promoted column carrying the object ids.
    #[serde(deserialize_with = "scalar_opt")]
    pub column: Option<String>,
}

impl fmt::Debug for OpenFgaSection {
    /// Redacts the API token — presence only, never the value.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenFgaSection")
            .field("api_url", &self.api_url)
            .field("store_id", &self.store_id)
            .field("authorization_model_id", &self.authorization_model_id)
            .field("api_token", &self.api_token.as_ref().map(|_| "<redacted>"))
            .field("session_ttl_secs", &self.session_ttl_secs)
            .field("consistency", &self.consistency)
            .field("request_timeout_secs", &self.request_timeout_secs)
            .field("visibility", &self.visibility)
            .field(
                "server_list_objects_deadline_ms",
                &self.server_list_objects_deadline_ms,
            )
            .finish()
    }
}

/// One `auth.tokens[…]` entry (RFC 0026 §3.1).
///
/// The `token` field is **secret**: the manual [`fmt::Debug`] impl redacts its
/// value (showing only presence), mirroring [`S3Section`], and [`parse`]
/// rejects an inline literal — the file may hold it only as an `${env:…}`
/// reference.
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(test, derive(serde::Serialize))]
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
#[cfg_attr(test, derive(serde::Serialize))]
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
    // Deserialise straight from the text (not via an intermediate
    // `serde_yaml_ng::Value`) so a schema error keeps its source location.
    // Validation runs on the
    // raw (pre-substitution) text, so any shape / unknown-key error names the
    // file's own text, never a resolved secret (RFC 0020 §3.5). `Option` lets an
    // empty / null document resolve to an all-default config (`None`) rather than
    // fail the `null`-into-struct type check.
    let mut config: FileConfig = serde_yaml_ng::from_str::<Option<FileConfig>>(yaml)
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
    for (index, entry) in auth.tokens.iter().flatten().enumerate() {
        if let Some(raw) = &entry.token
            && !is_env_reference(raw)
        {
            return Err(FileConfigError::InlineToken { index });
        }
    }
    // RFC 0047 §3.1: the OpenFGA API token follows the same rule; like an S3
    // credential, an empty value reads as "unset" (an OpenFGA without
    // API-token auth is a legitimate deployment).
    if let Some(raw) = auth.openfga.as_ref().and_then(|o| o.api_token.as_ref())
        && !raw.is_empty()
        && !is_env_reference(raw)
    {
        return Err(FileConfigError::InlineOpenFgaToken);
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
        self.miner.substitute(lookup)?;
        Ok(())
    }
}

impl AuthSection {
    fn substitute(
        &mut self,
        lookup: &dyn Fn(&str) -> Option<String>,
    ) -> Result<(), MalformedReference> {
        for entry in self.tokens.iter_mut().flatten() {
            substitute(&mut entry.name, lookup)?;
            substitute(&mut entry.token, lookup)?;
            for tenant in &mut entry.tenants {
                *tenant = env_subst::resolve(tenant, lookup)?;
            }
        }
        if let Some(oidc) = &mut self.oidc {
            substitute(&mut oidc.issuer, lookup)?;
            substitute(&mut oidc.audience, lookup)?;
            substitute(&mut oidc.tenant_claim, lookup)?;
            substitute(&mut oidc.name_claim, lookup)?;
            substitute(&mut oidc.clock_skew_secs, lookup)?;
            substitute(&mut oidc.agent_claim, lookup)?;
            substitute(&mut oidc.groups_claim, lookup)?;
        }
        if let Some(openfga) = &mut self.openfga {
            substitute(&mut openfga.api_url, lookup)?;
            substitute(&mut openfga.store_id, lookup)?;
            substitute(&mut openfga.authorization_model_id, lookup)?;
            substitute(&mut openfga.api_token, lookup)?;
            substitute(&mut openfga.session_ttl_secs, lookup)?;
            substitute(&mut openfga.consistency, lookup)?;
            substitute(&mut openfga.request_timeout_secs, lookup)?;
            substitute(&mut openfga.server_list_objects_deadline_ms, lookup)?;
            let visibility = &mut openfga.visibility;
            for object in &mut visibility.objects {
                substitute(&mut object.object_type, lookup)?;
                substitute(&mut object.column, lookup)?;
            }
            substitute(&mut visibility.self_principal_column, lookup)?;
            for column in visibility.identities.user_columns.iter_mut().flatten() {
                *column = env_subst::resolve(column, lookup)?;
            }
            for column in visibility.identities.agent_columns.iter_mut().flatten() {
                *column = env_subst::resolve(column, lookup)?;
            }
            for column in visibility.content_columns.iter_mut().flatten() {
                *column = env_subst::resolve(column, lookup)?;
            }
            substitute(&mut visibility.max_objects, lookup)?;
            substitute(&mut visibility.list_timeout_ms, lookup)?;
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
        for entry in self.resource.iter_mut().chain(self.log.iter_mut()) {
            entry.key = env_subst::resolve(&entry.key, lookup)?;
            if let Some(class) = entry.class.as_mut() {
                *class = env_subst::resolve(class, lookup)?;
            }
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
        self.grpc_tls.substitute(lookup)?;
        substitute(&mut self.http_addr, lookup)?;
        self.http_tls.substitute(lookup)?;
        substitute(&mut self.wal_root, lookup)?;
        substitute(&mut self.encode_workers, lookup)
    }
}

impl QuerierSection {
    fn substitute(
        &mut self,
        lookup: &dyn Fn(&str) -> Option<String>,
    ) -> Result<(), MalformedReference> {
        substitute(&mut self.enabled, lookup)?;
        substitute(&mut self.http_addr, lookup)?;
        self.http_tls.substitute(lookup)?;
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

/// [`scalar_vec`] for an optional list — absent and present differ (an
/// absent `content_columns` takes the default set; a present list replaces
/// it — and, validated at startup, may not be empty).
fn scalar_vec_opt<'de, D>(deserializer: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<Vec<Scalar>>::deserialize(deserializer)?
        .map(|scalars| scalars.into_iter().map(|s| s.0).collect()))
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

    /// RFC 0050 §3.2 — every `miner.*` leaf gets the scalar treatment:
    /// `${env:…}` substitution resolves on all three fields.
    #[test]
    fn miner_section_leaves_are_substituted() {
        let lookup = env(&[("MODE", "observe"), ("CAP", "2048"), ("ASSOC", "8")]);
        let yaml = "
miner:
  upstream_templates: ${env:MODE}
  upstream_template_byte_limit: ${env:CAP}
  upstream_association_limit: ${env:ASSOC}
";
        let cfg = parse(yaml, &lookup).expect("valid");
        assert_eq!(cfg.miner.upstream_templates.as_deref(), Some("observe"));
        assert_eq!(
            cfg.miner.upstream_template_byte_limit.as_deref(),
            Some("2048")
        );
        assert_eq!(cfg.miner.upstream_association_limit.as_deref(), Some("8"));
    }

    /// RFC 0020 §3.4 — the `miner.*` section is strict: an unknown key
    /// is a parse error, not a silent no-op.
    #[test]
    fn miner_section_rejects_unknown_keys() {
        let yaml = "
miner:
  upstream_template: adopt
";
        let err = parse(yaml, &|_| None).expect_err("unknown miner key must fail");
        assert!(matches!(err, FileConfigError::Schema(_)), "{err:?}");
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
        let keys = |entries: &[super::PromotedEntry]| {
            entries.iter().map(|e| e.key.clone()).collect::<Vec<_>>()
        };
        assert_eq!(
            keys(&cfg.storage.promoted_attributes.resource),
            ["k8s.namespace.name", "cloud.region"]
        );
        assert_eq!(keys(&cfg.storage.promoted_attributes.log), ["http.route"]);
        assert!(
            cfg.storage
                .promoted_attributes
                .resource
                .iter()
                .chain(&cfg.storage.promoted_attributes.log)
                .all(|e| e.class.is_none()),
            "bare entries carry no class token"
        );
    }

    /// RFC 0042 §3.2 — a list entry may be the typed mapping
    /// `{ key, type }`; both fields get the scalar treatment (so
    /// `${env:…}` substitution applies to each), `type` is optional
    /// (absent = the bare spelling), and an unknown field inside the
    /// mapping is rejected by the strict schema. The class token stays a
    /// raw string here — startup validation owns the vocabulary
    /// (RFC0042.6).
    #[test]
    fn promoted_typed_entries_parse_and_substitute() {
        let lookup = env(&[("COST_KEY", "cost_usd"), ("COST_TYPE", "f64")]);
        let yaml = "
storage:
  promoted_attributes:
    log:
      - model
      - { key: input_tokens, type: i64 }
      - key: ${env:COST_KEY}
        type: ${env:COST_TYPE}
      - { key: verbatim, type: not-a-class }
";
        let cfg = parse(yaml, &lookup).expect("valid");
        let entries = &cfg.storage.promoted_attributes.log;
        let shape: Vec<(&str, Option<&str>)> = entries
            .iter()
            .map(|e| (e.key.as_str(), e.class.as_deref()))
            .collect();
        assert_eq!(
            shape,
            [
                ("model", None),
                ("input_tokens", Some("i64")),
                ("cost_usd", Some("f64")),
                // The file layer does not police the vocabulary —
                // startup does, so the token passes through verbatim.
                ("verbatim", Some("not-a-class")),
            ]
        );
    }

    /// RFC 0020 §3.3 rule 7 — a bare entry keeps the full scalar
    /// treatment: an unquoted numeric or boolean key is captured as its
    /// string form, exactly as the pre-RFC-0042 `scalar_vec` model did.
    #[test]
    fn promoted_bare_entries_keep_the_scalar_model() {
        let lookup = env(&[]);
        let yaml = "
storage:
  promoted_attributes:
    log:
      - 404
      - true
      - 1.5
";
        let cfg = parse(yaml, &lookup).expect("valid");
        let keys: Vec<&str> = cfg
            .storage
            .promoted_attributes
            .log
            .iter()
            .map(|e| e.key.as_str())
            .collect();
        assert_eq!(keys, ["404", "true", "1.5"]);
    }

    /// RFC 0042 §3.2 — an unknown field inside a typed entry mapping is
    /// a parse error (strict schema, same posture as every section).
    #[test]
    fn promoted_typed_entry_unknown_field_rejected() {
        let lookup = env(&[]);
        let yaml = "
storage:
  promoted_attributes:
    log:
      - { key: cost_usd, type: f64, bloom: true }
";
        let err = parse(yaml, &lookup).expect_err("unknown field must fail");
        let msg = format!("{err}");
        assert!(
            msg.contains("bloom"),
            "error names the offending field: {msg}"
        );
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
        let tokens = auth.tokens.as_deref().expect("tokens list present");
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0].name.as_deref(), Some("edge-collector"));
        assert_eq!(tokens[0].token.as_deref(), Some("s3cr3t-edge"));
        assert_eq!(tokens[0].tenants, ["acme", "globex"]);
        assert_eq!(tokens[1].token.as_deref(), Some("")); // undefined, no default
        assert_eq!(tokens[1].tenants, ["*"]);

        assert!(
            parse("storage:\n  backend: local\n", &lookup)
                .expect("valid")
                .auth
                .is_none(),
            "no auth section parses to None (open mode)",
        );
        let empty = parse("auth:\n  tokens: []\n", &lookup).expect("valid");
        assert!(
            matches!(empty.auth.expect("present").tokens.as_deref(), Some([])),
            "an explicit empty list is distinguishable from an omitted one (RFC 0029 §3.1)",
        );
    }

    /// RFC0029.1 (schema) — `auth.oidc` parses with `${env:…}` substitution
    /// on every scalar, an omitted `tokens` list inside a present `auth`
    /// section parses to `None` (the oidc-only shape), and an unknown
    /// `oidc` field is a schema error.
    #[test]
    fn auth_oidc_parses_and_substitutes() {
        let lookup = env(&[("ISSUER", "https://dex.internal.example")]);
        let yaml = "
auth:
  oidc:
    issuer: ${env:ISSUER}
    audience: ourios
    tenant_claim: ourios_tenants
";
        let cfg = parse(yaml, &lookup).expect("valid");
        let auth = cfg.auth.expect("auth section present");
        assert!(auth.tokens.is_none(), "omitted tokens parses to None");
        let oidc = auth.oidc.expect("oidc present");
        assert_eq!(oidc.issuer.as_deref(), Some("https://dex.internal.example"));
        assert_eq!(oidc.audience.as_deref(), Some("ourios"));
        assert_eq!(oidc.tenant_claim.as_deref(), Some("ourios_tenants"));
        assert_eq!(oidc.name_claim, None, "name_claim defaults later, in core");

        let err = parse("auth:\n  oidc:\n    isser: x\n", &lookup).expect_err("typo");
        assert!(matches!(err, FileConfigError::Schema(_)), "got {err:?}");
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
        let rendered = format!(
            "{:?}",
            cfg.auth.expect("present").tokens.expect("list present")[0]
        );
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

    /// RFC 0047 §3.1 (`auth.openfga`): the section parses with `${env:…}`
    /// substitution on every leaf, an inline `api_token` literal is
    /// rejected (empty = unset, like an S3 credential), the resolved value
    /// is redacted in `Debug`, and the new OIDC claim keys parse.
    #[test]
    fn openfga_section_parses_substitutes_and_rejects_inline_token() {
        let lookup = env(&[
            ("FGA_URL", "http://openfga.auth.svc:8080"),
            ("FGA_TOKEN", "fga-s3cr3t"),
        ]);
        let cfg = parse(
            "auth:\n  oidc:\n    issuer: https://dex\n    audience: ourios\n    agent_claim: ourios_principal_type=agent\n    groups_claim: groups\n  openfga:\n    api_url: ${env:FGA_URL}\n    store_id: 01M07RYMXRDW4ND5M7XQV04W8R\n    api_token: ${env:FGA_TOKEN}\n    session_ttl_secs: 30\n    consistency: higher_consistency\n",
            &lookup,
        )
        .expect("valid");
        let auth = cfg.auth.expect("present");
        let oidc = auth.oidc.as_ref().expect("oidc");
        assert_eq!(
            oidc.agent_claim.as_deref(),
            Some("ourios_principal_type=agent")
        );
        assert_eq!(oidc.groups_claim.as_deref(), Some("groups"));
        let openfga = auth.openfga.as_ref().expect("openfga");
        assert_eq!(
            openfga.api_url.as_deref(),
            Some("http://openfga.auth.svc:8080")
        );
        assert_eq!(openfga.api_token.as_deref(), Some("fga-s3cr3t"));
        assert_eq!(openfga.session_ttl_secs.as_deref(), Some("30"));
        let rendered = format!("{openfga:?}");
        assert!(
            !rendered.contains("fga-s3cr3t"),
            "token redacted: {rendered}"
        );
        assert!(
            rendered.contains("<redacted>"),
            "shows presence: {rendered}"
        );

        let err = parse(
            "auth:\n  tokens:\n    - name: a\n      token: ${env:FGA_TOKEN}\n      tenants: [x]\n  openfga:\n    api_url: ${env:FGA_URL}\n    store_id: s\n    api_token: hardcoded\n",
            &lookup,
        )
        .expect_err("inline literal");
        assert!(
            matches!(err, FileConfigError::InlineOpenFgaToken),
            "got {err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("auth.openfga.api_token"),
            "names the key: {msg}"
        );
        assert!(!msg.contains("hardcoded"), "never the value: {msg}");

        parse(
            "auth:\n  tokens:\n    - name: a\n      token: ${env:FGA_TOKEN}\n      tenants: [x]\n  openfga:\n    api_url: ${env:FGA_URL}\n    store_id: s\n    api_token: \"\"\n",
            &lookup,
        )
        .expect("empty api_token reads as unset");

        let err = parse("auth:\n  openfga:\n    api_uri: x\n", &lookup).expect_err("typo");
        assert!(matches!(err, FileConfigError::Schema(_)), "got {err:?}");
    }

    /// RFC 0047 §3.4: the visibility section — `type` (a keyword, renamed
    /// onto `object_type`), substituted leaves, an explicit
    /// `content_columns` list distinct from an absent one, unknown keys
    /// rejected.
    #[test]
    fn openfga_visibility_section_parses() {
        let lookup = env(&[("CONV", "attr.gen_ai.conversation.id")]);
        let cfg = parse(
            "auth:\n  tokens:\n    - name: a\n      token: ${env:CONV}\n      tenants: [x]\n  openfga:\n    api_url: http://fga:8080\n    store_id: s\n    server_list_objects_deadline_ms: 3000\n    visibility:\n      objects:\n        - type: conversation\n          column: ${env:CONV}\n      identities:\n        user_columns: [\"${env:CONV}\", attr.enduser.pseudo.id]\n        agent_columns: [attr.bot.name]\n      self_principal_column: attr.user.hash\n      content_columns: [body, attr.prompt]\n      max_objects: 100\n      list_timeout_ms: 500\n",
            &lookup,
        )
        .expect("valid");
        let openfga = cfg.auth.expect("auth").openfga.expect("openfga");
        assert_eq!(
            openfga.server_list_objects_deadline_ms.as_deref(),
            Some("3000")
        );
        let visibility = &openfga.visibility;
        assert_eq!(
            visibility.objects[0].object_type.as_deref(),
            Some("conversation")
        );
        assert_eq!(
            visibility.objects[0].column.as_deref(),
            Some("attr.gen_ai.conversation.id"),
            "substituted"
        );
        assert_eq!(
            visibility.self_principal_column.as_deref(),
            Some("attr.user.hash")
        );
        assert_eq!(
            visibility.identities.user_columns.as_deref(),
            Some(
                &[
                    "attr.gen_ai.conversation.id".to_string(),
                    "attr.enduser.pseudo.id".to_string()
                ][..]
            ),
            "identities substituted (RFC 0048 §3.2)"
        );
        assert_eq!(
            visibility.identities.agent_columns.as_deref(),
            Some(&["attr.bot.name".to_string()][..])
        );
        assert_eq!(
            visibility.content_columns.as_deref(),
            Some(&["body".to_string(), "attr.prompt".to_string()][..]),
            "an explicit list replaces the default set (non-empty by startup validation)"
        );
        assert_eq!(visibility.max_objects.as_deref(), Some("100"));
        assert_eq!(visibility.list_timeout_ms.as_deref(), Some("500"));
        let cfg = parse(
            "auth:\n  tokens:\n    - name: a\n      token: ${env:CONV}\n      tenants: [x]\n  openfga:\n    api_url: http://fga:8080\n    store_id: s\n",
            &lookup,
        )
        .expect("valid");
        assert!(
            cfg.auth
                .expect("auth")
                .openfga
                .expect("openfga")
                .visibility
                .content_columns
                .is_none(),
            "absent = the default set"
        );
        let cfg = parse(
            "auth:\n  tokens:\n    - name: a\n      token: ${env:CONV}\n      tenants: [x]\n  openfga:\n    api_url: http://fga:8080\n    store_id: s\n",
            &lookup,
        )
        .expect("valid");
        let visibility = cfg.auth.expect("auth").openfga.expect("openfga").visibility;
        assert!(
            visibility.identities.user_columns.is_none()
                && visibility.identities.agent_columns.is_none(),
            "absent identities = the semconv defaults"
        );
        let err = parse(
            "auth:\n  openfga:\n    visibility:\n      objects:\n        - kind: conversation\n",
            &lookup,
        )
        .expect_err("typo");
        assert!(matches!(err, FileConfigError::Schema(_)), "got {err:?}");
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

    /// The substitution walk is a set of hand-maintained per-section
    /// field listings, and
    /// forgetting the line for a new field is a **silent** failure — the
    /// field simply never resolves its `${env:…}` reference (epic #745
    /// wave 0). This census parses a maximal config carrying a reference
    /// in every scalar leaf and asserts, generically over the serialized
    /// tree, that (a) no reference survives and (b) every reference in
    /// the YAML surfaced as a resolved leaf — so an omitted `substitute`
    /// line fails here instead of in production.
    ///
    /// **When you add a config field:** add it to this YAML with a
    /// `${env:…}` value. `deny_unknown_fields` keeps the document honest
    /// in the other direction.
    /// [`every_scalar_leaf_resolves_env_references`]'s generic tree
    /// walk: collect surviving references, count resolved sentinels.
    fn walk_census(v: &serde_json::Value, survivors: &mut Vec<String>, resolved: &mut usize) {
        match v {
            serde_json::Value::String(s) => {
                if s.contains("${env:") {
                    survivors.push(s.clone());
                } else if s.starts_with("resolved+") {
                    *resolved += 1;
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    walk_census(item, survivors, resolved);
                }
            }
            serde_json::Value::Object(map) => {
                for item in map.values() {
                    walk_census(item, survivors, resolved);
                }
            }
            _ => {}
        }
    }

    // The length is the maximal YAML document itself — one line per
    // config leaf is the census.
    #[allow(clippy::too_many_lines)]
    #[test]
    fn every_scalar_leaf_resolves_env_references() {
        let yaml = r"
storage:
  backend: ${env:L01}
  s3:
    bucket: ${env:L02}
    endpoint: ${env:L03}
    region: ${env:L04}
    prefix: ${env:L05}
    access_key_id: ${env:L06}
    secret_access_key: ${env:L07}
    session_token: ${env:L08}
  local:
    bucket_root: ${env:L09}
  promoted_attributes:
    resource:
      - key: ${env:L10}
        type: ${env:L11}
    log:
      - key: ${env:L12}
        type: ${env:L13}
receiver:
  enabled: ${env:L14}
  grpc_addr: ${env:L15}
  grpc_tls:
    cert_file: ${env:L16}
    key_file: ${env:L17}
    client_ca_file: ${env:L18}
    min_version: ${env:L19}
    reload_interval_secs: ${env:L20}
  http_addr: ${env:L21}
  http_tls:
    cert_file: ${env:L22}
    key_file: ${env:L23}
    client_ca_file: ${env:L24}
    min_version: ${env:L25}
    reload_interval_secs: ${env:L26}
  wal_root: ${env:L27}
  encode_workers: ${env:L28}
querier:
  enabled: ${env:L29}
  http_addr: ${env:L30}
  http_tls:
    cert_file: ${env:L31}
    key_file: ${env:L32}
    client_ca_file: ${env:L33}
    min_version: ${env:L34}
    reload_interval_secs: ${env:L35}
  default_window_secs: ${env:L36}
  mcp:
    enabled: ${env:L37}
compaction:
  enabled: ${env:L38}
  interval_secs: ${env:L39}
miner:
  upstream_templates: ${env:L40}
  upstream_template_byte_limit: ${env:L41}
  upstream_association_limit: ${env:L42}
auth:
  tokens:
    - name: ${env:L43}
      token: ${env:L44}
      tenants:
        - ${env:L45}
        - ${env:L46}
  oidc:
    issuer: ${env:L47}
    audience: ${env:L48}
    tenant_claim: ${env:L49}
    name_claim: ${env:L50}
    clock_skew_secs: ${env:L51}
    agent_claim: ${env:L52}
    groups_claim: ${env:L53}
  openfga:
    api_url: ${env:L54}
    store_id: ${env:L55}
    authorization_model_id: ${env:L56}
    api_token: ${env:L57}
    session_ttl_secs: ${env:L58}
    consistency: ${env:L59}
    request_timeout_secs: ${env:L60}
    server_list_objects_deadline_ms: ${env:L61}
    visibility:
      objects:
        - type: ${env:L62}
          column: ${env:L63}
      identities:
        user_columns:
          - ${env:L64}
        agent_columns:
          - ${env:L65}
      self_principal_column: ${env:L66}
      content_columns:
        - ${env:L67}
      max_objects: ${env:L68}
      list_timeout_ms: ${env:L69}
";
        let refs_in_yaml = yaml.matches("${env:").count();
        let config = parse(yaml, &|name| Some(format!("resolved+{name}")))
            .expect("the maximal config parses");
        let tree = serde_json::to_value(&config).expect("serializes");

        let mut survivors = Vec::new();
        let mut resolved = 0usize;
        walk_census(&tree, &mut survivors, &mut resolved);

        assert!(
            survivors.is_empty(),
            "leaves whose substitute line is missing: {survivors:?}",
        );
        assert_eq!(
            resolved, refs_in_yaml,
            "every YAML reference must surface as a resolved leaf \
             (a mismatch means a field in this YAML never reached the tree)",
        );
    }
}
