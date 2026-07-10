//! RFC 0030 §5 — the server-owned scenarios: the plaintext-auth
//! startup warning (`.7`) and TLS on the querier surface (`.3`) are
//! live; the served end-to-end (`.8`) remains a stub. The receiver arms
//! live in
//! `crates/ourios-ingester/tests/it/rfc0030_tls.rs` per §6.

use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::time::timeout;

/// Scenario RFC0030.3 — querier + MCP over TLS: with the querier's
/// `http_tls` set and a static bearer configured, a query (valid bearer
/// + `X-Ourios-Tenant`) and an MCP `initialize` (valid bearer) both
/// succeed over TLS; a plaintext request to the same port fails at the
/// transport layer. Drives `querier::serve` directly (a real listener
/// + handshake) rather than the spawned binary.
/// See `docs/rfcs/0030-tls-mtls-listeners.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0030_3_querier_and_mcp_over_tls() {
    use ourios_core::auth::{TokenSpec, build_token_store};
    use ourios_ingester::receiver::AuthResolver;
    use ourios_ingester::receiver::tls::TlsSettings;
    use ourios_parquet::StoreConfig;
    use ourios_server::querier::{QuerierConfig, serve};

    let tmp = tempfile::TempDir::new().expect("temp");
    let signed =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])
            .expect("mint");
    let cert_pem = signed.cert.pem();
    let cert_path = tmp.path().join("server.crt");
    let key_path = tmp.path().join("server.key");
    std::fs::write(&cert_path, &cert_pem).expect("write cert");
    std::fs::write(&key_path, signed.signing_key.serialize_pem()).expect("write key");
    let http_tls = TlsSettings::from_parts(
        "querier.http_tls",
        Some(&cert_path.display().to_string()),
        Some(&key_path.display().to_string()),
        None,
        None,
        None,
    )
    .expect("valid")
    .expect("configured");

    let store_root = tmp.path().join("store");
    std::fs::create_dir_all(&store_root).expect("store root");
    let tokens = build_token_store(Some(&[TokenSpec {
        name: Some("query-client".to_string()),
        token: Some("tok-q".to_string()),
        tenants: vec!["acme".to_string()],
    }]))
    .expect("valid")
    .expect("enabled");

    let handle = serve(QuerierConfig {
        http_addr: "127.0.0.1:0".parse().expect("addr"),
        http_tls: Some(http_tls),
        store: StoreConfig::Local(store_root),
        mcp_enabled: true,
        auth: AuthResolver::static_only(Some(std::sync::Arc::new(tokens))),
        default_window_nanos: 3600 * 1_000_000_000,
    })
    .await
    .expect("querier serves");
    let port = handle.http_addr.port();

    let client = reqwest::Client::builder()
        .add_root_certificate(reqwest::Certificate::from_pem(cert_pem.as_bytes()).expect("root"))
        .build()
        .expect("client");

    // Query over TLS (empty store → 200, zero rows). Auth is held valid
    // so transport is the only variable under test.
    let resp = client
        .post(format!("https://127.0.0.1:{port}/v1/query"))
        .header("content-type", "text/plain")
        .header("x-ourios-tenant", "acme")
        .header("authorization", "Bearer tok-q")
        .body("template_id == 1")
        .send()
        .await
        .expect("query over TLS");
    assert!(resp.status().is_success(), "query status {}", resp.status());

    // MCP `initialize` over TLS.
    let initialize = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "rfc0030-test", "version": "0"}
        }
    });
    let resp = client
        .post(format!("https://127.0.0.1:{port}/mcp"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("authorization", "Bearer tok-q")
        .body(initialize.to_string())
        .send()
        .await
        .expect("MCP initialize over TLS");
    assert!(resp.status().is_success(), "mcp status {}", resp.status());

    // Plaintext to the TLS port fails the handshake.
    let plaintext = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/query"))
        .header("x-ourios-tenant", "acme")
        .body("template_id == 1")
        .send()
        .await;
    assert!(plaintext.is_err(), "plaintext to the TLS querier must fail");

    handle.shutdown().await.ok();
}

/// Spawn the server with the given config file, collect stderr until
/// the querier readiness line appears on stdout, and return how many
/// stderr lines contained `needle`. The readiness line is the "warning
/// window" bound: `startup_guards` runs before any role announces
/// readiness, so a warning that exists is on stderr by then.
async fn warnings_before_ready(config_yaml: &str, tmp: &tempfile::TempDir, needle: &str) -> usize {
    let config_path = tmp.path().join("ourios.yaml");
    // write + close before the spawn — no handle stays open across it.
    std::fs::write(&config_path, config_yaml).expect("write config");

    let mut child = Command::new(env!("CARGO_BIN_EXE_ourios-server"))
        .arg("--config")
        .arg(&config_path)
        .env("EDGE_TOKEN", "rfc0030-test-token")
        // Deterministic regardless of the harness environment: an
        // inherited RUST_LOG=error would filter the warning off stderr.
        .env("RUST_LOG", "info")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn ourios-server");

    let stderr = child.stderr.take().expect("server stderr piped");
    let stdout = child.stdout.take().expect("server stdout piped");

    let collector = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        let mut collected = Vec::new();
        while let Some(line) = lines.next_line().await.expect("read stderr") {
            collected.push(line);
        }
        collected
    });

    let mut stdout_lines = BufReader::new(stdout).lines();
    timeout(Duration::from_secs(15), async {
        while let Some(line) = stdout_lines.next_line().await.expect("read stdout") {
            if line.contains("querier HTTP listening on") {
                return;
            }
        }
        panic!("the querier never announced readiness");
    })
    .await
    .expect("readiness before timeout");

    // Readiness follows the startup guards, so the warning (if any) is
    // already written; kill the child to end the stderr stream, and
    // reap it so no zombie outlives the test.
    child.start_kill().expect("kill server");
    timeout(Duration::from_secs(15), child.wait())
        .await
        .expect("child exits after kill")
        .expect("wait on child");
    let collected = timeout(Duration::from_secs(15), collector)
        .await
        .expect("stderr drains after kill")
        .expect("collector task");
    collected.iter().filter(|l| l.contains(needle)).count()
}

