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
//! token's audit *name* and its tenant set only.

use std::sync::Arc;

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use ourios_core::auth::{TenantSet, TokenStore};

use crate::receiver::pipeline::ReceiveError;
use crate::receiver::tenant::{TenantRule, derive_for_group};

/// The authenticated identity a listener attaches to a request: the
/// token's audit/metric label and its tenant binding — never the token
/// value (RFC 0026 §3.4).
#[derive(Debug, Clone)]
pub struct AuthBinding {
    token_name: String,
    tenants: TenantSet,
}

impl AuthBinding {
    /// The matched token's audit/metric label.
    #[must_use]
    pub fn token_name(&self) -> &str {
        &self.token_name
    }

    /// The tenant set the token may speak for.
    #[must_use]
    pub fn tenants(&self) -> &TenantSet {
        &self.tenants
    }
}

/// A request failed authentication (→ `UNAUTHENTICATED` / 401). One
/// undifferentiated value: the wire deliberately does not distinguish
/// missing vs malformed vs unknown (that split would be a probing oracle);
/// telemetry attributes rejections as `error.type = unauthenticated`
/// (RFC 0026 §3.4, the telemetry slice).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unauthenticated;

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
/// [`Unauthenticated`] on a missing, malformed, or unknown credential.
pub fn authenticate_bearer(
    store: Option<&TokenStore>,
    authorization: Option<&str>,
) -> Result<Option<AuthBinding>, Unauthenticated> {
    let Some(store) = store else {
        return Ok(None);
    };
    let token = authorization
        .and_then(parse_bearer)
        .ok_or(Unauthenticated)?;
    let entry = store.authenticate(token).ok_or(Unauthenticated)?;
    Ok(Some(AuthBinding {
        token_name: entry.name().to_string(),
        tenants: entry.tenants().clone(),
    }))
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
/// feature). Open mode (§3.1) only when *nothing* is configured. Async
/// because an OIDC unseen-`kid` miss may refetch the JWKS; the static
/// path never awaits.
#[derive(Clone)]
pub struct AuthResolver {
    store: Option<Arc<TokenStore>>,
    #[cfg(feature = "oidc")]
    oidc: Option<Arc<ourios_core::auth::oidc::OidcVerifier>>,
}

impl std::fmt::Debug for AuthResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut d = f.debug_struct("AuthResolver");
        d.field("static_store", &self.store.is_some());
        #[cfg(feature = "oidc")]
        d.field("oidc", &self.oidc.is_some());
        d.finish()
    }
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
        }
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
    /// RFC 0029 §3.3). Same contract as [`authenticate_bearer`]:
    /// `Ok(None)` in open mode, one undifferentiated error otherwise.
    ///
    /// # Errors
    ///
    /// [`Unauthenticated`] on a missing, malformed, or unknown credential
    /// — including a JWT that fails verification.
    pub async fn authenticate(
        &self,
        authorization: Option<&str>,
    ) -> Result<Option<AuthBinding>, Unauthenticated> {
        if self.is_open() {
            return Ok(None);
        }
        let token = authorization
            .and_then(parse_bearer)
            .ok_or(Unauthenticated)?;
        if let Some(store) = self.store.as_deref()
            && let Some(entry) = store.authenticate(token)
        {
            return Ok(Some(AuthBinding {
                token_name: entry.name().to_string(),
                tenants: entry.tenants().clone(),
            }));
        }
        #[cfg(feature = "oidc")]
        if let Some(oidc) = &self.oidc
            && let Some(identity) = oidc.verify(token).await
        {
            return Ok(Some(AuthBinding {
                token_name: identity.name,
                tenants: identity.tenants,
            }));
        }
        Err(Unauthenticated)
    }
}

/// Enforce the §3.2 per-batch tenant binding: derive every `ResourceLogs`
/// group's tenant (the same rule and error surface as the fan-out — RFC
/// 0003 §6.3 derivation is unchanged) and require each to fall inside the
/// binding's set.
///
/// Runs before the WAL append *and* before the fan-out's materialisation,
/// so a denied batch does no ingest work at all. Groups with zero log
/// records still have their derived tenant checked — the RFC binds the
/// batch's *claimed* tenants, not just the record-bearing ones.
///
/// # Errors
///
/// - [`ReceiveError::TenantResolution`] if a group fails derivation
///   (identical to the fan-out's rejection, RFC0003.4).
/// - [`ReceiveError::TenantDenied`] on the first out-of-set tenant — the
///   whole batch is rejected (`PERMISSION_DENIED` / 403).
pub(crate) fn check_binding(
    request: &ExportLogsServiceRequest,
    rule: &TenantRule,
    binding: &AuthBinding,
) -> Result<(), ReceiveError> {
    for (index, resource_logs) in request.resource_logs.iter().enumerate() {
        let tenant_id = derive_for_group(resource_logs, index, rule)?;
        if !binding.tenants.allows(tenant_id.as_str()) {
            return Err(ReceiveError::TenantDenied {
                token_name: binding.token_name.clone(),
                tenant: tenant_id,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use ourios_core::auth::{TokenSpec, build_token_store};

    use super::{Unauthenticated, authenticate_bearer, parse_bearer};

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
        assert!(binding.tenants().allows("acme"));

        for bad in [
            None,                       // missing header
            Some("Bearer tok-unknown"), // unknown token
            Some("Basic dXNlcjpwYXNz"), // wrong scheme
            Some("tok-edge"),           // no scheme
            Some("Bearer "),            // empty token
        ] {
            assert_eq!(
                authenticate_bearer(Some(&store), bad).expect_err("rejected"),
                Unauthenticated,
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
