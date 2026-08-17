//! Per-request bearer authentication and tenant binding for the OTLP
//! listeners (RFC 0026 §3.2).
//!
//! Both transports share one path: [`authenticate_bearer`] resolves the
//! request's `Authorization` value against the configured
//! [`TokenStore`] (`None` store = open mode, §3.1 — every request passes
//! unbound). The gRPC interceptor and the HTTP handler run it *before any
//! wire decode*, then attach the resulting [`AuthBinding`] to the request;
//! the pipeline enforces the §3.2 per-batch tenant binding against it via
//! `check_binding` (crate-internal) — every `ResourceLogs` group's derived
//! tenant must
//! fall inside the token's set, else the **whole batch** is rejected
//! before the WAL append (partial acceptance would make the OTLP
//! partial-success surface a tenancy oracle).
//!
//! Nothing here carries or renders a token value: the binding holds the
//! token's audit *name* and its tenant sets only.
//!
//! With the RFC 0047 `openfga` resolver configured, the static store /
//! OIDC path still decides *who* the bearer is; the graph then decides
//! which tenants that principal may query and write, and the credential's
//! own tenant list (if any) can only narrow the answer. Every `OpenFGA`
//! failure is fail-closed: [`AuthError::Unavailable`] (`503` /
//! `UNAVAILABLE`), never an open door.

use std::sync::Arc;

use ourios_core::auth::openfga::{Principal, PrincipalKind};
use ourios_core::auth::{TenantSet, TokenStore};
use ourios_core::tenant::TenantId;

use crate::metrics::AuthMetrics;
use crate::receiver::pipeline::ReceiveError;

/// The authenticated identity a listener attaches to a request: the
/// principal's audit/metric label and its tenant bindings — never the
/// token value (RFC 0026 §3.4). Read and write are separate sets
/// (RFC 0047 §3.1): a static token or OIDC claim binds both identically,
/// the graph resolver binds `can_query` and `can_write` independently.
#[derive(Debug, Clone)]
pub struct AuthBinding {
    token_name: String,
    read: TenantSet,
    write: TenantSet,
}

impl AuthBinding {
    /// The matched credential's audit/metric label.
    #[must_use]
    pub fn token_name(&self) -> &str {
        &self.token_name
    }

    /// The tenant set the principal may query.
    #[must_use]
    pub fn read_tenants(&self) -> &TenantSet {
        &self.read
    }

    /// The tenant set the principal may write into.
    #[must_use]
    pub fn write_tenants(&self) -> &TenantSet {
        &self.write
    }

    /// Whether the principal may query `tenant`.
    #[must_use]
    pub fn may_read(&self, tenant: &str) -> bool {
        self.read.allows(tenant)
    }

    /// Whether the principal may write into `tenant`.
    #[must_use]
    pub fn may_write(&self, tenant: &str) -> bool {
        self.write.allows(tenant)
    }

    fn same(token_name: String, tenants: TenantSet) -> Self {
        Self {
            token_name,
            read: tenants.clone(),
            write: tenants,
        }
    }
}

/// Why a request did not resolve to a binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthError {
    /// The request failed authentication (→ `UNAUTHENTICATED` / 401). One
    /// undifferentiated value: the wire deliberately does not distinguish
    /// missing vs malformed vs unknown vs unbound (that split would be a
    /// probing oracle); telemetry attributes rejections as
    /// `error.type = unauthenticated` (RFC 0026 §3.4).
    Unauthenticated,
    /// The RFC 0047 resolver could not answer (`OpenFGA` unreachable, timed
    /// out, or errored) → `UNAVAILABLE` / 503, fail-closed
    /// (`error.type = upstream_unavailable`).
    Unavailable,
}

/// Authenticate a request's `Authorization` value against the store
/// (RFC 0026 §3.2).
///
/// Open mode (`store` is `None`) passes every request, unbound
/// (`Ok(None)`). With auth enabled, the value must be `Bearer <token>`
/// (scheme case-insensitive, per RFC 6750) and the token must match a
/// configured entry — the comparison is the store's constant-time one.
///
/// # Errors
///
/// [`AuthError::Unauthenticated`] on a missing, malformed, or unknown
/// credential.
pub fn authenticate_bearer(
    store: Option<&TokenStore>,
    authorization: Option<&str>,
) -> Result<Option<AuthBinding>, AuthError> {
    let Some(store) = store else {
        return Ok(None);
    };
    let token = authorization
        .and_then(parse_bearer)
        .ok_or(AuthError::Unauthenticated)?;
    let entry = store
        .authenticate(token)
        .ok_or(AuthError::Unauthenticated)?;
    Ok(Some(AuthBinding::same(
        entry.name().to_string(),
        entry.tenants().clone(),
    )))
}

