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
    OpenFgaResolver, TupleKey,
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
}

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
    Ok(OpenFgaConfig {
        api_url,
        store_id,
        authorization_model_id,
        api_token,
        session_ttl,
        consistency,
        request_timeout: Duration::from_secs(request_timeout_secs),
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
