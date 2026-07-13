//! RFC 0032 §5 — the query-schema / cost-model resource, all six
//! scenarios.
//!
//! All six are green. Placement note: the §5 map lives here, in the
//! RFC 0027 in-process MCP harness area (`rfc0027_mcp.rs`'s router +
//! JSON-RPC-over-`/mcp` shape, RFC 0032 §6), matching the RFC 0033
//! precedent of one file per RFC for §5 traceability. The anti-drift
//! substance of `.3`/`.4` is unit tests beside the resource builder in
//! `mcp.rs` (RFC 0032 §6 — asserted against `SeverityName::floor`/`ceil`
//! and the bloom set harvested from a real writer footer); the `.3`/`.4`
//! arms here assert the same scenarios through the served surface.

use std::collections::BTreeSet;

use axum::Router;
use axum::http::StatusCode;
use ourios_parquet::promoted::{ATTR_PREFIX, RESOURCE_PREFIX};
use ourios_parquet::{PromotedAttributes, SERVICE_NAME_KEY, columns};
use ourios_querier::dsl::ir::SeverityName;

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

/// Scenario RFC0032.3 — severity scale correctness, through the served
/// surface: each band in the served body equals the DSL compiler's own
/// `SeverityName::floor`/`ceil` — the functions, never repeated
/// literals. (The unit test beside the builder in `mcp.rs` is the
/// primary §6 mapping; this arm pins the same contract end to end.)
/// See `docs/rfcs/0032-query-schema-cost-model-resource.md` §5.
#[tokio::test]
async fn rfc0032_3_severity_scale_correctness() {
    let bucket = tempfile::tempdir().expect("temp");
    let router = router_with_promoted(bucket.path(), &PromotedAttributes::default());
    let doc = read_query_schema(&router).await;
    let names = doc["severity"]["names"].as_array().expect("severity.names");
    let expected = [
        ("trace", SeverityName::Trace),
        ("debug", SeverityName::Debug),
        ("info", SeverityName::Info),
        ("warn", SeverityName::Warn),
        ("error", SeverityName::Error),
        ("fatal", SeverityName::Fatal),
    ];
    assert_eq!(names.len(), expected.len(), "the six names: {doc}");
    for (name, level) in expected {
        let entry = names
            .iter()
            .find(|e| e["name"] == name)
            .unwrap_or_else(|| panic!("{name} present: {doc}"));
        assert_eq!(entry["floor"], level.floor(), "{name} floor");
        assert_eq!(entry["ceil"], level.ceil(), "{name} ceil");
    }
}

/// Scenario RFC0032.4 — cost-tier classification stability, through the
/// served surface: the bloom-mechanism kinds, placeholders expanded over
/// the document's own promoted section, cover exactly the DSL fields
/// backed by the §3.2 bloom set derived from `PromotedAttributes::
/// column_names` and the writer's column constants; severity says
/// statistics, never bloom; structure, never numbers. (The unit test
/// beside the builder harvests the same set from a real writer footer —
/// the two derivations pin each other.)
/// See `docs/rfcs/0032-query-schema-cost-model-resource.md` §5.
#[tokio::test]
async fn rfc0032_4_cost_tier_classification_stability() {
    let promoted = PromotedAttributes::new(
        ["k8s.namespace.name".to_string()],
        ["http.route".to_string()],
    );
    let bucket = tempfile::tempdir().expect("temp");
    let router = router_with_promoted(bucket.path(), &promoted);
    let doc = read_query_schema(&router).await;

    // The bloom-backed column set for this deployment, derived from the
    // configured value and the writer's column constants.
    let derived: BTreeSet<String> = [columns::TEMPLATE_ID, columns::TRACE_ID, columns::SPAN_ID]
        .into_iter()
        .map(str::to_string)
        .chain(promoted.column_names())
        .collect();

    // Expand the served document's bloom-mechanism kinds over its own
    // promoted_attributes section, mapping each DSL field onto its
    // storage column.
    let classification = doc["cost_model"]["classification"]
        .as_array()
        .expect("classification");
    let keys = |section: &str| -> Vec<String> {
        doc["promoted_attributes"][section]
            .as_array()
            .expect("promoted key array")
            .iter()
            .map(|k| k.as_str().expect("key").to_string())
            .collect()
    };
    let mut backed = BTreeSet::new();
    for entry in classification {
        if entry["mechanism"] != "bloom" {
            continue;
        }
        assert_eq!(
            entry["tier"], "index_backed",
            "bloom implies index-backed: {entry}",
        );
        for field in entry["fields"].as_array().expect("fields") {
            match field.as_str().expect("field") {
                "template_id" => {
                    backed.insert(columns::TEMPLATE_ID.to_string());
                }
                "trace_id" => {
                    backed.insert(columns::TRACE_ID.to_string());
                }
                "span_id" => {
                    backed.insert(columns::SPAN_ID.to_string());
                }
                "service" => {
                    backed.insert(format!("{RESOURCE_PREFIX}{SERVICE_NAME_KEY}"));
                }
                "resource.<promoted key>" => backed.extend(
                    keys("resource")
                        .into_iter()
                        .map(|k| format!("{RESOURCE_PREFIX}{k}")),
                ),
                "attr.<promoted key>" => {
                    backed.extend(keys("log").into_iter().map(|k| format!("{ATTR_PREFIX}{k}")));
                }
                other => panic!("a bloom entry names an unknown field: {other}"),
            }
        }
    }
    assert_eq!(
        backed, derived,
        "index-backed equality covers exactly the bloom-backed fields: {doc}",
    );

    // Severity prunes through min/max statistics, never bloom.
    let severity = classification
        .iter()
        .find(|e| {
            e["fields"]
                .as_array()
                .is_some_and(|f| f.iter().any(|v| v == "severity"))
        })
        .expect("a severity classification entry");
    assert_eq!(severity["mechanism"], "statistics", "{severity}");

    // Structure, never numbers: the only numeric leaves in the whole
    // document are the format_version and the severity bands.
    let mut numeric = Vec::new();
    collect_number_paths(&doc, "", &mut numeric);
    for path in numeric {
        let in_band = path
            .strip_suffix(".floor")
            .or_else(|| path.strip_suffix(".ceil"))
            .is_some_and(|entry| entry.starts_with("severity.names["));
        assert!(
            path == "format_version" || in_band,
            "numeric leaf outside the severity scale: {path}",
        );
    }
}