/// Scenario RFC0030.7 — plaintext-auth warning: credentials configured
/// and a listener without a `*_tls` block get exactly one startup
/// warning naming that listener; with the block configured, none.
/// See `docs/rfcs/0030-tls-mtls-listeners.md` §5.
#[tokio::test]
async fn rfc0030_7_plaintext_auth_warning() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let base = format!(
        "storage:\n  local:\n    bucket_root: {root}\n\
         auth:\n  tokens:\n    - name: edge\n      token: ${{env:EDGE_TOKEN}}\n      tenants: [\"acme\"]\n\
         querier:\n  enabled: true\n  http_addr: 127.0.0.1:0\n",
        root = tmp.path().display(),
    );

    // Plaintext listener + credentials: exactly one warning, naming it.
    let count = warnings_before_ready(
        &base,
        &tmp,
        "querier.http_addr serves bearer credentials over plaintext",
    )
    .await;
    assert_eq!(count, 1, "exactly one warning names the listener");

    // The same listener with its *_tls block: no warning.
    let signed = rcgen::generate_simple_self_signed(vec!["localhost".into()])
        .expect("mint a self-signed pair");
    let cert_path = tmp.path().join("server.crt");
    let key_path = tmp.path().join("server.key");
    std::fs::write(&cert_path, signed.cert.pem()).expect("write cert");
    std::fs::write(&key_path, signed.signing_key.serialize_pem()).expect("write key");
    let with_tls = format!(
        "{base}  http_tls:\n    cert_file: {cert}\n    key_file: {key}\n",
        cert = cert_path.display(),
        key = key_path.display(),
    );
    let count =
        warnings_before_ready(&with_tls, &tmp, "serves bearer credentials over plaintext").await;
    assert_eq!(count, 0, "a TLS-configured listener draws no warning");
}

/// Scenario RFC0030.8 — served end-to-end (Collector-shaped client).
/// See `docs/rfcs/0030-tls-mtls-listeners.md` §5.
#[test]
#[ignore = "RFC0030.8 stub — implemented in the served green slice"]
fn rfc0030_8_served_end_to_end() {
    todo!(
        "RFC0030.8 — served ourios-server, both roles, TLS on both \
         receiver listeners + mTLS on gRPC + TLS querier: a \
         Collector-shaped gRPC exporter (ca_file + client pair) and \
         an HTTPS exporter both land batches queryable over the TLS \
         querier, no plaintext hop"
    );
}
