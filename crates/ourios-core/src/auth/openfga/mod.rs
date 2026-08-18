//! The RFC 0047 §3.1 `OpenFGA` resolver: configuration and the principal
//! vocabulary here (always compiled — they are plain data the config
//! layer maps onto), the HTTP client and the session resolver in
//! the `client` submodule behind the `openfga` feature (re-exported here).
//!
//! `OpenFGA` does not authenticate: the static store or the OIDC verifier
//! establishes *who* the bearer is, and this layer asks the graph *what*
//! that principal may query and write, producing the same tenant binding
//! RFC 0026 enforcement consumes. Everything it answers is fail-closed —
//! an unreachable or slow `OpenFGA` is a `503`, never an open door.

use std::fmt;
use std::time::Duration;

#[cfg(feature = "openfga")]
mod client;
#[cfg(feature = "openfga")]
pub use client::{
    Grants, ListObjectsRequest, MAX_CONTEXTUAL_TUPLES, OpenFgaClient, OpenFgaError,
    OpenFgaResolver, TupleKey, Visibility,
};

/// The default `session_ttl_secs` (RFC 0047 §3.1): revocation latency.
pub const DEFAULT_SESSION_TTL_SECS: u64 = 60;
/// The default per-call `OpenFGA` request timeout — the fail-closed bound
/// on every resolution round-trip.
pub const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 5;

/// The raw `auth.openfga` section as the config layer hands it over
/// (RFC 0047 §3.1) — substituted but not yet validated. `api_token` is
/// **secret**: the manual [`fmt::Debug`] redacts it and the
/// `ourios-server` config path accepts it only as an `${env:…}` reference.
#[derive(Default, Clone)]
pub struct OpenFgaSpec {
    /// The `OpenFGA` HTTP API root (`http://openfga.auth.svc:8080`).
    pub api_url: Option<String>,
    /// The store id.
    pub store_id: Option<String>,
    /// The pinned authorization model id; absent = the store's latest.
    pub authorization_model_id: Option<String>,
    /// The API bearer token, if the server requires one (**secret**).
    pub api_token: Option<String>,
    /// How long a resolved binding is cached per credential.
    pub session_ttl_secs: Option<String>,
    /// `minimize_latency` (default) or `higher_consistency`.
    pub consistency: Option<String>,
    /// The per-call request timeout in seconds.
    pub request_timeout_secs: Option<String>,
    /// The layer-2 visibility section (RFC 0047 §3.4).
    pub visibility: VisibilitySpec,
    /// The `OpenFGA` server's own `OPENFGA_LIST_OBJECTS_DEADLINE`, in
    /// milliseconds, as the operator declares it (default 3000).
    pub server_list_objects_deadline_ms: Option<String>,
}

/// The raw `auth.openfga.visibility` section (RFC 0047 §3.4) — nothing
/// here is secret.
#[derive(Debug, Default, Clone)]
pub struct VisibilitySpec {
    /// `objects[]`: graph object type → the promoted column carrying its id.
    pub objects: Vec<VisibilityObjectSpec>,
    /// The promoted column compared to a `user:` principal's subject (the
    /// §3.3 self fast path); unset disables the path.
    pub self_principal_column: Option<String>,
    /// The columns a metadata-only reader sees as NULL and may not filter or
    /// aggregate on. `None` = the `GenAI` content default set.
    pub content_columns: Option<Vec<String>>,
    /// The bound on tenant-scoped ids per enumeration (default 10 000).
    pub max_objects: Option<String>,
    /// The client-side enumeration timeout in milliseconds (default 2000).
    pub list_timeout_ms: Option<String>,
}

/// One `visibility.objects[]` entry, raw.
#[derive(Debug, Default, Clone)]
pub struct VisibilityObjectSpec {
    /// The `OpenFGA` object type (`conversation`).
    pub object_type: Option<String>,
    /// The promoted column (`attr.gen_ai.conversation.id`).
    pub column: Option<String>,
}

impl fmt::Debug for OpenFgaSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenFgaSpec")
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

