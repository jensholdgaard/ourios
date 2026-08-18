//! The `OpenFGA` HTTP client and the session resolver (RFC 0047 §3.1).
//!
//! Three calls, all fail-closed: `Check`, `Write`, and the **streamed**
//! `ListObjects` — never the plain one, whose 1000-object cap is silent
//! (RFC 0047 §10: HTTP 200, no truncation marker). The resolver turns a
//! principal into the tenant sets it may query and write, caches the
//! answer per credential for `session_ttl_secs`, and never caches an error.

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::{
    CONVERSATION_TYPE, Consistency, OpenFgaConfig, Principal, PrincipalKind, TENANT_TYPE,
    TenantObjects, VisibilityConfig, is_object_id,
};

/// `OpenFGA`'s cap on contextual tuples per request (RFC 0047 §3.1): a
/// token carrying more groups than this fails resolution closed.
pub const MAX_CONTEXTUAL_TUPLES: usize = 100;
/// The bound on a principal's tenant set at session establishment.
/// Tenant sets are small by construction (RFC 0047 §3.1); reaching this
/// is a misconfiguration, answered fail-closed rather than with a
/// truncated binding.
const MAX_TENANTS_PER_PRINCIPAL: usize = 10_000;
/// The longest single NDJSON line accepted from a stream — one object id
/// plus framing; anything longer is not an `OpenFGA` response.
const MAX_LINE_BYTES: usize = 64 * 1024;
/// Cache-size threshold past which an insert first sweeps expired entries.
const CACHE_SWEEP_THRESHOLD: usize = 1024;
/// Hard capacity of the session cache: past it an insert evicts the entry
/// closest to expiry, so many live principals cannot grow the map (or the
/// per-insert sweep) without bound.
const MAX_CACHE_ENTRIES: usize = 4096;
/// How much of a non-2xx body is kept for the error message.
const MAX_ERROR_BODY_BYTES: usize = 512;

/// One relationship tuple / tuple key on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TupleKey {
    /// `<type>:<id>` or a userset `<type>:<id>#<relation>`.
    pub user: String,
    /// The relation name.
    pub relation: String,
    /// `<type>:<id>`.
    pub object: String,
}

impl TupleKey {
    /// A tuple key from its three parts.
    #[must_use]
    pub fn new(
        user: impl Into<String>,
        relation: impl Into<String>,
        object: impl Into<String>,
    ) -> Self {
        Self {
            user: user.into(),
            relation: relation.into(),
            object: object.into(),
        }
    }
}

/// Why an `OpenFGA` call did not produce an answer. Every variant is
/// fail-closed at the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenFgaError {
    /// Transport failure, timeout before an answer, or a non-2xx response
    /// — the `503` / `UNAVAILABLE` class (`error.type =
    /// upstream_unavailable`).
    Unavailable(String),
    /// More contextual tuples than `OpenFGA` accepts per request.
    TooManyContextualTuples {
        /// How many the caller wanted to send.
        count: usize,
    },
    /// A principal id (OIDC `sub` / static token name) that cannot form an
    /// `OpenFGA` user id — a credential/config defect, 401-class.
    InvalidPrincipal,
    /// A tenant id that cannot form an `OpenFGA` object id — nothing in the
    /// graph can refer to it, so every question about it fails closed.
    InvalidTenant,
    /// A token group name that cannot form an `OpenFGA` object id
    /// (empty, over 256 bytes, or containing `:`, `#` or whitespace) — a
    /// credential defect, answered like the cap: named, 401-class.
    InvalidGroup {
        /// The offending group's position in the token's list.
        index: usize,
    },
    /// A streamed enumeration reached the caller's bound before ending.
    BoundExceeded {
        /// The bound that was hit.
        bound: usize,
    },
    /// A streamed enumeration was cut off (timeout mid-stream) — a
    /// partial set is never accepted as an answer.
    Incomplete,
}

impl fmt::Display for OpenFgaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(why) => write!(f, "openfga unavailable: {why}"),
            Self::TooManyContextualTuples { count } => write!(
                f,
                "openfga: {count} contextual tuples exceed the per-request cap of \
                 {MAX_CONTEXTUAL_TUPLES}"
            ),
            Self::InvalidPrincipal => f.write_str(
                "openfga: principal id cannot form a user id (empty, too long, or contains \
                 ':', '#' or whitespace)",
            ),
            Self::InvalidTenant => f.write_str(
                "openfga: tenant id cannot form an object id (empty, too long, or contains \
                 ':', '#' or whitespace)",
            ),
            Self::InvalidGroup { index } => write!(
                f,
                "openfga: token group #{index} cannot form an object id (empty, too long, or \
                 contains ':', '#' or whitespace)"
            ),
            Self::BoundExceeded { bound } => {
                write!(
                    f,
                    "openfga: enumeration exceeds the bound of {bound} objects"
                )
            }
            Self::Incomplete => f.write_str("openfga: enumeration incomplete (stream cut off)"),
        }
    }
}

impl std::error::Error for OpenFgaError {}

/// A streamed `ListObjects` request.
#[derive(Debug, Clone, Copy)]
pub struct ListObjectsRequest<'a> {
    /// The principal (`<type>:<id>`).
    pub user: &'a str,
    /// The relation to enumerate.
    pub relation: &'a str,
    /// The object type to enumerate.
    pub object_type: &'a str,
    /// Request-scoped, never persisted tuples.
    pub contextual_tuples: &'a [TupleKey],
}

#[derive(Serialize)]
struct ContextualTuples<'a> {
    tuple_keys: &'a [TupleKey],
}

#[derive(Serialize)]
struct CheckBody<'a> {
    tuple_key: &'a TupleKey,
    #[serde(skip_serializing_if = "Option::is_none")]
    authorization_model_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    contextual_tuples: Option<ContextualTuples<'a>>,
    consistency: &'static str,
}

#[derive(Deserialize)]
struct CheckResponse {
    allowed: bool,
}

#[derive(Serialize)]
struct ListObjectsBody<'a> {
    r#type: &'a str,
    relation: &'a str,
    user: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    authorization_model_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    contextual_tuples: Option<ContextualTuples<'a>>,
    consistency: &'static str,
}

