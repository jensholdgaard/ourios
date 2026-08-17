//! RFC 0047 §5 — the `OpenFGA` resolver on the served binary, against a
//! **real `OpenFGA` container** (testcontainers; CI-gated like the RFC 0029
//! Dex job — `#[ignore]`d in the default run) loaded with the in-tree
//! model (`deploy/openfga/model.json`, the `fga model transform` of
//! `model.fga`).
//!
//! Scenarios RFC0047.1 (resolver binding), RFC0047.2 (ingest binding
//! through the resolver) and RFC0047.3 (fail closed) — the layer-1 slice.
//! See `docs/rfcs/0047-rebac-resolver-and-graph-visibility.md` §5.

use std::time::Duration;

use opentelemetry_proto::tonic::collector::logs::v1::logs_service_client::LogsServiceClient;
use ourios_core::auth::openfga::{OpenFgaClient, OpenFgaSpec, TupleKey, build_openfga_config};
use testcontainers_modules::testcontainers::core::ContainerPort;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{GenericImage, ImageExt};
use tokio::time::timeout;

use crate::rfc0029_oidc::claim_binding::{query_status, spawn_with_auth};
use crate::rfc0029_oidc::ingest_binding::{make_key, serve_issuer, tenant_request};

const OPENFGA_IMAGE: &str = "openfga/openfga";
/// Pinned by digest for reproducibility (v1.11.1).
const OPENFGA_TAG: &str =
    "v1.11.1@sha256:1f9187961aded3ce60e3c4b7ccc39074ce88291aa51d9e3be09db4ff51e7b692";
const MODEL_JSON: &str = include_str!("../../../../deploy/openfga/model.json");
const COLLECTOR_TOKEN: &str = "tok-collector-cluster1";

/// A JWT for `sub` from the fixture issuer — no tenant claim (the graph
/// binds), optional groups.
fn mint(encoding: &jsonwebtoken::EncodingKey, issuer: &str, sub: &str, groups: &[&str]) -> String {
    let now = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("epoch")
            .as_secs(),
    )
    .expect("fits");
    let mut claims = serde_json::json!({
        "iss": issuer, "aud": "ourios", "exp": now + 600, "sub": sub,
    });
    if !groups.is_empty() {
        claims["groups"] = serde_json::json!(groups);
    }
    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::ES256);
    header.kid = Some("key-1".to_string());
    jsonwebtoken::encode(&header, &claims, encoding).expect("mint")
}

/// Create a store and load the in-tree model; returns `(store_id, model_id)`.
async fn provision(api_url: &str) -> (String, String) {
    let http = reqwest::Client::new();
    let store: serde_json::Value = http
        .post(format!("{api_url}/stores"))
        .json(&serde_json::json!({ "name": "ourios-it" }))
        .send()
        .await
        .expect("create store")
        .error_for_status()
        .expect("store 2xx")
        .json()
        .await
        .expect("store json");
    let store_id = store["id"].as_str().expect("store id").to_string();
    let model: serde_json::Value = http
        .post(format!("{api_url}/stores/{store_id}/authorization-models"))
        .header("content-type", "application/json")
        .body(MODEL_JSON)
        .send()
        .await
        .expect("write model")
        .error_for_status()
        .expect("model 2xx")
        .json()
        .await
        .expect("model json");
    let model_id = model["authorization_model_id"]
        .as_str()
        .expect("model id")
        .to_string();
    (store_id, model_id)
}

fn tuple(user: &str, relation: &str, object: &str) -> TupleKey {
    TupleKey::new(user, relation, object)
}

