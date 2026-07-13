//! RFC 0032 §5 — the query-schema / cost-model resource, all six
//! scenarios.
//!
//! `.1`/`.2` are green (the resource + config-threading slice); the
//! remaining stubs are `#[ignore]`d, each naming the green slice that
//! discharges it.
//!
//! Placement note: all six scenarios live here, in the RFC 0027
//! in-process MCP harness area (`rfc0027_mcp.rs`'s router +
//! JSON-RPC-over-`/mcp` shape, RFC 0032 §6), matching the RFC 0033 red
//! precedent of one file per RFC for §5→stub traceability. The
//! `.3`/`.4` green work is unit tests beside the resource builder in
//! `mcp.rs` (RFC 0032 §6); their stubs stay here so the §5 map is one
//! file.

use axum::Router;
use axum::http::StatusCode;
use ourios_parquet::PromotedAttributes;

use crate::rfc0027_mcp::{mcp_post, rpc_payload};

const QUERY_SCHEMA_URI: &str = "ourios://query-schema";

/// Drive the MCP handshake on `router` and return the session id.
async fn mcp_handshake(router: &Router) -> String {
    let init = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                   "clientInfo": {"name": "rfc0032-test", "version": "0"}}
    });
    let (status, _, session) = mcp_post(router.clone(), None, None, init).await;
    assert_eq!(status, StatusCode::OK, "initialize");
    let session = session.expect("session id issued");
    let initialized = serde_json::json!({
        "jsonrpc": "2.0", "method": "notifications/initialized"
    });
    let (status, _, _) = mcp_post(router.clone(), None, Some(&session), initialized).await;
    assert!(status.is_success(), "initialized notification: {status}");
    session
}

/// Read `ourios://query-schema` through the full protocol dance and
/// parse its text contents as JSON.
async fn read_query_schema(router: &Router) -> serde_json::Value {
    let session = mcp_handshake(router).await;
    let read = serde_json::json!({
        "jsonrpc": "2.0", "id": 2, "method": "resources/read",
        "params": {"uri": QUERY_SCHEMA_URI}
    });
    let (status, body, _) = mcp_post(router.clone(), None, Some(&session), read).await;
    assert_eq!(status, StatusCode::OK, "resources/read");
    let rpc = rpc_payload(&body);
    let text = rpc["result"]["contents"][0]["text"]
        .as_str()
        .expect("text contents");
    serde_json::from_str(text).expect("the resource body is JSON")
}

/// An open-mode MCP router over an empty store, with `promoted` as the
/// deployment's effective set.
fn router_with_promoted(bucket: &std::path::Path, promoted: &PromotedAttributes) -> Router {
    ourios_server::querier::router_with_mcp_promoted(
        bucket.to_path_buf(),
        3_600_000_000_000,
        ourios_ingester::receiver::AuthResolver::static_only(None),
        true,
        promoted,
    )
}

/// Scenario RFC0032.1 — listed and readable.
/// See `docs/rfcs/0032-query-schema-cost-model-resource.md` §5.
#[tokio::test]
async fn rfc0032_1_listed_and_readable() {
    let bucket = tempfile::tempdir().expect("temp");
    let router = ourios_server::querier::router_with_mcp(
        bucket.path().to_path_buf(),
        3_600_000_000_000,
        ourios_ingester::receiver::AuthResolver::static_only(None),
        true,
    );
    let session = mcp_handshake(&router).await;

    // resources/list advertises exactly two — the RFC 0027 grammar
    // resource and the query schema, as JSON.
    let list = serde_json::json!({
        "jsonrpc": "2.0", "id": 2, "method": "resources/list", "params": {}
    });
    let (status, body, _) = mcp_post(router.clone(), None, Some(&session), list).await;
    assert_eq!(status, StatusCode::OK);
    let rpc = rpc_payload(&body);
    let resources = rpc["result"]["resources"].as_array().expect("resources");
    assert_eq!(resources.len(), 2, "exactly two resources: {rpc}");
    let mut uris: Vec<&str> = resources
        .iter()
        .map(|r| r["uri"].as_str().expect("uri"))
        .collect();
    uris.sort_unstable();
    assert_eq!(uris, ["ourios://dsl-grammar", QUERY_SCHEMA_URI]);
    let schema = resources
        .iter()
        .find(|r| r["uri"] == QUERY_SCHEMA_URI)
        .expect("query schema advertised");
    assert_eq!(
        schema["mimeType"], "application/json",
        "advertised as JSON: {schema}",
    );

    // resources/read parses as JSON with format_version 1 and the §3.2
    // top-level keys.
    let doc = read_query_schema(&router).await;
    assert_eq!(doc["format_version"], 1, "{doc}");
    for key in ["fields", "severity", "promoted_attributes", "cost_model"] {
        assert!(!doc[key].is_null(), "carries the §3.2 key {key}: {doc}");
    }

    // tools/list still advertises exactly the RFC 0027 §3.2 three.
    let list = serde_json::json!({
        "jsonrpc": "2.0", "id": 3, "method": "tools/list", "params": {}
    });
    let (status, body, _) = mcp_post(router.clone(), None, Some(&session), list).await;
    assert_eq!(status, StatusCode::OK);
    let rpc = rpc_payload(&body);
    let mut names: Vec<&str> = rpc["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| t["name"].as_str().expect("name"))
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        ["list_templates", "query_logs", "template_drift"],
        "no new tool",
    );
}

