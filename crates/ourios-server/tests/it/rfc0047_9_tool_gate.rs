//! Scenario RFC0047.9 — the MCP tool gate, in-process against a fake
//! `OpenFGA` (no container): the tenant-wide bypass issues no `Check`, a
//! scoped principal needs an explicit `caller` grant per tool, and an
//! unanswerable graph fails the call closed. Complements the served-binary
//! arm in `rfc0047_visibility` (real container).
//! See `docs/rfcs/0047-rebac-resolver-and-graph-visibility.md` §5.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::extract::State;
use axum::routing::post;
use ourios_core::auth::openfga::{
    OpenFgaResolver, OpenFgaSpec, VisibilityObjectSpec, VisibilitySpec, build_openfga_config,
};
use ourios_core::auth::{TokenSpec, build_token_store};
use ourios_ingester::receiver::AuthResolver;
use ourios_parquet::PromotedAttributes;
use ourios_server::querier::router_with_mcp_promoted;

use crate::rfc0016_query_endpoint::SHARED_HUGE_WINDOW;
use crate::rfc0027_mcp::{mcp_tool_call, rpc_payload};

/// A fake graph: `wide` reads the tenant (`can_read_content`), `bot` is a
/// scoped reader with `caller` on `query_logs` only, `other` is scoped with
/// no tool grant. Counts `check` calls per relation.
#[derive(Clone)]
struct Fake {
    checks: Arc<AtomicUsize>,
    tool_checks: Arc<AtomicUsize>,
    down: bool,
}

async fn check(
    State(fake): State<Fake>,
    body: axum::body::Bytes,
) -> (axum::http::StatusCode, String) {
    fake.checks.fetch_add(1, Ordering::SeqCst);
    if fake.down {
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "down".to_string(),
        );
    }
    let request: serde_json::Value = serde_json::from_slice(&body).expect("json");
    let user = request["tuple_key"]["user"].as_str().unwrap_or_default();
    let relation = request["tuple_key"]["relation"]
        .as_str()
        .unwrap_or_default();
    let object = request["tuple_key"]["object"].as_str().unwrap_or_default();
    if relation == "can_call" {
        fake.tool_checks.fetch_add(1, Ordering::SeqCst);
    }
    let allowed = matches!(
        (user, relation, object),
        (
            "service_account:wide",
            "can_read_content" | "can_read_metadata",
            "tenant:acme"
        ) | ("service_account:bot", "can_call", "tool:acme/query_logs")
    );
    (
        axum::http::StatusCode::OK,
        serde_json::json!({ "allowed": allowed }).to_string(),
    )
}

async fn streamed(body: axum::body::Bytes) -> String {
    let request: serde_json::Value = serde_json::from_slice(&body).expect("json");
    let user = request["user"].as_str().unwrap_or_default();
    let relation = request["relation"].as_str().unwrap_or_default();
    let object_type = request["type"].as_str().unwrap_or_default();
    // Session binding: everyone may query acme; only wide may write.
    let objects: Vec<&str> = match (object_type, relation) {
        ("tenant", "can_query") => vec!["tenant:acme"],
        ("tenant", "can_write") if user == "service_account:wide" => vec!["tenant:acme"],
        ("conversation", "can_read_content") if user == "service_account:bot" => {
            vec!["conversation:acme/c-1"]
        }
        _ => vec![],
    };
    let mut lines = String::new();
    for object in objects {
        lines.push_str("{\"result\":{\"object\":\"");
        lines.push_str(object);
        lines.push_str("\"}}\n");
    }
    lines
}

async fn serve(fake: Fake) -> String {
    let app = Router::new()
        .route("/stores/{store}/check", post(check))
        .route("/stores/{store}/streamed-list-objects", post(streamed))
        .with_state(fake);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    url
}

