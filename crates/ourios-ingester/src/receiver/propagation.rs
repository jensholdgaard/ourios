//! Inbound W3C trace-context extraction (RFC 0039).
//!
//! Every Ourios ingress — OTLP gRPC/HTTP, the query API, the MCP tools —
//! receives its caller's trace context as HTTP headers, so one
//! [`HeaderExtractor`] over [`http::HeaderMap`] serves all of them (the gRPC
//! path extracts from the raw HTTP headers at its tower layer, so no tonic
//! `MetadataMap` adapter is needed).
//!
//! [`extract_context`] resolves the context through the globally installed
//! propagator (`ourios-telemetry` installs `TraceContextPropagator` in `init`).
//! Callers then run the span-producing future under that context —
//! `future.with_context(cx)` — rather than calling `set_parent` on the span:
//! `set_parent` fails with `AlreadyStarted` on a span that has been entered,
//! which every `#[tracing::instrument]` span is for the whole of its body, so
//! the parent would be silently dropped (RFC 0039 §3.3).

use axum::http::HeaderMap;
use opentelemetry::Context;
use opentelemetry::propagation::Extractor;

/// Reads W3C trace-context headers out of an [`http::HeaderMap`].
pub struct HeaderExtractor<'a>(pub &'a HeaderMap);

impl Extractor for HeaderExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|value| value.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(axum::http::HeaderName::as_str).collect()
    }
}

/// The caller's trace context from `headers`, via the globally installed
/// propagator. Absent, malformed, or unparsable context yields a context with
/// no remote span — the span that runs under it becomes a fresh root, which is
/// exactly the pre-RFC behaviour (RFC0039.2 / RFC0039.5).
#[must_use]
pub fn extract_context(headers: &HeaderMap) -> Context {
    opentelemetry::global::get_text_map_propagator(|propagator| {
        propagator.extract(&HeaderExtractor(headers))
    })
}

#[cfg(test)]
mod tests {
    use opentelemetry::trace::TraceContextExt as _;

    use super::{HeaderExtractor, extract_context};

    /// A `traceparent` in the carrier round-trips to the remote `SpanContext`
    /// it names — the extraction half of RFC0039.1. Uses an explicit
    /// `TraceContextPropagator` rather than the global one so the unit test
    /// does not depend on install order.
    #[test]
    fn traceparent_round_trips_to_a_remote_span_context() {
        use opentelemetry::propagation::TextMapPropagator as _;
        use opentelemetry_sdk::propagation::TraceContextPropagator;

        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            "traceparent",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
                .parse()
                .expect("valid header value"),
        );
        let cx = TraceContextPropagator::new().extract(&HeaderExtractor(&headers));
        let span = cx.span();
        let sc = span.span_context();
        assert_eq!(
            sc.trace_id().to_string(),
            "4bf92f3577b34da6a3ce929d0e0e4736",
        );
        assert_eq!(sc.span_id().to_string(), "00f067aa0ba902b7");
        assert!(sc.is_sampled(), "the `-01` flag is the sampled bit");
        assert!(sc.is_remote(), "an extracted context is remote");
    }

    /// No `traceparent` yields no remote span, so the span run under it roots
    /// itself (RFC0039.2). Goes through the global propagator, which is a no-op
    /// propagator unless something installed one — either way the result must
    /// be an invalid (absent) span context, never a panic.
    #[test]
    fn absent_traceparent_yields_no_remote_span() {
        let cx = extract_context(&axum::http::HeaderMap::new());
        assert!(
            !cx.span().span_context().is_valid(),
            "no carrier means no parent",
        );
    }

    /// A syntactically invalid `traceparent` is treated as absent, not an
    /// error (RFC0039.5) — a malformed caller header must never fail a request.
    #[test]
    fn malformed_traceparent_is_treated_as_absent() {
        use opentelemetry::propagation::TextMapPropagator as _;
        use opentelemetry_sdk::propagation::TraceContextPropagator;

        for bad in ["not-a-traceparent", "00-tooshort-00f067aa0ba902b7-01", ""] {
            let mut headers = axum::http::HeaderMap::new();
            headers.insert("traceparent", bad.parse().expect("valid header value"));
            let cx = TraceContextPropagator::new().extract(&HeaderExtractor(&headers));
            assert!(
                !cx.span().span_context().is_valid(),
                "malformed traceparent {bad:?} must not yield a parent",
            );
        }
    }
}
