//! `ourios-serving` — shared serving infrastructure for the Ourios
//! roles (RFC 0051).
//!
//! Extracted from `ourios-ingester`'s receiver so the querier role
//! stops depending on the ingest crate for role-independent plumbing:
//!
//! - [`auth`] — per-request bearer authentication and tenant binding
//!   (RFC 0026 §3.2), the RFC 0029 OIDC path, and the RFC 0047 graph
//!   resolver seam.
//! - [`tls`] / [`tls_serve`] — listener TLS settings and the reloading
//!   acceptors (RFC 0030).
//! - [`propagation`] — inbound W3C trace-context extraction (RFC 0039).
//! - [`metrics`] — the `ourios.auth.resolutions` instrument and the
//!   shared `error.type` class values the roles tag rejections with.
//!
//! This crate sits below both roles: it depends on `ourios-core`
//! (types) and never on `ourios-ingester`, `ourios-querier` or
//! `ourios-parquet` (RFC 0051 §3.3).

#![deny(unsafe_code)]

pub mod auth;
pub mod metrics;
#[cfg(feature = "oidc")]
pub mod oidc;
#[cfg(feature = "openfga")]
pub mod openfga;
pub mod propagation;
pub mod tls;
pub mod tls_serve;

pub use auth::{AuthBinding, AuthError, AuthResolver, GraphIdentity, authenticate_bearer};
pub use metrics::AuthMetrics;
pub use propagation::{
    HeaderExtractor, MetadataExtractor, extract_context, extract_context_from_metadata,
};
pub use tls::TlsSettings;
