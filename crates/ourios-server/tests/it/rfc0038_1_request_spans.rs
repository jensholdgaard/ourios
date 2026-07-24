//! RFC0038.1 — the served query request-scope span is emitted, one per
//! operation, with the §3.5 kind contract.
//!
//! Slice 2 covered the ingest arm (`crates/ourios-ingester/tests/it/`); this
//! file covers the served query (`POST /v1/query`, SERVER), driving the real
//! in-process router (`oneshot`) under a scoped `WithSubscriber` over an
//! `InMemorySpanExporter` — no process-global tracer, so it stays in the
//! consolidated harness (see this crate's `tests/it/main.rs` on the
//! global-installer exclusion).
//!
//! The MCP tool span (`execute_tool <tool>`) is NOT tested here: rmcp's
//! streamable-HTTP service dispatches the tool call on a `tokio::spawn`ed
//! task, and a scoped subscriber does not cross that boundary (the same
//! `tokio::spawn` context trap RFC0038.3 is about). Verifying it needs a
//! process-global tracer, so it clusters with the RFC0038.3 spawn-boundary
//! test in a follow-up harness-exempt binary. (The MCP span itself is wired
//! and works under the production global tracer — slice 1's `serve_inner`
//! correlation proved it.)

use axum::body::Body;
use axum::http::{Request, header};
use opentelemetry::trace::{SpanKind, TracerProvider as _};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};
use tower::ServiceExt as _;
use tracing::instrument::WithSubscriber as _;
use tracing_subscriber::prelude::*;

/// One hour, in nanoseconds — the default query window these tests run under
/// (they assert on spans, not on the window).
const WINDOW_NANOS: u64 = 3_600_000_000_000;

/// A tracer over a fresh in-memory span exporter, plus a scoped subscriber
/// wiring `tracing` spans onto it. The caller MUST keep the returned
/// `SdkTracerProvider` alive until after it has driven its operation and
/// flushed — dropping it shuts the provider down, so no spans export.
fn scoped_tracer() -> (
    InMemorySpanExporter,
    SdkTracerProvider,
    impl tracing::Subscriber + Send + Sync,
) {
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let subscriber = tracing_subscriber::registry()
        .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("ourios-test")));
    (exporter, provider, subscriber)
}

fn spans_named<'a>(spans: &'a [SpanData], name: &str) -> Vec<&'a SpanData> {
    spans.iter().filter(|s| s.name.as_ref() == name).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn rfc0038_1_query_emits_one_server_span() {
    let bucket = tempfile::tempdir().expect("temp");
    let app = ourios_server::querier::router(bucket.path().to_path_buf(), WINDOW_NANOS);
    let (exporter, provider, subscriber) = scoped_tracer();

    // An empty store is enough — the span is opened at handler entry
    // regardless of whether the query matches anything.
    let request = Request::builder()
        .method("POST")
        .uri("/v1/query")
        .header(header::CONTENT_TYPE, "text/plain")
        .header("X-Ourios-Tenant", "acme")
        .body(Body::from("template_id == 1"))
        .expect("build request");
    let _ = app
        .oneshot(request)
        .with_subscriber(subscriber)
        .await
        .expect("oneshot");

    provider.force_flush().expect("spans flush");
    let spans = exporter.get_finished_spans().expect("spans exported");
    let query = spans_named(&spans, "POST /v1/query");
    assert_eq!(
        query.len(),
        1,
        "exactly one query span, got {:?}",
        spans.iter().map(|s| s.name.clone()).collect::<Vec<_>>(),
    );
    assert_eq!(
        query[0].span_kind,
        SpanKind::Server,
        "POST /v1/query is a SERVER span",
    );
}
