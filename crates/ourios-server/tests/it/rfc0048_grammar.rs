//! RFC 0048 §3.1 at the served surfaces (RFC0048.1) and, behind the
//! Docker gate, the verbatim naming rule against a real `OpenFGA`
//! (RFC0048.2). The config and claim boundaries are unit-tested where
//! they live (`ourios-core::auth`); the OTLP boundary lives in the
//! RFC 0046 end-to-end test. Here: the querier header and the MCP
//! `tenant` argument speak the same grammar, and the graph holds the
//! exact bytes the grammar admits.

use std::io::Write as _;
use std::time::Duration;

use ourios_core::auth::openfga::{
    OpenFgaSpec, VisibilityObjectSpec, VisibilitySpec, build_openfga_config,
};
use ourios_core::record::MinedRecord;
use ourios_ingester::graph_emitter::GraphEmitter;
use testcontainers_modules::testcontainers::core::ContainerPort;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{GenericImage, ImageExt};
use tokio::time::timeout;

use crate::rfc0029_oidc::claim_binding::spawn_with_auth;
use crate::rfc0047_openfga::{OPENFGA_IMAGE, OPENFGA_TAG, provision};
use crate::rfc0047_visibility::row_at;

/// Off-grammar values a client can put in an HTTP header (non-ASCII ones
/// are refused by clients before the wire; the RFC 0046 end-to-end test
/// covers those at the OTLP boundary).
const OFF_GRAMMAR: [&str; 4] = ["a/b", "a:b", "a#b", "a b"];

/// RFC0048.1 — the querier header applies the grammar (400, named reason).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0048_1_querier_header_speaks_the_grammar() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let (mut child, _grpc, _http, addr) = spawn_with_auth(&tmp, "", &[]).await;
    let http = reqwest::Client::new();
    let query = |tenant: String| {
        let http = http.clone();
        async move {
            let response = http
                .post(format!("http://{addr}/v1/query"))
                .header("content-type", "text/plain")
                .header("x-ourios-tenant", tenant)
                .body("true | limit 1")
                .send()
                .await
                .expect("sent");
            let status = response.status();
            let body: serde_json::Value = response.json().await.unwrap_or_default();
            (
                status,
                body["error"]["kind"].as_str().unwrap_or("").to_string(),
            )
        }
    };
    let long = "x".repeat(129);
    for bad in OFF_GRAMMAR.iter().copied().chain([long.as_str()]) {
        let (status, kind) = query(bad.to_string()).await;
        assert_eq!(status, 400, "{bad:?}");
        assert_eq!(kind, "invalid_tenant", "{bad:?} carries the grammar kind");
    }
    // Present-but-empty keeps the RFC 0026 contract: `missing_tenant`,
    // never conflated with an off-grammar value.
    let (status, kind) = query("   ".to_string()).await;
    assert_eq!(status, 400);
    assert_eq!(kind, "missing_tenant");
    let (status, kind) = query("team-eu.%1~x".to_string()).await;
    assert_eq!(status, 200, "graphic punctuation is inside the grammar");
    assert_eq!(kind, "", "no error object on success");
    child.kill().await.expect("kill");
}

/// RFC0048.3 (startup arms) — a graph column outside the promoted set is
/// a startup error naming the key; so is a `self_principal_column` outside
/// `user_columns` (refused in config validation before any store work).
#[tokio::test]
async fn rfc0048_3_unpromoted_identity_column_is_a_startup_error() {
    for (yaml, needle) in [
        (
            "      identities:\n        user_columns: [attr.not.promoted]\n",
            "identities.user_columns: `attr.not.promoted` is not a promoted column",
        ),
        (
            "      identities:\n        agent_columns: [attr.also.missing]\n",
            "identities.agent_columns: `attr.also.missing` is not a promoted column",
        ),
        (
            "      self_principal_column: attr.gen_ai.conversation.id\n",
            "must be one of identities.user_columns",
        ),
    ] {
        let tmp = tempfile::TempDir::new().expect("temp");
        let config_path = tmp.path().join("ourios.yaml");
        let mut file = std::fs::File::create(&config_path).expect("create config");
        write!(
            file,
            "storage:\n  local:\n    bucket_root: {}\n  promoted_attributes:\n    log: [gen_ai.conversation.id, user.hash, enduser.pseudo.id, gen_ai.agent.id]\n\
             receiver:\n  enabled: true\n  grpc_addr: 127.0.0.1:0\n  http_addr: 127.0.0.1:0\n  wal_root: {}\n\
             auth:\n  tokens:\n    - name: a\n      token: ${{env:RFC0048_TOKEN}}\n      tenants: [\"*\"]\n  openfga:\n    api_url: http://openfga.invalid:8080\n    store_id: s\n    visibility:\n      objects:\n        - type: conversation\n          column: attr.gen_ai.conversation.id\n{yaml}",
            tmp.path().display(),
            tmp.path().join("wal").display(),
        )
        .expect("write config");
        let output = timeout(
            Duration::from_secs(15),
            tokio::process::Command::new(env!("CARGO_BIN_EXE_ourios-server"))
                .arg("--config")
                .arg(&config_path)
                .env("RFC0048_TOKEN", "tok-a")
                .kill_on_drop(true)
                .output(),
        )
        .await
        .expect("server exits before timeout")
        .expect("run ourios-server");
        assert!(!output.status.success(), "{yaml:?} must fail startup");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(needle), "{needle:?} not in: {stderr}");
    }
}