/// `OpenFGA`'s per-request consistency preference (RFC 0047 §3.1):
/// `higher_consistency` bypasses the server's check cache, so a grant
/// written a moment ago is honoured on the next resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Consistency {
    /// Serve from `OpenFGA`'s cache when it can.
    MinimizeLatency,
    /// Bypass `OpenFGA`'s cache.
    HigherConsistency,
}

impl Consistency {
    /// The wire value in the `OpenFGA` HTTP API.
    #[must_use]
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::MinimizeLatency => "MINIMIZE_LATENCY",
            Self::HigherConsistency => "HIGHER_CONSISTENCY",
        }
    }
}

/// The validated `auth.openfga` configuration (RFC 0047 §3.1).
#[derive(Clone, PartialEq, Eq)]
pub struct OpenFgaConfig {
    api_url: String,
    store_id: String,
    authorization_model_id: Option<String>,
    api_token: Option<String>,
    session_ttl: Duration,
    consistency: Consistency,
    request_timeout: Duration,
    visibility: VisibilityConfig,
}

/// The validated `auth.openfga.visibility` configuration (RFC 0047 §3.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibilityConfig {
    objects: Vec<VisibilityObject>,
    self_principal_column: Option<String>,
    content_columns: Vec<String>,
    max_objects: usize,
    list_timeout: Duration,
}

/// One bound object type: which promoted column carries its ids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibilityObject {
    object_type: String,
    column: String,
}

impl VisibilityObject {
    /// The `OpenFGA` object type.
    #[must_use]
    pub fn object_type(&self) -> &str {
        &self.object_type
    }

    /// The promoted column carrying the object ids.
    #[must_use]
    pub fn column(&self) -> &str {
        &self.column
    }
}

impl VisibilityConfig {
    /// The bound object types, in configuration order.
    #[must_use]
    pub fn objects(&self) -> &[VisibilityObject] {
        &self.objects
    }

    /// The self-fast-path column, when enabled.
    #[must_use]
    pub fn self_principal_column(&self) -> Option<&str> {
        self.self_principal_column.as_deref()
    }

    /// The content columns (DSL names: `body`, `attr.<key>`).
    #[must_use]
    pub fn content_columns(&self) -> &[String] {
        &self.content_columns
    }

    /// The per-tenant enumeration bound.
    #[must_use]
    pub fn max_objects(&self) -> usize {
        self.max_objects
    }

    /// The client-side enumeration timeout.
    #[must_use]
    pub fn list_timeout(&self) -> Duration {
        self.list_timeout
    }
}

/// The RFC 0047 §3.4 default content columns: the `GenAI` semconv content
/// attributes plus the log body.
pub const DEFAULT_CONTENT_COLUMNS: [&str; 6] = [
    "body",
    "attr.gen_ai.input.messages",
    "attr.gen_ai.output.messages",
    "attr.gen_ai.system_instructions",
    "attr.gen_ai.tool.call.arguments",
    "attr.gen_ai.tool.call.result",
];
/// The default `visibility.max_objects`.
pub const DEFAULT_MAX_OBJECTS: usize = 10_000;
/// The default `visibility.list_timeout_ms`.
pub const DEFAULT_LIST_TIMEOUT_MS: u64 = 2_000;
/// The default `server_list_objects_deadline_ms` (`OpenFGA`'s own default).
pub const DEFAULT_SERVER_LIST_OBJECTS_DEADLINE_MS: u64 = 3_000;
/// The `OpenFGA` object type of a conversation — the one bindable type in v1.
pub const CONVERSATION_TYPE: &str = "conversation";
/// The RFC 0027 MCP tools as graph objects (RFC 0047 §3.5):
/// `tool:<T>/<name>`; the emitter writes their `parent` tuples per tenant.
pub const MCP_TOOL_NAMES: [&str; 3] = ["query_logs", "list_templates", "template_drift"];

impl fmt::Debug for OpenFgaConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenFgaConfig")
            .field("api_url", &self.api_url)
            .field("store_id", &self.store_id)
            .field("authorization_model_id", &self.authorization_model_id)
            .field("api_token", &self.api_token.as_ref().map(|_| "<redacted>"))
            .field("session_ttl", &self.session_ttl)
            .field("consistency", &self.consistency)
            .field("request_timeout", &self.request_timeout)
            .field("visibility", &self.visibility)
            .finish()
    }
}

