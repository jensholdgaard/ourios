//! The resolved RFC 0026 token store: bearer-token authentication and
//! tenant binding.
//!
//! [`build_token_store`] validates raw token specs — the shape the
//! `ourios-server` config schema (`auth.tokens`, RFC 0026 §3.1) maps onto —
//! into a [`TokenStore`], the type every enforcement point consumes: the
//! OTLP receiver's listeners (§3.2, in `ourios-ingester`) and the HTTP
//! query API (§3.3, in `ourios-server`). It lives here so the ingest side
//! can enforce without depending on the server crate. An absent spec list
//! resolves to `None` (open mode, §3.1); an empty one is a startup error
//! rather than a locked-out server.
//!
//! Token comparison is constant-time by construction
//! ([`TokenStore::authenticate`] goes through `subtle`), and no type here
//! renders a token value: `Debug` shows entry names and tenant sets only,
//! and validation errors name an entry's `name`/index, never its token
//! (RFC 0026 §3.1 / RFC 0020 §3.5).

use std::collections::BTreeSet;
use std::fmt;

use subtle::ConstantTimeEq;

/// One raw `auth.tokens[…]` entry as the config layer hands it over —
/// substituted but not yet validated. The field names mirror the RFC 0026
/// §3.1 schema; [`build_token_store`] is the single validation path.
///
/// The `token` field is **secret**: the manual [`fmt::Debug`] impl redacts
/// its value (showing only presence). On the `ourios-server` config path an
/// inline literal is additionally rejected at parse time, so there a token
/// only ever arrives through `${env:…}` indirection; any other constructor
/// owns the same discipline — never write a token value into a committable
/// or rendered surface.
#[derive(Default, Clone)]
pub struct TokenSpec {
    /// Audit/metric label for this token — never secret (RFC 0026 §3.4).
    pub name: Option<String>,
    /// The bearer token value (**secret**).
    pub token: Option<String>,
    /// The allowed tenant set: exact tenant ids, or the single wildcard `"*"`.
    pub tenants: Vec<String>,
}

impl fmt::Debug for TokenSpec {
    /// Redacts the token value — a `Debug` rendering shows only whether it is
    /// present, never its value (RFC 0026 §3.1 / RFC 0020 §3.5).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenSpec")
            .field("name", &self.name)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("tenants", &self.tenants)
            .finish()
    }
}

/// The raw `auth.oidc` section as the config layer hands it over (RFC 0029
/// §3.1) — substituted but not yet validated. Nothing here is secret: the
/// issuer, audience, and claim names are deployment topology, not
/// credentials.
#[derive(Debug, Default, Clone)]
pub struct OidcSpec {
    /// The OIDC discovery root (`/.well-known/openid-configuration` lives
    /// under it).
    pub issuer: Option<String>,
    /// The required `aud` value — a deployment must never accept tokens
    /// minted for another service (RFC 0029 §3.1).
    pub audience: Option<String>,
    /// The claim carrying the tenant list (or the wildcard `"*"`).
    pub tenant_claim: Option<String>,
    /// The claim feeding the audit/metric label. Defaults to `sub`.
    pub name_claim: Option<String>,
}

/// The validated `auth.oidc` configuration (RFC 0029 §3.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OidcConfig {
    issuer: String,
    audience: String,
    tenant_claim: String,
    name_claim: String,
}

impl OidcConfig {
    /// The OIDC discovery root.
    #[must_use]
    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    /// The required `aud` value.
    #[must_use]
    pub fn audience(&self) -> &str {
        &self.audience
    }

    /// The claim carrying the tenant list.
    #[must_use]
    pub fn tenant_claim(&self) -> &str {
        &self.tenant_claim
    }

    /// The claim feeding the audit/metric label (`sub` unless configured).
    #[must_use]
    pub fn name_claim(&self) -> &str {
        &self.name_claim
    }
}

/// The resolved `auth` section (RFC 0026 §3.1 + RFC 0029 §3.1): the static
/// token store, the OIDC layer, or both — at least one by construction
/// ([`build_auth_config`]). An absent `auth` section never constructs this
/// type; open mode stays `None` at the callers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthConfig {
    /// The static bearer-token store (RFC 0026), if configured.
    pub static_tokens: Option<TokenStore>,
    /// The OIDC layer (RFC 0029), if configured.
    pub oidc: Option<OidcConfig>,
}