/// RFC0048.7 — the deadline assumption is loud: one startup line carries
/// both values (and the RFC 0047 startup rejection for a timeout at or
/// above the deadline stays — asserted in the config unit tests).
#[tokio::test]
async fn rfc0048_7_list_deadline_event_at_startup() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let config_path = tmp.path().join("ourios.yaml");
    let mut file = std::fs::File::create(&config_path).expect("create config");
    write!(
        file,
        "storage:\n  local:\n    bucket_root: {}\n  promoted_attributes:\n    log: [gen_ai.conversation.id]\n\
         receiver:\n  enabled: true\n  grpc_addr: 127.0.0.1:0\n  http_addr: 127.0.0.1:0\n  wal_root: {}\n\
         auth:\n  tokens:\n    - name: a\n      token: ${{env:RFC0048_TOKEN}}\n      tenants: [\"*\"]\n\
         \x20\x20openfga:\n    api_url: http://openfga.invalid:8080\n    store_id: s\n    server_list_objects_deadline_ms: 2500\n    visibility:\n      objects:\n        - type: conversation\n          column: attr.gen_ai.conversation.id\n      list_timeout_ms: 1500\n",
        tmp.path().display(),
        tmp.path().join("wal").display(),
    )
    .expect("write config");
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_ourios-server"))
        .arg("--config")
        .arg(&config_path)
        .env("RFC0048_TOKEN", "tok-a")
        .env("RUST_LOG", "info")
        .stderr(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn");
    let stderr = child.stderr.take().expect("stderr piped");
    let found = timeout(Duration::from_secs(15), async {
        use tokio::io::AsyncBufReadExt as _;
        let mut lines = tokio::io::BufReader::new(stderr).lines();
        while let Some(line) = lines.next_line().await.expect("read stderr") {
            if line.contains("list_timeout_ms 1500")
                && line.contains("server_list_objects_deadline_ms 2500")
            {
                return true;
            }
        }
        false
    })
    .await
    .expect("startup line before timeout");
    assert!(found, "the deadline event names both values");
    child.kill().await.expect("kill");
}

/// RFC0048.2 — verbatim objects on a real graph: write → `Read`
/// byte-for-byte → streamed prefix filter; the 114/115 budget and a `/`
/// conversation id, end to end through the emitter.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)] // one container: budget, `/` id, byte-for-byte read in sequence
#[ignore = "RFC0048.2 — needs Docker (real OpenFGA container); run by the openfga-resolver CI job via --ignored"]
async fn rfc0048_2_verbatim_tenant_on_a_real_graph() {
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
    let config = build_openfga_config(&OpenFgaSpec {
        api_url: Some(api_url.clone()),
        store_id: Some(store_id),
        authorization_model_id: Some(model_id),
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
    let fga = ourios_serving::openfga::OpenFgaClient::new(&config).expect("client");
    let emitter = GraphEmitter::from_config(&config)
        .expect("emitter")
        .expect("conversation bound");

    // A 128-byte tenant, the exact RFC 0048 §3.1 budget: a 114-byte id and
    // an id containing `/` land verbatim; a 115-byte id is skipped by the
    // emitter (derive produces no tuple for it — never sent).
    let tenant = "t".repeat(128);
    let fits = "c".repeat(114);
    let over = "c".repeat(115);
    let rows: Vec<MinedRecord> = vec![
        row_at(1_000, &fits, "alice"),
        row_at(2_000, &over, "alice"),
        row_at(3_000, "sess/42", "bob"),
    ];
    let tuples = emitter.derive(&tenant, &rows);
    let objects: Vec<&str> = tuples
        .iter()
        .map(|t| t.object.as_str())
        .filter(|o| o.starts_with("conversation:"))
        .collect();
    let full_fits = format!("conversation:{tenant}/{fits}");
    let full_slash = format!("conversation:{tenant}/sess/42");
    assert!(objects.contains(&full_fits.as_str()), "114 fits");
    assert!(objects.contains(&full_slash.as_str()), "`/` id fits");
    assert!(
        !objects.iter().any(|o| o.contains(&over)),
        "115 is skipped, never sent"
    );
    assert_eq!(full_fits.len(), 256, "the budget is exact");
    emitter.emit(&tuples).await.expect("emit");

    // `Read` returns the exact bytes — no encoding step anywhere.
    let read = fga.read_by_object(&full_fits).await.expect("read");
    assert!(
        read.iter().any(|t| t.object == full_fits),
        "byte-for-byte: {read:?}"
    );
    let read = fga.read_by_object(&full_slash).await.expect("read");
    assert!(read.iter().any(|t| t.object == full_slash), "{read:?}");
    drop(container);
}
