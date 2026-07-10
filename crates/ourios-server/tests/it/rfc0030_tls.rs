//! RFC 0030 §5 — the server-owned scenarios: the plaintext-auth startup
//! warning (`.7`), TLS on the querier surface (`.3`), and the served
//! end-to-end (`.8`, transport-only scope) are all live. The receiver arms
//! live in `crates/ourios-ingester/tests/it/rfc0030_tls.rs` per §6.

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

/// Scenario RFC0030.8 — served end-to-end, **transport-only** scope (§5 as
/// amended 2026-07-10). The served `ourios-server` binary runs both roles
/// with TLS on both receiver listeners, mTLS on gRPC, and TLS on the
/// querier. A Collector-shaped gRPC exporter (CA + client pair + bearer)
/// and an HTTPS exporter (CA + bearer) both land batches; the query surface
/// serves over TLS; and no listener answers plaintext. Landing is read back
/// from the store after a graceful drain — the full stack, no plaintext
/// hop. See `docs/rfcs/0030-tls-mtls-listeners.md` §5.
///
/// Unix-only: graceful shutdown (which flushes the sink so the batches
/// become readable) is driven by `kill -TERM` (the `rfc0003_16` /
/// `rfc0008_10` precedent).
#[cfg(unix)]
#[allow(clippy::too_many_lines)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0030_8_served_end_to_end() {
    use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
    use opentelemetry_proto::tonic::collector::logs::v1::logs_service_client::LogsServiceClient;
    use prost::Message as _;
    use tonic::transport::{Certificate, ClientTlsConfig, Endpoint, Identity};

    const BEARER: &str = "rfc0030-8-token";
    const TENANT: &str = "acme";
    const GRPC_BODY: &str = "logged in over mtls grpc";
    const HTTP_BODY: &str = "logged in over https";

    // One string-body OTLP batch; `service.name` is the tenant (RFC 0003
    // §6.3), which the bearer's tenant set must contain.
    fn batch(service: &str, body: &str) -> ExportLogsServiceRequest {
        use opentelemetry_proto::tonic::common::v1::any_value::Value;
        use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue};
        use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
        use opentelemetry_proto::tonic::resource::v1::Resource;
        let string = |s: &str| AnyValue {
            value: Some(Value::StringValue(s.to_owned())),
        };
        ExportLogsServiceRequest {
            resource_logs: vec![ResourceLogs {
                resource: Some(Resource {
                    attributes: vec![KeyValue {
                        key: "service.name".to_owned(),
                        value: Some(string(service)),
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
                scope_logs: vec![ScopeLogs {
                    log_records: vec![LogRecord {
                        body: Some(string(body)),
                        severity_number: 9,
                        time_unix_nano: 1_775_127_480_000_000_000,
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        }
    }

    let tmp = tempfile::TempDir::new().expect("temp");
    // Server leaf (loopback SANs so tonic pins `localhost` and reqwest
    // verifies `127.0.0.1`); a separate client leaf is the mTLS identity and
    // its own PEM is the `client_ca_file` the gRPC listener verifies against.
    let server =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])
            .expect("server cert");
    let client = rcgen::generate_simple_self_signed(vec!["edge-collector".to_string()])
        .expect("client cert");
    let server_pem = server.cert.pem();
    let cert_path = tmp.path().join("server.crt");
    let key_path = tmp.path().join("server.key");
    let client_ca_path = tmp.path().join("client-ca.crt");
    std::fs::write(&cert_path, &server_pem).expect("write cert");
    std::fs::write(&key_path, server.signing_key.serialize_pem()).expect("write key");
    std::fs::write(&client_ca_path, client.cert.pem()).expect("write client CA");

    let wal = tmp.path().join("wal");
    std::fs::create_dir_all(&wal).expect("wal dir");
    let config_path = tmp.path().join("ourios.yaml");
    std::fs::write(
        &config_path,
        format!(
            "storage:\n  local:\n    bucket_root: {bucket}\n\
             receiver:\n  enabled: true\n  grpc_addr: 127.0.0.1:0\n\
             \x20\x20grpc_tls:\n    cert_file: {cert}\n    key_file: {key}\n    client_ca_file: {cca}\n\
             \x20\x20http_addr: 127.0.0.1:0\n\
             \x20\x20http_tls:\n    cert_file: {cert}\n    key_file: {key}\n  wal_root: {wal}\n\
             querier:\n  enabled: true\n  http_addr: 127.0.0.1:0\n\
             \x20\x20http_tls:\n    cert_file: {cert}\n    key_file: {key}\n\
             auth:\n  tokens:\n    - name: edge\n      token: ${{env:EDGE_TOKEN}}\n      tenants: [\"{TENANT}\"]\n",
            bucket = tmp.path().display(),
            cert = cert_path.display(),
            key = key_path.display(),
            cca = client_ca_path.display(),
            wal = wal.display(),
        ),
    )
    .expect("write config");

    let mut child = Command::new(env!("CARGO_BIN_EXE_ourios-server"))
        .arg("--config")
        .arg(&config_path)
        .env("EDGE_TOKEN", BEARER)
        .env("RUST_LOG", "info")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn ourios-server");
    let stdout = child.stdout.take().expect("stdout piped");
    let mut out_lines = BufReader::new(stdout).lines();
    let (grpc_addr, http_addr, querier_addr) = timeout(Duration::from_secs(15), async {
        let (mut g, mut h, mut q) = (None, None, None);
        while let Some(line) = out_lines.next_line().await.expect("read stdout") {
            if let Some(a) = line.strip_prefix("receiver gRPC listening on ") {
                g = Some(a.trim().to_string());
            }
            if let Some(a) = line.strip_prefix("receiver HTTP listening on ") {
                h = Some(a.trim().to_string());
            }
            if let Some(a) = line.strip_prefix("querier HTTP listening on ") {
                q = Some(a.trim().to_string());
            }
            if let (Some(g), Some(h), Some(q)) = (&g, &h, &q) {
                return (g.clone(), h.clone(), q.clone());
            }
        }
        panic!("all three listeners must announce readiness");
    })
    .await
    .expect("server ready before timeout");

    // gRPC over mTLS (client identity) + bearer → the batch acks.
    let channel = Endpoint::from_shared(format!("https://{grpc_addr}"))
        .expect("endpoint")
        .tls_config(
            ClientTlsConfig::new()
                .ca_certificate(Certificate::from_pem(server_pem.as_bytes()))
                .identity(Identity::from_pem(
                    client.cert.pem(),
                    client.signing_key.serialize_pem(),
                ))
                .domain_name("localhost"),
        )
        .expect("tls config")
        .connect()
        .await
        .expect("mTLS connect");
    let mut grpc = LogsServiceClient::new(channel);
    let mut req = tonic::Request::new(batch(TENANT, GRPC_BODY));
    req.metadata_mut().insert(
        "authorization",
        format!("Bearer {BEARER}").parse().expect("md"),
    );
    grpc.export(req).await.expect("mTLS gRPC export acks");

    // OTLP/HTTP over TLS + bearer → 200.
    let https = reqwest::Client::builder()
        .add_root_certificate(reqwest::Certificate::from_pem(server_pem.as_bytes()).expect("root"))
        .build()
        .expect("https client");
    let resp = https
        .post(format!("https://{http_addr}/v1/logs"))
        .header("content-type", "application/x-protobuf")
        .header("authorization", format!("Bearer {BEARER}"))
        .body(batch(TENANT, HTTP_BODY).encode_to_vec())
        .send()
        .await
        .expect("HTTPS export");
    assert!(
        resp.status().is_success(),
        "HTTPS export status {}",
        resp.status()
    );

    // The query surface serves over TLS with the same bearer (empty store
    // pre-flush → 200, zero rows): the read transport composes.
    let query = https
        .post(format!("https://{querier_addr}/v1/query"))
        .header("content-type", "text/plain")
        .header("x-ourios-tenant", TENANT)
        .header("authorization", format!("Bearer {BEARER}"))
        .body("template_id == 1")
        .send()
        .await
        .expect("TLS query");
    assert_eq!(query.status(), reqwest::StatusCode::OK, "TLS query serves");

    // No plaintext hop: every listener refuses plaintext.
    let plain = reqwest::Client::new();
    assert!(
        plain
            .post(format!("http://{http_addr}/v1/logs"))
            .body(batch(TENANT, "plaintext").encode_to_vec())
            .send()
            .await
            .is_err(),
        "plaintext to the TLS HTTP receiver must fail",
    );
    assert!(
        plain
            .post(format!("http://{querier_addr}/v1/query"))
            .header("x-ourios-tenant", TENANT)
            .body("template_id == 1")
            .send()
            .await
            .is_err(),
        "plaintext to the TLS querier must fail",
    );
    // tonic defers the handshake to the first RPC, so connect can lazily
    // succeed — the plaintext export against the TLS listener is where it
    // fails. Fold connect + export so either failure counts.
    let plaintext_grpc = async {
        let mut c = LogsServiceClient::connect(format!("http://{grpc_addr}")).await?;
        c.export(tonic::Request::new(batch(TENANT, "plaintext")))
            .await?;
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    };
    assert!(
        plaintext_grpc.await.is_err(),
        "plaintext to the TLS+mTLS gRPC receiver must fail",
    );

    // Graceful drain (SIGTERM) flushes the sink; then read the store back and
    // assert both TLS-delivered batches landed under the bearer's tenant.
    let pid = child.id().expect("pid").to_string();
    Command::new("kill")
        .args(["-TERM", &pid])
        .status()
        .await
        .expect("kill")
        .success()
        .then_some(())
        .expect("SIGTERM delivered");
    timeout(Duration::from_secs(20), child.wait())
        .await
        .expect("exit before timeout")
        .expect("child exits");

    let tenant = ourios_core::tenant::TenantId::new(TENANT);
    let registry = ourios_querier::derive_template_registry(
        ourios_querier::StoreRef::Local(tmp.path()),
        &tenant,
    )
    .expect("derive registry");
    let mut rendered = Vec::new();
    let mut stack = vec![tmp.path().join("data")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "parquet") {
                for record in ourios_parquet::Reader::open_file(&path)
                    .expect("open data file")
                    .read_all()
                    .expect("read records")
                {
                    assert_eq!(
                        record.tenant_id, tenant,
                        "records bind to the bearer's tenant"
                    );
                    if let ourios_querier::LogBody::Rendered { line, .. } =
                        ourios_querier::render_log_body(&record, &registry)
                    {
                        rendered.push(String::from_utf8(line).expect("utf8"));
                    }
                }
            }
        }
    }
    rendered.sort();
    let mut want = vec![GRPC_BODY.to_owned(), HTTP_BODY.to_owned()];
    want.sort();
    assert_eq!(
        rendered, want,
        "both the mTLS-gRPC and HTTPS batches landed and reconstruct — the \
         full stack, no plaintext hop",
    );
}