impl OpenFgaConfig {
    /// The `OpenFGA` HTTP API root, without a trailing slash.
    #[must_use]
    pub fn api_url(&self) -> &str {
        &self.api_url
    }

    /// The store id.
    #[must_use]
    pub fn store_id(&self) -> &str {
        &self.store_id
    }

    /// The pinned authorization model id, if any.
    #[must_use]
    pub fn authorization_model_id(&self) -> Option<&str> {
        self.authorization_model_id.as_deref()
    }

    /// The API bearer token, if any (**secret** — never log it).
    #[must_use]
    pub fn api_token(&self) -> Option<&str> {
        self.api_token.as_deref()
    }

    /// The per-credential binding cache lifetime.
    #[must_use]
    pub fn session_ttl(&self) -> Duration {
        self.session_ttl
    }

    /// The consistency preference sent on every call.
    #[must_use]
    pub fn consistency(&self) -> Consistency {
        self.consistency
    }

    /// The per-call request timeout.
    #[must_use]
    pub fn request_timeout(&self) -> Duration {
        self.request_timeout
    }

    /// The layer-2 visibility configuration.
    #[must_use]
    pub fn visibility(&self) -> &VisibilityConfig {
        &self.visibility
    }
}

/// Validate a raw [`OpenFgaSpec`] into the resolved [`OpenFgaConfig`]
/// (RFC 0047 §3.1).
///
/// # Errors
///
/// `api_url` (an `http://` / `https://` root) and `store_id` are required
/// and must be non-empty without surrounding whitespace; the optional
/// fields must parse (`session_ttl_secs` / `request_timeout_secs` as
/// non-negative integers — the timeout additionally non-zero —
/// `consistency` as one of its two values). Error text never carries the
/// token value.
pub fn build_openfga_config(spec: &OpenFgaSpec) -> Result<OpenFgaConfig, String> {
    let required = |key: &str, value: Option<&str>| match value {
        Some(v) if !v.is_empty() && v.trim() == v => Ok(v.to_string()),
        _ => Err(format!(
            "auth.openfga.{key} is required and must be non-empty without \
             surrounding whitespace (RFC 0047 §3.1)"
        )),
    };
    let api_url = required("api_url", spec.api_url.as_deref())?;
    if !(api_url.starts_with("http://") || api_url.starts_with("https://")) {
        return Err(
            "auth.openfga.api_url must be an http:// or https:// root (RFC 0047 §3.1)".to_string(),
        );
    }
    let api_url = api_url.trim_end_matches('/').to_string();
    let store_id = required("store_id", spec.store_id.as_deref())?;
    let authorization_model_id = match spec.authorization_model_id.as_deref() {
        None | Some("") => None,
        some => Some(required("authorization_model_id", some)?),
    };
    let api_token = match spec.api_token.as_deref() {
        None | Some("") => None,
        Some(token) => Some(token.to_string()),
    };
    let secs = |key: &str, raw: Option<&str>, default: u64| -> Result<u64, String> {
        match raw {
            None => Ok(default),
            Some(raw) => raw.trim().parse().map_err(|_| {
                format!(
                    "auth.openfga.{key} must be a non-negative integer number of \
                     seconds (RFC 0047 §3.1)"
                )
            }),
        }
    };
    let session_ttl = Duration::from_secs(secs(
        "session_ttl_secs",
        spec.session_ttl_secs.as_deref(),
        DEFAULT_SESSION_TTL_SECS,
    )?);
    let request_timeout_secs = secs(
        "request_timeout_secs",
        spec.request_timeout_secs.as_deref(),
        DEFAULT_REQUEST_TIMEOUT_SECS,
    )?;
    if request_timeout_secs == 0 {
        return Err(
            "auth.openfga.request_timeout_secs must be at least 1 — a resolution \
             without a deadline cannot fail closed (RFC 0047 §3.1)"
                .to_string(),
        );
    }
    let consistency = match spec.consistency.as_deref() {
        None | Some("minimize_latency") => Consistency::MinimizeLatency,
        Some("higher_consistency") => Consistency::HigherConsistency,
        Some(_) => {
            return Err(
                "auth.openfga.consistency must be minimize_latency or higher_consistency \
                 (RFC 0047 §3.1)"
                    .to_string(),
            );
        }
    };
    let visibility = build_visibility_config(
        &spec.visibility,
        spec.server_list_objects_deadline_ms.as_deref(),
    )?;
    Ok(OpenFgaConfig {
        api_url,
        store_id,
        authorization_model_id,
        api_token,
        session_ttl,
        consistency,
        request_timeout: Duration::from_secs(request_timeout_secs),
        visibility,
    })
}

