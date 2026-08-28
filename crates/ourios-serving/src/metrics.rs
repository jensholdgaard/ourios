//! The auth-resolution instrument and the shared `error.type` class
//! values (RFC 0026 §3.4 / RFC 0047 §5). Moved with the resolver from
//! `ourios-ingester` (RFC 0051); the ingest metrics that tag batch
//! rejections re-import the class constants from here so the two
//! instruments can never drift apart on the value space.

use opentelemetry::metrics::Counter;
use opentelemetry::{KeyValue, global};
use ourios_semconv as semconv;

/// The `error.type` value for a missing/malformed/unknown bearer
/// (RFC 0026 §3.4).
pub const ERROR_TYPE_UNAUTHENTICATED: &str = "unauthenticated";
/// The `error.type` value for an authenticated cross-tenant rejection
/// (RFC 0026 §3.4).
pub const ERROR_TYPE_PERMISSION_DENIED: &str = "permission_denied";
/// The `error.type` value for a request failed closed because the
/// RFC 0047 resolver could not answer (`OpenFGA` unreachable / timed out).
pub const ERROR_TYPE_UPSTREAM_UNAVAILABLE: &str = "upstream_unavailable";

/// The OpenTelemetry-standard `error.type` attribute key (semconv, stable).
/// Deliberately **not** in the Ourios weaver registry — it is an upstream
/// OpenTelemetry attribute used here per the "recording errors on metrics"
/// convention, not an Ourios-coined name.
const ERROR_TYPE: &str = "error.type";

/// `ourios.auth.resolutions` (RFC 0047 §5): one count per credential
/// resolution by the [`AuthResolver`](crate::auth::AuthResolver),
/// tagged with `error.type` on failure. Owned by the resolver so every
/// role — ingest listeners, querier, MCP — records through the same
/// instrument.
pub struct AuthMetrics {
    resolutions: Counter<u64>,
}

impl AuthMetrics {
    /// Build the instrument with its registry unit.
    #[must_use]
    pub fn new() -> Self {
        let meter = global::meter("ourios.auth");
        Self {
            resolutions: meter
                .u64_counter(semconv::OURIOS_AUTH_RESOLUTIONS)
                .with_unit("{resolution}")
                .build(),
        }
    }

    /// Record one resolution: `None` for a bound credential, else the
    /// failure class.
    pub fn record(&self, error: Option<crate::auth::AuthError>) {
        match error {
            None => self.resolutions.add(1, &[]),
            Some(crate::auth::AuthError::Unauthenticated) => self
                .resolutions
                .add(1, &[KeyValue::new(ERROR_TYPE, ERROR_TYPE_UNAUTHENTICATED)]),
            Some(crate::auth::AuthError::Unavailable) => self.resolutions.add(
                1,
                &[KeyValue::new(ERROR_TYPE, ERROR_TYPE_UPSTREAM_UNAVAILABLE)],
            ),
        }
    }
}

impl Default for AuthMetrics {
    fn default() -> Self {
        Self::new()
    }
}