fn router_for(url: &str, bucket: &std::path::Path) -> Router {
    let store = build_token_store(Some(&[
        TokenSpec {
            name: Some("wide".to_string()),
            token: Some("tok-wide".to_string()),
            tenants: vec!["*".to_string()],
        },
        TokenSpec {
            name: Some("bot".to_string()),
            token: Some("tok-bot".to_string()),
            tenants: vec!["*".to_string()],
        },
        TokenSpec {
            name: Some("other".to_string()),
            token: Some("tok-other".to_string()),
            tenants: vec!["*".to_string()],
        },
    ]))
    .expect("valid")
    .expect("enabled");
    let openfga = build_openfga_config(&OpenFgaSpec {
        api_url: Some(url.to_string()),
        store_id: Some("s".to_string()),
        request_timeout_secs: Some("1".to_string()),
        session_ttl_secs: Some("0".to_string()),
        visibility: VisibilitySpec {
            objects: vec![VisibilityObjectSpec {
                object_type: Some("conversation".to_string()),
                column: Some("attr.gen_ai.conversation.id".to_string()),
            }],
            ..VisibilitySpec::default()
        },
        ..OpenFgaSpec::default()
    })
    .expect("config");
    let auth = AuthResolver::static_only(Some(Arc::new(store)))
        .with_openfga(Arc::new(OpenFgaResolver::new(&openfga).expect("resolver")));
    router_with_mcp_promoted(
        bucket.to_path_buf(),
        SHARED_HUGE_WINDOW,
        auth,
        true,
        &PromotedAttributes::default(),
    )
}

/// RFC0047.9 (gate contract): a tenant-wide reader calls every tool and
/// the gate issues no `can_call` check; a scoped principal calls exactly
/// the tools it holds `caller` on and is denied — naming the tool — for
/// the rest; a scoped principal with no grants is denied everywhere.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0047_9_gate_allows_by_grant_and_bypasses_for_tenant_wide() {
    let fake = Fake {
        checks: Arc::new(AtomicUsize::new(0)),
        tool_checks: Arc::new(AtomicUsize::new(0)),
        down: false,
    };
    let tool_checks = Arc::clone(&fake.tool_checks);
    let url = serve(fake).await;
    let bucket = tempfile::TempDir::new().expect("temp");
    let router = router_for(&url, bucket.path());
    let query = serde_json::json!({"tenant": "acme", "query": "true", "limit": 10});
    let drift = serde_json::json!({"tenant": "acme", "from": "-1h", "to": "now"});
    let list = serde_json::json!({"tenant": "acme"});

    for (tool, args) in [
        ("query_logs", &query),
        ("list_templates", &list),
        ("template_drift", &drift),
    ] {
        let payload =
            rpc_payload(&mcp_tool_call(&router, Some("Bearer tok-wide"), tool, args.clone()).await);
        assert!(
            payload.get("error").is_none(),
            "wide calls {tool}: {payload}"
        );
    }
    assert_eq!(
        tool_checks.load(Ordering::SeqCst),
        0,
        "no can_call check for a tenant-wide reader"
    );

    let payload = rpc_payload(
        &mcp_tool_call(&router, Some("Bearer tok-bot"), "query_logs", query.clone()).await,
    );
    assert!(
        payload.get("error").is_none(),
        "bot holds caller on query_logs: {payload}"
    );
    assert_eq!(
        tool_checks.load(Ordering::SeqCst),
        1,
        "one can_call check per call"
    );
    for (tool, args) in [("list_templates", &list), ("template_drift", &drift)] {
        let payload =
            rpc_payload(&mcp_tool_call(&router, Some("Bearer tok-bot"), tool, args.clone()).await);
        let message = payload["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("permission denied") && message.contains(tool),
            "bot denied {tool}, naming it: {payload}"
        );
    }
    let payload =
        rpc_payload(&mcp_tool_call(&router, Some("Bearer tok-other"), "query_logs", query).await);
    assert!(
        payload["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("permission denied"),
        "no grant → denied: {payload}"
    );
}

/// RFC0047.9 (fail closed): when the graph cannot answer, no tool call
/// proceeds — the error is a retryable server error, never a silent allow.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0047_9_gate_fails_closed_when_the_graph_is_down() {
    let fake = Fake {
        checks: Arc::new(AtomicUsize::new(0)),
        tool_checks: Arc::new(AtomicUsize::new(0)),
        down: true,
    };
    let url = serve(fake).await;
    let bucket = tempfile::TempDir::new().expect("temp");
    let router = router_for(&url, bucket.path());
    // The session still binds (the streamed list answers), so the request
    // reaches the gate; the two-step's Check fails → 503-class, no call.
    let payload = rpc_payload(
        &mcp_tool_call(
            &router,
            Some("Bearer tok-bot"),
            "query_logs",
            serde_json::json!({"tenant": "acme", "query": "true", "limit": 10}),
        )
        .await,
    );
    let error = &payload["error"];
    assert!(
        !error.is_null(),
        "no result on an unanswerable graph: {payload}"
    );
    assert!(
        error["message"]
            .as_str()
            .unwrap_or_default()
            .contains("unavailable"),
        "named as unavailable, retryable: {payload}"
    );
}
