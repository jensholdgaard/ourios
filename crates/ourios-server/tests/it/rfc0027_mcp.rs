//! RFC 0027 §5 — the MCP query surface, all seven scenarios.
//!
//! `.1` is green (the transport slice); the remaining stubs are
//! `#[ignore]`d so the default run stays green while the RFC works
//! through its slices, each naming the slice that discharges it.

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt as _;

/// A minimal MCP `initialize` request (JSON-RPC over streamable HTTP).
fn initialize_request(bearer: Option<&str>) -> Request<Body> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "rfc0027-test", "version": "0"}
        }
    });
    let mut req = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json, text/event-stream")
        // rmcp validates Host (DNS-rebinding protection); a real client
        // always sends it, `oneshot` does not.
        .header(header::HOST, "127.0.0.1");
    if let Some(value) = bearer {
        req = req.header(header::AUTHORIZATION, value);
    }
    req.body(Body::from(body.to_string()))
        .expect("build request")
}

/// Scenario RFC0027.1 — gating and placement.
///
/// The no-new-crate half is structural (the adapter is
/// `ourios_server::mcp`, asserted by this crate compiling it); the
/// JSON-API-unchanged half is the RFC 0016/0026 suites in this same
/// harness, which run against auth-only routers.
/// See `docs/rfcs/0027-mcp-query-surface.md` §5.
#[tokio::test]
async fn rfc0027_1_gating_and_placement() {
    let bucket = tempfile::tempdir().expect("temp");

    // Flag off (the default constructors): /mcp does not exist.
    let off = ourios_server::querier::router(bucket.path().to_path_buf(), 3_600_000_000_000);
    let response = off
        .oneshot(initialize_request(None))
        .await
        .expect("oneshot");
    assert_eq!(response.status(), StatusCode::NOT_FOUND, "off ⇒ 404");

    // Flag on: /mcp speaks MCP — the initialize handshake answers with
    // the server info, on the same listener/router.
    let on = ourios_server::querier::router_with_mcp(
        bucket.path().to_path_buf(),
        3_600_000_000_000,
        None,
        true,
    );
    let response = on.oneshot(initialize_request(None)).await.expect("oneshot");
    assert_eq!(response.status(), StatusCode::OK, "on ⇒ MCP answers");
    let bytes = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("read body");
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.contains("serverInfo") && text.contains("protocolVersion"),
        "an MCP initialize result: {text}",
    );

    // The RFC 0026 gate applies at the transport (the full .2 matrix
    // lands with the tools slice): with auth enabled, no bearer ⇒ 401
    // before any MCP dispatch; a valid bearer ⇒ the handshake proceeds.
    let auth = std::sync::Arc::new(
        ourios_core::auth::build_token_store(Some(&[ourios_core::auth::TokenSpec {
            name: Some("cli".to_string()),
            token: Some("tok-mcp".to_string()),
            tenants: vec!["acme".to_string()],
        }]))
        .expect("valid")
        .expect("enabled"),
    );
    let gated = || {
        ourios_server::querier::router_with_mcp(
            bucket.path().to_path_buf(),
            3_600_000_000_000,
            Some(auth.clone()),
            true,
        )
    };
    let response = gated()
        .oneshot(initialize_request(None))
        .await
        .expect("oneshot");
    assert_eq!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "no bearer ⇒ 401"
    );
    let response = gated()
        .oneshot(initialize_request(Some("Bearer tok-mcp")))
        .await
        .expect("oneshot");
    assert_eq!(response.status(), StatusCode::OK, "valid bearer ⇒ served");
}

/// Scenario RFC0027.2 — the RFC 0026 gate applies verbatim.
/// See `docs/rfcs/0027-mcp-query-surface.md` §5.
#[test]
#[ignore = "RFC0027.2 stub — implemented in the transport green slice"]
fn rfc0027_2_rfc0026_gate_applies_verbatim() {
    todo!(
        "RFC0027.2 — missing/unknown bearer rejected before tool \
         dispatch; out-of-set tenant fails tenant-denied touching no \
         data; open mode serves MCP as it serves the JSON API"
    );
}

/// Scenario RFC0027.3 — `query_logs`.
/// See `docs/rfcs/0027-mcp-query-surface.md` §5.
#[test]
#[ignore = "RFC0027.3 stub — implemented in the tools green slice"]
fn rfc0027_3_query_logs() {
    todo!(
        "RFC0027.3 — count + limited rendered rows + pruning stats, \
         equal to the JSON API's answer for the same statement; DSL \
         errors surface as tool errors, never transport failures"
    );
}

/// Scenario RFC0027.4 — `list_templates`.
/// See `docs/rfcs/0027-mcp-query-surface.md` §5.
#[test]
#[ignore = "RFC0027.4 stub — implemented in the tools green slice"]
fn rfc0027_4_list_templates() {
    todo!(
        "RFC0027.4 — (template_id, rendered_template, version) rows \
         matching the RFC 0017 registry surface for the tenant"
    );
}

/// Scenario RFC0027.5 — `template_drift`.
/// See `docs/rfcs/0027-mcp-query-surface.md` §5.
#[test]
#[ignore = "RFC0027.5 stub — implemented in the tools green slice"]
fn rfc0027_5_template_drift() {
    todo!(
        "RFC0027.5 — the analysis over [from, to) equals the RFC 0010 \
         drift surface's for the same half-open window"
    );
}

/// Scenario RFC0027.6 — the grammar resource.
/// See `docs/rfcs/0027-mcp-query-surface.md` §5.
#[test]
#[ignore = "RFC0027.6 stub — implemented in the resource green slice"]
fn rfc0027_6_grammar_resource() {
    todo!(
        "RFC0027.6 — the served resource is byte-identical to the \
         RFC 0002 §7 grammar section of docs/rfcs/0002-query-dsl.md \
         (include_str!, trimmed at startup)"
    );
}

/// Scenario RFC0027.7 — output discipline.
/// See `docs/rfcs/0027-mcp-query-surface.md` §5.
#[test]
#[ignore = "RFC0027.7 stub — implemented in the tools green slice"]
fn rfc0027_7_output_discipline() {
    todo!(
        "RFC0027.7 — tool results are the RFC 0016 JSON shapes as MCP \
         content; every tool description carries the treat-log-bodies-\
         as-data warning; no tenant enumeration, no SQL"
    );
}
