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
use opentelemetry::Value;
use opentelemetry::trace::{SpanKind, TracerProvider as _};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
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

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn rfc0038_1_query_emits_one_server_span() {
    let bucket = tempfile::tempdir().expect("temp");
    let app = ourios_server::querier::router(bucket.path().to_path_buf(), WINDOW_NANOS);
    let (exporter, provider, subscriber) = scoped_tracer();

    // A well-formed query against a valid (empty) tenant is a success with
    // an empty result — the span opens at handler entry regardless.
    let request = Request::builder()
        .method("POST")
        .uri("/v1/query")
        .header(header::CONTENT_TYPE, "text/plain")
        .header("X-Ourios-Tenant", "acme")
        .body(Body::from("template_id == 1"))
        .expect("build request");
    let response = app
        .oneshot(request)
        .with_subscriber(subscriber)
        .await
        .expect("oneshot");
    // Assert the request actually succeeded, so a routing/auth/parse
    // regression can't pass just because the handler opened a span.
    assert!(
        response.status().is_success(),
        "query request should succeed, got {}",
        response.status(),
    );

    provider.force_flush().expect("spans flush");
    let spans = exporter.get_finished_spans().expect("spans exported");
    // Exactly one span, and it is the query server span — asserting the total
    // count catches accidental extra instrumentation (the "one span per
    // operation" contract). The query path emits no other span: the miner /
    // DataFusion inner work stays span-free (RFC0038.2).
    assert_eq!(
        spans.len(),
        1,
        "exactly one span total, got {:?}",
        spans.iter().map(|s| s.name.clone()).collect::<Vec<_>>(),
    );
    assert_eq!(spans[0].name.as_ref(), "POST /v1/query");
    assert_eq!(
        spans[0].span_kind,
        SpanKind::Server,
        "POST /v1/query is a SERVER span",
    );

    // §3.5 — the query span carries the standard HTTP server attributes plus
    // the tenant it scoped to (a successful query against tenant `acme`).
    let attr = |key: &str| {
        spans[0]
            .attributes
            .iter()
            .find(|kv| kv.key.as_str() == key)
            .map(|kv| kv.value.clone())
    };
    assert!(
        matches!(attr("http.request.method"), Some(Value::String(s)) if s.as_str() == "POST"),
        "http.request.method = POST; attrs = {:?}",
        spans[0].attributes,
    );
    assert!(
        matches!(attr("http.route"), Some(Value::String(s)) if s.as_str() == "/v1/query"),
        "http.route = /v1/query",
    );
    assert!(
        matches!(attr("ourios.tenant"), Some(Value::String(s)) if s.as_str() == "acme"),
        "ourios.tenant carries the query's scoped tenant",
    );
    assert!(
        matches!(attr("http.response.status_code"), Some(Value::I64(200))),
        "http.response.status_code = 200, got {:?}",
        attr("http.response.status_code"),
    );
}
