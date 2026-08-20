//! Tenant identity for the multi-tenant miner.
//!
//! `[CLAUDE.md §3.7]`: every code path that touches data takes a
//! tenant id. This module ships the type; routing, storage, and
//! per-tenant state live in the consuming crates.

use std::fmt;

/// The RFC 0048 §3.1 upper bound on a tenant id, in bytes. Chosen so the
/// full `conversation:<T>/<id>` object string leaves 114 bytes for a
/// conversation id under `OpenFGA`'s 256-byte whole-string cap.
pub const MAX_TENANT_BYTES: usize = 128;

/// Why a value cannot be a tenant id (RFC 0048 §3.1). One grammar, one
/// error, every boundary: the OTLP selector, the querier header, the MCP
/// `tenant` argument, `auth.tokens[].tenants`, the OIDC tenant claim and
/// the `OpenFGA` object all speak this vocabulary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TenantIdError {
    /// Empty (after the boundary's own trimming, where it trims).
    Empty,
    /// Longer than [`MAX_TENANT_BYTES`].
    TooLong {
        /// The offending length in bytes.
        found: usize,
    },
    /// A byte outside ASCII graphic (`0x21`–`0x7E`), or one of the three
    /// excluded characters `:`, `#`, `/`.
    InvalidCharacter {
        /// The first offending character.
        found: char,
    },
}

impl fmt::Display for TenantIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("a tenant id must be non-empty"),
            Self::TooLong { found } => write!(
                f,
                "a tenant id is at most {MAX_TENANT_BYTES} bytes, found {found}"
            ),
            Self::InvalidCharacter { found } => write!(
                f,
                "a tenant id is ASCII graphic characters excluding ':', '#' and '/'; \
                 found {found:?}"
            ),
        }
    }
}

impl std::error::Error for TenantIdError {}

/// The RFC 0048 §3.1 tenant id grammar — **the** rule, applied once at
/// every boundary: 1–[`MAX_TENANT_BYTES`] bytes of ASCII graphic
/// characters (`0x21`–`0x7E`) with `:`, `#` and `/` excluded.
///
/// # Errors
///
/// [`TenantIdError`] naming the first failed rule.
pub fn validate_tenant_id(value: &str) -> Result<(), TenantIdError> {
    if value.is_empty() {
        return Err(TenantIdError::Empty);
    }
    if value.len() > MAX_TENANT_BYTES {
        return Err(TenantIdError::TooLong { found: value.len() });
    }
    match value
        .chars()
        .find(|c| !c.is_ascii_graphic() || matches!(c, ':' | '#' | '/'))
    {
        Some(found) => Err(TenantIdError::InvalidCharacter { found }),
        None => Ok(()),
    }
}

/// An opaque, operator-facing tenant identifier.
///
/// Backed by a `String` because tenant ids in deployed systems
/// are usually slugs (`"acme-corp"`), UUIDs, or k8s-style names —
/// human-readable matters more at this layer than column-store
/// width. A future `TenantIdHash` newtype may carry a fixed-width
/// hash for Parquet column efficiency, but only once the writer
/// crate exists and we have a benchmark that asks for it.
///
/// Equality is byte-for-byte (`String` `Eq`) — `"Acme"` and
/// `"acme"` are distinct tenants. No normalisation, no folding,
/// no validation. If a downstream caller wants validation
/// (reject empty, reject control characters), it can layer a
/// `try_new` constructor on top; we don't preempt that contract.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TenantId(String);

impl TenantId {
    /// Wrap an owned or borrowed string into a `TenantId`. No
    /// validation — see the type-level note; boundaries validate with
    /// [`try_new`](Self::try_new) instead (readers of already-stored ids
    /// do not re-litigate the grammar).
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// Wrap after the RFC 0048 §3.1 grammar check — the constructor for
    /// every request/config boundary.
    ///
    /// # Errors
    ///
    /// [`TenantIdError`] naming the first failed rule.
    pub fn try_new(s: impl Into<String>) -> Result<Self, TenantIdError> {
        let s = s.into();
        validate_tenant_id(&s)?;
        Ok(Self(s))
    }

    /// Borrow the underlying string. Useful for log messages,
    /// metric labels, and any code that needs the raw bytes.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for TenantId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TenantId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_TENANT_BYTES, TenantId, TenantIdError, validate_tenant_id};

    /// RFC0048.1 — the grammar table, one row per rule.
    #[test]
    fn grammar_accepts_and_rejects_by_rule() {
        for good in [
            "acme",
            "a",
            "acme-corp_01.eu",
            "A~!$%^&*()+=@?", // every other ASCII graphic is fine
            &"t".repeat(MAX_TENANT_BYTES),
        ] {
            assert_eq!(validate_tenant_id(good), Ok(()), "{good:?}");
        }
        assert_eq!(validate_tenant_id(""), Err(TenantIdError::Empty));
        assert_eq!(
            validate_tenant_id(&"t".repeat(MAX_TENANT_BYTES + 1)),
            Err(TenantIdError::TooLong {
                found: MAX_TENANT_BYTES + 1
            })
        );
        for (bad, found) in [
            ("a/b", '/'),
            ("a:b", ':'),
            ("a#b", '#'),
            ("a b", ' '),
            ("a\tb", '\t'),
            ("\u{e9}-tenant", '\u{e9}'),
            ("a\u{7f}b", '\u{7f}'),
        ] {
            assert_eq!(
                validate_tenant_id(bad),
                Err(TenantIdError::InvalidCharacter { found }),
                "{bad:?}"
            );
        }
        assert!(TenantId::try_new("acme").is_ok());
        assert_eq!(
            TenantId::try_new("a/b").unwrap_err(),
            TenantIdError::InvalidCharacter { found: '/' }
        );
    }
}