impl AuthConfig {
    /// The store the enforcement points consume today.
    ///
    /// With OIDC configured but no static tokens this is an *empty* store:
    /// auth stays enforced (every bearer is rejected) rather than open. The
    /// RFC 0029 verifier slice replaces the gates' store parameter with the
    /// full config and retires this bridge.
    #[must_use]
    pub fn enforcement_store(&self) -> TokenStore {
        self.static_tokens.clone().unwrap_or(TokenStore {
            entries: Vec::new(),
        })
    }
}

/// Validate a raw `auth` section's halves into the resolved [`AuthConfig`]
/// (RFC 0026 §3.1 + RFC 0029 §3.1). Callers only invoke this for a present
/// `auth` section — an absent section is open mode and never reaches here.
///
/// # Errors
///
/// Neither half configured is a startup error (a present-but-empty `auth`
/// section is never the intent); each present half fails on its own rules
/// ([`build_token_store`] — including the unconditional empty-list error —
/// and [`build_oidc_config`]).
pub fn build_auth_config(
    tokens: Option<&[TokenSpec]>,
    oidc: Option<&OidcSpec>,
) -> Result<AuthConfig, String> {
    if tokens.is_none() && oidc.is_none() {
        return Err(
            "auth must configure tokens, oidc, or both — remove the auth section \
             entirely for open mode (RFC 0026 §3.1, RFC 0029 §3.1)"
                .to_string(),
        );
    }
    Ok(AuthConfig {
        static_tokens: build_token_store(tokens)?,
        oidc: oidc.map(build_oidc_config).transpose()?,
    })
}

/// Validate a raw [`OidcSpec`] into the resolved [`OidcConfig`] (RFC 0029
/// §3.1).
///
/// # Errors
///
/// `issuer`, `audience`, and `tenant_claim` are each required and must be
/// non-empty without surrounding whitespace; `name_claim` defaults to `sub`
/// and must be non-empty without surrounding whitespace when given.
pub fn build_oidc_config(spec: &OidcSpec) -> Result<OidcConfig, String> {
    let required = |key: &str, value: Option<&str>| match value {
        Some(v) if !v.is_empty() && v.trim() == v => Ok(v.to_string()),
        _ => Err(format!(
            "auth.oidc.{key} is required and must be non-empty without \
             surrounding whitespace (RFC 0029 §3.1)"
        )),
    };
    let name_claim = match spec.name_claim.as_deref() {
        None => "sub".to_string(),
        some => required("name_claim", some)?,
    };
    Ok(OidcConfig {
        issuer: required("issuer", spec.issuer.as_deref())?,
        audience: required("audience", spec.audience.as_deref())?,
        tenant_claim: required("tenant_claim", spec.tenant_claim.as_deref())?,
        name_claim,
    })
}

/// The tenant set a token is bound to (RFC 0026 §3.1): the single wildcard
/// `"*"`, or an exact-string allow-list. An enum rather than an optional
/// list so "all tenants" and "no tenants" cannot be conflated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TenantSet {
    /// `tenants: ["*"]` — every tenant is allowed.
    All,
    /// An exact-string allow-list (non-empty by construction).
    Listed(BTreeSet<String>),
}

impl TenantSet {
    /// Whether `tenant` falls inside this set.
    #[must_use]
    pub fn allows(&self, tenant: &str) -> bool {
        match self {
            Self::All => true,
            Self::Listed(set) => set.contains(tenant),
        }
    }
}

/// One validated token: the audit/metric label, the secret value, and the
/// tenant binding.
#[derive(Clone, PartialEq, Eq)]
pub struct ResolvedToken {
    name: String,
    token: String,
    tenants: TenantSet,
}

impl ResolvedToken {
    /// The audit/metric label for this token — never secret (RFC 0026 §3.4).
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The tenant set this token may speak for.
    #[must_use]
    pub fn tenants(&self) -> &TenantSet {
        &self.tenants
    }
}

impl fmt::Debug for ResolvedToken {
    /// Never renders the token value — name and tenant binding only.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedToken")
            .field("name", &self.name)
            .field("token", &"<redacted>")
            .field("tenants", &self.tenants)
            .finish()
    }
}

/// The validated `auth.tokens` store (RFC 0026 §3.1) — non-empty by
/// construction ([`build_token_store`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenStore {
    entries: Vec<ResolvedToken>,
}