/// Extract the token from a `Bearer <token>` credential (RFC 6750 §2.1;
/// the scheme is case-insensitive per RFC 9110 §11.1). `None` for any
/// other shape.
fn parse_bearer(value: &str) -> Option<&str> {
    let (scheme, token) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    (!token.is_empty()).then_some(token)
}

/// The full request resolution in front of the RFC 0026 enforcement:
/// the constant-time static store first, then — when configured and the
/// static store does not match — RFC 0029 OIDC verification (`oidc`
/// feature); then, when configured, the RFC 0047 `OpenFGA` resolver binds
/// the authenticated principal's tenants (`openfga` feature). Open mode
/// (§3.1) only when *nothing* authenticates. Async because an OIDC
/// unseen-`kid` miss may refetch the JWKS and the graph resolver may
/// round-trip; the static-only path never awaits.
#[derive(Clone)]
pub struct AuthResolver {
    store: Option<Arc<TokenStore>>,
    #[cfg(feature = "oidc")]
    oidc: Option<Arc<ourios_core::auth::oidc::OidcVerifier>>,
    #[cfg(feature = "openfga")]
    openfga: Option<Arc<ourios_core::auth::openfga::OpenFgaResolver>>,
    /// `ourios.auth.resolutions` (RFC 0047 §5, RFC0047.3). Resolves by
    /// name through the global meter, so every clone aggregates.
    metrics: Arc<AuthMetrics>,
}

impl std::fmt::Debug for AuthResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut d = f.debug_struct("AuthResolver");
        d.field("static_store", &self.store.is_some());
        #[cfg(feature = "oidc")]
        d.field("oidc", &self.oidc.is_some());
        #[cfg(feature = "openfga")]
        d.field("openfga", &self.openfga.is_some());
        d.finish_non_exhaustive()
    }
}

/// Who a bearer is, before the tenant binding is decided: the audit
/// label, the RFC 0047 principal, the credential's own tenant list (a
/// static token's `tenants`, an OIDC tenant claim; `None` when the graph
/// alone binds), and the OIDC group claim.
// Without `openfga` only `name`/`tenants` are read; the principal and
// groups exist for the graph layer.
#[cfg_attr(not(feature = "openfga"), allow(dead_code))]
struct Identity {
    name: String,
    principal: Principal,
    tenants: Option<TenantSet>,
    groups: Vec<String>,
}

impl AuthResolver {
    /// A resolver over the static store only (`None` = open mode) — the
    /// RFC 0026 shape, and the whole story when the `oidc` feature is
    /// off or unconfigured.
    #[must_use]
    pub fn static_only(store: Option<Arc<TokenStore>>) -> Self {
        Self {
            store,
            #[cfg(feature = "oidc")]
            oidc: None,
            #[cfg(feature = "openfga")]
            openfga: None,
            metrics: Arc::new(AuthMetrics::new()),
        }
    }

    /// A resolver with an OIDC verifier alongside the (optional) static
    /// store — RFC 0029 §3.3 coexistence: each credential authenticates
    /// via its own path, carrying its own tenant binding.
    #[cfg(feature = "oidc")]
    #[must_use]
    pub fn with_oidc(
        store: Option<Arc<TokenStore>>,
        oidc: Arc<ourios_core::auth::oidc::OidcVerifier>,
    ) -> Self {
        Self {
            store,
            oidc: Some(oidc),
            #[cfg(feature = "openfga")]
            openfga: None,
            metrics: Arc::new(AuthMetrics::new()),
        }
    }

    /// The same resolver with the RFC 0047 §3.1 `OpenFGA` layer: after the
    /// static store or OIDC establishes the principal, the graph binds its
    /// tenants (fail-closed).
    #[cfg(feature = "openfga")]
    #[must_use]
    pub fn with_openfga(
        mut self,
        openfga: Arc<ourios_core::auth::openfga::OpenFgaResolver>,
    ) -> Self {
        self.openfga = Some(openfga);
        self
    }

