//! RFC0039.6 — the MCP tool call joins the caller's trace, correctly shaped.
//!
//! Harness-exempt (RFC0028.2, see `tests/README.md`): installs the
//! **process-global** `OTel` tracer, required because rmcp's streamable-HTTP
//! service dispatches the tool call on a `tokio::spawn`ed task that a scoped
//! subscriber cannot cross.
//!
//! The shape under test is two spans, not one:
//!
//! - `POST /mcp` — kind `SERVER`, continuing the caller's remote trace. MCP
//!   `tools/call` is JSON-RPC over HTTP and both conventions require the
//!   inbound server span to be `SERVER`.
//! - `execute_tool <tool>` — kind `INTERNAL`, nested **locally** under that
//!   server span. The spec defines `INTERNAL` as an operation "as opposed to
//!   an operations with remote parents or children", so parenting it straight
//!   to the remote caller would contradict its own kind. Its parent must be
//!   the local `SERVER` span, and its trace the caller's.
//!
//! Only the `tools/call` request carries a `traceparent`; the handshake
//! requests deliberately do not, so the same run also covers the no-inbound
//! -context case for `/mcp` (RFC0039.2).

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use opentelemetry::trace::{SpanKind, TracerProvider as _};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};
use tower::ServiceExt as _;
use tracing_subscriber::prelude::*;

/// The trace the calling agent claims to be inside.
const CALLER_TRACE: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
const CALLER_SPAN: &str = "00f067aa0ba902b7";

/// POST one MCP JSON-RPC frame at `/mcp`, optionally carrying `traceparent`;
/// returns status + any session id.
async fn mcp_post(
    router: &Router,
    session: Option<&str>,
    traceparent: Option<&str>,
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
    if let Some(tp) = traceparent {
        req = req.header("traceparent", tp);
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

fn named<'a>(spans: &'a [SpanData], name: &str) -> Vec<&'a SpanData> {
    spans.iter().filter(|s| s.name.as_ref() == name).collect()
}

/// The `/mcp` span's kind, its remote parent, and its HTTP semconv payload.
///
/// The `otel.name` check is a guard on the `weaver registry live-check` CI gate:
/// `otel.name` is a *synthetic* tracing field that the bridge consumes to set the
/// span name. It is not a registry key, so were it to survive as a real
/// attribute, live-check would reject it.
fn assert_server_span(span: &SpanData) {
    assert_eq!(
        span.span_kind,
        SpanKind::Server,
        "the inbound `/mcp` span is SERVER, per the RPC and HTTP conventions",
    );
    assert_eq!(
        span.parent_span_id.to_string(),
        CALLER_SPAN,
        "the server span continues the caller's span",
    );

    let attr = |key: &str| -> Option<String> {
        span.attributes
            .iter()
            .find(|kv| kv.key.as_str() == key)
            .map(|kv| kv.value.as_str().into_owned())
    };
    assert_eq!(
        attr("http.request.method").as_deref(),
        Some("POST"),
        "attrs = {:?}",
        span.attributes,
    );
    assert_eq!(attr("http.route").as_deref(), Some("/mcp"));
    assert!(
        attr("otel.name").is_none(),
        "`otel.name` is consumed as the span name, never exported as an attribute",
    );

    // The variant matters, not just the rendering: the bridge stringifies
    // `u64`-valued fields, so recording the status as `u16` would export
    // `String("200")` where the convention wants an integer. `as_str()` renders
    // both identically, so this has to inspect the `Value` itself.
    let status = span
        .attributes
        .iter()
        .find(|kv| kv.key.as_str() == "http.response.status_code")
        .map(|kv| kv.value.clone());
    assert!(
        matches!(status, Some(opentelemetry::Value::I64(200))),
        "http.response.status_code is an integer 200, got {status:?}",
    );
}

/// `initialize` + `notifications/initialized`, deliberately **without** a
/// `traceparent` — these two requests are the RFC0039.2 arm of this test.
/// Returns the issued session id.
async fn handshake(router: &Router) -> String {
    let init = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                   "clientInfo": {"name": "rfc0039-test", "version": "0"}}
    });
    let (status, session) = mcp_post(router, None, None, init).await;
    assert_eq!(status, StatusCode::OK, "initialize");
    let session = session.expect("session id issued");

    let (status, _) = mcp_post(
        router,
        Some(&session),
        None,
        serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    )
    .await;
    assert!(status.is_success(), "initialized notification");
    session
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0039_6_mcp_tool_call_joins_the_caller_trace() {
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    tracing_subscriber::registry()
        .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("ourios-test")))
        .try_init()
        .expect("install global subscriber");
    opentelemetry::global::set_tracer_provider(provider.clone());
    // What the handler's `extract_context` resolves through; `ourios_telemetry::init`
    // installs this in production (RFC 0039 §3.1).
    opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());

    let bucket = tempfile::tempdir().expect("temp");
    let router = ourios_server::querier::router_with_mcp(
        bucket.path().to_path_buf(),
        3_600_000_000_000,
        ourios_ingester::receiver::AuthResolver::static_only(None),
        true,
    );

    let session = handshake(&router).await;

    // The one request that carries the caller's context.
    let traceparent = format!("00-{CALLER_TRACE}-{CALLER_SPAN}-01");
    let call = serde_json::json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "list_templates", "arguments": {"tenant": "acme"}}
    });
    let (status, _) = mcp_post(&router, Some(&session), Some(&traceparent), call).await;
    assert_eq!(status, StatusCode::OK, "tools/call");

    provider.force_flush().expect("spans flush");
    let spans = exporter.get_finished_spans().expect("spans exported");

    // --- The SERVER span for the traced request. ---
    let server: Vec<_> = named(&spans, "POST /mcp")
        .into_iter()
        .filter(|s| s.span_context.trace_id().to_string() == CALLER_TRACE)
        .collect();
    assert_eq!(
        server.len(),
        1,
        "one `POST /mcp` span in the caller's trace, got {:?}",
        spans
            .iter()
            .map(|s| (s.name.clone(), s.span_context.trace_id().to_string()))
            .collect::<Vec<_>>(),
    );
    assert_server_span(server[0]);

    // --- The tool span: caller's trace, but a LOCAL parent. ---
    let tool = named(&spans, "execute_tool list_templates");
    assert_eq!(
        tool.len(),
        1,
        "one `execute_tool list_templates` span, got {:?}",
        spans.iter().map(|s| s.name.clone()).collect::<Vec<_>>(),
    );
    assert_eq!(
        tool[0].span_context.trace_id().to_string(),
        CALLER_TRACE,
        "the tool span joins the caller's trace across rmcp's dispatch spawn",
    );
    assert_eq!(
        tool[0].parent_span_id,
        server[0].span_context.span_id(),
        "the tool span nests under the local SERVER span, not the remote caller",
    );
    assert_eq!(
        tool[0].span_kind,
        SpanKind::Internal,
        "and so it stays INTERNAL — a kind the spec defines as excluding remote parents",
    );

    // --- RFC0039.2 for `/mcp`: the untraced handshake rooted itself. ---
    let rooted: Vec<_> = named(&spans, "POST /mcp")
        .into_iter()
        .filter(|s| s.span_context.trace_id().to_string() != CALLER_TRACE)
        .collect();
    assert!(
        !rooted.is_empty(),
        "the handshake requests, carrying no traceparent, are their own roots",
    );
    for span in rooted {
        assert_eq!(
            span.parent_span_id,
            opentelemetry::trace::SpanId::INVALID,
            "a request with no inbound context has no parent",
        );
    }
}
