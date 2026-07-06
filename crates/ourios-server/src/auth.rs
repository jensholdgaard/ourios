//! The resolved RFC 0026 token store: bearer-token authentication and
//! tenant binding.
//!
//! [`build_token_store`] validates the parsed `auth` section
//! ([`config::file::AuthSection`](crate::config::file::AuthSection)) into a
//! [`TokenStore`] — the shape both enforcement points consume: the OTLP
//! receiver's listeners (RFC 0026 §3.2) and the HTTP query API (§3.3). An
//! absent section resolves to `None` (open mode, §3.1); an empty token list
//! is a startup error rather than a locked-out server.
//!
//! Token comparison is constant-time by construction
//! ([`TokenStore::authenticate`] goes through `subtle`), and no type here
//! renders a token value: `Debug` shows entry names and tenant sets only,
//! and validation errors name an entry's `name`/index, never its token
//! (RFC 0026 §3.1 / RFC 0020 §3.5).

use std::collections::BTreeSet;
use std::fmt;

use subtle::ConstantTimeEq;

use crate::config::file::{AuthSection, TokenEntry};

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

/// Validate the parsed `auth` section into the resolved [`TokenStore`]
/// (RFC 0026 §3.1). `None` in, `None` out — open mode.
///
/// # Errors
///
/// A present section must hold at least one token; every entry must carry a
/// non-empty `name` (unique across entries, it is the audit label), a
/// `token` that resolved non-empty (an empty one means the `${env:…}`
/// variable was unset), a unique token value (two entries with one value
/// would make the tenant binding ambiguous), and a non-empty `tenants` list
/// — exact ids without surrounding whitespace, or the wildcard `"*"` alone.
/// Error text names entries by `name`/index only, never a token value.
pub fn build_token_store(section: Option<&AuthSection>) -> Result<Option<TokenStore>, String> {
    let Some(section) = section else {
        return Ok(None);
    };
    if section.tokens.is_empty() {
        return Err(
            "auth.tokens must not be empty — remove the auth section entirely for \
             open mode (RFC 0026 §3.1)"
                .to_string(),
        );
    }
    let mut entries: Vec<ResolvedToken> = Vec::with_capacity(section.tokens.len());
    for (index, entry) in section.tokens.iter().enumerate() {
        let name = match entry.name.as_deref() {
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
        let token = match entry.token.as_deref() {
            Some(t) if !t.is_empty() => t.to_string(),
            _ => {
                return Err(format!(
                    "auth.tokens[{index}] ({name}): token resolved to empty — is the \
                     ${{env:…}} variable set?"
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
            tenants: build_tenant_set(index, entry)?,
        });
    }
    Ok(Some(TokenStore { entries }))
}

/// Validate one entry's `tenants` list into a [`TenantSet`].
fn build_tenant_set(index: usize, entry: &TokenEntry) -> Result<TenantSet, String> {
    if entry.tenants.is_empty() {
        return Err(format!(
            "auth.tokens[{index}].tenants must not be empty — list the allowed \
             tenants, or [\"*\"] for all (RFC 0026 §3.1)"
        ));
    }
    if entry.tenants.iter().any(|t| t == "*") {
        if entry.tenants.len() > 1 {
            return Err(format!(
                "auth.tokens[{index}].tenants: the wildcard \"*\" must be the only \
                 entry (RFC 0026 §3.1 — no patterns)"
            ));
        }
        return Ok(TenantSet::All);
    }
    if entry.tenants.iter().any(|t| t.is_empty() || t.trim() != t) {
        return Err(format!(
            "auth.tokens[{index}].tenants entries must be non-empty tenant ids \
             without surrounding whitespace"
        ));
    }
    Ok(TenantSet::Listed(entry.tenants.iter().cloned().collect()))
}

#[cfg(test)]
mod tests {
    use crate::config::file::{AuthSection, TokenEntry};

    use super::{TenantSet, build_token_store};

    fn entry(name: &str, token: &str, tenants: &[&str]) -> TokenEntry {
        TokenEntry {
            name: Some(name.to_string()),
            token: Some(token.to_string()),
            tenants: tenants.iter().map(|t| (*t).to_string()).collect(),
        }
    }

    fn section(tokens: Vec<TokenEntry>) -> AuthSection {
        AuthSection { tokens }
    }

    /// The constant-time comparison helper's API shape (RFC 0026 §6): a
    /// configured token authenticates to its entry, an unknown or empty
    /// presentation to `None`, and a prefix (the length-mismatch
    /// short-circuit) to `None`.
    #[test]
    fn authenticate_matches_exact_tokens_only() {
        let store = build_token_store(Some(&section(vec![
            entry("edge", "tok-edge-1", &["acme"]),
            entry("admin", "tok-admin-2", &["*"]),
        ])))
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
        let store = build_token_store(Some(&section(vec![
            entry("edge", "t1", &["acme", "globex"]),
            entry("admin", "t2", &["*"]),
        ])))
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

    /// An absent section is open mode (`None`); an empty token list is a
    /// startup error, not a locked-out server (RFC 0026 §3.1).
    #[test]
    fn absent_is_open_mode_and_empty_is_an_error() {
        assert!(build_token_store(None).expect("open mode").is_none());
        let err = build_token_store(Some(&section(Vec::new()))).expect_err("empty list");
        assert!(err.contains("auth.tokens"), "names the key: {err}");
    }

    /// Each validation arm rejects with an error that names the entry by
    /// index/name and never echoes a token value.
    #[test]
    fn validation_errors_name_entries_never_values() {
        let cases: Vec<(AuthSection, &str)> = vec![
            (section(vec![entry("", "tok-a", &["x"])]), "name"),
            (
                section(vec![
                    entry("a", "tok-a", &["x"]),
                    entry("a", "tok-b", &["y"]),
                ]),
                "duplicates",
            ),
            (section(vec![entry("a", "", &["x"])]), "resolved to empty"),
            (
                section(vec![
                    entry("a", "tok-same", &["x"]),
                    entry("b", "tok-same", &["y"]),
                ]),
                "ambiguous",
            ),
            (section(vec![entry("a", "tok-a", &[])]), "tenants"),
            (section(vec![entry("a", "tok-a", &["*", "acme"])]), "only"),
            (section(vec![entry("a", "tok-a", &[" acme"])]), "whitespace"),
        ];
        for (auth, needle) in cases {
            let err = build_token_store(Some(&auth)).expect_err("invalid");
            assert!(err.contains(needle), "{needle:?} not in {err:?}");
            assert!(!err.contains("tok-"), "no token value leaks: {err:?}");
        }
    }

    /// `Debug` renders names and tenant sets, never a token value — the
    /// store is safe to log (RFC 0026 §3.1 / RFC 0020 §3.5).
    #[test]
    fn debug_never_renders_a_token_value() {
        let store = build_token_store(Some(&section(vec![entry(
            "edge",
            "s3cr3t-token",
            &["acme"],
        )])))
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
