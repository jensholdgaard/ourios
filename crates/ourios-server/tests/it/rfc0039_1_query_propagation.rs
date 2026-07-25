//! RFC0039.1 / .2 / .5 (query arm) — the `POST /v1/query` server span
//! continues the caller's trace when a `traceparent` arrives, and roots itself
//! when one does not.
//!
//! Uses the same scoped-`WithSubscriber` harness as
//! `rfc0038_1_request_spans.rs` (no process-global tracer, so it stays in the
//! consolidated `it` binary). The propagator is passed explicitly rather than
//! relying on the global one that `ourios_telemetry::init` installs: these tests
//! must not depend on another test having initialised telemetry first.

use axum::body::Body;
use axum::http::{Request, header};
use opentelemetry::propagation::TextMapPropagator as _;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
use tower::ServiceExt as _;
use tracing::instrument::WithSubscriber as _;
use tracing_subscriber::prelude::*;

const WINDOW_NANOS: u64 = 3_600_000_000_000;

/// The trace the caller claims to be inside.
const CALLER_TRACE: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
const CALLER_SPAN: &str = "00f067aa0ba902b7";

/// Install the global propagator exactly once for this test binary. The tests
/// below run in parallel, so writing the process-global propagator per call
/// would mean concurrent writes to shared state; `Once` makes it a single
/// install that all of them observe.
fn install_propagator() {
    static INSTALL: std::sync::Once = std::sync::Once::new();
    INSTALL.call_once(|| {
        opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
    });
}

/// Drive one query through the real router, optionally carrying `traceparent`,
/// and return the exported spans.
async fn query_spans(traceparent: Option<&str>) -> Vec<opentelemetry_sdk::trace::SpanData> {
    // The global propagator is what `extract_context` consults.
    install_propagator();

    let bucket = tempfile::tempdir().expect("temp");
    let app = ourios_server::querier::router(bucket.path().to_path_buf(), WINDOW_NANOS);

    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let subscriber = tracing_subscriber::registry()
        .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("ourios-test")));

    let mut builder = Request::builder()
        .method("POST")
        .uri("/v1/query")
        .header(header::CONTENT_TYPE, "text/plain")
        .header("X-Ourios-Tenant", "acme");
    if let Some(tp) = traceparent {
        builder = builder.header("traceparent", tp);
    }
    let response = app
        .oneshot(
            builder
                .body(Body::from("template_id == 1"))
                .expect("build request"),
        )
        .with_subscriber(subscriber)
        .await
        .expect("oneshot");
    assert!(
        response.status().is_success(),
        "query should succeed, got {}",
        response.status(),
    );

    provider.force_flush().expect("spans flush");
    exporter.get_finished_spans().expect("spans exported")
}

/// The one `POST /v1/query` span among the exported spans.
fn query_span(spans: &[opentelemetry_sdk::trace::SpanData]) -> &opentelemetry_sdk::trace::SpanData {
    let matches: Vec<_> = spans
        .iter()
        .filter(|s| s.name.as_ref() == "POST /v1/query")
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "exactly one query span, got {:?}",
        spans.iter().map(|s| s.name.clone()).collect::<Vec<_>>(),
    );
    matches[0]
}

/// Scenario RFC0039.1 (query arm) — a `traceparent` makes the server span a
/// child of the caller's span, inside the caller's trace.
/// See `docs/rfcs/0039-inbound-trace-context-propagation.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn rfc0039_1_query_span_continues_the_caller_trace() {
    let spans = query_spans(Some(&format!("00-{CALLER_TRACE}-{CALLER_SPAN}-01"))).await;
    let span = query_span(&spans);
    assert_eq!(
        span.span_context.trace_id().to_string(),
        CALLER_TRACE,
        "the query span joins the caller's trace",
    );
    assert_eq!(
        span.parent_span_id.to_string(),
        CALLER_SPAN,
        "the query span is parented to the caller's span",
    );
}

/// Scenario RFC0039.2 — with no `traceparent` the span is a fresh, valid root:
/// a newly minted trace id and no parent (the pre-RFC behaviour, unchanged).
/// See `docs/rfcs/0039-inbound-trace-context-propagation.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn rfc0039_2_no_traceparent_is_a_fresh_root() {
    let spans = query_spans(None).await;
    let span = query_span(&spans);
    assert!(
        span.span_context.trace_id() != opentelemetry::trace::TraceId::INVALID,
        "a root span still gets a real trace id",
    );
    assert_ne!(
        span.span_context.trace_id().to_string(),
        CALLER_TRACE,
        "no carrier means the caller's trace is not joined",
    );
    assert_eq!(
        span.parent_span_id,
        opentelemetry::trace::SpanId::INVALID,
        "a root span has no parent",
    );
}

/// Scenario RFC0039.5 — a malformed `traceparent` is treated as absent: the
/// request succeeds (asserted in `query_spans`) and the span roots itself
/// rather than erroring or inheriting a bogus parent.
/// See `docs/rfcs/0039-inbound-trace-context-propagation.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn rfc0039_5_malformed_traceparent_is_treated_as_absent() {
    let spans = query_spans(Some("not-a-valid-traceparent")).await;
    let span = query_span(&spans);
    assert_eq!(
        span.parent_span_id,
        opentelemetry::trace::SpanId::INVALID,
        "a malformed carrier yields no parent",
    );
    assert!(
        span.span_context.trace_id() != opentelemetry::trace::TraceId::INVALID,
        "and the span is still a valid root",
    );
}

/// The extraction the handler performs is the propagator's, not a bespoke
/// parse: the same carrier fed to `TraceContextPropagator` names the same
/// remote span the exported span is parented to. Guards against the test above
/// passing on a coincidence.
#[test]
fn caller_traceparent_names_the_expected_remote_span() {
    use opentelemetry::trace::TraceContextExt as _;

    struct Carrier(axum::http::HeaderMap);
    impl opentelemetry::propagation::Extractor for Carrier {
        fn get(&self, key: &str) -> Option<&str> {
            self.0.get(key).and_then(|v| v.to_str().ok())
        }
        fn keys(&self) -> Vec<&str> {
            self.0.keys().map(axum::http::HeaderName::as_str).collect()
        }
    }

    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        "traceparent",
        format!("00-{CALLER_TRACE}-{CALLER_SPAN}-01")
            .parse()
            .expect("valid header"),
    );
    let cx = TraceContextPropagator::new().extract(&Carrier(headers));
    let span = cx.span();
    assert_eq!(span.span_context().trace_id().to_string(), CALLER_TRACE);
    assert_eq!(span.span_context().span_id().to_string(), CALLER_SPAN);
}