/// Scenarios RFC0047.1–.3 on the served binary. One container, one
/// server: alice (tenant reader), the collector (tenant writer, static
/// token), bob (participant + binding tuple), fin (metadata reader) and a
/// principal with no tuples establish sessions; then `OpenFGA` is stopped
/// and every path fails closed once the short session TTL lapses.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)] // one container + one server, every arm in sequence
#[ignore = "RFC0047.1–.3 — needs Docker (real OpenFGA container); run by the openfga-resolver CI job via --ignored"]
async fn rfc0047_1_to_3_resolver_end_to_end() {
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
    fga.write(
        &[
            tuple("user:alice", "reader", "tenant:acme"),
            tuple(
                "service_account:collector-cluster1",
                "writer",
                "tenant:acme",
            ),
            tuple("user:fin", "metadata_reader", "tenant:acme"),
            tuple("tenant:acme", "parent", "conversation:acme/c-1"),
            tuple("user:bob", "participant", "conversation:acme/c-1"),
            tuple("user:bob", "scoped_reader", "tenant:acme"),
            tuple("user:mallory", "reader", "tenant:globex"),
        ],
        &[],
    )
    .await
    .expect("seed tuples");
    // Writes are idempotent (`on_duplicate: ignore`): re-seeding is not an
    // error — the RFC0047.10 emitter relies on this.
    fga.write(&[tuple("user:alice", "reader", "tenant:acme")], &[])
        .await
        .expect("duplicate write ignored");
    fga.write(&[], &[tuple("user:nobody", "reader", "tenant:acme")])
        .await
        .expect("missing delete ignored");

    // --- Issuer + server ---------------------------------------------------
    let (encoding, jwk) = make_key("key-1");
    let issuer = serve_issuer(jwk).await;
    let tmp = tempfile::TempDir::new().expect("temp");
    let auth_yaml = format!(
        "auth:\n\
         \x20\x20tokens:\n\
         \x20\x20\x20\x20- name: collector-cluster1\n\
         \x20\x20\x20\x20\x20\x20token: ${{env:COLLECTOR_TOKEN}}\n\
         \x20\x20\x20\x20\x20\x20tenants: [\"*\"]\n\
         \x20\x20oidc:\n\
         \x20\x20\x20\x20issuer: {issuer}\n\
         \x20\x20\x20\x20audience: ourios\n\
         \x20\x20\x20\x20groups_claim: groups\n\
         \x20\x20openfga:\n\
         \x20\x20\x20\x20api_url: {api_url}\n\
         \x20\x20\x20\x20store_id: {store_id}\n\
         \x20\x20\x20\x20authorization_model_id: {model_id}\n\
         \x20\x20\x20\x20session_ttl_secs: 1\n\
         \x20\x20\x20\x20request_timeout_secs: 2\n"
    );
    let (mut child, grpc, _http, querier) =
        spawn_with_auth(&tmp, &auth_yaml, &[("COLLECTOR_TOKEN", COLLECTOR_TOKEN)]).await;
    let mut logs = LogsServiceClient::connect(format!("http://{grpc}"))
        .await
        .expect("grpc connect");
    let export = |client: &mut LogsServiceClient<tonic::transport::Channel>,
                  bearer: Option<&str>,
                  tenant: &str| {
        let mut request = tenant_request(tenant);
        if let Some(bearer) = bearer {
            request.metadata_mut().insert(
                "authorization",
                format!("Bearer {bearer}").parse().expect("metadata"),
            );
        }
        let mut client = client.clone();
        async move { client.export(request).await.map(drop).map_err(|s| s.code()) }
    };

    // --- RFC0047.1: resolver binding ---------------------------------------
    let alice = mint(&encoding, &issuer, "alice", &[]);
    assert!(
        query_status(querier, Some(&alice), Some("acme"))
            .await
            .contains("200"),
        "alice reads acme"
    );
    assert!(
        query_status(querier, Some(&alice), Some("globex"))
            .await
            .contains("403"),
        "alice's read set is exactly {{acme}}"
    );
    assert_eq!(
        export(&mut logs, Some(&alice), "acme").await,
        Err(tonic::Code::PermissionDenied),
        "alice's write set is empty"
    );
    assert!(
        query_status(querier, Some(COLLECTOR_TOKEN), Some("acme"))
            .await
            .contains("403"),
        "the collector's read set is empty (writer only)"
    );
    // bob (participant + binding tuple) and fin (metadata only) reach the
    // planner — bound to acme — while holding no tenant-wide content read.
    let bob = mint(&encoding, &issuer, "bob", &[]);
    let fin = mint(&encoding, &issuer, "fin", &[]);
    for (who, token) in [("bob", &bob), ("fin", &fin)] {
        assert!(
            query_status(querier, Some(token), Some("acme"))
                .await
                .contains("200"),
            "{who} binds acme"
        );
        assert!(
            !fga.check(
                &tuple(&format!("user:{who}"), "can_read_content", "tenant:acme"),
                &[]
            )
            .await
            .expect("check"),
            "{who} has no tenant-wide content read"
        );
    }
    assert!(
        fga.check(&tuple("user:alice", "can_read_content", "tenant:acme"), &[])
            .await
            .expect("check"),
        "alice does"
    );
    // A principal with no tuples is unbound — 401, never empty-but-open.
    let nobody = mint(&encoding, &issuer, "nobody", &[]);
    assert!(
        query_status(querier, Some(&nobody), Some("acme"))
            .await
            .contains("401"),
        "no tuples → unbound"
    );
    assert_eq!(
        export(&mut logs, Some(&nobody), "acme").await,
        Err(tonic::Code::Unauthenticated)
    );
    // Contextual tuples: a group claim binds through `team#member` on a
    // stored `tenant#reader@team#member` edge without any per-user tuple.
    fga.write(
        &[tuple("team:platform#member", "reader", "tenant:globex")],
        &[],
    )
    .await
    .expect("team edge");
    let carol = mint(&encoding, &issuer, "carol", &["platform"]);
    assert!(
        query_status(querier, Some(&carol), Some("globex"))
            .await
            .contains("200"),
        "group claim → contextual team membership → tenant read"
    );

    // --- RFC0047.2: ingest binding through the resolver --------------------
    assert_eq!(
        export(&mut logs, Some(COLLECTOR_TOKEN), "acme").await,
        Ok(()),
        "the collector writes acme"
    );
    assert_eq!(
        export(&mut logs, Some(COLLECTOR_TOKEN), "globex").await,
        Err(tonic::Code::PermissionDenied),
        "selector outside the write set"
    );

    // --- RFC0047.3: fail closed --------------------------------------------
    container.stop().await.expect("stop openfga");
    // Past the 1 s session TTL every cached binding is gone; nothing may
    // be served from a stale grant.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        query_status(querier, Some(&alice), Some("acme"))
            .await
            .contains("503"),
        "query fails closed"
    );
    assert_eq!(
        export(&mut logs, Some(COLLECTOR_TOKEN), "acme").await,
        Err(tonic::Code::Unavailable),
        "ingest fails closed"
    );
    assert!(
        query_status(querier, Some("tok-unknown"), Some("acme"))
            .await
            .contains("401"),
        "authentication still precedes authorization"
    );

    child.kill().await.expect("kill the server");
}
