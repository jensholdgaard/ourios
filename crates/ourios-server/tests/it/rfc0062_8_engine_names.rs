//! RFC 0062 — attribute paths into map values, the served surfaces (H6).
//! See `docs/rfcs/0062-attribute-map-paths.md` §5.

use axum::http::StatusCode;
use ourios_core::otlp::any_value::Value;
use ourios_core::otlp::{AnyValue, KeyValue, KeyValueList};
use ourios_core::record::MinedRecord;
use ourios_parquet::{
    DEFAULT_ZSTD_LEVEL, PartitionKey, PromotedAttributes, PromotedClass, PromotedKey, Store, Writer,
};

use crate::rfc0016_query_endpoint::{SHARED_HUGE_WINDOW, mined, post_for_equivalence};
use crate::rfc0027_mcp::{mcp_post, mcp_tool_call, rpc_payload};

/// Engine names a response must never carry: the internal UDF, the
/// prefilter operator, and `DataFusion` itself and its plan vocabulary.
fn assert_no_engine_name(context: &str, shown: &str) {
    assert!(!shown.contains("LIKE"), "{context} leaked LIKE: {shown}");
    let lower = shown.to_ascii_lowercase();
    for token in [
        "ourios_attr_cmp",
        "datafusion",
        "regexp_like",
        "logicalplan",
        "physical_plan",
        "recordbatch",
    ] {
        assert!(
            !lower.contains(token),
            "{context} leaked {token:?}: {shown}"
        );
    }
}

/// One `acme` record holding `user = {id: "u7"}`, written once without
/// promotion and once with `user.id` promoted as `i64`, so an integer
/// literal compiles to the integer JSON arm instead of being rejected.
fn seed(bucket: &std::path::Path) {
    let user = KeyValue {
        key: "user".to_string(),
        value: Some(AnyValue {
            value: Some(Value::KvlistValue(KeyValueList {
                values: vec![KeyValue {
                    key: "id".to_string(),
                    value: Some(AnyValue {
                        value: Some(Value::StringValue("u7".to_string())),
                    }),
                    ..Default::default()
                }],
            })),
        }),
        ..Default::default()
    };
    let record = MinedRecord {
        attributes: vec![user],
        ..mined("acme", 1)
    };
    let store = Store::local(bucket).expect("local store");
    let typed = PromotedAttributes::new_typed(
        [],
        [PromotedKey {
            key: "user.id".into(),
            class: PromotedClass::I64,
        }],
    );
    for promoted in [PromotedAttributes::default(), typed] {
        let part = PartitionKey::derive(&record).expect("derive partition");
        let mut w = Writer::open_in_with_promoted(&store, part, DEFAULT_ZSTD_LEVEL, promoted)
            .expect("open writer");
        w.append_records(std::slice::from_ref(&record))
            .expect("append");
        w.close().expect("close");
    }
}

/// Scenario RFC0062.8 — no engine name reaches the user (H6).
/// See `docs/rfcs/0062-attribute-map-paths.md` §5.
#[tokio::test]
async fn rfc0062_8_no_engine_name_reaches_the_user() {
    let bucket = tempfile::tempdir().expect("temp");
    seed(bucket.path());
    let router = ourios_server::querier::router_with_mcp(
        bucket.path().to_path_buf(),
        SHARED_HUGE_WINDOW,
        ourios_serving::AuthResolver::static_only(None),
        true,
    );

    // The path holds a string, so an integer literal matches nothing.
    for (query, rows) in [("attr.user.id == 7", 0), ("attr.user.id != 7", 0)] {
        let (status, json) = post_for_equivalence(bucket.path(), Some("acme"), query).await;
        assert_eq!(status, StatusCode::OK, "{query}: {json}");
        assert_eq!(json["rows"], rows, "{query}: {json}");
        assert_no_engine_name(query, &json.to_string());

        let body = mcp_tool_call(
            &router,
            None,
            "query_logs",
            serde_json::json!({"tenant": "acme", "query": query}),
        )
        .await;
        assert_no_engine_name(query, &body);
        let rpc = rpc_payload(&body);
        let text = rpc["result"]["content"][0]["text"]
            .as_str()
            .expect("text content");
        let payload: serde_json::Value = serde_json::from_str(text).expect("tool payload");
        assert_eq!(payload["rows"], rows, "MCP {query}: {payload}");
    }

    // A malformed path is a DSL error on both surfaces.
    let malformed = "attr.user..id == \"u7\"";
    let (status, json) = post_for_equivalence(bucket.path(), Some("acme"), malformed).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
    assert!(json["error"]["message"].is_string(), "{json}");
    assert_no_engine_name(malformed, &json.to_string());
    let body = mcp_tool_call(
        &router,
        None,
        "query_logs",
        serde_json::json!({"tenant": "acme", "query": malformed}),
    )
    .await;
    assert!(
        body.contains("invalid query"),
        "a tool-level DSL error: {body}"
    );
    assert_no_engine_name(malformed, &body);

    // The query-schema resource states the §3.1 rule.
    let schema = query_schema_text(&router).await;
    let doc: serde_json::Value = serde_json::from_str(&schema).expect("json");
    let rule = doc["attribute_keys"]["rule"].as_str().expect("rule text");
    for phrase in [
        "dots included",
        "path into map values",
        "longest leading key",
    ] {
        assert!(rule.contains(phrase), "the rule names {phrase:?}: {rule}");
    }
    assert_no_engine_name("query-schema", &schema);
}

async fn query_schema_text(router: &axum::Router) -> String {
    let init = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                   "clientInfo": {"name": "rfc0062-test", "version": "0"}}
    });
    let (_, _, session) = mcp_post(router.clone(), None, None, init).await;
    let session = session.expect("session id issued");
    let initialized = serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
    mcp_post(router.clone(), None, Some(&session), initialized).await;
    let read = serde_json::json!({
        "jsonrpc": "2.0", "id": 2, "method": "resources/read",
        "params": {"uri": "ourios://query-schema"}
    });
    let (status, body, _) = mcp_post(router.clone(), None, Some(&session), read).await;
    assert_eq!(status, StatusCode::OK, "resources/read");
    rpc_payload(&body)["result"]["contents"][0]["text"]
        .as_str()
        .expect("text contents")
        .to_string()
}
