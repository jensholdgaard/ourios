//! The config-schema half of the RFC 0026 token store.
//!
//! The store itself — [`TokenStore`], [`TenantSet`], the validation, the
//! constant-time comparison — lives in [`ourios_core::auth`] so the ingest
//! enforcement point (`ourios-ingester`, RFC 0026 §3.2) can consume it
//! without depending on this crate; the types are re-exported here for the
//! query-side consumers. This module only maps the parsed `auth` section
//! ([`config::file::AuthSection`](crate::config::file::AuthSection)) onto
//! the core spec shape — the single validation path stays in core.

use ourios_core::auth::TokenSpec;
pub use ourios_core::auth::{ResolvedToken, TenantSet, TokenStore};

use crate::config::file::AuthSection;

/// Validate the parsed `auth` section into the resolved [`TokenStore`]
/// (RFC 0026 §3.1). `None` in, `None` out — open mode.
///
/// # Errors
///
/// The [`ourios_core::auth::build_token_store`] rules: a present section
/// must hold at least one token, with a unique non-empty `name`, a token
/// that resolved non-empty (and unique), and a valid `tenants` list. Error
/// text names entries by `name`/index only, never a token value.
pub fn build_token_store(section: Option<&AuthSection>) -> Result<Option<TokenStore>, String> {
    let specs: Option<Vec<TokenSpec>> = section.map(|section| {
        section
            .tokens
            .iter()
            .map(|entry| TokenSpec {
                name: entry.name.clone(),
                token: entry.token.clone(),
                tenants: entry.tenants.clone(),
            })
            .collect()
    });
    ourios_core::auth::build_token_store(specs.as_deref())
}

#[cfg(test)]
mod tests {
    use crate::config::file::{AuthSection, TokenEntry};

    use super::build_token_store;

    /// The `AuthSection` → `TokenSpec` mapping is field-faithful: a parsed
    /// entry authenticates in the resolved store with its name and tenant
    /// binding intact, an absent section resolves open, and the core
    /// validation (here: the empty list) surfaces through unchanged. The
    /// full validation matrix lives with the store, in `ourios_core::auth`.
    #[test]
    fn auth_section_maps_field_for_field_onto_the_core_store() {
        let section = AuthSection {
            tokens: vec![TokenEntry {
                name: Some("edge".to_string()),
                token: Some("tok-edge".to_string()),
                tenants: vec!["acme".to_string()],
            }],
        };
        let store = build_token_store(Some(&section))
            .expect("valid")
            .expect("enabled");
        let entry = store.authenticate("tok-edge").expect("match");
        assert_eq!(entry.name(), "edge");
        assert!(entry.tenants().allows("acme"));
        assert!(!entry.tenants().allows("globex"));

        assert!(build_token_store(None).expect("open mode").is_none());
        let err =
            build_token_store(Some(&AuthSection { tokens: Vec::new() })).expect_err("empty list");
        assert!(err.contains("auth.tokens"), "names the key: {err}");
    }
}
