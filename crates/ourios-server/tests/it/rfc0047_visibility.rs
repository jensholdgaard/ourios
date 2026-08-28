//! RFC 0047 §3.4 — layer-2 visibility on the served binary against a
//! **real `OpenFGA` container** (testcontainers; CI-gated like the layer-1
//! test — `#[ignore]`d in the default run), over Parquet pre-written with
//! the promoted `gen_ai.conversation.id` / `user.hash` / `cost_usd`
//! columns.
//!
//! Scenarios RFC0047.4 (tenant-wide reader), .5 (participant + self fast
//! path), .6 (agent principal + revocable delegation), .7 (bounded
//! enumeration, per tenant), .8 (metadata without content), .9 (the MCP
//! tool gate).
//! See `docs/rfcs/0047-rebac-resolver-and-graph-visibility.md` §5.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use ourios_core::auth::openfga::{OpenFgaSpec, build_openfga_config};
use ourios_core::otlp::any_value::Value as AvValue;
use ourios_core::otlp::{AnyValue, KeyValue};
use ourios_core::record::{BodyKind, MinedRecord, Param};
use ourios_core::tenant::TenantId;
use ourios_parquet::{
    DEFAULT_ZSTD_LEVEL, PartitionKey, PromotedAttributes, PromotedClass, PromotedKey, Store, Writer,
};
use ourios_serving::openfga::OpenFgaClient;
use testcontainers_modules::testcontainers::core::ContainerPort;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{GenericImage, ImageExt};
use tokio::time::timeout;

use crate::rfc0029_oidc::claim_binding::spawn_with_auth_and_storage;
use crate::rfc0029_oidc::ingest_binding::{make_key, serve_issuer};
use crate::rfc0047_openfga::{OPENFGA_IMAGE, OPENFGA_TAG, mint, provision, tuple};

fn kv(key: &str, value: &str) -> KeyValue {
    KeyValue {
        key: key.to_string(),
        value: Some(AnyValue {
            value: Some(AvValue::StringValue(value.to_string())),
        }),
        ..Default::default()
    }
}

fn kv_double(key: &str, value: f64) -> KeyValue {
    KeyValue {
        key: key.to_string(),
        value: Some(AnyValue {
            value: Some(AvValue::DoubleValue(value)),
        }),
        ..Default::default()
    }
}

fn recent_ns(offset: u64) -> u64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("epoch")
        .as_nanos();
    u64::try_from(now).expect("fits") - 60_000_000_000 + offset * 1_000_000
}

/// One `acme` row in `conversation` with the given `user.hash`, a content
/// attribute, a model and a $1.50 cost, timestamped a minute ago.
pub(crate) fn row(i: u64, conversation: &str, user: &str) -> MinedRecord {
    row_at(recent_ns(i), conversation, user)
}

/// [`row`] at an explicit timestamp.
pub(crate) fn row_at(time_unix_nano: u64, conversation: &str, user: &str) -> MinedRecord {
    MinedRecord {
        tenant_id: TenantId::new("acme"),
        template_id: 1,
        template_version: 1,
        severity_number: 9,
        severity_text: Some("INFO".to_string()),
        scope_name: Some("agent".to_string()),
        scope_version: None,
        scope_attributes: Vec::new(),
        resource_schema_url: None,
        scope_schema_url: None,
        time_unix_nano,
        observed_time_unix_nano: None,
        attributes: vec![
            kv("gen_ai.conversation.id", conversation),
            kv("user.hash", user),
            kv("gen_ai.input.messages", "the secret prompt"),
            kv("model", "gpt"),
            kv_double("cost_usd", 1.5),
        ],
        dropped_attributes_count: 0,
        resource_attributes: vec![kv("service.name", "agent")],
        trace_id: None,
        span_id: None,
        flags: 0,
        event_name: None,
        body_kind: BodyKind::String,
        params: vec![Param {
            type_tag: ourios_core::audit::ParamType::Num,
            value: "42".to_string(),
        }],
        separators: vec![String::new(), " ".to_string()],
        body: None,
        confidence: 1.0,
        lossy_flag: false,
    }
}