/// Validate `visibility.objects[]`: v1 binds at most the `conversation`
/// type, once, to a promoted column.
fn build_visibility_objects(
    specs: &[VisibilityObjectSpec],
    promoted_column: impl Fn(&str, &str) -> Result<String, String>,
) -> Result<Vec<VisibilityObject>, String> {
    let mut objects: Vec<VisibilityObject> = Vec::with_capacity(specs.len());
    for (index, object) in specs.iter().enumerate() {
        let object_type = match object.object_type.as_deref() {
            Some(CONVERSATION_TYPE) => CONVERSATION_TYPE.to_string(),
            _ => {
                return Err(format!(
                    "auth.openfga.visibility.objects[{index}].type must be \
                     `conversation` — the one object type v1 binds (RFC 0047 §3.4)"
                ));
            }
        };
        if objects.iter().any(|o| o.object_type == object_type) {
            return Err(format!(
                "auth.openfga.visibility.objects[{index}]: type `{object_type}` \
                 bound twice (RFC 0047 §3.4)"
            ));
        }
        let column = promoted_column(
            &format!("objects[{index}].column"),
            object.column.as_deref().unwrap_or_default(),
        )?;
        objects.push(VisibilityObject {
            object_type,
            column,
        });
    }
    Ok(objects)
}

/// Validate the raw visibility section (RFC 0047 §3.4).
///
/// # Errors
///
/// v1 binds at most the `conversation` type, to an `attr.`/`resource.`
/// promoted column; `self_principal_column` must be such a column;
/// `content_columns` entries must be `body` or `attr.`/`resource.` names;
/// `max_objects` ≥ 1; `list_timeout_ms` ≥ 1 and **strictly below**
/// `server_list_objects_deadline_ms` — the client timeout must be the one
/// that fires, so an incomplete enumeration is always detected here.
fn build_visibility_config(
    spec: &VisibilitySpec,
    server_deadline_ms: Option<&str>,
) -> Result<VisibilityConfig, String> {
    let promoted_column = |key: &str, value: &str| -> Result<String, String> {
        if value.is_empty()
            || value.trim() != value
            || !(value.starts_with("attr.") || value.starts_with("resource."))
        {
            return Err(format!(
                "auth.openfga.visibility.{key} must name a promoted column as \
                 `attr.<key>` or `resource.<key>` (RFC 0047 §3.4)"
            ));
        }
        Ok(value.to_string())
    };
    let objects = build_visibility_objects(&spec.objects, promoted_column)?;
    let self_principal_column = match spec.self_principal_column.as_deref() {
        None | Some("") => None,
        Some(column) => Some(promoted_column("self_principal_column", column)?),
    };
    let content_columns = match &spec.content_columns {
        None => DEFAULT_CONTENT_COLUMNS
            .iter()
            .map(|c| (*c).to_string())
            .collect(),
        // Masking is never silently disabled: an empty list would let a
        // metadata-only reader read every content column.
        Some(columns) if columns.is_empty() => {
            return Err(
                "auth.openfga.visibility.content_columns must not be empty — omit it for the \
                 default set; metadata-only readers always have content masked (RFC 0047 §3.4)"
                    .to_string(),
            );
        }
        Some(columns) => columns
            .iter()
            .enumerate()
            .map(|(index, column)| match column.as_str() {
                "body" => Ok(column.clone()),
                other => promoted_column(&format!("content_columns[{index}]"), other),
            })
            .collect::<Result<Vec<_>, _>>()?,
    };
    let count = |key: &str, raw: Option<&str>, default: u64| -> Result<u64, String> {
        match raw {
            None => Ok(default),
            Some(raw) => match raw.trim().parse::<u64>() {
                Ok(n) if n >= 1 => Ok(n),
                _ => Err(format!(
                    "auth.openfga.{key} must be a positive integer (RFC 0047 §3.4)"
                )),
            },
        }
    };
    let max_objects = usize::try_from(count(
        "visibility.max_objects",
        spec.max_objects.as_deref(),
        DEFAULT_MAX_OBJECTS as u64,
    )?)
    .map_err(|_| "auth.openfga.visibility.max_objects is out of range".to_string())?;
    let list_timeout_ms = count(
        "visibility.list_timeout_ms",
        spec.list_timeout_ms.as_deref(),
        DEFAULT_LIST_TIMEOUT_MS,
    )?;
    let server_deadline_ms = count(
        "server_list_objects_deadline_ms",
        server_deadline_ms,
        DEFAULT_SERVER_LIST_OBJECTS_DEADLINE_MS,
    )?;
    if list_timeout_ms >= server_deadline_ms {
        return Err(format!(
            "auth.openfga.visibility.list_timeout_ms ({list_timeout_ms}) must be strictly \
             below auth.openfga.server_list_objects_deadline_ms ({server_deadline_ms}): the \
             client timeout must be the one that fires, so an incomplete enumeration is \
             detected and failed closed here (RFC 0047 §3.4)"
        ));
    }
    Ok(VisibilityConfig {
        objects,
        self_principal_column,
        content_columns,
        max_objects,
        list_timeout: Duration::from_millis(list_timeout_ms),
    })
}

