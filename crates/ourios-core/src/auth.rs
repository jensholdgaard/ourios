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
/// its value (showing only presence). The config layer additionally rejects
/// inline literals at parse time — a spec's token only ever arrives through
/// `${env:…}` indirection.
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
/// `token` that resolved non-empty (an empty one means the `${env:…}`
/// variable was unset), a unique token value (two entries with one value
/// would make the tenant binding ambiguous), and a non-empty `tenants` list
/// — exact ids without surrounding whitespace, or the wildcard `"*"` alone.
/// Error text names entries by `name`/index only, never a token value.
pub fn build_token_store(specs: Option<&[TokenSpec]>) -> Result<Option<TokenStore>, String> {
    let Some(specs) = specs else {
        return Ok(None);
    };
    if specs.is_empty() {
        return Err(
            "auth.tokens must not be empty — remove the auth section entirely for \
             open mode (RFC 0026 §3.1)"
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
            // A literal empty string never reaches here (the config layer's
            // parse-time reference rule rejects it), so empty means the
            // reference resolved against an unset variable.
            Some(_) => {
                return Err(format!(
                    "auth.tokens[{index}] ({name}): token resolved to empty — is the \
                     ${{env:…}} variable set?"
                ));
            }
            None => {
                return Err(format!(
                    "auth.tokens[{index}] ({name}): token is required — an \
                     ${{env:…}} reference (RFC 0026 §3.1)"
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
    use super::{TenantSet, TokenSpec, build_token_store};

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
            (vec![spec("a", "", &["x"])], "resolved to empty"),
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
