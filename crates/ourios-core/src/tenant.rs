//! Tenant identity for the multi-tenant miner.
//!
//! `[CLAUDE.md §3.7]`: every code path that touches data takes a
//! tenant id. This module ships the type; routing, storage, and
//! per-tenant state live in the consuming crates.

use std::fmt;

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
    /// validation — see the type-level note.
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
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