/// The principal a credential maps to (RFC 0047 §3.1): the `OpenFGA` user
/// type is fixed by *how* the bearer authenticated, the id by who it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PrincipalKind {
    /// An OIDC subject without the agent claim → `user:<sub>`.
    User,
    /// An OIDC subject carrying the configured agent claim → `agent:<sub>`.
    Agent,
    /// A static token → `service_account:<name>`.
    ServiceAccount,
}

impl PrincipalKind {
    /// The `OpenFGA` type name — must match `deploy/openfga/model.fga`.
    #[must_use]
    pub fn type_name(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Agent => "agent",
            Self::ServiceAccount => "service_account",
        }
    }
}

/// A resolved principal: `<type>:<id>` on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Principal {
    kind: PrincipalKind,
    id: String,
}

impl Principal {
    /// A principal of `kind` with the given id (an OIDC `sub` or a static
    /// token name).
    #[must_use]
    pub fn new(kind: PrincipalKind, id: impl Into<String>) -> Self {
        Self {
            kind,
            id: id.into(),
        }
    }

    /// The principal's `OpenFGA` type.
    #[must_use]
    pub fn kind(&self) -> PrincipalKind {
        self.kind
    }

    /// The principal's id without the type prefix.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }
}

impl fmt::Display for Principal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.kind.type_name(), self.id)
    }
}

/// The `OpenFGA` object type of an RFC 0046 tenant — the RFC 0047 §3.2
/// `tenant` type, the object every resource hangs off.
pub const TENANT_TYPE: &str = "tenant";

/// `OpenFGA`'s object-id limit.
pub const MAX_OBJECT_ID_BYTES: usize = 256;

/// Whether `id` can be the id half of an `OpenFGA` object or user
/// (`type:id`): non-empty, at most 256 bytes, no `:`, `#` or whitespace.
#[must_use]
pub fn is_object_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_OBJECT_ID_BYTES
        && !id
            .chars()
            .any(|c| c == ':' || c == '#' || c.is_whitespace())
}

/// The naming rule for tenant-scoped objects (RFC 0047 §3.3) — the **one**
/// place it lives, used by the planner and the emitter alike. The tenant is
/// its own object, `tenant:<T>`; a conversation inside it is
/// `conversation:<enc(T)>/<id>` where `enc` percent-encodes `%` and `/` in
/// the tenant so the `/` separator is unambiguous — `a` + `b/c-1` and
/// `a/b` + `c-1` are two different objects — and the raw conversation id
/// follows verbatim (it may itself contain `/`).
///
/// A tenant that cannot be an object id at all (`:`, `#`, whitespace, too
/// long, empty) has no graph objects; callers fail closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TenantObjects {
    tenant_object: String,
    conversation_prefix: String,
    tool_prefix: String,
}