    /// Whether every request passes unbound (§3.1 open mode).
    #[must_use]
    pub fn is_open(&self) -> bool {
        #[cfg(feature = "oidc")]
        {
            self.store.is_none() && self.oidc.is_none()
        }
        #[cfg(not(feature = "oidc"))]
        {
            self.store.is_none()
        }
    }

    /// Resolve a request's `Authorization` value (RFC 0026 §3.2 /
    /// RFC 0029 §3.3 / RFC 0047 §3.1). `Ok(None)` in open mode; one
    /// undifferentiated [`AuthError::Unauthenticated`] for every rejected
    /// credential; [`AuthError::Unavailable`] only when the graph resolver
    /// could not answer.
    ///
    /// # Errors
    ///
    /// [`AuthError::Unauthenticated`] on a missing, malformed, unknown, or
    /// unbound credential — including a JWT that fails verification;
    /// [`AuthError::Unavailable`] when `OpenFGA` is configured and
    /// unreachable, slow, or erroring (fail-closed).
    // Without `oidc`/`openfga` the only .awaits disappear, but the
    // signature must stay async across feature configs — callers await it
    // either way.
    #[cfg_attr(
        not(any(feature = "oidc", feature = "openfga")),
        allow(clippy::unused_async)
    )]
    pub async fn authenticate(
        &self,
        authorization: Option<&str>,
    ) -> Result<Option<AuthBinding>, AuthError> {
        if self.is_open() {
            return Ok(None);
        }
        let outcome = match self.identify(authorization).await {
            Ok(identity) => self.bind(identity).await,
            Err(e) => Err(e),
        };
        self.metrics.record(outcome.as_ref().err().copied());
        outcome
    }

    /// Who the bearer is: the static store first, then OIDC.
    #[cfg_attr(not(feature = "oidc"), allow(clippy::unused_async))]
    async fn identify(&self, authorization: Option<&str>) -> Result<Identity, AuthError> {
        let token = authorization
            .and_then(parse_bearer)
            .ok_or(AuthError::Unauthenticated)?;
        if let Some(store) = self.store.as_deref()
            && let Some(entry) = store.authenticate(token)
        {
            return Ok(Identity {
                name: entry.name().to_string(),
                principal: Principal::new(PrincipalKind::ServiceAccount, entry.name()),
                tenants: Some(entry.tenants().clone()),
                groups: Vec::new(),
            });
        }
        #[cfg(feature = "oidc")]
        if let Some(oidc) = &self.oidc
            && let Some(identity) = oidc.verify(token).await
        {
            let kind = if identity.is_agent {
                PrincipalKind::Agent
            } else {
                PrincipalKind::User
            };
            return Ok(Identity {
                name: identity.name,
                principal: Principal::new(kind, identity.subject),
                tenants: identity.tenants,
                groups: identity.groups,
            });
        }
        Err(AuthError::Unauthenticated)
    }

    /// Which tenants the principal may query and write: the graph when
    /// configured (narrowed by the credential's own list), else the
    /// credential's list for both.
    #[cfg_attr(not(feature = "openfga"), allow(clippy::unused_async))]
    async fn bind(&self, identity: Identity) -> Result<Option<AuthBinding>, AuthError> {
        #[cfg(feature = "openfga")]
        if let Some(openfga) = &self.openfga {
            use ourios_core::auth::openfga::OpenFgaError;
            let grants = match openfga.resolve(&identity.principal, &identity.groups).await {
                Ok(grants) => grants,
                // Credential defects (a group list past the cap, a group
                // name that is no object id): named, 401-class.
                Err(
                    e @ (OpenFgaError::TooManyContextualTuples { .. }
                    | OpenFgaError::InvalidGroup { .. }),
                ) => {
                    tracing::warn!(
                        token_name = %identity.name,
                        error = %e,
                        "openfga: token groups unusable; resolution fails closed (RFC 0047 §3.1)"
                    );
                    return Err(AuthError::Unauthenticated);
                }
                Err(e) => {
                    tracing::warn!(
                        token_name = %identity.name,
                        error = %e,
                        "openfga resolution failed; request fails closed (RFC 0047 §3.1)"
                    );
                    return Err(AuthError::Unavailable);
                }
            };
            let credential = identity.tenants.unwrap_or(TenantSet::All);
            let read = credential.intersect(&grants.query);
            let write = credential.intersect(&grants.write);
            if read.is_empty() && write.is_empty() {
                return Err(AuthError::Unauthenticated);
            }
            return Ok(Some(AuthBinding {
                token_name: identity.name,
                read,
                write,
            }));
        }
        // Without the graph a credential must carry its own tenant list —
        // configuration guarantees it; an absent one still fails closed.
        let tenants = identity.tenants.ok_or(AuthError::Unauthenticated)?;
        Ok(Some(AuthBinding::same(identity.name, tenants)))
    }
}