/// One NDJSON line of a streamed `ListObjects` response: a result, or the
/// grpc-gateway error framing when the server aborts the stream.
#[derive(Deserialize)]
struct StreamLine {
    #[serde(default)]
    result: Option<StreamResult>,
    #[serde(default)]
    error: Option<StreamError>,
}

#[derive(Deserialize)]
struct StreamResult {
    object: String,
}

#[derive(Deserialize)]
struct StreamError {
    #[serde(default)]
    message: Option<String>,
}

#[derive(Serialize)]
struct WriteKeys<'a> {
    tuple_keys: &'a [TupleKey],
    /// `on_duplicate` (writes) / `on_missing` (deletes) = `ignore`
    /// (`OpenFGA` ≥ 1.10): the Write is idempotent — an existing tuple or an
    /// already-gone one is not an error, so the RFC 0047 §3.3 emitter and
    /// §3.6 erasure need no read-then-diff pass.
    #[serde(skip_serializing_if = "Option::is_none")]
    on_duplicate: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    on_missing: Option<&'static str>,
}

#[derive(Serialize)]
struct WriteBody<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    writes: Option<WriteKeys<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    deletes: Option<WriteKeys<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    authorization_model_id: Option<&'a str>,
}

/// The `OpenFGA` HTTP API over one store.
#[derive(Clone)]
pub struct OpenFgaClient {
    http: reqwest::Client,
    store_url: String,
    authorization_model_id: Option<String>,
    api_token: Option<String>,
    consistency: Consistency,
    request_timeout: Duration,
}

impl fmt::Debug for OpenFgaClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenFgaClient")
            .field("store_url", &self.store_url)
            .field("authorization_model_id", &self.authorization_model_id)
            .field("api_token", &self.api_token.as_ref().map(|_| "<redacted>"))
            .field("consistency", &self.consistency)
            .field("request_timeout", &self.request_timeout)
            .finish_non_exhaustive()
    }
}

impl OpenFgaClient {
    /// A client over the configured store.
    ///
    /// # Errors
    ///
    /// When the underlying HTTP client cannot be built (a startup error).
    pub fn new(config: &OpenFgaConfig) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .timeout(config.request_timeout())
            .build()
            .map_err(|e| format!("auth.openfga: http client: {e}"))?;
        Ok(Self {
            http,
            store_url: format!("{}/stores/{}", config.api_url(), config.store_id()),
            authorization_model_id: config.authorization_model_id().map(str::to_string),
            api_token: config.api_token().map(str::to_string),
            consistency: config.consistency(),
            request_timeout: config.request_timeout(),
        })
    }

    /// The configured per-call timeout.
    #[must_use]
    pub fn request_timeout(&self) -> Duration {
        self.request_timeout
    }

    fn post(
        &self,
        path: &str,
        body: &impl Serialize,
    ) -> Result<reqwest::RequestBuilder, OpenFgaError> {
        let body = serde_json::to_vec(body)
            .map_err(|e| OpenFgaError::Unavailable(format!("encode request: {e}")))?;
        let mut request = self
            .http
            .post(format!("{}/{path}", self.store_url))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body);
        if let Some(token) = &self.api_token {
            request = request.bearer_auth(token);
        }
        Ok(request)
    }

    fn contextual(tuples: &[TupleKey]) -> Result<Option<ContextualTuples<'_>>, OpenFgaError> {
        match tuples.len() {
            0 => Ok(None),
            count if count > MAX_CONTEXTUAL_TUPLES => {
                Err(OpenFgaError::TooManyContextualTuples { count })
            }
            _ => Ok(Some(ContextualTuples { tuple_keys: tuples })),
        }
    }

    /// `Check`: whether `key.user` holds `key.relation` on `key.object`,
    /// with request-scoped contextual tuples.
    ///
    /// # Errors
    ///
    /// [`OpenFgaError::Unavailable`] on transport/timeout/non-2xx;
    /// [`OpenFgaError::TooManyContextualTuples`] past the cap.
    pub async fn check(
        &self,
        key: &TupleKey,
        contextual_tuples: &[TupleKey],
    ) -> Result<bool, OpenFgaError> {
        let body = CheckBody {
            tuple_key: key,
            authorization_model_id: self.authorization_model_id.as_deref(),
            contextual_tuples: Self::contextual(contextual_tuples)?,
            consistency: self.consistency.as_wire(),
        };
        let response = self
            .post("check", &body)?
            .send()
            .await
            .map_err(|e| transport(&e))?;
        let response = ok_status(response).await?;
        let bytes = response
            .bytes()
            .await
            .map_err(|e| OpenFgaError::Unavailable(format!("read check: {e}")))?;
        let parsed: CheckResponse = serde_json::from_slice(&bytes)
            .map_err(|e| OpenFgaError::Unavailable(format!("decode check: {e}")))?;
        Ok(parsed.allowed)
    }

    /// Streamed `ListObjects`, consumed to completion within `timeout`:
    /// every object id the stream yields is offered to `keep`; kept ids
    /// are returned, and reaching `max_kept` kept ids **before the stream
    /// ends** is [`OpenFgaError::BoundExceeded`] — a truncated set is never
    /// an answer (RFC 0047 §3.4). A stream cut off by `timeout` is
    /// [`OpenFgaError::Incomplete`] for the same reason.
    ///
    /// # Errors
    ///
    /// The variants above, plus [`OpenFgaError::Unavailable`] on transport
    /// failure, a non-2xx status, or a server-side error frame.
    pub async fn streamed_list_objects(
        &self,
        request: ListObjectsRequest<'_>,
        timeout: Duration,
        max_kept: usize,
        mut keep: impl FnMut(&str) -> bool,
    ) -> Result<Vec<String>, OpenFgaError> {
        let body = ListObjectsBody {
            r#type: request.object_type,
            relation: request.relation,
            user: request.user,
            authorization_model_id: self.authorization_model_id.as_deref(),
            contextual_tuples: Self::contextual(request.contextual_tuples)?,
            consistency: self.consistency.as_wire(),
        };
        let response = self
            .post("streamed-list-objects", &body)?
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| transport(&e))?;
        let mut response = ok_status(response).await?;
        let mut kept = Vec::new();
        let mut buffer: Vec<u8> = Vec::new();
        loop {
            let chunk = match response.chunk().await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                Err(e) if e.is_timeout() => return Err(OpenFgaError::Incomplete),
                Err(e) => return Err(transport(&e)),
            };
            buffer.extend_from_slice(&chunk);
            while let Some(newline) = buffer.iter().position(|byte| *byte == b'\n') {
                let line: Vec<u8> = buffer.drain(..=newline).collect();
                offer_line(&line, &mut keep, max_kept, &mut kept)?;
            }
            if buffer.len() > MAX_LINE_BYTES {
                return Err(OpenFgaError::Unavailable(
                    "streamed-list-objects frame exceeds 64 KiB".to_string(),
                ));
            }
        }
        offer_line(&buffer, &mut keep, max_kept, &mut kept)?;
        Ok(kept)
    }

    /// `Write`: add `writes` and remove `deletes` in one transactional,
    /// **idempotent** call (`on_duplicate` / `on_missing` = `ignore`, so a
    /// tuple already present or already gone is not an error). `OpenFGA`
    /// caps a transactional write at 100 tuples; chunking (RFC 0047 §3.3)
    /// is the caller's job.
    ///
    /// # Errors
    ///
    /// [`OpenFgaError::Unavailable`] on transport/timeout/non-2xx.
    pub async fn write(
        &self,
        writes: &[TupleKey],
        deletes: &[TupleKey],
    ) -> Result<(), OpenFgaError> {
        let body = WriteBody {
            writes: (!writes.is_empty()).then_some(WriteKeys {
                tuple_keys: writes,
                on_duplicate: Some("ignore"),
                on_missing: None,
            }),
            deletes: (!deletes.is_empty()).then_some(WriteKeys {
                tuple_keys: deletes,
                on_duplicate: None,
                on_missing: Some("ignore"),
            }),
            authorization_model_id: self.authorization_model_id.as_deref(),
        };
        let response = self
            .post("write", &body)?
            .send()
            .await
            .map_err(|e| transport(&e))?;
        ok_status(response).await.map(drop)
    }
}