impl TenantObjects {
    /// The graph objects of `tenant`, or `None` when the tenant id cannot
    /// form an object id — raw, or once its segment is encoded (the
    /// encoding can only grow it; a tenant whose encoded segment plus the
    /// `/` separator leaves no room for a conversation id has no
    /// tenant-scoped objects).
    #[must_use]
    pub fn new(tenant: &str) -> Option<Self> {
        if !is_object_id(tenant) {
            return None;
        }
        let encoded = encode_tenant_segment(tenant);
        if encoded.len() + 1 >= MAX_OBJECT_ID_BYTES {
            return None;
        }
        Some(Self {
            tenant_object: format!("{TENANT_TYPE}:{tenant}"),
            conversation_prefix: format!("{CONVERSATION_TYPE}:{encoded}/"),
            tool_prefix: format!("tool:{encoded}/"),
        })
    }

    /// `tenant:<T>`.
    #[must_use]
    pub fn tenant(&self) -> &str {
        &self.tenant_object
    }

    /// `conversation:<enc(T)>/` — every conversation of the tenant starts
    /// with this; the remainder is the raw conversation id.
    #[must_use]
    pub fn conversation_prefix(&self) -> &str {
        &self.conversation_prefix
    }

    /// `conversation:<enc(T)>/<id>`.
    #[must_use]
    pub fn conversation(&self, id: &str) -> String {
        format!("{}{id}", self.conversation_prefix)
    }

    /// Whether `conversation:<enc(T)>/<id>` is a valid object: `id` must be
    /// an object id itself and the combined id half must fit `OpenFGA`'s
    /// 256-byte limit. The emitter skips ids that do not.
    #[must_use]
    pub fn conversation_fits(&self, id: &str) -> bool {
        is_object_id(id)
            && self.conversation_prefix.len() - CONVERSATION_TYPE.len() - 1 + id.len()
                <= MAX_OBJECT_ID_BYTES
    }

    /// `tool:<enc(T)>/<name>`.
    #[must_use]
    pub fn tool(&self, name: &str) -> String {
        format!("{}{name}", self.tool_prefix)
    }
}

