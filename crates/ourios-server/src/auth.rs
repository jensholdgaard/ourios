//! The config-schema half of the RFC 0026 + RFC 0029 `auth` section.
//!
//! The resolved types — [`TokenStore`], [`TenantSet`], [`OidcConfig`],
//! [`AuthConfig`], the validation, the constant-time comparison — live in
//! [`ourios_core::auth`] so the ingest enforcement point (`ourios-ingester`,
//! RFC 0026 §3.2) can consume them without depending on this crate; the
//! types are re-exported here for the query-side consumers. This module only
//! maps the parsed `auth` section
//! ([`config::file::AuthSection`](crate::config::file::AuthSection)) onto
//! the core spec shapes — the single validation path stays in core.

pub use ourios_core::auth::{AuthConfig, OidcConfig, ResolvedToken, TenantSet, TokenStore};
use ourios_core::auth::{OidcSpec, TokenSpec};

use crate::config::file::AuthSection;

/// Validate the parsed `auth` section into the resolved [`AuthConfig`]
/// (RFC 0026 §3.1 + RFC 0029 §3.1). `None` in, `None` out — open mode.
///
/// # Errors
///
/// The [`ourios_core::auth::build_auth_config`] rules: a present section
/// must configure at least one of `tokens` / `oidc`; a present token list
/// must be non-empty (unconditionally) with valid entries; a present `oidc`
/// half needs `issuer`, `audience`, and `tenant_claim`. Error text names
/// entries by `name`/index only, never a token value.
pub fn build_auth_config(section: Option<&AuthSection>) -> Result<Option<AuthConfig>, String> {
    let Some(section) = section else {
        return Ok(None);
    };
    let token_specs: Option<Vec<TokenSpec>> = section.tokens.as_ref().map(|entries| {
        entries
            .iter()
            .map(|entry| TokenSpec {
                name: entry.name.clone(),
                token: entry.token.clone(),
                tenants: entry.tenants.clone(),
            })
            .collect()
    });
    let oidc_spec = section.oidc.as_ref().map(|oidc| OidcSpec {
        issuer: oidc.issuer.clone(),
        audience: oidc.audience.clone(),
        tenant_claim: oidc.tenant_claim.clone(),
        name_claim: oidc.name_claim.clone(),
        clock_skew_secs: oidc.clock_skew_secs.clone(),
    });
    ourios_core::auth::build_auth_config(token_specs.as_deref(), oidc_spec.as_ref()).map(Some)
}

#[cfg(test)]
mod tests {
    use crate::config::file::{AuthSection, OidcSection, TokenEntry};

    use super::build_auth_config;

    fn token_entry() -> TokenEntry {
        TokenEntry {
            name: Some("edge".to_string()),
            token: Some("tok-edge".to_string()),
            tenants: vec!["acme".to_string()],
        }
    }

    fn oidc_section() -> OidcSection {
        OidcSection {
            issuer: Some("https://dex.internal.example".to_string()),
            audience: Some("ourios".to_string()),
            tenant_claim: Some("ourios_tenants".to_string()),
            name_claim: None,
            clock_skew_secs: None,
        }
    }

    /// The `AuthSection` → core-spec mapping is field-faithful: a parsed
    /// entry authenticates in the resolved store with its name and tenant
    /// binding intact, an absent section resolves open, and the core
    /// validation (here: the empty list) surfaces through unchanged. The
    /// full validation matrix lives with the store, in `ourios_core::auth`.
    #[test]
    fn auth_section_maps_field_for_field_onto_the_core_store() {
        let section = AuthSection {
            tokens: Some(vec![token_entry()]),
            oidc: None,
        };
        let config = build_auth_config(Some(&section))
            .expect("valid")
            .expect("enabled");
        let store = config.static_tokens.expect("static half");
        let entry = store.authenticate("tok-edge").expect("match");
        assert_eq!(entry.name(), "edge");
        assert!(entry.tenants().allows("acme"));
        assert!(!entry.tenants().allows("globex"));
        assert!(config.oidc.is_none());

        assert!(build_auth_config(None).expect("open mode").is_none());
        let err = build_auth_config(Some(&AuthSection {
            tokens: Some(Vec::new()),
            oidc: None,
        }))
        .expect_err("empty list");
        assert!(err.contains("auth.tokens"), "names the key: {err}");
    }

    /// Scenario RFC0029.1 (mapping) — the `oidc` half maps field-for-field
    /// (with the `sub` default applied in core), an oidc-only section
    /// resolves with no static store, and a section with neither half fails.
    /// See `docs/rfcs/0029-oidc-bearer-layer.md` §5.
    #[test]
    fn rfc0029_1_oidc_half_maps_onto_the_core_config() {
        let both = build_auth_config(Some(&AuthSection {
            tokens: Some(vec![token_entry()]),
            oidc: Some(oidc_section()),
        }))
        .expect("valid")
        .expect("enabled");
        let oidc = both.oidc.expect("oidc half");
        assert_eq!(oidc.issuer(), "https://dex.internal.example");
        assert_eq!(oidc.audience(), "ourios");
        assert_eq!(oidc.tenant_claim(), "ourios_tenants");
        assert_eq!(oidc.name_claim(), "sub");
        assert!(both.static_tokens.is_some());

        let oidc_only = build_auth_config(Some(&AuthSection {
            tokens: None,
            oidc: Some(oidc_section()),
        }))
        .expect("oidc-only")
        .expect("enabled");
        assert!(oidc_only.static_tokens.is_none());
        assert!(oidc_only.oidc.is_some());

        let err = build_auth_config(Some(&AuthSection {
            tokens: None,
            oidc: None,
        }))
        .expect_err("neither half");
        assert!(
            err.contains("tokens, oidc, or both"),
            "names the rule: {err}"
        );
    }
}