fn transport(e: &reqwest::Error) -> OpenFgaError {
    OpenFgaError::Unavailable(if e.is_timeout() {
        "request timed out".to_string()
    } else {
        // reqwest's Display includes the URL, which is topology, not secret.
        e.to_string()
    })
}

async fn ok_status(response: reqwest::Response) -> Result<reqwest::Response, OpenFgaError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    // Read only enough of the error body to be diagnostic: reqwest sets no
    // response-size limit and this path runs per failed request.
    let mut response = response;
    let mut raw: Vec<u8> = Vec::new();
    while raw.len() < MAX_ERROR_BODY_BYTES {
        match response.chunk().await {
            Ok(Some(chunk)) => raw.extend_from_slice(&chunk),
            Ok(None) | Err(_) => break,
        }
    }
    raw.truncate(MAX_ERROR_BODY_BYTES);
    let body = String::from_utf8_lossy(&raw);
    Err(OpenFgaError::Unavailable(format!("HTTP {status}: {body}")))
}

fn offer_line(
    line: &[u8],
    keep: &mut impl FnMut(&str) -> bool,
    max_kept: usize,
    kept: &mut Vec<String>,
) -> Result<(), OpenFgaError> {
    if line.iter().all(u8::is_ascii_whitespace) {
        return Ok(());
    }
    let parsed: StreamLine = serde_json::from_slice(line)
        .map_err(|e| OpenFgaError::Unavailable(format!("decode streamed-list-objects: {e}")))?;
    if let Some(error) = parsed.error {
        return Err(OpenFgaError::Unavailable(format!(
            "streamed-list-objects aborted: {}",
            error.message.unwrap_or_default()
        )));
    }
    let Some(result) = parsed.result else {
        return Ok(());
    };
    if !keep(&result.object) {
        return Ok(());
    }
    if kept.len() >= max_kept {
        return Err(OpenFgaError::BoundExceeded { bound: max_kept });
    }
    kept.push(result.object);
    Ok(())
}

/// The tenant sets the graph grants a principal (RFC 0047 §3.1): what it
/// may `can_query` (bind for reading — tenant-wide or scoped, the planner
/// decides which rows) and what it may `can_write`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Grants {
    /// Tenants the principal may query.
    pub query: BTreeSet<String>,
    /// Tenants the principal may write into.
    pub write: BTreeSet<String>,
}

impl Grants {
    /// No queryable and no writable tenant — the session is unbound.
    #[must_use]
    pub fn is_unbound(&self) -> bool {
        self.query.is_empty() && self.write.is_empty()
    }
}

struct CachedGrants {
    expires: Instant,
    grants: Grants,
}

/// The session-cache key: a structured (principal, sorted groups) pair, so
/// two credentials can never collide through the encoding — `sub` and
/// group names are untrusted strings and may contain any separator.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    principal: Principal,
    groups: Vec<String>,
}

/// The session resolver: principal → [`Grants`], cached per credential
/// for the configured TTL, fail-closed. Errors are never cached, and a
/// cached answer is never served past its TTL — revocation latency is
/// exactly `session_ttl_secs`.
pub struct OpenFgaResolver {
    client: OpenFgaClient,
    session_ttl: Duration,
    visibility: VisibilityConfig,
    cache: Mutex<HashMap<CacheKey, CachedGrants>>,
    /// The RFC 0047 §3.4 two-step outcome per (session, tenant), cached
    /// like the binding — the enumeration behind `Scoped` is never cached.
    branches: Mutex<HashMap<(CacheKey, String), CachedBranch>>,
}

struct CachedBranch {
    expires: Instant,
    branch: Branch,
}

/// The two-step's outcome for a principal in a tenant, before any
/// enumeration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Branch {
    TenantWide,
    MetadataOnly,
    Scoped,
}

/// What a principal may see inside a tenant (RFC 0047 §3.4): the whole
/// tenant, the whole tenant with content masked, or exactly the listed
/// conversations (ids with the `conversation:<tenant>/` prefix stripped).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Visibility {
    /// `Check(can_read_content, tenant)` allowed — the tenant predicate only.
    TenantWide,
    /// `Check(can_read_metadata, tenant)` allowed — every row, content
    /// columns masked.
    MetadataOnly,
    /// A scoped principal: the tenant's conversation ids it may read
    /// (empty when none, or when no object type is bound).
    Scoped {
        /// Conversation ids, prefix stripped, tenant-filtered.
        conversations: BTreeSet<String>,
    },
}

