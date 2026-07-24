//! RFC0038.1 (MCP arm) — the MCP tool span is emitted, one per tool call,
//! with the §3.5 kind contract.
//!
//! Harness-exempt (RFC0028.2): installs the **process-global** `OTel` tracer, so
//! its own top-level binary with a single test. A global (not scoped) tracer
//! is required because rmcp's streamable-HTTP service dispatches the tool call
//! on a `tokio::spawn`ed task — a scoped subscriber can't cross that boundary
//! (see `tests/it/rfc0038_1_request_spans.rs`), so the served-query arm lives
//! there and the MCP arm lives here.

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use opentelemetry::trace::{SpanKind, TracerProvider as _};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
use tower::ServiceExt as _;
use tracing_subscriber::prelude::*;

/// POST one MCP JSON-RPC frame at `/mcp`; returns status + any session id.
/// (A minimal inline copy of the `it` harness's `mcp_post` — this standalone
/// binary can't share that module.)
async fn mcp_post(
    router: &Router,
    session: Option<&str>,
    body: serde_json::Value,
) -> (StatusCode, Option<String>) {
    let mut req = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json, text/event-stream")
        .header(header::HOST, "127.0.0.1");
    if let Some(id) = session {
        req = req.header("mcp-session-id", id);
    }
    let response = router
        .clone()
        .oneshot(
            req.body(Body::from(body.to_string()))
                .expect("build request"),
        )
        .await
        .expect("oneshot");
    let status = response.status();
    let session_id = response
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let _ = to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .expect("read body");
    (status, session_id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0038_1_mcp_tool_emits_one_internal_span() {
    // Process-global tracer over an in-memory exporter — reaches rmcp's spawned
    // dispatch task. Keep `provider` alive until after the flush.
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    tracing_subscriber::registry()
        .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("ourios-test")))
        .try_init()
        .expect("install global subscriber");
    opentelemetry::global::set_tracer_provider(provider.clone());

    let bucket = tempfile::tempdir().expect("temp");
    let router = ourios_server::querier::router_with_mcp(
        bucket.path().to_path_buf(),
        3_600_000_000_000,
        ourios_ingester::receiver::AuthResolver::static_only(None),
        true,
    );

    // Handshake, then one tools/call. `list_templates` is the leanest tool —
    // an empty registry still opens the `execute_tool list_templates` span.
    let init = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                   "clientInfo": {"name": "rfc0038-test", "version": "0"}}
    });
    let (status, session) = mcp_post(&router, None, init).await;
    assert_eq!(status, StatusCode::OK, "initialize");
    let session = session.expect("session id issued");

    let (status, _) = mcp_post(
        &router,
        Some(&session),
        serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    )
    .await;
    assert!(status.is_success(), "initialized notification");

    let call = serde_json::json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "list_templates", "arguments": {"tenant": "acme"}}
    });
    let (status, _) = mcp_post(&router, Some(&session), call).await;
    assert_eq!(status, StatusCode::OK, "tools/call");

    provider.force_flush().expect("spans flush");
    let spans = exporter.get_finished_spans().expect("spans exported");
    let mcp: Vec<_> = spans
        .iter()
        .filter(|s| s.name.as_ref() == "execute_tool list_templates")
        .collect();
    assert_eq!(
        mcp.len(),
        1,
        "one `execute_tool list_templates` span survived rmcp's spawn, got {:?}",
        spans.iter().map(|s| s.name.clone()).collect::<Vec<_>>(),
    );
    assert_eq!(
        mcp[0].span_kind,
        SpanKind::Internal,
        "`execute_tool list_templates` is an INTERNAL span",
    );
}