/// Percent-encode the two bytes that would make the tenant segment of a
/// `conversation:<T>/<id>` object ambiguous.
fn encode_tenant_segment(tenant: &str) -> String {
    let mut out = String::with_capacity(tenant.len());
    for c in tenant.chars() {
        match c {
            '%' => out.push_str("%25"),
            '/' => out.push_str("%2F"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{Consistency, OpenFgaSpec, Principal, PrincipalKind, build_openfga_config};

    fn spec() -> OpenFgaSpec {
        OpenFgaSpec {
            api_url: Some("http://openfga.auth.svc:8080/".to_string()),
            store_id: Some("01M07RYMXRDW4ND5M7XQV04W8R".to_string()),
            ..OpenFgaSpec::default()
        }
    }

    /// RFC 0047 §3.1 config validation: required roots, defaults, and
    /// each rejected shape naming its key — never the token value.
    #[test]
    fn config_requires_roots_and_defaults_the_rest() {
        let config = build_openfga_config(&spec()).expect("valid");
        assert_eq!(config.api_url(), "http://openfga.auth.svc:8080");
        assert_eq!(config.store_id(), "01M07RYMXRDW4ND5M7XQV04W8R");
        assert_eq!(config.authorization_model_id(), None);
        assert_eq!(config.api_token(), None);
        assert_eq!(config.session_ttl(), Duration::from_secs(60));
        assert_eq!(config.consistency(), Consistency::MinimizeLatency);
        assert_eq!(config.request_timeout(), Duration::from_secs(5));

        let full = build_openfga_config(&OpenFgaSpec {
            authorization_model_id: Some("01M07RZE9RHPVPTYCV22RX0TDA".to_string()),
            api_token: Some("s3cr3t".to_string()),
            session_ttl_secs: Some("0".to_string()),
            consistency: Some("higher_consistency".to_string()),
            request_timeout_secs: Some("2".to_string()),
            ..spec()
        })
        .expect("valid");
        assert_eq!(
            full.authorization_model_id(),
            Some("01M07RZE9RHPVPTYCV22RX0TDA")
        );
        assert_eq!(full.api_token(), Some("s3cr3t"));
        assert_eq!(full.session_ttl(), Duration::ZERO);
        assert_eq!(full.consistency(), Consistency::HigherConsistency);
        assert_eq!(full.request_timeout(), Duration::from_secs(2));
        let rendered = format!("{full:?}");
        assert!(!rendered.contains("s3cr3t"), "token redacted: {rendered}");

        for (key, spec) in [
            (
                "api_url",
                OpenFgaSpec {
                    api_url: None,
                    ..spec()
                },
            ),
            (
                "api_url",
                OpenFgaSpec {
                    api_url: Some("openfga:8080".to_string()),
                    ..spec()
                },
            ),
            (
                "store_id",
                OpenFgaSpec {
                    store_id: Some(" x".to_string()),
                    ..spec()
                },
            ),
            (
                "session_ttl_secs",
                OpenFgaSpec {
                    session_ttl_secs: Some("soon".to_string()),
                    ..spec()
                },
            ),
            (
                "request_timeout_secs",
                OpenFgaSpec {
                    request_timeout_secs: Some("0".to_string()),
                    ..spec()
                },
            ),
            (
                "consistency",
                OpenFgaSpec {
                    consistency: Some("eventual".to_string()),
                    ..spec()
                },
            ),
        ] {
            let err = build_openfga_config(&spec).expect_err("invalid");
            assert!(
                err.contains(&format!("auth.openfga.{key}")),
                "{key} named: {err}"
            );
        }
    }

    /// RFC 0047 §3.4 visibility validation: defaults, the conversation-only
    /// binding, promoted-column names, and the client-below-server timeout
    /// rule — each rejection naming its key.
    #[test]
    #[allow(clippy::too_many_lines)] // one validation matrix, one test
    fn visibility_config_defaults_and_rules() {
        use super::{VisibilityObjectSpec, VisibilitySpec};
        let defaults = build_openfga_config(&spec())
            .expect("valid")
            .visibility()
            .clone();
        assert!(defaults.objects().is_empty());
        assert_eq!(defaults.self_principal_column(), None);
        assert_eq!(defaults.content_columns()[0], "body");
        assert_eq!(defaults.content_columns().len(), 6);
        assert_eq!(defaults.max_objects(), 10_000);
        assert_eq!(defaults.list_timeout(), Duration::from_secs(2));

        let bound = build_openfga_config(&OpenFgaSpec {
            visibility: VisibilitySpec {
                objects: vec![VisibilityObjectSpec {
                    object_type: Some("conversation".to_string()),
                    column: Some("attr.gen_ai.conversation.id".to_string()),
                }],
                self_principal_column: Some("attr.user.hash".to_string()),
                content_columns: Some(vec!["body".to_string(), "attr.prompt".to_string()]),
                max_objects: Some("100".to_string()),
                list_timeout_ms: Some("500".to_string()),
            },
            server_list_objects_deadline_ms: Some("1000".to_string()),
            ..spec()
        })
        .expect("valid");
        let visibility = bound.visibility();
        assert_eq!(visibility.objects()[0].object_type(), "conversation");
        assert_eq!(
            visibility.objects()[0].column(),
            "attr.gen_ai.conversation.id"
        );
        assert_eq!(visibility.self_principal_column(), Some("attr.user.hash"));
        assert_eq!(visibility.content_columns(), ["body", "attr.prompt"]);
        assert_eq!(visibility.max_objects(), 100);
        assert_eq!(visibility.list_timeout(), Duration::from_millis(500));

        let object = |object_type: &str, column: &str| VisibilityObjectSpec {
            object_type: Some(object_type.to_string()),
            column: Some(column.to_string()),
        };
        for (key, visibility, deadline) in [
            (
                "objects[0].type",
                VisibilitySpec {
                    objects: vec![object("tool", "attr.tool")],
                    ..VisibilitySpec::default()
                },
                None,
            ),
            (
                "objects[1]",
                VisibilitySpec {
                    objects: vec![
                        object("conversation", "attr.a"),
                        object("conversation", "attr.b"),
                    ],
                    ..VisibilitySpec::default()
                },
                None,
            ),
            (
                "objects[0].column",
                VisibilitySpec {
                    objects: vec![object("conversation", "gen_ai.conversation.id")],
                    ..VisibilitySpec::default()
                },
                None,
            ),
            (
                "self_principal_column",
                VisibilitySpec {
                    self_principal_column: Some("user.hash".to_string()),
                    ..VisibilitySpec::default()
                },
                None,
            ),
            (
                "content_columns[1]",
                VisibilitySpec {
                    content_columns: Some(vec!["body".to_string(), "severity".to_string()]),
                    ..VisibilitySpec::default()
                },
                None,
            ),
            (
                "visibility.max_objects",
                VisibilitySpec {
                    max_objects: Some("0".to_string()),
                    ..VisibilitySpec::default()
                },
                None,
            ),
            (
                "visibility.list_timeout_ms",
                VisibilitySpec {
                    list_timeout_ms: Some("3000".to_string()),
                    ..VisibilitySpec::default()
                },
                None,
            ),
            (
                "visibility.list_timeout_ms",
                VisibilitySpec::default(),
                Some("2000"),
            ),
            (
                "content_columns must not be empty",
                VisibilitySpec {
                    content_columns: Some(Vec::new()),
                    ..VisibilitySpec::default()
                },
                None,
            ),
        ] {
            let err = build_openfga_config(&OpenFgaSpec {
                visibility,
                server_list_objects_deadline_ms: deadline.map(str::to_string),
                ..spec()
            })
            .expect_err("invalid");
            assert!(err.contains(key), "{key} named: {err}");
        }
    }

    /// The tenant-scoped object naming rule is injective in the tenant
    /// (`/` and `%` percent-encoded in the tenant segment) and refuses a
    /// tenant that cannot be an object id.
    #[test]
    fn tenant_objects_are_unambiguous() {
        use super::TenantObjects;
        let a = TenantObjects::new("a").expect("valid");
        let ab = TenantObjects::new("a/b").expect("valid");
        assert_eq!(a.tenant(), "tenant:a");
        assert_eq!(a.conversation("b/c-1"), "conversation:a/b/c-1");
        assert_eq!(ab.conversation("c-1"), "conversation:a%2Fb/c-1");
        assert_ne!(a.conversation("b/c-1"), ab.conversation("c-1"));
        assert_eq!(ab.conversation_prefix(), "conversation:a%2Fb/");
        assert_eq!(
            TenantObjects::new("100%").expect("valid").conversation("x"),
            "conversation:100%25/x"
        );
        assert_eq!(a.tool("query_logs"), "tool:a/query_logs");
        for bad in ["", "a b", "a:b", "a#b"] {
            assert!(TenantObjects::new(bad).is_none(), "{bad:?}");
        }
        // The encoding may not push the segment past the object-id limit,
        // and a conversation id must fit next to it.
        let slashes = "/".repeat(90); // 90 raw bytes → 270 encoded
        assert!(TenantObjects::new(&slashes).is_none());
        let long = "x".repeat(200);
        let t = TenantObjects::new(&long).expect("fits alone");
        assert!(t.conversation_fits("c-1"));
        assert!(!t.conversation_fits(&"y".repeat(60)), "201 + 60 > 256");
        assert!(!t.conversation_fits("a b"));
    }

    /// The principal vocabulary renders exactly the model's type names.
    #[test]
    fn principals_render_model_types() {
        assert_eq!(
            Principal::new(PrincipalKind::User, "alice").to_string(),
            "user:alice"
        );
        assert_eq!(
            Principal::new(PrincipalKind::Agent, "bot").to_string(),
            "agent:bot"
        );
        assert_eq!(
            Principal::new(PrincipalKind::ServiceAccount, "collector").to_string(),
            "service_account:collector"
        );
    }
}