impl fmt::Debug for OpenFgaResolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenFgaResolver")
            .field("client", &self.client)
            .field("session_ttl", &self.session_ttl)
            .finish_non_exhaustive()
    }
}

impl OpenFgaResolver {
    /// A resolver over the configured store.
    ///
    /// # Errors
    ///
    /// When the HTTP client cannot be built (a startup error).
    pub fn new(config: &OpenFgaConfig) -> Result<Self, String> {
        Ok(Self {
            client: OpenFgaClient::new(config)?,
            session_ttl: config.session_ttl(),
            visibility: config.visibility().clone(),
            cache: Mutex::new(HashMap::new()),
            branches: Mutex::new(HashMap::new()),
        })
    }

    /// The layer-2 visibility configuration.
    #[must_use]
    pub fn visibility_config(&self) -> &VisibilityConfig {
        &self.visibility
    }

    /// The underlying client, for the planner and tool gate.
    #[must_use]
    pub fn client(&self) -> &OpenFgaClient {
        &self.client
    }

    /// The contextual `team:<group>#member@<principal>` tuples a token's
    /// group claim contributes (RFC 0047 §3.1). Only `user:` and
    /// `service_account:` principals may be team members in the model
    /// (`team.member: [user, service_account, team#member]`); an `agent:`
    /// principal's groups contribute nothing rather than a tuple the server
    /// would reject.
    ///
    /// # Errors
    ///
    /// [`OpenFgaError::TooManyContextualTuples`] past the per-request cap;
    /// [`OpenFgaError::InvalidGroup`] for a name that cannot be an object id.
    pub fn group_tuples(
        principal: &Principal,
        groups: &[String],
    ) -> Result<Vec<TupleKey>, OpenFgaError> {
        if groups.len() > MAX_CONTEXTUAL_TUPLES {
            return Err(OpenFgaError::TooManyContextualTuples {
                count: groups.len(),
            });
        }
        if let Some(index) = groups.iter().position(|group| !is_object_id(group)) {
            return Err(OpenFgaError::InvalidGroup { index });
        }
        if principal.kind() == PrincipalKind::Agent {
            return Ok(Vec::new());
        }
        let user = principal.to_string();
        Ok(groups
            .iter()
            .map(|group| TupleKey::new(user.clone(), "member", format!("team:{group}")))
            .collect())
    }

    /// Resolve `principal` (with its token's `groups`) into the tenant sets
    /// it may query and write.
    ///
    /// # Errors
    ///
    /// [`OpenFgaError`] — every variant is fail-closed at the caller and
    /// none is cached.
    pub async fn resolve(
        &self,
        principal: &Principal,
        groups: &[String],
    ) -> Result<Grants, OpenFgaError> {
        let cache_key = Self::cache_key(principal, groups)?;
        let now = Instant::now();
        {
            let cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(cached) = cache.get(&cache_key)
                && cached.expires > now
            {
                return Ok(cached.grants.clone());
            }
        }
        let contextual = Self::group_tuples(principal, &cache_key.groups)?;
        let user = principal.to_string();
        let query = self.list_tenants(&user, "can_query", &contextual).await?;
        let write = self.list_tenants(&user, "can_write", &contextual).await?;
        let grants = Grants { query, write };
        if !self.session_ttl.is_zero() {
            let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
            if cache.len() >= CACHE_SWEEP_THRESHOLD {
                cache.retain(|_, entry| entry.expires > now);
            }
            if cache.len() >= MAX_CACHE_ENTRIES
                && let Some(soonest) = cache
                    .iter()
                    .min_by_key(|(_, entry)| entry.expires)
                    .map(|(key, _)| key.clone())
            {
                cache.remove(&soonest);
            }
            cache.insert(
                cache_key,
                CachedGrants {
                    expires: now + self.session_ttl,
                    grants: grants.clone(),
                },
            );
        }
        Ok(grants)
    }

    fn cache_key(principal: &Principal, groups: &[String]) -> Result<CacheKey, OpenFgaError> {
        if !is_object_id(principal.id()) {
            return Err(OpenFgaError::InvalidPrincipal);
        }
        let mut groups: Vec<String> = groups.to_vec();
        groups.sort();
        groups.dedup();
        Ok(CacheKey {
            principal: principal.clone(),
            groups,
        })
    }