pub(crate) fn promoted() -> PromotedAttributes {
    PromotedAttributes::new_typed(
        [],
        [
            PromotedKey::string("gen_ai.conversation.id".to_string()),
            PromotedKey::string("user.hash".to_string()),
            PromotedKey::string("model".to_string()),
            PromotedKey {
                key: "cost_usd".into(),
                class: PromotedClass::F64,
            },
        ],
    )
}

pub(crate) fn write_records(bucket: &Path, recs: &[MinedRecord]) {
    let store = Store::local(bucket).expect("local store");
    let mut by_part: HashMap<PartitionKey, Vec<MinedRecord>> = HashMap::new();
    for r in recs {
        by_part
            .entry(PartitionKey::derive(r).expect("derive partition"))
            .or_default()
            .push(r.clone());
    }
    for (part, rs) in by_part {
        let mut w = Writer::open_in_with_promoted(&store, part, DEFAULT_ZSTD_LEVEL, promoted())
            .expect("open writer");
        w.append_records(&rs).expect("append");
        w.close().expect("close");
    }
}

/// `POST /v1/query` with a bearer and tenant; returns (status, JSON body).
pub(crate) async fn query(
    http: &reqwest::Client,
    addr: std::net::SocketAddr,
    bearer: &str,
    tenant: &str,
    dsl: &str,
) -> (u16, serde_json::Value) {
    let response = http
        .post(format!("http://{addr}/v1/query"))
        .bearer_auth(bearer)
        .header("x-ourios-tenant", tenant)
        .header("content-type", "text/plain")
        .body(dsl.to_string())
        .send()
        .await
        .expect("query");
    let status = response.status().as_u16();
    let body = response.text().await.expect("body");
    (
        status,
        serde_json::from_str(&body).unwrap_or(serde_json::Value::String(body)),
    )
}

/// One MCP `tools/call` over HTTP against the served `/mcp` (initialize →
/// initialized → call); returns the JSON-RPC payload.
async fn mcp_call(
    http: &reqwest::Client,
    addr: std::net::SocketAddr,
    bearer: &str,
    tool: &str,
    arguments: serde_json::Value,
) -> serde_json::Value {
    let url = format!("http://{addr}/mcp");
    let post = |session: Option<String>, body: serde_json::Value| {
        let mut request = http
            .post(&url)
            .bearer_auth(bearer)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .json(&body);
        if let Some(session) = session {
            request = request.header("mcp-session-id", session);
        }
        request.send()
    };
    let init = post(
        None,
        serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                       "clientInfo": {"name": "rfc0047-test", "version": "0"}}
        }),
    )
    .await
    .expect("initialize");
    assert_eq!(init.status().as_u16(), 200, "initialize");
    let session = init
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .expect("session id")
        .to_string();
    let initialized = post(
        Some(session.clone()),
        serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    )
    .await
    .expect("initialized");
    assert!(
        initialized.status().is_success(),
        "initialized notification: {}",
        initialized.status()
    );
    let call = post(
        Some(session),
        serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": tool, "arguments": arguments}
        }),
    )
    .await
    .expect("tools/call");
    assert_eq!(call.status().as_u16(), 200, "tools/call {tool}");
    let body = call.text().await.expect("body");
    let payload = body
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .find(|payload| !payload.trim().is_empty())
        .unwrap_or(&body);
    serde_json::from_str(payload.trim()).expect("json-rpc payload")
}

/// The sorted conversation ids of a row response.
pub(crate) fn conversations(body: &serde_json::Value) -> Vec<String> {
    let mut ids: Vec<String> = body["records"]
        .as_array()
        .expect("records")
        .iter()
        .map(|record| {
            record["attributes"]
                .as_array()
                .expect("attributes")
                .iter()
                .find(|kv| kv["key"] == "gen_ai.conversation.id")
                .and_then(|kv| kv["value"]["stringValue"].as_str())
                .expect("conversation id")
                .to_string()
        })
        .collect();
    ids.sort();
    ids
}

