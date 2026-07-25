//! RFC0039.4 — the caller's sampling decision governs, end to end.
//!
//! Propagation is only meaningful if the parent's sampled flag is honoured:
//! a caller who sampled trace `T` should see Ourios's spans inside it, and a
//! caller who did not should see Ourios add nothing — otherwise the trace is
//! half-recorded and inconsistent across the boundary.
//!
//! The parent-based resolution itself is upstream SDK behaviour; what is under
//! test here is Ourios's *wiring* — that the context extracted at the ingress
//! actually reaches the sampler. The sampler is therefore pinned explicitly
//! rather than relying on `SdkTracerProvider`'s default (which is
//! `ParentBased(AlwaysOn)` today): the test should state the regime it asserts
//! under, not inherit it.
//!
//! Both arms also assert the **response** is unaffected. Sampling is a
//! telemetry decision; a request must never succeed or fail based on whether
//! its caller chose to record the trace.

use axum::body::Body;
use axum::http::{Request, header};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::{InMemorySpanExporter, Sampler, SdkTracerProvider, SpanData};
use tower::ServiceExt as _;
use tracing::instrument::WithSubscriber as _;
use tracing_subscriber::prelude::*;

const WINDOW_NANOS: u64 = 3_600_000_000_000;

/// The trace the caller claims to be inside.
const CALLER_TRACE: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
const CALLER_SPAN: &str = "00f067aa0ba902b7";

/// Install the global propagator once. `extract_context` resolves through the
/// process-global propagator, so it has to be present; sibling modules in this
/// binary install the same one, which is why this is idempotent by design.
fn install_propagator() {
    static INSTALL: std::sync::Once = std::sync::Once::new();
    INSTALL.call_once(|| {
        opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
    });
}

/// Drive one query carrying `traceparent` under a `ParentBased(AlwaysOn)`
/// sampler; returns the exported spans. Asserts the request succeeded, so a
/// caller reading the span assertions knows the response was never in question.
async fn query_spans_under_parentbased(traceparent: &str) -> Vec<SpanData> {
    install_propagator();

    let bucket = tempfile::tempdir().expect("temp");
    let app = ourios_server::querier::router(bucket.path().to_path_buf(), WINDOW_NANOS);

    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_sampler(Sampler::ParentBased(Box::new(Sampler::AlwaysOn)))
        .with_simple_exporter(exporter.clone())
        .build();
    let subscriber = tracing_subscriber::registry()
        .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("ourios-test")));

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/query")
                .header(header::CONTENT_TYPE, "text/plain")
                .header("X-Ourios-Tenant", "acme")
                .header("traceparent", traceparent)
                .body(Body::from("template_id == 1"))
                .expect("build request"),
        )
        .with_subscriber(subscriber)
        .await
        .expect("oneshot");
    assert!(
        response.status().is_success(),
        "the query succeeds regardless of the caller's sampling choice, got {}",
        response.status(),
    );

    provider.force_flush().expect("spans flush");
    exporter.get_finished_spans().expect("spans exported")
}

fn query_spans(spans: &[SpanData]) -> Vec<&SpanData> {
    spans
        .iter()
        .filter(|s| s.name.as_ref() == "POST /v1/query")
        .collect()
}

/// Scenario RFC0039.4 (sampled arm) — a `-01` parent is recorded, inside the
/// caller's trace. See `docs/rfcs/0039-inbound-trace-context-propagation.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn rfc0039_4_sampled_parent_is_recorded_in_the_caller_trace() {
    let spans = query_spans_under_parentbased(&format!("00-{CALLER_TRACE}-{CALLER_SPAN}-01")).await;
    let query = query_spans(&spans);
    assert_eq!(
        query.len(),
        1,
        "a sampled parent exports the query span, got {:?}",
        spans.iter().map(|s| s.name.clone()).collect::<Vec<_>>(),
    );
    assert_eq!(
        query[0].span_context.trace_id().to_string(),
        CALLER_TRACE,
        "and it lands in the caller's trace",
    );
}

/// Scenario RFC0039.4 (unsampled arm) — a `-00` parent exports **nothing**, so
/// Ourios does not half-record a trace its caller chose to drop. The `-00` and
/// `-01` arms differ only in that flag, which is what makes this the sampler's
/// decision rather than an accident of the harness.
/// See `docs/rfcs/0039-inbound-trace-context-propagation.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn rfc0039_4_unsampled_parent_exports_no_span() {
    let spans = query_spans_under_parentbased(&format!("00-{CALLER_TRACE}-{CALLER_SPAN}-00")).await;
    assert!(
        query_spans(&spans).is_empty(),
        "an unsampled parent must export no query span, got {:?}",
        spans.iter().map(|s| s.name.clone()).collect::<Vec<_>>(),
    );
    assert!(
        spans.is_empty(),
        "and nothing else from the request either, got {:?}",
        spans.iter().map(|s| s.name.clone()).collect::<Vec<_>>(),
    );
}