    /// The RFC 0047 §3.4 two-step for `principal` in `tenant`:
    /// `Check(can_read_content)` → [`Visibility::TenantWide`]; else
    /// `Check(can_read_metadata)` → [`Visibility::MetadataOnly`]; else the
    /// scoped enumeration — the **streamed** `ListObjects(can_read_content,
    /// conversation)`, filtered to this tenant's `conversation:<tenant>/`
    /// prefix, counting only those toward `visibility.max_objects`, within
    /// `visibility.list_timeout` — a truncated or cut-off set is never an
    /// answer. The two checks are cached with the session TTL; the
    /// enumeration never is.
    ///
    /// # Errors
    ///
    /// [`OpenFgaError::BoundExceeded`] past `max_objects` tenant ids,
    /// [`OpenFgaError::Incomplete`] when the stream is cut off,
    /// [`OpenFgaError::Unavailable`] on transport/status failure, and the
    /// credential-defect variants ([`OpenFgaError::InvalidPrincipal`],
    /// [`OpenFgaError::InvalidGroup`], [`OpenFgaError::TooManyContextualTuples`]).
    pub async fn visibility(
        &self,
        principal: &Principal,
        groups: &[String],
        tenant: &str,
    ) -> Result<Visibility, OpenFgaError> {
        let cache_key = Self::cache_key(principal, groups)?;
        let contextual = Self::group_tuples(principal, &cache_key.groups)?;
        let user = principal.to_string();
        let objects = TenantObjects::new(tenant).ok_or(OpenFgaError::InvalidTenant)?;
        let tenant_object = objects.tenant().to_string();
        let now = Instant::now();
        let branch_key = (cache_key, tenant.to_string());
        let cached = {
            let branches = self.branches.lock().unwrap_or_else(PoisonError::into_inner);
            branches
                .get(&branch_key)
                .filter(|entry| entry.expires > now)
                .map(|entry| entry.branch)
        };
        let branch = if let Some(branch) = cached {
            branch
        } else {
            let branch = self.two_step(&user, &tenant_object, &contextual).await?;
            if !self.session_ttl.is_zero() {
                let mut branches = self.branches.lock().unwrap_or_else(PoisonError::into_inner);
                if branches.len() >= CACHE_SWEEP_THRESHOLD {
                    branches.retain(|_, entry| entry.expires > now);
                }
                if branches.len() >= MAX_CACHE_ENTRIES
                    && let Some(soonest) = branches
                        .iter()
                        .min_by_key(|(_, entry)| entry.expires)
                        .map(|(key, _)| key.clone())
                {
                    branches.remove(&soonest);
                }
                branches.insert(
                    branch_key,
                    CachedBranch {
                        expires: now + self.session_ttl,
                        branch,
                    },
                );
            }
            branch
        };
        match branch {
            Branch::TenantWide => Ok(Visibility::TenantWide),
            Branch::MetadataOnly => Ok(Visibility::MetadataOnly),
            Branch::Scoped => {
                let bound = self
                    .visibility
                    .objects()
                    .iter()
                    .any(|object| object.object_type() == CONVERSATION_TYPE);
                if !bound {
                    return Ok(Visibility::Scoped {
                        conversations: BTreeSet::new(),
                    });
                }
                let prefix = objects.conversation_prefix().to_string();
                let objects = self
                    .client
                    .streamed_list_objects(
                        ListObjectsRequest {
                            user: &user,
                            relation: "can_read_content",
                            object_type: CONVERSATION_TYPE,
                            contextual_tuples: &contextual,
                        },
                        self.visibility.list_timeout(),
                        self.visibility.max_objects(),
                        |object| object.starts_with(&prefix),
                    )
                    .await?;
                Ok(Visibility::Scoped {
                    conversations: objects
                        .into_iter()
                        .map(|object| object[prefix.len()..].to_string())
                        .collect(),
                })
            }
        }
    }

    /// The two `Check`s of RFC 0047 §3.4 steps 1–2.
    async fn two_step(
        &self,
        user: &str,
        tenant_object: &str,
        contextual: &[TupleKey],
    ) -> Result<Branch, OpenFgaError> {
        let content = TupleKey::new(user, "can_read_content", tenant_object);
        if self.client.check(&content, contextual).await? {
            return Ok(Branch::TenantWide);
        }
        let metadata = TupleKey::new(user, "can_read_metadata", tenant_object);
        if self.client.check(&metadata, contextual).await? {
            return Ok(Branch::MetadataOnly);
        }
        Ok(Branch::Scoped)
    }