/// Scenario RFC0032.2 — content matches the running config.
/// See `docs/rfcs/0032-query-schema-cost-model-resource.md` §5.
#[tokio::test]
async fn rfc0032_2_content_matches_running_config() {
    let bucket = tempfile::tempdir().expect("temp");
    // Configured resource and log keys, with duplicates and an explicit
    // `service.name`, so the effective-set rules (dedup preserving
    // order, implicit `service.name` first) are all exercised.
    let promoted = PromotedAttributes::new(
        [
            "k8s.namespace.name".to_string(),
            "service.name".to_string(),
            "k8s.namespace.name".to_string(),
        ],
        ["http.route".to_string(), "http.route".to_string()],
    );
    let configured = router_with_promoted(bucket.path(), &promoted);
    let doc = read_query_schema(&configured).await;
    assert_eq!(
        doc["promoted_attributes"]["resource"],
        serde_json::json!(promoted.resource_keys()),
        "the effective resource keys, verbatim: {doc}",
    );
    assert_eq!(
        doc["promoted_attributes"]["log"],
        serde_json::json!(promoted.log_keys()),
        "the effective log keys, verbatim: {doc}",
    );
    assert_eq!(
        doc["promoted_attributes"]["resource"],
        serde_json::json!(["service.name", "k8s.namespace.name"]),
        "service.name first, configured keys deduplicated in order",
    );
    assert_eq!(
        doc["promoted_attributes"]["log"],
        serde_json::json!(["http.route"]),
    );

    // With the section omitted (the default set): service.name only.
    let defaulted = router_with_promoted(bucket.path(), &PromotedAttributes::default());
    let default_doc = read_query_schema(&defaulted).await;
    assert_eq!(
        default_doc["promoted_attributes"]["resource"],
        serde_json::json!(["service.name"]),
    );
    assert!(
        default_doc["promoted_attributes"]["log"]
            .as_array()
            .expect("log array")
            .is_empty(),
        "{default_doc}",
    );

    // Two servers with different promoted sets serve different resource
    // bodies — the per-deployment property.
    assert_ne!(doc, default_doc);
}

/// Scenario RFC0032.3 — severity scale correctness.
/// See `docs/rfcs/0032-query-schema-cost-model-resource.md` §5.
#[test]
#[ignore = "RFC0032.3 stub — implemented in the document-correctness green slice (unit tests beside the mcp.rs builder)"]
fn rfc0032_3_severity_scale_correctness() {
    todo!(
        "RFC0032.3 — the severity.names entries equal the DSL's \
         SeverityName mapping: for each of the six names, floor \
         equals SeverityName::floor and ceil equals \
         SeverityName::ceil, asserted against the ourios-querier \
         functions, not repeated literals, so the resource cannot \
         drift from the compiler"
    );
}

/// Scenario RFC0032.4 — cost-tier classification stability.
/// See `docs/rfcs/0032-query-schema-cost-model-resource.md` §5.
#[test]
#[ignore = "RFC0032.4 stub — implemented in the document-correctness green slice (unit tests beside the mcp.rs builder)"]
fn rfc0032_4_cost_tier_classification_stability() {
    todo!(
        "RFC0032.4 — every cost_model.classification entry with \
         mechanism \"bloom\" names only columns the writer actually \
         bloom-filters: the expected set derives from the writer's \
         properties for the configured PromotedAttributes \
         (template_id, trace_id, span_id, and every \
         PromotedAttributes::column_names column) and the resource's \
         index-backed equality kinds cover exactly the DSL fields \
         backed by that set; severity's entry carries mechanism \
         \"statistics\", never \"bloom\"; no classification entry \
         carries a numeric cost value"
    );
}

/// Scenario RFC0032.5 — tool-description placement.
/// See `docs/rfcs/0032-query-schema-cost-model-resource.md` §5.
#[test]
#[ignore = "RFC0032.5 stub — implemented in the tool-descriptions + read-only green slice"]
fn rfc0032_5_tool_description_placement() {
    todo!(
        "RFC0032.5 — tools/list: each of query_logs, list_templates, \
         and template_drift carries exactly one advisory sentence \
         naming ourios://query-schema, and no tool description \
         enumerates tiers, severity bands, or promoted keys (the \
         full tiering lives only in the resource)"
    );
}

/// Scenario RFC0032.6 — read-only contract preserved.
/// See `docs/rfcs/0032-query-schema-cost-model-resource.md` §5.
#[test]
#[ignore = "RFC0032.6 stub — implemented in the tool-descriptions + read-only green slice"]
fn rfc0032_6_read_only_contract_preserved() {
    todo!(
        "RFC0032.6 — with the amendment applied, the RFC 0027 §5 \
         suite passes verbatim (same tools, same outputs, grammar \
         resource byte-identical); reading ourios://query-schema \
         performs no query, touches no tenant data, and its body \
         contains no ingested-telemetry-derived content; an unknown \
         resource URI still returns the resource-not-found error"
    );
}