impl TokenStore {
    /// Authenticate a presented bearer token, returning the matching entry.
    ///
    /// Every candidate comparison is constant-time in the token *content*
    /// (`subtle`; a length mismatch short-circuits, which reveals only
    /// length), so a probing client cannot binary-search a configured token
    /// byte by byte through response timing.
    #[must_use]
    pub fn authenticate(&self, presented: &str) -> Option<&ResolvedToken> {
        self.entries
            .iter()
            .find(|entry| entry.token.as_bytes().ct_eq(presented.as_bytes()).into())
    }
}

/// Validate raw token specs into the resolved [`TokenStore`] (RFC 0026
/// §3.1). `None` in, `None` out — open mode.
///
/// # Errors
///
/// A present spec list must hold at least one token; every entry must carry
/// a non-empty `name` (unique across entries, it is the audit label), a
/// non-empty `token`, a unique token value (two entries with one value
/// would make the tenant binding ambiguous), and a non-empty `tenants` list
/// — exact ids without surrounding whitespace, or the wildcard `"*"` alone.
/// Error text names entries by `name`/index only, never a token value.
pub fn build_token_store(specs: Option<&[TokenSpec]>) -> Result<Option<TokenStore>, String> {
    let Some(specs) = specs else {
        return Ok(None);
    };
    if specs.is_empty() {
        return Err(
            "auth.tokens must not be empty — omit the tokens list (oidc-only) or \
             the whole auth section (open mode) (RFC 0026 §3.1, RFC 0029 §3.1)"
                .to_string(),
        );
    }
    let mut entries: Vec<ResolvedToken> = Vec::with_capacity(specs.len());
    for (index, spec) in specs.iter().enumerate() {
        let name = match spec.name.as_deref() {
            Some(n) if !n.is_empty() && n.trim() == n => n.to_string(),
            _ => {
                return Err(format!(
                    "auth.tokens[{index}].name must be a non-empty label without \
                     surrounding whitespace (it is the audit/metric label, RFC 0026 §3.4)"
                ));
            }
        };
        if entries.iter().any(|e| e.name == name) {
            return Err(format!(
                "auth.tokens[{index}].name {name:?} duplicates an earlier entry — \
                 names are the audit/metric label and must be unique"
            ));
        }
        let token = match spec.token.as_deref() {
            Some(t) if !t.is_empty() => t.to_string(),
            // The error names the likely cause on the config path (where a
            // literal "" is already rejected at parse time, an empty token
            // is almost always an `${env:…}` reference against an unset
            // variable) without asserting it — the spec is config-agnostic.
            Some(_) => {
                return Err(format!(
                    "auth.tokens[{index}] ({name}): token is empty — an ${{env:…}} \
                     reference against an unset variable is the usual cause"
                ));
            }
            None => {
                return Err(format!(
                    "auth.tokens[{index}] ({name}): token is required (RFC 0026 §3.1)"
                ));
            }
        };
        if let Some(prior) = entries
            .iter()
            .find(|e| e.token.as_bytes().ct_eq(token.as_bytes()).into())
        {
            return Err(format!(
                "auth.tokens[{index}] ({name}): token value duplicates entry {:?} — \
                 one value bound to two tenant sets is ambiguous",
                prior.name,
            ));
        }
        entries.push(ResolvedToken {
            name,
            token,
            tenants: build_tenant_set(index, spec)?,
        });
    }
    Ok(Some(TokenStore { entries }))
}

/// Validate one spec's `tenants` list into a [`TenantSet`].
fn build_tenant_set(index: usize, spec: &TokenSpec) -> Result<TenantSet, String> {
    if spec.tenants.is_empty() {
        return Err(format!(
            "auth.tokens[{index}].tenants must not be empty — list the allowed \
             tenants, or [\"*\"] for all (RFC 0026 §3.1)"
        ));
    }
    if spec.tenants.iter().any(|t| t == "*") {
        if spec.tenants.len() > 1 {
            return Err(format!(
                "auth.tokens[{index}].tenants: the wildcard \"*\" must be the only \
                 entry (RFC 0026 §3.1 — no patterns)"
            ));
        }
        return Ok(TenantSet::All);
    }
    if spec.tenants.iter().any(|t| t.is_empty() || t.trim() != t) {
        return Err(format!(
            "auth.tokens[{index}].tenants entries must be non-empty tenant ids \
             without surrounding whitespace"
        ));
    }
    Ok(TenantSet::Listed(spec.tenants.iter().cloned().collect()))
}