    async fn list_tenants(
        &self,
        user: &str,
        relation: &str,
        contextual_tuples: &[TupleKey],
    ) -> Result<BTreeSet<String>, OpenFgaError> {
        let prefix = format!("{TENANT_TYPE}:");
        let objects = self
            .client
            .streamed_list_objects(
                ListObjectsRequest {
                    user,
                    relation,
                    object_type: TENANT_TYPE,
                    contextual_tuples,
                },
                self.client.request_timeout(),
                MAX_TENANTS_PER_PRINCIPAL,
                |object| object.starts_with(&prefix),
            )
            .await?;
        Ok(objects
            .into_iter()
            .map(|object| object[prefix.len()..].to_string())
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use axum::Router;
    use axum::body::Body;
    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::post;
    use serde_json::{Value, json};

    use super::super::{OpenFgaSpec, Principal, PrincipalKind, build_openfga_config};
    use super::{
        Grants, ListObjectsRequest, MAX_CONTEXTUAL_TUPLES, OpenFgaClient, OpenFgaError,
        OpenFgaResolver, TupleKey, Visibility,
    };

    /// A loopback stand-in for the `OpenFGA` HTTP API: `check` answers from
    /// the requested user, `streamed-list-objects` streams a fixed object
    /// list (optionally stalling after the first frame), and every call is
    /// counted so tests can assert on cache hits.
    #[derive(Clone)]
    struct Fake {
        calls: Arc<AtomicUsize>,
        objects: Arc<Vec<String>>,
        stall: bool,
        status: StatusCode,
    }

    async fn check(State(fake): State<Fake>, body: axum::body::Bytes) -> impl IntoResponse {
        fake.calls.fetch_add(1, Ordering::SeqCst);
        if fake.status != StatusCode::OK {
            return (fake.status, "nope").into_response();
        }
        let request: Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(request["consistency"], "MINIMIZE_LATENCY");
        let allowed = request["tuple_key"]["user"] == "user:alice";
        axum::Json(json!({ "allowed": allowed })).into_response()
    }

    async fn streamed(State(fake): State<Fake>, body: axum::body::Bytes) -> impl IntoResponse {
        fake.calls.fetch_add(1, Ordering::SeqCst);
        if fake.status != StatusCode::OK {
            return (fake.status, "nope").into_response();
        }
        let request: Value = serde_json::from_slice(&body).expect("json");
        // The resolver's contextual tuples arrive as-is.
        if let Some(keys) = request["contextual_tuples"]["tuple_keys"].as_array() {
            assert!(keys.iter().all(|k| k["relation"] == "member"));
        }
        let relation = request["relation"].as_str().expect("relation").to_string();
        let objects = Arc::clone(&fake.objects);
        let stall = fake.stall;
        let stream = async_stream(move |tx| async move {
            for (i, object) in objects.iter().enumerate() {
                // `can_write` yields only the first object; `can_query` all.
                if relation == "can_write" && i > 0 {
                    break;
                }
                let line = format!("{{\"result\":{{\"object\":\"{object}\"}}}}\n");
                if tx.send(Ok::<_, std::io::Error>(line)).await.is_err() {
                    return;
                }
                if stall {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                }
            }
        });
        axum::response::Response::builder()
            .header("content-type", "application/x-ndjson")
            .body(Body::from_stream(stream))
            .expect("response")
    }

    /// A channel-backed body stream: the producer runs on a task, the
    /// receiver is the body.
    fn async_stream<F, Fut>(
        producer: F,
    ) -> impl futures_core::Stream<Item = Result<String, std::io::Error>>
    where
        F: FnOnce(tokio::sync::mpsc::Sender<Result<String, std::io::Error>>) -> Fut,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        tokio::spawn(producer(tx));
        tokio_stream::wrappers::ReceiverStream::new(rx)
    }

    async fn serve(fake: Fake) -> String {
        let app = Router::new()
            .route("/stores/{store}/check", post(check))
            .route("/stores/{store}/streamed-list-objects", post(streamed))
            .with_state(fake);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        url
    }

    fn fake(objects: &[&str]) -> Fake {
        Fake {
            calls: Arc::new(AtomicUsize::new(0)),
            objects: Arc::new(objects.iter().map(|o| (*o).to_string()).collect()),
            stall: false,
            status: StatusCode::OK,
        }
    }

    fn client_for(url: &str, ttl_secs: &str) -> (OpenFgaClient, OpenFgaResolver) {
        let config = build_openfga_config(&OpenFgaSpec {
            api_url: Some(url.to_string()),
            store_id: Some("s".to_string()),
            session_ttl_secs: Some(ttl_secs.to_string()),
            request_timeout_secs: Some("1".to_string()),
            ..OpenFgaSpec::default()
        })
        .expect("config");
        (
            OpenFgaClient::new(&config).expect("client"),
            OpenFgaResolver::new(&config).expect("resolver"),
        )
    }

    /// `Check` round-trips the answer; the streamed enumeration keeps only
    /// what the filter admits and strips nothing itself.
    #[tokio::test]
    async fn check_and_streamed_list_objects() {
        let url = serve(fake(&[
            "tenant:acme",
            "tenant:globex",
            "conversation:acme/c-1",
        ]))
        .await;
        let (client, _) = client_for(&url, "60");
        assert!(
            client
                .check(
                    &TupleKey::new("user:alice", "can_read_content", "tenant:acme"),
                    &[]
                )
                .await
                .expect("check")
        );
        assert!(
            !client
                .check(
                    &TupleKey::new("user:bob", "can_read_content", "tenant:acme"),
                    &[]
                )
                .await
                .expect("check")
        );
        let tenants = client
            .streamed_list_objects(
                ListObjectsRequest {
                    user: "user:alice",
                    relation: "can_query",
                    object_type: "tenant",
                    contextual_tuples: &[],
                },
                Duration::from_secs(1),
                10,
                |object| object.starts_with("tenant:"),
            )
            .await
            .expect("stream");
        assert_eq!(tenants, vec!["tenant:acme", "tenant:globex"]);
    }

    /// RFC 0047 §3.4 fail-closed shapes: hitting the bound is an error (no
    /// partial set), a stalled stream is `Incomplete`, a 5xx is
    /// `Unavailable`, and more than 100 contextual tuples never leaves the
    /// process.
    #[tokio::test]
    async fn enumeration_fails_closed() {
        let url = serve(fake(&["tenant:a", "tenant:b", "tenant:c"])).await;
        let (client, _) = client_for(&url, "60");
        let request = ListObjectsRequest {
            user: "user:alice",
            relation: "can_query",
            object_type: "tenant",
            contextual_tuples: &[],
        };
        assert_eq!(
            client
                .streamed_list_objects(request, Duration::from_secs(1), 2, |_| true)
                .await
                .expect_err("bound"),
            OpenFgaError::BoundExceeded { bound: 2 }
        );
        // Exactly at the bound is fine — the bound is on *exceeding*.
        assert_eq!(
            client
                .streamed_list_objects(request, Duration::from_secs(1), 3, |_| true)
                .await
                .expect("at bound")
                .len(),
            3
        );

        let stalled = serve(Fake {
            stall: true,
            ..fake(&["tenant:a", "tenant:b"])
        })
        .await;
        let (client, _) = client_for(&stalled, "60");
        assert_eq!(
            client
                .streamed_list_objects(request, Duration::from_millis(200), 10, |_| true)
                .await
                .expect_err("stalled"),
            OpenFgaError::Incomplete
        );

        let broken = serve(Fake {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            ..fake(&[])
        })
        .await;
        let (client, _) = client_for(&broken, "60");
        assert!(matches!(
            client
                .check(
                    &TupleKey::new("user:alice", "can_query", "tenant:acme"),
                    &[]
                )
                .await,
            Err(OpenFgaError::Unavailable(_))
        ));

        let too_many: Vec<TupleKey> = (0..=MAX_CONTEXTUAL_TUPLES)
            .map(|i| TupleKey::new("user:alice", "member", format!("team:{i}")))
            .collect();
        assert_eq!(
            client
                .check(
                    &TupleKey::new("user:alice", "can_query", "tenant:acme"),
                    &too_many
                )
                .await
                .expect_err("cap"),
            OpenFgaError::TooManyContextualTuples {
                count: MAX_CONTEXTUAL_TUPLES + 1
            }
        );
    }

    /// A grant-table fake for the two-step: `check` answers membership of
    /// (user, relation, object); `streamed-list-objects` lists the grants of
    /// the requested type/relation for the user, optionally stalling after
    /// the first frame.
    #[derive(Clone)]
    struct GrantFake {
        calls: Arc<AtomicUsize>,
        streams: Arc<AtomicUsize>,
        grants: Arc<Vec<(&'static str, &'static str, &'static str)>>,
        stall: bool,
    }

    async fn grant_check(
        State(fake): State<GrantFake>,
        body: axum::body::Bytes,
    ) -> impl IntoResponse {
        fake.calls.fetch_add(1, Ordering::SeqCst);
        let request: Value = serde_json::from_slice(&body).expect("json");
        let key = &request["tuple_key"];
        let allowed = fake
            .grants
            .iter()
            .any(|(u, r, o)| key["user"] == *u && key["relation"] == *r && key["object"] == *o);
        axum::Json(json!({ "allowed": allowed })).into_response()
    }

    async fn grant_streamed(
        State(fake): State<GrantFake>,
        body: axum::body::Bytes,
    ) -> impl IntoResponse {
        fake.calls.fetch_add(1, Ordering::SeqCst);
        fake.streams.fetch_add(1, Ordering::SeqCst);
        let request: Value = serde_json::from_slice(&body).expect("json");
        let user = request["user"].as_str().expect("user").to_string();
        let relation = request["relation"].as_str().expect("relation").to_string();
        let prefix = format!("{}:", request["type"].as_str().expect("type"));
        let objects: Vec<String> = fake
            .grants
            .iter()
            .filter(|(u, r, o)| *u == user && *r == relation && o.starts_with(&prefix))
            .map(|(_, _, o)| (*o).to_string())
            .collect();
        let stall = fake.stall;
        let stream = async_stream(move |tx| async move {
            for object in objects {
                let line = format!("{{\"result\":{{\"object\":\"{object}\"}}}}\n");
                if tx.send(Ok::<_, std::io::Error>(line)).await.is_err() {
                    return;
                }
                if stall {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                }
            }
        });
        axum::response::Response::builder()
            .header("content-type", "application/x-ndjson")
            .body(Body::from_stream(stream))
            .expect("response")
    }

    async fn serve_grants(fake: GrantFake) -> String {
        let app = Router::new()
            .route("/stores/{store}/check", post(grant_check))
            .route(
                "/stores/{store}/streamed-list-objects",
                post(grant_streamed),
            )
            .with_state(fake);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        url
    }

    fn visibility_resolver(url: &str, max_objects: &str, list_timeout_ms: &str) -> OpenFgaResolver {
        use super::super::{VisibilityObjectSpec, VisibilitySpec};
        let config = build_openfga_config(&OpenFgaSpec {
            api_url: Some(url.to_string()),
            store_id: Some("s".to_string()),
            request_timeout_secs: Some("1".to_string()),
            visibility: VisibilitySpec {
                objects: vec![VisibilityObjectSpec {
                    object_type: Some("conversation".to_string()),
                    column: Some("attr.gen_ai.conversation.id".to_string()),
                }],
                max_objects: Some(max_objects.to_string()),
                list_timeout_ms: Some(list_timeout_ms.to_string()),
                ..VisibilitySpec::default()
            },
            ..OpenFgaSpec::default()
        })
        .expect("config");
        OpenFgaResolver::new(&config).expect("resolver")
    }

    /// RFC 0047 §3.4 two-step (RFC0047.4/.5/.7 at the resolver): a
    /// tenant-wide reader never enumerates; a metadata reader is masked,
    /// never enumerated; a scoped principal's conversations are enumerated
    /// through the **streamed** call, filtered to the tenant prefix, and
    /// only tenant ids count toward the bound; the branch is cached per
    /// session while the enumeration is not.
    #[tokio::test]
    #[allow(clippy::too_many_lines)] // one grant table, every branch of the two-step in sequence
    async fn two_step_visibility() {
        let fake = GrantFake {
            calls: Arc::new(AtomicUsize::new(0)),
            streams: Arc::new(AtomicUsize::new(0)),
            grants: Arc::new(vec![
                ("user:alice", "can_read_content", "tenant:acme"),
                ("user:alice", "can_read_metadata", "tenant:acme"),
                ("user:fin", "can_read_metadata", "tenant:acme"),
                ("user:bob", "can_read_content", "conversation:acme/c-1"),
                ("user:bob", "can_read_content", "conversation:acme/c-2"),
                ("user:bob", "can_read_content", "conversation:globex/c-1"),
                ("agent:bot", "can_read_content", "conversation:acme/c-3"),
                ("agent:bot", "can_read_content", "conversation:acme/c-4"),
                ("agent:bot", "can_read_content", "conversation:globex/c-5"),
                ("agent:bot", "can_read_content", "conversation:globex/c-6"),
                ("agent:bot", "can_read_content", "conversation:globex/c-7"),
            ]),
            stall: false,
        };
        let streams = Arc::clone(&fake.streams);
        let calls = Arc::clone(&fake.calls);
        let url = serve_grants(fake).await;
        let resolver = visibility_resolver(&url, "2", "500");
        let alice = Principal::new(PrincipalKind::User, "alice");
        let fin = Principal::new(PrincipalKind::User, "fin");
        let bob = Principal::new(PrincipalKind::User, "bob");
        let bot = Principal::new(PrincipalKind::Agent, "bot");

        assert_eq!(
            resolver
                .visibility(&alice, &[], "acme")
                .await
                .expect("alice"),
            Visibility::TenantWide
        );
        assert_eq!(
            streams.load(Ordering::SeqCst),
            0,
            "RFC0047.4: no enumeration"
        );
        assert_eq!(
            resolver.visibility(&fin, &[], "acme").await.expect("fin"),
            Visibility::MetadataOnly
        );
        assert_eq!(
            streams.load(Ordering::SeqCst),
            0,
            "RFC0047.8: no enumeration"
        );
        assert_eq!(
            resolver.visibility(&bob, &[], "acme").await.expect("bob"),
            Visibility::Scoped {
                conversations: BTreeSet::from(["c-1".to_string(), "c-2".to_string()])
            },
            "RFC0047.5: exactly bob's acme conversations, globex filtered out"
        );
        assert_eq!(streams.load(Ordering::SeqCst), 1);
        // The branch is cached; the enumeration is not.
        let before = calls.load(Ordering::SeqCst);
        resolver.visibility(&bob, &[], "acme").await.expect("bob");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            before + 1,
            "one stream, no checks"
        );
        // RFC0047.7: bot has 2 acme + 3 globex conversations under a bound
        // of 2 — succeeds with exactly the 2 (only acme ids count) …
        assert_eq!(
            resolver.visibility(&bot, &[], "acme").await.expect("bot"),
            Visibility::Scoped {
                conversations: BTreeSet::from(["c-3".to_string(), "c-4".to_string()])
            }
        );
        // … and fails closed past the bound in globex (3 > 2).
        assert_eq!(
            resolver
                .visibility(&bot, &[], "globex")
                .await
                .expect_err("bound"),
            OpenFgaError::BoundExceeded { bound: 2 }
        );
        // A principal with no grant on the tenant scopes to nothing.
        assert_eq!(
            resolver
                .visibility(&fin, &[], "globex")
                .await
                .expect("fin/globex"),
            Visibility::Scoped {
                conversations: BTreeSet::new()
            }
        );
        // Enumeration errors are never cached: a stalled stream fails closed
        // with `Incomplete`, every time.
        let stalled = serve_grants(GrantFake {
            calls: Arc::new(AtomicUsize::new(0)),
            streams: Arc::new(AtomicUsize::new(0)),
            grants: Arc::new(vec![
                ("user:bob", "can_read_content", "conversation:acme/c-1"),
                ("user:bob", "can_read_content", "conversation:acme/c-2"),
            ]),
            stall: true,
        })
        .await;
        let resolver = visibility_resolver(&stalled, "10", "200");
        for _ in 0..2 {
            assert_eq!(
                resolver
                    .visibility(&bob, &[], "acme")
                    .await
                    .expect_err("stalled"),
                OpenFgaError::Incomplete
            );
        }
        // A principal whose id is no object id is a credential defect; a
        // tenant that is none fails closed before any call.
        assert_eq!(
            resolver
                .visibility(&Principal::new(PrincipalKind::User, "a b"), &[], "acme")
                .await
                .expect_err("invalid principal"),
            OpenFgaError::InvalidPrincipal
        );
        assert_eq!(
            resolver
                .visibility(&bob, &[], "acme:prod")
                .await
                .expect_err("invalid tenant"),
            OpenFgaError::InvalidTenant
        );
    }

    /// The conversation prefix is the injective tenant encoding: a tenant
    /// containing `/` filters on `conversation:<enc>/` — `a` never sees
    /// `a/b`'s conversations even though both spell `conversation:a/b/...`
    /// without the encoding.
    #[tokio::test]
    async fn scoped_enumeration_uses_the_encoded_tenant_prefix() {
        let fake = GrantFake {
            calls: Arc::new(AtomicUsize::new(0)),
            streams: Arc::new(AtomicUsize::new(0)),
            grants: Arc::new(vec![
                ("user:bob", "can_read_content", "conversation:a/b/c-1"),
                ("user:bob", "can_read_content", "conversation:a%2Fb/c-2"),
            ]),
            stall: false,
        };
        let url = serve_grants(fake).await;
        let resolver = visibility_resolver(&url, "10", "500");
        let bob = Principal::new(PrincipalKind::User, "bob");
        assert_eq!(
            resolver.visibility(&bob, &[], "a").await.expect("a"),
            Visibility::Scoped {
                conversations: BTreeSet::from(["b/c-1".to_string()])
            }
        );
        assert_eq!(
            resolver.visibility(&bob, &[], "a/b").await.expect("a/b"),
            Visibility::Scoped {
                conversations: BTreeSet::from(["c-2".to_string()])
            }
        );
    }

    /// The resolver: `can_query` / `can_write` sets with the `tenant:`
    /// prefix stripped, cached per (principal, groups) for the TTL — and
    /// re-resolved past it; errors are not cached; a group list past the
    /// cap fails closed before any call.
    #[tokio::test]
    async fn resolver_binds_and_caches() {
        let fake = fake(&["tenant:acme", "tenant:globex"]);
        let calls = Arc::clone(&fake.calls);
        let url = serve(fake).await;
        let (_, resolver) = client_for(&url, "60");
        let alice = Principal::new(PrincipalKind::User, "alice");
        let grants = resolver.resolve(&alice, &[]).await.expect("resolve");
        assert_eq!(
            grants,
            Grants {
                query: BTreeSet::from(["acme".to_string(), "globex".to_string()]),
                write: BTreeSet::from(["acme".to_string()]),
            }
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2, "one stream per relation");
        resolver.resolve(&alice, &[]).await.expect("cached");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "served from the session cache"
        );
        // A different group set is a different session.
        resolver
            .resolve(&alice, &["platform".to_string()])
            .await
            .expect("resolve with groups");
        assert_eq!(calls.load(Ordering::SeqCst), 4);

        let (_, uncached) = client_for(&url, "0");
        uncached.resolve(&alice, &[]).await.expect("resolve");
        uncached.resolve(&alice, &[]).await.expect("resolve");
        assert_eq!(calls.load(Ordering::SeqCst), 8, "ttl 0 never caches");

        let groups: Vec<String> = (0..=MAX_CONTEXTUAL_TUPLES).map(|i| i.to_string()).collect();
        assert!(matches!(
            resolver.resolve(&alice, &groups).await,
            Err(OpenFgaError::TooManyContextualTuples { .. })
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 8, "rejected before any call");

        // Group names are validated before any call, and an agent's groups
        // contribute no team tuples (the model forbids agent team members).
        assert!(matches!(
            resolver.resolve(&alice, &["ops team".to_string()]).await,
            Err(OpenFgaError::InvalidGroup { index: 0 })
        ));
        assert!(matches!(
            resolver
                .resolve(&alice, &["ok".to_string(), "a:b".to_string()])
                .await,
            Err(OpenFgaError::InvalidGroup { .. })
        ));
        assert!(
            OpenFgaResolver::group_tuples(
                &Principal::new(PrincipalKind::Agent, "bot"),
                &["platform".to_string()]
            )
            .expect("valid")
            .is_empty()
        );
        assert_eq!(
            OpenFgaResolver::group_tuples(&alice, &["platform".to_string()])
                .expect("valid")
                .len(),
            1
        );

        // A subject that is no object id is a credential defect, before any
        // call; and the cache key is structured, so a subject or group
        // carrying a separator can never alias another session (`"a|b"`
        // with no groups vs `"a"` in group `"b"`).
        assert_eq!(
            resolver
                .resolve(&Principal::new(PrincipalKind::User, "a\nb"), &[])
                .await
                .expect_err("whitespace in sub"),
            OpenFgaError::InvalidPrincipal
        );
        let before = calls.load(Ordering::SeqCst);
        let odd = Principal::new(PrincipalKind::User, "a|b");
        resolver.resolve(&odd, &[]).await.expect("resolve");
        resolver
            .resolve(
                &Principal::new(PrincipalKind::User, "a"),
                &["b".to_string()],
            )
            .await
            .expect("resolve");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            before + 4,
            "two distinct sessions, two resolutions — no key collision"
        );
    }
}
