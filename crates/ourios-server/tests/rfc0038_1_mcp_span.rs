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
use opentelemetry::trace::{SpanKind, Status};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};
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

/// Install the process-global subscriber and tracer provider over an
/// in-memory exporter; they reach rmcp's spawned dispatch task. Keep the
/// provider alive until after the flush.
fn install_global_tracer() -> (InMemorySpanExporter, SdkTracerProvider) {
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    tracing_subscriber::registry()
        .with(tracing_opentelemetry::layer().with_tracer(ourios_telemetry::test_tracer(&provider)))
        .try_init()
        .expect("install global subscriber");
    opentelemetry::global::set_tracer_provider(provider.clone());
    (exporter, provider)
}

/// The MCP handshake; the session id.
async fn handshake(router: &Router) -> String {
    let init = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                   "clientInfo": {"name": "rfc0038-test", "version": "0"}}
    });
    let (status, session) = mcp_post(router, None, init).await;
    assert_eq!(status, StatusCode::OK, "initialize");
    let session = session.expect("session id issued");
    let (status, _) = mcp_post(
        router,
        Some(&session),
        serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    )
    .await;
    assert!(status.is_success(), "initialized notification");
    session
}

/// One `list_templates` tools/call for `tenant`. `list_templates` is the
/// leanest tool: an empty registry still opens its `execute_tool` span.
async fn list_templates(router: &Router, session: &str, id: u64, tenant: &str) {
    let call = serde_json::json!({
        "jsonrpc": "2.0", "id": id, "method": "tools/call",
        "params": {"name": "list_templates", "arguments": {"tenant": tenant}}
    });
    let (status, _) = mcp_post(router, Some(session), call).await;
    assert_eq!(status, StatusCode::OK, "tools/call");
}

fn attribute(span: &SpanData, key: &str) -> Option<String> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .map(|kv| kv.value.as_str().into_owned())
}

/// The one successful call's span: INTERNAL, with the canonical GenAI/MCP
/// semconv attributes (RFC0038.7, the agent-observability payload) and
/// status left unset.
fn assert_successful_tool_span(spans: &[SpanData], session: &str) {
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
    let span = mcp[0];
    assert_eq!(
        span.span_kind,
        SpanKind::Internal,
        "`execute_tool list_templates` is an INTERNAL span",
    );
    assert_eq!(
        attribute(span, "gen_ai.operation.name").as_deref(),
        Some("execute_tool"),
        "gen_ai.operation.name = execute_tool; attrs = {:?}",
        span.attributes,
    );
    assert_eq!(
        attribute(span, "gen_ai.tool.name").as_deref(),
        Some("list_templates"),
        "gen_ai.tool.name names the invoked tool",
    );
    assert_eq!(
        attribute(span, "mcp.method.name").as_deref(),
        Some("tools/call"),
        "mcp.method.name = tools/call",
    );
    assert_eq!(
        attribute(span, "mcp.session.id").as_deref(),
        Some(session),
        "mcp.session.id carries the caller's session, correlating its tool calls",
    );
    assert_eq!(
        attribute(span, "ourios.tenant").as_deref(),
        Some("acme"),
        "ourios.tenant names the tenant the tool ran against, as on POST /v1/query",
    );
    assert_eq!(
        span.status,
        Status::Unset,
        "a successful call is not an error"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0038_1_mcp_tool_emits_one_internal_span() {
    let (exporter, provider) = install_global_tracer();
    let bucket = tempfile::tempdir().expect("temp");
    let router = ourios_server::querier::router_with_mcp(
        bucket.path().to_path_buf(),
        3_600_000_000_000,
        ourios_serving::AuthResolver::static_only(None),
        true,
    );
    let session = handshake(&router).await;

    list_templates(&router, &session, 2, "acme").await;
    provider.force_flush().expect("spans flush");
    assert_successful_tool_span(&exporter.get_finished_spans().expect("spans"), &session);

    // A call the tool rejects (an off-grammar tenant is JSON-RPC
    // `invalid_params`, -32602) fails its span, with the code as `error.type`.
    exporter.reset();
    list_templates(&router, &session, 3, "").await;
    provider.force_flush().expect("spans flush");
    let spans = exporter.get_finished_spans().expect("spans");
    let failed = spans
        .iter()
        .find(|s| s.name.as_ref() == "execute_tool list_templates")
        .expect("the failed call's span");
    assert!(
        matches!(failed.status, Status::Error { .. }),
        "{:?}",
        failed.status
    );
    assert_eq!(attribute(failed, "error.type").as_deref(), Some("-32602"));
    assert_eq!(
        attribute(failed, "ourios.tenant"),
        None,
        "an off-grammar tenant argument is never recorded as a tenant id",
    );
}