#[cfg(test)]
mod tests {
    use super::{
        OidcSpec, TenantSet, TokenSpec, build_auth_config, build_oidc_config, build_token_store,
    };

    fn spec(name: &str, token: &str, tenants: &[&str]) -> TokenSpec {
        TokenSpec {
            name: Some(name.to_string()),
            token: Some(token.to_string()),
            tenants: tenants.iter().map(|t| (*t).to_string()).collect(),
        }
    }

    /// The constant-time comparison helper's API shape (RFC 0026 §6): a
    /// configured token authenticates to its entry, an unknown or empty
    /// presentation to `None`, and a prefix (the length-mismatch
    /// short-circuit) to `None`.
    #[test]
    fn authenticate_matches_exact_tokens_only() {
        let store = build_token_store(Some(&[
            spec("edge", "tok-edge-1", &["acme"]),
            spec("admin", "tok-admin-2", &["*"]),
        ]))
        .expect("valid")
        .expect("enabled");

        assert_eq!(
            store.authenticate("tok-edge-1").expect("match").name(),
            "edge"
        );
        assert_eq!(
            store.authenticate("tok-admin-2").expect("match").name(),
            "admin"
        );
        assert!(store.authenticate("tok-edge-2").is_none());
        assert!(
            store.authenticate("tok-edge-").is_none(),
            "prefix is no match"
        );
        assert!(store.authenticate("").is_none());
    }

    /// Tenant binding: a listed set allows exactly its members; the wildcard
    /// allows everything (RFC 0026 §3.1).
    #[test]
    fn tenant_sets_allow_members_and_wildcard_allows_all() {
        let store = build_token_store(Some(&[
            spec("edge", "t1", &["acme", "globex"]),
            spec("admin", "t2", &["*"]),
        ]))
        .expect("valid")
        .expect("enabled");

        let edge = store.authenticate("t1").expect("edge");
        assert!(edge.tenants().allows("acme"));
        assert!(edge.tenants().allows("globex"));
        assert!(!edge.tenants().allows("initech"));
        assert!(
            !edge.tenants().allows("*"),
            "no literal-star tenant leaks in"
        );

        let admin = store.authenticate("t2").expect("admin");
        assert_eq!(*admin.tenants(), TenantSet::All);
        assert!(admin.tenants().allows("anything"));
    }

    /// An absent spec list is open mode (`None`); an empty one is a startup
    /// error, not a locked-out server (RFC 0026 §3.1).
    #[test]
    fn absent_is_open_mode_and_empty_is_an_error() {
        assert!(build_token_store(None).expect("open mode").is_none());
        let err = build_token_store(Some(&[])).expect_err("empty list");
        assert!(err.contains("auth.tokens"), "names the key: {err}");
    }

    /// Each validation arm rejects with an error that names the entry by
    /// index/name and never echoes a token value.
    #[test]
    fn validation_errors_name_entries_never_values() {
        let cases: Vec<(Vec<TokenSpec>, &str)> = vec![
            (vec![spec("", "tok-a", &["x"])], "name"),
            (
                vec![spec("a", "tok-a", &["x"]), spec("a", "tok-b", &["y"])],
                "duplicates",
            ),
            (vec![spec("a", "", &["x"])], "unset variable"),
            (
                vec![TokenSpec {
                    name: Some("a".to_string()),
                    token: None,
                    tenants: vec!["x".to_string()],
                }],
                "required",
            ),
            (
                vec![spec("a", "tok-same", &["x"]), spec("b", "tok-same", &["y"])],
                "ambiguous",
            ),
            (vec![spec("a", "tok-a", &[])], "tenants"),
            (vec![spec("a", "tok-a", &["*", "acme"])], "only"),
            (vec![spec("a", "tok-a", &[" acme"])], "whitespace"),
        ];
        for (specs, needle) in cases {
            let err = build_token_store(Some(&specs)).expect_err("invalid");
            assert!(err.contains(needle), "{needle:?} not in {err:?}");
            assert!(!err.contains("tok-"), "no token value leaks: {err:?}");
        }
    }

    fn oidc_spec() -> OidcSpec {
        OidcSpec {
            issuer: Some("https://dex.internal.example".to_string()),
            audience: Some("ourios".to_string()),
            tenant_claim: Some("ourios_tenants".to_string()),
            name_claim: None,
        }
    }