/// Scenarios RFC0047.4–.9 on the served binary (one container, one
/// server, one seeded store).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)] // one container + one server, every arm in sequence
#[ignore = "RFC0047.4–.9 — needs Docker (real OpenFGA container); run by the openfga-resolver CI job via --ignored"]
async fn rfc0047_4_to_9_visibility_end_to_end() {
    // --- OpenFGA -----------------------------------------------------------
    let container = GenericImage::new(OPENFGA_IMAGE, OPENFGA_TAG)
        .with_exposed_port(ContainerPort::Tcp(8080))
        .with_cmd(["run"])
        .start()
        .await
        .expect("openfga started");
    let port = container
        .get_host_port_ipv4(8080)
        .await
        .expect("mapped port");
    let api_url = format!("http://127.0.0.1:{port}");
    let http = reqwest::Client::new();
    timeout(Duration::from_secs(60), async {
        loop {
            if let Ok(response) = http.get(format!("{api_url}/healthz")).send().await
                && response.status().is_success()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .expect("openfga healthy before timeout");
    let (store_id, model_id) = provision(&api_url).await;
    let fga = OpenFgaClient::new(
        &build_openfga_config(&OpenFgaSpec {
            api_url: Some(api_url.clone()),
            store_id: Some(store_id.clone()),
            authorization_model_id: Some(model_id.clone()),
            ..OpenFgaSpec::default()
        })
        .expect("config"),
    )
    .expect("client");
    let conv = |id: &str| format!("conversation:acme/{id}");
    let mut tuples = vec![
        tuple("user:alice", "reader", "tenant:acme"),
        tuple("user:fin", "metadata_reader", "tenant:acme"),
        tuple("user:mallory", "reader", "tenant:globex"),
        // bob: participant of c-1? No — bob is participant of c-2 in acme
        // and of globex/c-1 (another tenant's id that collides with acme's).
        tuple("user:bob", "participant", &conv("c-2")),
        tuple("user:bob", "scoped_reader", "tenant:acme"),
        tuple("user:bob", "participant", "conversation:globex/c-1"),
        tuple("user:bob", "scoped_reader", "tenant:globex"),
        // bot: actor on c-3, c-4; delegate on alice's c-7.
        tuple("agent:bot", "actor", &conv("c-3")),
        tuple("agent:bot", "actor", &conv("c-4")),
        tuple("agent:bot", "delegate", &conv("c-7")),
        tuple("agent:bot", "scoped_reader", "tenant:acme"),
        // other: actor on c-5.
        tuple("agent:other", "actor", &conv("c-5")),
        tuple("agent:other", "scoped_reader", "tenant:acme"),
        // big: 4 acme conversations under a bound of 3 → refused.
        tuple("agent:big", "scoped_reader", "tenant:acme"),
        // mixed: 2 acme + 4 globex conversations → acme succeeds with 2.
        tuple("agent:mixed", "scoped_reader", "tenant:acme"),
    ];
    for id in ["c-11", "c-12", "c-13", "c-14"] {
        tuples.push(tuple("agent:big", "actor", &conv(id)));
    }
    for id in ["c-15", "c-16"] {
        tuples.push(tuple("agent:mixed", "actor", &conv(id)));
    }
    for id in ["g-1", "g-2", "g-3", "g-4"] {
        tuples.push(tuple(
            "agent:mixed",
            "actor",
            &format!("conversation:globex/{id}"),
        ));
    }
    for id in [
        "c-1", "c-2", "c-3", "c-4", "c-5", "c-7", "c-9", "c-11", "c-12", "c-13", "c-14", "c-15",
        "c-16",
    ] {
        tuples.push(tuple("tenant:acme", "parent", &conv(id)));
    }
    // RFC0047.9: the MCP tools as objects; bot may call query_logs only.
    for tool in ["query_logs", "list_templates", "template_drift"] {
        tuples.push(tuple("tenant:acme", "parent", &format!("tool:acme/{tool}")));
    }
    tuples.push(tuple("agent:bot", "caller", "tool:acme/query_logs"));
    for chunk in tuples.chunks(100) {
        fga.write(chunk, &[]).await.expect("seed tuples");
    }

    // --- Parquet + server --------------------------------------------------
    let tmp = tempfile::TempDir::new().expect("temp");
    let recs = vec![
        row(1, "c-1", "alice"),
        row(2, "c-1", "alice"),
        row(3, "c-2", "bob"),
        row(4, "c-3", "bot"),
        row(5, "c-4", "bot"),
        row(6, "c-5", "other"),
        row(7, "c-7", "alice"),
        // c-9: rows carry user.hash = bob but no tuple yet (self fast path).
        row(8, "c-9", "bob"),
        row(9, "c-11", "big"),
        row(10, "c-12", "big"),
        row(11, "c-13", "big"),
        row(12, "c-14", "big"),
        row(13, "c-15", "mixed"),
        row(14, "c-16", "mixed"),
    ];
    write_records(tmp.path(), &recs);
    let total = recs.len();

    let (encoding, jwk) = make_key("key-1");
    let issuer = serve_issuer(jwk).await;
    let storage_yaml = "  promoted_attributes:\n    log: [gen_ai.conversation.id, user.hash, model, {key: cost_usd, type: f64}]\n";
    let auth_yaml = format!(
        "auth:\n\
         \x20\x20oidc:\n\
         \x20\x20\x20\x20issuer: {issuer}\n\
         \x20\x20\x20\x20audience: ourios\n\
         \x20\x20\x20\x20agent_claim: ourios_principal_type=agent\n\
         \x20\x20openfga:\n\
         \x20\x20\x20\x20api_url: {api_url}\n\
         \x20\x20\x20\x20store_id: {store_id}\n\
         \x20\x20\x20\x20authorization_model_id: {model_id}\n\
         \x20\x20\x20\x20session_ttl_secs: 1\n\
         \x20\x20\x20\x20request_timeout_secs: 2\n\
         \x20\x20\x20\x20visibility:\n\
         \x20\x20\x20\x20\x20\x20objects:\n\
         \x20\x20\x20\x20\x20\x20\x20\x20- type: conversation\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20column: attr.gen_ai.conversation.id\n\
         \x20\x20\x20\x20\x20\x20self_principal_column: attr.user.hash\n\
         \x20\x20\x20\x20\x20\x20max_objects: 3\n\
         \x20\x20\x20\x20\x20\x20list_timeout_ms: 1500\n"
    );
    let (mut child, _grpc, _http, querier) =
        spawn_with_auth_and_storage(&tmp, storage_yaml, &auth_yaml, &[]).await;
    let all = "true | limit 100";

    // --- RFC0047.4: tenant-wide reader ------------------------------------
    let alice = mint(&encoding, &issuer, "alice", &[], false);
    let (status, body) = query(&http, querier, &alice, "acme", all).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["rows"], total, "every row, tenant predicate only");
    // No enumeration is issued for a tenant-wide reader — pinned by the
    // resolver unit test (`two_step_visibility`); here: the plan is complete.

    // --- RFC0047.5: participant + self fast path ---------------------------
    let bob = mint(&encoding, &issuer, "bob", &[], false);
    let (status, body) = query(&http, querier, &bob, "acme", all).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        conversations(&body),
        ["c-2", "c-9"],
        "bob: his participant conversation + rows carrying his subject; \
         never acme/c-1 although he is participant of globex/c-1"
    );
    let (status, body) = query(
        &http,
        querier,
        &bob,
        "acme",
        "attr.user.hash == \"alice\" | limit 100",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["rows"], 0,
        "the user's own predicate ANDs with visibility"
    );

    // --- RFC0047.6: agent as principal + revocable delegation --------------
    let bot = mint(&encoding, &issuer, "bot", &[], true);
    let (status, body) = query(&http, querier, &bot, "acme", all).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        conversations(&body),
        ["c-3", "c-4", "c-7"],
        "bot: its actor conversations + the delegated c-7, none of other's"
    );
    let other = mint(&encoding, &issuer, "other", &[], true);
    let (status, body) = query(&http, querier, &other, "acme", all).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(conversations(&body), ["c-5"]);
    fga.write(&[], &[tuple("agent:bot", "delegate", &conv("c-7"))])
        .await
        .expect("revoke delegation");
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let (status, body) = query(&http, querier, &bot, "acme", all).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        conversations(&body),
        ["c-3", "c-4"],
        "delegation revoked past the TTL"
    );

    // --- RFC0047.7: bounded enumeration, per tenant ------------------------
    let big = mint(&encoding, &issuer, "big", &[], true);
    let (status, body) = query(&http, querier, &big, "acme", all).await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["error"]["kind"], "visibility_bound", "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("exceeds 3 objects in tenant acme"),
        "{body}"
    );
    let mixed = mint(&encoding, &issuer, "mixed", &[], true);
    let (status, body) = query(&http, querier, &mixed, "acme", all).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        conversations(&body),
        ["c-15", "c-16"],
        "only tenant-acme ids count toward the bound (4 globex ids ignored)"
    );

    // --- RFC0047.8: metadata without content -------------------------------
    let fin = mint(&encoding, &issuer, "fin", &[], false);
    let (status, body) = query(
        &http,
        querier,
        &fin,
        "acme",
        "true | sum(attr.cost_usd) by attr.model",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let groups = body["aggregate"].as_array().expect("aggregate");
    assert_eq!(groups.len(), 1, "{body}");
    let sum = groups[0]["value"].as_f64().expect("sum");
    assert!(
        (sum - 1.5 * f64::from(u32::try_from(total).expect("small"))).abs() < 1e-6,
        "every row of the tenant: {body}"
    );
    let (status, body) = query(&http, querier, &fin, "acme", all).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["rows"], total);
    for record in body["records"].as_array().expect("records") {
        assert_eq!(record["body"]["kind"], "masked", "{record}");
        let content = record["attributes"]
            .as_array()
            .expect("attributes")
            .iter()
            .find(|kv| kv["key"] == "gen_ai.input.messages")
            .expect("key kept");
        assert!(content["value"].is_null(), "value is null: {content}");
    }
    let (status, body) = query(
        &http,
        querier,
        &fin,
        "acme",
        "contains(body, \"secret\") | limit 10",
    )
    .await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["error"]["kind"], "column_forbidden", "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("`body`"),
        "{body}"
    );
    // Template-level queries need tenant-wide content read.
    let (status, body) = query(&http, querier, &fin, "acme", "drift from -1h to now").await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["error"]["kind"], "visibility_scoped", "{body}");
    let (status, _) = query(&http, querier, &alice, "acme", "drift from -1h to now").await;
    assert_eq!(status, 200, "tenant-wide readers may");

    // --- RFC0047.9: tool gate ----------------------------------------------
    let payload = mcp_call(
        &http,
        querier,
        &bot,
        "query_logs",
        serde_json::json!({"tenant": "acme", "query": "true", "limit": 100}),
    )
    .await;
    assert!(
        payload.get("error").is_none(),
        "bot may call query_logs: {payload}"
    );
    let text = payload["result"]["content"][0]["text"]
        .as_str()
        .expect("tool text");
    let result: serde_json::Value = serde_json::from_str(text).expect("tool json");
    assert_eq!(
        conversations(&result),
        ["c-3", "c-4"],
        "and its data access is scoped exactly as the JSON API's"
    );
    for (tool, args) in [
        (
            "template_drift",
            serde_json::json!({"tenant": "acme", "from": "-1h", "to": "now"}),
        ),
        ("list_templates", serde_json::json!({"tenant": "acme"})),
    ] {
        let payload = mcp_call(&http, querier, &bot, tool, args).await;
        let message = payload["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("permission denied") && message.contains(tool),
            "the denial names the tool: {payload}"
        );
    }
    let payload = mcp_call(
        &http,
        querier,
        &alice,
        "template_drift",
        serde_json::json!({"tenant": "acme", "from": "-1h", "to": "now"}),
    )
    .await;
    assert!(
        payload.get("error").is_none(),
        "a tenant-wide reader calls everything: {payload}"
    );

    child.kill().await.expect("kill the server");
    drop(container);
}
