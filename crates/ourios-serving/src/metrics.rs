//! The auth-resolution instrument and the shared `error.type` class
//! values (RFC 0026 §3.4 / RFC 0047 §5). Moved with the resolver from
//! `ourios-ingester` (RFC 0051); the ingest metrics that tag batch
//! rejections re-import the class constants from here so the two
//! instruments can never drift apart on the value space.

use opentelemetry::metrics::Counter;
use opentelemetry::{KeyValue, global};
use ourios_semconv as semconv;

/// The `error.type` value for every [`AuthError::Unauthenticated`]
/// resolution — a missing, malformed, unknown, or otherwise
/// unresolvable bearer; the classes are intentionally
/// undifferentiated on the wire (RFC 0026 §3.4).
///
/// [`AuthError::Unauthenticated`]: crate::auth::AuthError::Unauthenticated
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

#[cfg(test)]
mod tests {
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
    use ourios_semconv as semconv;

    use super::{AuthMetrics, ERROR_TYPE_UNAUTHENTICATED, ERROR_TYPE_UPSTREAM_UNAVAILABLE};
    use crate::auth::AuthError;

    /// One resolution per outcome class: success counts bare, each
    /// failure counts under its `error.type` value. A single test owns
    /// the in-memory provider — `init_in_memory` installs the *global*
    /// meter, so two such tests in one binary would race.
    #[test]
    fn record_counts_success_bare_and_failures_by_error_type() {
        let (guard, exporter) = ourios_telemetry::init_in_memory("ourios-serving-test");
        let metrics = AuthMetrics::new();
        metrics.record(None);
        metrics.record(None);
        metrics.record(Some(AuthError::Unauthenticated));
        metrics.record(Some(AuthError::Unavailable));
        guard.force_flush().expect("force_flush succeeds");

        let rms = exporter.get_finished_metrics().expect("metrics exported");
        let metric = rms
            .iter()
            .flat_map(opentelemetry_sdk::metrics::data::ResourceMetrics::scope_metrics)
            .flat_map(opentelemetry_sdk::metrics::data::ScopeMetrics::metrics)
            .find(|m| m.name() == semconv::OURIOS_AUTH_RESOLUTIONS)
            .expect("resolutions counter exported");
        let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric.data() else {
            panic!("resolutions should be a u64 sum");
        };

        let value_for = |error_type: Option<&str>| {
            sum.data_points()
                .find(|dp| {
                    let tagged = dp
                        .attributes()
                        .find(|kv| kv.key.as_str() == "error.type")
                        .map(|kv| kv.value.as_str().to_string());
                    tagged.as_deref() == error_type
                })
                .unwrap_or_else(|| panic!("missing datapoint for {error_type:?}"))
                .value()
        };
        assert_eq!(value_for(None), 2, "successes count bare");
        assert_eq!(value_for(Some(ERROR_TYPE_UNAUTHENTICATED)), 1);
        assert_eq!(value_for(Some(ERROR_TYPE_UPSTREAM_UNAVAILABLE)), 1);
    }
}