    /// Scenario RFC0029.1 (validation matrix) — `issuer`, `audience`, and
    /// `tenant_claim` are each required (a missing `audience` in particular
    /// is a startup error, RFC 0029 §3.1); `name_claim` defaults to `sub`.
    /// See `docs/rfcs/0029-oidc-bearer-layer.md` §5.
    #[test]
    fn oidc_config_requires_its_fields_and_defaults_name_claim() {
        let full = build_oidc_config(&oidc_spec()).expect("valid");
        assert_eq!(full.issuer(), "https://dex.internal.example");
        assert_eq!(full.audience(), "ourios");
        assert_eq!(full.tenant_claim(), "ourios_tenants");
        assert_eq!(full.name_claim(), "sub", "defaults to sub");

        let named = build_oidc_config(&OidcSpec {
            name_claim: Some("email".to_string()),
            ..oidc_spec()
        })
        .expect("valid");
        assert_eq!(named.name_claim(), "email");

        for (key, spec) in [
            (
                "issuer",
                OidcSpec {
                    issuer: None,
                    ..oidc_spec()
                },
            ),
            (
                "audience",
                OidcSpec {
                    audience: None,
                    ..oidc_spec()
                },
            ),
            (
                "tenant_claim",
                OidcSpec {
                    tenant_claim: None,
                    ..oidc_spec()
                },
            ),
            (
                "audience",
                OidcSpec {
                    audience: Some(" ourios".to_string()),
                    ..oidc_spec()
                },
            ),
            (
                "name_claim",
                OidcSpec {
                    name_claim: Some(String::new()),
                    ..oidc_spec()
                },
            ),
        ] {
            let err = build_oidc_config(&spec).expect_err("invalid");
            assert!(
                err.contains(&format!("auth.oidc.{key}")),
                "{key} named: {err}"
            );
        }
    }

    /// Scenario RFC0029.1 (section rules) — neither half is a startup error;
    /// an explicit empty token list stays a startup error **even with oidc
    /// configured**; oidc-only and both-halves resolve; the oidc-only
    /// enforcement bridge rejects every bearer rather than opening the
    /// gates. See `docs/rfcs/0029-oidc-bearer-layer.md` §5.
    #[test]
    fn auth_config_rules_and_oidc_only_bridge() {
        let err = build_auth_config(None, None).expect_err("neither half");
        assert!(
            err.contains("tokens, oidc, or both"),
            "names the rule: {err}"
        );

        let err = build_auth_config(Some(&[]), Some(&oidc_spec())).expect_err("empty list");
        assert!(
            err.contains("auth.tokens") && err.contains("oidc-only"),
            "the unconditional empty-list error points at omitting the list: {err}"
        );

        let oidc_only = build_auth_config(None, Some(&oidc_spec())).expect("oidc-only");
        assert!(oidc_only.static_tokens.is_none());
        assert_eq!(oidc_only.oidc.as_ref().expect("oidc").audience(), "ourios");
        let bridge = oidc_only.enforcement_store();
        assert!(
            bridge.authenticate("any-bearer").is_none(),
            "the bridge store matches nothing — enforced, not open"
        );

        let both = build_auth_config(
            Some(&[spec("edge", "tok-edge", &["acme"])]),
            Some(&oidc_spec()),
        )
        .expect("both halves");
        assert_eq!(
            both.enforcement_store()
                .authenticate("tok-edge")
                .expect("static half intact")
                .name(),
            "edge"
        );
        assert!(both.oidc.is_some());
    }

    /// `Debug` renders names and tenant sets, never a token value — the
    /// store (and the raw spec) are safe to log (RFC 0026 §3.1 / RFC 0020
    /// §3.5).
    #[test]
    fn debug_never_renders_a_token_value() {
        let raw = spec("edge", "s3cr3t-token", &["acme"]);
        let rendered = format!("{raw:?}");
        assert!(
            !rendered.contains("s3cr3t-token"),
            "spec token redacted: {rendered}"
        );

        let store = build_token_store(Some(&[raw]))
            .expect("valid")
            .expect("enabled");
        let rendered = format!("{store:?}");
        assert!(rendered.contains("edge"), "names stay visible: {rendered}");
        assert!(
            !rendered.contains("s3cr3t-token"),
            "token redacted: {rendered}"
        );
        assert!(
            rendered.contains("<redacted>"),
            "shows presence: {rendered}"
        );
    }
}
