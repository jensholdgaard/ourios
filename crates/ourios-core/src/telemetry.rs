//! Instrumentation scopes for Ourios's own telemetry.
//!
//! Every meter and tracer an Ourios crate resolves carries the release it
//! was built from as its scope version, so a backend can tell which build
//! emitted a series or a span. `opentelemetry::global::meter` and
//! `global::tracer` leave the version empty and are disallowed by the
//! workspace `clippy.toml`; resolve through [`meter`] and [`tracer`]
//! instead, or pass [`scope`] to a provider's `*_with_scope`.
//!
//! [`record_span_error`] and [`record_http_server_status`] set a span's
//! status the one way semconv *Recording errors* gives, so no call site
//! chooses its own.

use opentelemetry::InstrumentationScope;
use opentelemetry::global::{self, BoxedTracer};
use opentelemetry::metrics::Meter;

/// The scope `name`, versioned with this workspace release.
#[must_use]
pub fn scope(name: &'static str) -> InstrumentationScope {
    InstrumentationScope::builder(name)
        .with_version(env!("CARGO_PKG_VERSION"))
        .build()
}

/// The process-global meter for scope `name`, versioned.
#[must_use]
pub fn meter(name: &'static str) -> Meter {
    global::meter_with_scope(scope(name))
}

/// The process-global tracer for scope `name`, versioned.
#[must_use]
pub fn tracer(name: &'static str) -> BoxedTracer {
    global::tracer_with_scope(scope(name))
}

/// The upstream `error.type` attribute key.
pub const ERROR_TYPE: &str = "error.type";

/// Mark `span` failed: status `Error` and `error.type = error_type`, per
/// semconv *Recording errors*. The span must declare `otel.status_code`
/// and `error.type` as `tracing::field::Empty`; `tracing` drops a record of
/// an undeclared field.
pub fn record_span_error(span: &tracing::Span, error_type: &str) {
    span.record("otel.status_code", "ERROR");
    span.record(ERROR_TYPE, error_type);
}

/// The `error.type` an HTTP **server** span fails with for `status`: the
/// code itself for a `5xx`, `None` otherwise. Semconv leaves a server
/// span unset on a `4xx`, which is the client's error, and an extension
/// code outside `500..=599` is not a `5xx`.
#[must_use]
pub fn http_server_error_type(status: u16) -> Option<String> {
    (500..=599).contains(&status).then(|| status.to_string())
}

/// Record an HTTP **server** span's response status, failing the span on a
/// `5xx` ([`http_server_error_type`]).
pub fn record_http_server_status(span: &tracing::Span, status: u16) {
    // `i64`: the bridge stringifies `u64`, where semconv types the code int.
    span.record("http.response.status_code", i64::from(status));
    if let Some(error_type) = http_server_error_type(status) {
        record_span_error(span, &error_type);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn scope_carries_the_workspace_version() {
        let scope = super::scope("ourios.test");
        assert_eq!(scope.name(), "ourios.test");
        assert_eq!(scope.version(), Some(env!("CARGO_PKG_VERSION")));
        assert!(!env!("CARGO_PKG_VERSION").is_empty());
    }

    #[test]
    fn only_a_5xx_fails_an_http_server_span() {
        use super::http_server_error_type as error_type;
        assert_eq!(error_type(200), None);
        assert_eq!(error_type(404), None);
        assert_eq!(error_type(499), None);
        assert_eq!(error_type(500).as_deref(), Some("500"));
        assert_eq!(error_type(599).as_deref(), Some("599"));
        assert_eq!(error_type(600), None, "an extension code is not a 5xx");
        assert_eq!(error_type(999), None);
    }
}