/// Enforce the §3.2 per-batch tenant binding: the out-of-band selected
/// `tenant` (RFC 0046 §3.1) must fall inside the binding's write set.
///
/// Runs before the WAL append *and* before materialisation, so a denied
/// batch does no ingest work at all.
///
/// # Errors
///
/// [`ReceiveError::TenantDenied`] when `tenant` is outside the set — the
/// whole batch is rejected (`PERMISSION_DENIED` / 403).
pub(crate) fn check_binding(tenant: &TenantId, binding: &AuthBinding) -> Result<(), ReceiveError> {
    if binding.may_write(tenant.as_str()) {
        Ok(())
    } else {
        Err(ReceiveError::TenantDenied {
            token_name: binding.token_name.clone(),
            tenant: tenant.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use ourios_core::auth::{TokenSpec, build_token_store};

    use super::{AuthError, authenticate_bearer, parse_bearer};

    fn store() -> ourios_core::auth::TokenStore {
        build_token_store(Some(&[TokenSpec {
            name: Some("edge".to_string()),
            token: Some("tok-edge".to_string()),
            tenants: vec!["acme".to_string()],
        }]))
        .expect("valid")
        .expect("enabled")
    }

    /// Open mode passes any request, unbound; enabled mode requires a
    /// well-formed, known bearer.
    #[test]
    fn open_mode_passes_and_enabled_mode_authenticates() {
        assert!(
            authenticate_bearer(None, None).expect("open").is_none(),
            "open mode is unbound",
        );

        let store = store();
        let binding = authenticate_bearer(Some(&store), Some("Bearer tok-edge"))
            .expect("known token")
            .expect("bound");
        assert_eq!(binding.token_name(), "edge");
        assert!(binding.may_read("acme") && binding.may_write("acme"));
        assert!(!binding.may_read("globex"));

        for bad in [
            None,                       // missing header
            Some("Bearer tok-unknown"), // unknown token
            Some("Basic dXNlcjpwYXNz"), // wrong scheme
            Some("tok-edge"),           // no scheme
            Some("Bearer "),            // empty token
        ] {
            assert_eq!(
                authenticate_bearer(Some(&store), bad).expect_err("rejected"),
                AuthError::Unauthenticated,
                "{bad:?} must not authenticate",
            );
        }
    }

    /// The credential parser: case-insensitive scheme, exactly the RFC 6750
    /// shape.
    #[test]
    fn bearer_scheme_is_case_insensitive() {
        assert_eq!(parse_bearer("Bearer t"), Some("t"));
        assert_eq!(parse_bearer("bearer t"), Some("t"));
        assert_eq!(parse_bearer("BEARER t"), Some("t"));
        assert_eq!(parse_bearer("Bearer  t "), Some("t"), "padding tolerated");
        assert_eq!(parse_bearer("Bearer"), None);
        assert_eq!(parse_bearer("Basic t"), None);
    }
}

#[cfg(all(test, feature = "openfga"))]
mod openfga_tests {
    use std::sync::Arc;

    use axum::Router;
    use axum::extract::State;
    use axum::routing::post;
    use ourios_core::auth::openfga::{OpenFgaResolver, OpenFgaSpec, build_openfga_config};
    use ourios_core::auth::{TokenSpec, build_token_store};
    use serde_json::Value;

    use super::{AuthError, AuthResolver};

    /// A fake `OpenFGA` whose `streamed-list-objects` answers from a fixed
    /// grant table: `service_account:collector` writes `acme` (and may
    /// query nothing); `service_account:wide` writes+queries everything
    /// listed; `service_account:nobody` holds nothing.
    type Grant = (&'static str, &'static str, &'static str);

    async fn streamed(State(grants): State<Arc<Vec<Grant>>>, body: axum::body::Bytes) -> String {
        let request: Value = serde_json::from_slice(&body).expect("json");
        let user = request["user"].as_str().expect("user");
        let relation = request["relation"].as_str().expect("relation");
        let mut lines = String::new();
        for (_, _, object) in grants
            .iter()
            .filter(|(u, r, _)| *u == user && *r == relation)
        {
            lines.push_str("{\"result\":{\"object\":\"");
            lines.push_str(object);
            lines.push_str("\"}}\n");
        }
        lines
    }

    async fn resolver(grants: Vec<Grant>) -> AuthResolver {
        let app = Router::new()
            .route("/stores/{store}/streamed-list-objects", post(streamed))
            .with_state(Arc::new(grants));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        with_openfga(&url)
    }

    fn with_openfga(url: &str) -> AuthResolver {
        let store = build_token_store(Some(&[
            TokenSpec {
                name: Some("collector".to_string()),
                token: Some("tok-collector".to_string()),
                tenants: vec!["*".to_string()],
            },
            TokenSpec {
                name: Some("wide".to_string()),
                token: Some("tok-wide".to_string()),
                tenants: vec!["acme".to_string()],
            },
            TokenSpec {
                name: Some("nobody".to_string()),
                token: Some("tok-nobody".to_string()),
                tenants: vec!["*".to_string()],
            },
        ]))
        .expect("valid")
        .expect("enabled");
        let openfga = build_openfga_config(&OpenFgaSpec {
            api_url: Some(url.to_string()),
            store_id: Some("s".to_string()),
            request_timeout_secs: Some("1".to_string()),
            ..OpenFgaSpec::default()
        })
        .expect("config");
        AuthResolver::static_only(Some(Arc::new(store)))
            .with_openfga(Arc::new(OpenFgaResolver::new(&openfga).expect("resolver")))
    }

    /// Scenario RFC0047.1 (resolver binding, static half) — the graph's
    /// `can_query` / `can_write` sets become the binding's read / write
    /// sets, narrowed by the token's own tenant list; a principal the graph
    /// grants nothing is unbound (401-class), never empty-but-open.
    /// See `docs/rfcs/0047-rebac-resolver-and-graph-visibility.md` §5.
    #[tokio::test]
    async fn rfc0047_1_graph_binds_read_and_write_sets() {
        let resolver = resolver(vec![
            ("service_account:collector", "can_write", "tenant:acme"),
            ("service_account:wide", "can_query", "tenant:acme"),
            ("service_account:wide", "can_query", "tenant:globex"),
            ("service_account:wide", "can_write", "tenant:globex"),
        ])
        .await;

        let collector = resolver
            .authenticate(Some("Bearer tok-collector"))
            .await
            .expect("resolves")
            .expect("bound");
        assert_eq!(collector.token_name(), "collector");
        assert!(collector.may_write("acme") && !collector.may_write("globex"));
        assert!(
            !collector.may_read("acme"),
            "write-only principal reads nothing"
        );

        // `wide` may query acme+globex and write globex in the graph, but
        // its token lists only acme — the credential narrows the graph.
        let wide = resolver
            .authenticate(Some("Bearer tok-wide"))
            .await
            .expect("resolves")
            .expect("bound");
        assert!(wide.may_read("acme"));
        assert!(!wide.may_read("globex"), "token list narrows the graph");
        assert!(!wide.may_write("globex"), "narrowed away");

        assert_eq!(
            resolver
                .authenticate(Some("Bearer tok-nobody"))
                .await
                .expect_err("no grants"),
            AuthError::Unauthenticated,
            "no queryable and no writable tenant is unbound"
        );
        assert_eq!(
            resolver
                .authenticate(Some("Bearer tok-unknown"))
                .await
                .expect_err("unknown"),
            AuthError::Unauthenticated
        );
    }

    /// Scenario RFC0047.3 (fail closed, resolver arm) — with `OpenFGA`
    /// unreachable a known credential resolves to `Unavailable`, never to a
    /// binding and never to the 401 that would let a retry through an
    /// open door. (The `error.type = upstream_unavailable` count is
    /// asserted in the telemetry test.)
    #[tokio::test]
    async fn rfc0047_3_unreachable_openfga_fails_closed() {
        let closed = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}", closed.local_addr().expect("addr"));
        drop(closed);
        let resolver = with_openfga(&url);
        assert_eq!(
            resolver
                .authenticate(Some("Bearer tok-collector"))
                .await
                .expect_err("openfga down"),
            AuthError::Unavailable
        );
        // An unknown credential is still 401 — authentication precedes
        // authorization, and nothing was asked of the graph.
        assert_eq!(
            resolver
                .authenticate(Some("Bearer tok-unknown"))
                .await
                .expect_err("unknown"),
            AuthError::Unauthenticated
        );
    }
}