/// Record the dotted path of every JSON number in `value`.
fn collect_number_paths(value: &serde_json::Value, path: &str, out: &mut Vec<String>) {
    match value {
        serde_json::Value::Number(_) => out.push(path.to_string()),
        serde_json::Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                collect_number_paths(item, &format!("{path}[{i}]"), out);
            }
        }
        serde_json::Value::Object(map) => {
            for (key, item) in map {
                let child = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                collect_number_paths(item, &child, out);
            }
        }
        _ => {}
    }
}

/// Scenario RFC0032.5 — tool-description placement: each of the three
/// RFC 0027 tools carries exactly one advisory sentence naming
/// `ourios://query-schema`, and no description enumerates tiers,
/// severity bands, or promoted keys — the full tiering lives only in
/// the machine-readable resource (§3.3, the #465 placement rule).
/// See `docs/rfcs/0032-query-schema-cost-model-resource.md` §5.
#[tokio::test]
async fn rfc0032_5_tool_description_placement() {
    let bucket = tempfile::tempdir().expect("temp");
    let router = ourios_server::querier::router_with_mcp(
        bucket.path().to_path_buf(),
        3_600_000_000_000,
        ourios_ingester::receiver::AuthResolver::static_only(None),
        true,
    );
    let session = mcp_handshake(&router).await;
    let list = serde_json::json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}
    });
    let (status, body, _) = mcp_post(router.clone(), None, Some(&session), list).await;
    assert_eq!(status, StatusCode::OK);
    let rpc = rpc_payload(&body);
    let tools = rpc["result"]["tools"].as_array().expect("tools array");
    let mut names: Vec<&str> = tools
        .iter()
        .map(|t| t["name"].as_str().expect("name"))
        .collect();
    names.sort_unstable();
    assert_eq!(names, ["list_templates", "query_logs", "template_drift"]);
    for tool in tools {
        // Doc-comment sourced descriptions carry line breaks; normalize
        // before the phrase checks.
        let description = tool["description"]
            .as_str()
            .expect("description")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(
            description.matches(QUERY_SCHEMA_URI).count(),
            1,
            "{}: exactly one advisory sentence names the resource: {description}",
            tool["name"],
        );
        for needle in ["index_backed", "pruned", "scan"] {
            assert!(
                !description.contains(needle),
                "{}: tier vocabulary stays out of descriptions ({needle}): {description}",
                tool["name"],
            );
        }
        assert!(
            !description.chars().any(|c| c.is_ascii_digit()),
            "{}: no severity bands in descriptions: {description}",
            tool["name"],
        );
        assert!(
            !description.contains(SERVICE_NAME_KEY),
            "{}: no promoted keys in descriptions: {description}",
            tool["name"],
        );
    }
}

/// Scenario RFC0032.6 — read-only contract preserved. The suite half is
/// the RFC 0027 §5 tests running unmodified in this same binary
/// (`rfc0027_mcp.rs` — same tools, same outputs, grammar byte-identity
/// and mime assertions intact); this arm pins the remaining clauses:
/// the resource body derives from configuration alone (a populated and
/// an empty store serve identical bytes — reading it performs no query
/// and touches no tenant data), and an unknown URI still errors.
/// See `docs/rfcs/0032-query-schema-cost-model-resource.md` §5.
#[tokio::test]
async fn rfc0032_6_read_only_contract_preserved() {
    let promoted = PromotedAttributes::new(
        ["k8s.namespace.name".to_string()],
        std::iter::empty::<String>(),
    );

    // A store holding ingested tenant data and a template-audit stream…
    let populated = tempfile::tempdir().expect("temp");
    crate::rfc0016_query_endpoint::seed_two_records(populated.path());
    crate::rfc0016_query_endpoint::seed_template_audit(populated.path(), "acme");
    let populated_router = router_with_promoted(populated.path(), &promoted);
    let populated_doc = read_query_schema(&populated_router).await;

    // …serves exactly what the same configuration over an empty store
    // serves: no ingested-telemetry-derived content, no tenant data.
    let empty = tempfile::tempdir().expect("temp");
    let empty_router = router_with_promoted(empty.path(), &promoted);
    assert_eq!(
        populated_doc,
        read_query_schema(&empty_router).await,
        "the body is independent of ingested telemetry",
    );
    assert!(
        !populated_doc.to_string().contains("acme"),
        "no tenant identifier leaks: {populated_doc}",
    );

    // An unknown resource URI still returns the resource-not-found
    // error (JSON-RPC -32002), never some other resource's content.
    let session = mcp_handshake(&populated_router).await;
    let read = serde_json::json!({
        "jsonrpc": "2.0", "id": 3, "method": "resources/read",
        "params": {"uri": "ourios://no-such-resource"}
    });
    let (_, body, _) = mcp_post(populated_router.clone(), None, Some(&session), read).await;
    let rpc = rpc_payload(&body);
    assert_eq!(rpc["error"]["code"], -32002, "resource-not-found: {rpc}");
    assert!(
        rpc["error"]["message"]
            .as_str()
            .expect("message")
            .contains("unknown resource"),
        "{rpc}",
    );
}
