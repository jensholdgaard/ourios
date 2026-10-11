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

/// Record an HTTP **server** span's response status. Only a `5xx` fails the
/// span, with the status code as `error.type`: semconv leaves a server
/// span's status unset on a `4xx`, which is the client's error.
pub fn record_http_server_status(span: &tracing::Span, status: u16) {
    // `i64`: the bridge stringifies `u64`, where semconv types the code int.
    span.record("http.response.status_code", i64::from(status));
    if status >= 500 {
        record_span_error(span, &status.to_string());
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
}
