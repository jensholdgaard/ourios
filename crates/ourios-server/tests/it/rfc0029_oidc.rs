//! RFC 0029 §5 — the OIDC bearer layer, all seven scenarios.
//!
//! `.1` is green: the schema/substitution arms live in
//! `ourios_server::config::file`, the validation matrix in
//! `ourios_core::auth`, the file→config mapping in `src/main.rs`
//! (`rfc0029_1_*`), and the startup-observable arms — the three
//! configuration errors and the oidc-only-serves-enforced shape —
//! here, against the spawned binary (the `rfc0026_auth` pattern; the
//! missing-`auth` open-mode arm is RFC 0026 §5.1's, re-asserted there).
//! Remaining stubs are `#[ignore]`d so the default run stays green
//! while the RFC is red; each names the green slice that discharges it.

use std::io::Write as _;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::time::timeout;

/// Scenario RFC0029.1 (startup errors) — a missing `auth.oidc.audience`,
/// an `auth` section with neither half, and an explicit `tokens: []`
/// (even with `oidc` configured) each fail startup, naming the key.
/// See `docs/rfcs/0029-oidc-bearer-layer.md` §5.
#[tokio::test]
async fn rfc0029_1_startup_configuration_errors() {
    let oidc = "  oidc:\n    issuer: https://dex.internal.example\n    audience: ourios\n    tenant_claim: ourios_tenants\n";
    let cases: [(&str, String); 3] = [
        (
            "auth.oidc.audience",
            "auth:\n  oidc:\n    issuer: https://dex.internal.example\n    tenant_claim: ourios_tenants\n".to_string(),
        ),
        ("tokens, oidc, or both", "auth: {}\n".to_string()),
        ("auth.tokens", format!("auth:\n  tokens: []\n{oidc}")),
    ];
    for (needle, auth_section) in cases {
        let tmp = tempfile::TempDir::new().expect("temp");
        let config_path = tmp.path().join("ourios.yaml");
        let mut file = std::fs::File::create(&config_path).expect("create config");
        write!(
            file,
            "storage:\n  local:\n    bucket_root: {}\n{auth_section}",
            tmp.path().display(),
        )
        .expect("write config");

        let output = timeout(
            Duration::from_secs(15),
            Command::new(env!("CARGO_BIN_EXE_ourios-server"))
                .arg("--config")
                .arg(&config_path)
                // If the startup error ever regressed into a running
                // server, the timeout would drop this future — don't
                // leave that child behind.
                .kill_on_drop(true)
                .output(),
        )
        .await
        .expect("server exits before timeout")
        .expect("run ourios-server");

        assert!(
            !output.status.success(),
            "{needle}: the config must fail startup, got {:?}",
            output.status,
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(needle), "names the rule: {stderr}");
    }
}

/// Scenario RFC0029.1 (oidc-only serves, enforced) — an `auth` section
/// with only `oidc` starts and serves, and the gates stay *enforced*:
/// until the verifier lands, no bearer matches, so a query without one
/// is 401 — never open mode (and no open-mode warning is emitted).
/// See `docs/rfcs/0029-oidc-bearer-layer.md` §5.
#[tokio::test]
async fn rfc0029_1_oidc_only_starts_and_enforces() {
    // The querier role resolves OIDC at startup now (the binding slice),
    // so an oidc-only config needs a reachable issuer: the loopback
    // fixture (discovery + JWKS), same as the receiver arms.
    let (_, jwk) = ingest_binding::make_key("key-1");
    let issuer = ingest_binding::serve_issuer(jwk).await;
    let tmp = tempfile::TempDir::new().expect("temp");
    let config_path = tmp.path().join("ourios.yaml");
    let mut file = std::fs::File::create(&config_path).expect("create config");
    write!(
        file,
        "storage:\n  local:\n    bucket_root: {}\n\
         querier:\n  enabled: true\n  http_addr: 127.0.0.1:0\n\
         auth:\n  oidc:\n    issuer: {}\n    audience: ourios\n    tenant_claim: ourios_tenants\n",
        tmp.path().display(),
        issuer,
    )
    .expect("write config");

    let mut child = Command::new(env!("CARGO_BIN_EXE_ourios-server"))
        .arg("--config")
        .arg(&config_path)
        .env("RUST_LOG", "info")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn ourios-server");

    // Buffer stderr concurrently (an undrained pipe can block the child);
    // the collected lines feed the no-open-mode-warning assertion below.
    let stderr = child.stderr.take().expect("stderr piped");
    let drain = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        let mut collected = Vec::new();
        while let Some(line) = lines.next_line().await.expect("read stderr") {
            collected.push(line);
        }
        collected
    });

    let stdout = child.stdout.take().expect("stdout piped");
    let mut out_lines = BufReader::new(stdout).lines();
    let querier_addr: std::net::SocketAddr = timeout(Duration::from_secs(15), async {
        while let Some(line) = out_lines.next_line().await.expect("read stdout") {
            if let Some(addr) = line.strip_prefix("querier HTTP listening on ") {
                return addr.trim().parse().expect("parse announced addr");
            }
        }
        panic!("querier line never appeared — oidc-only must serve");
    })
    .await
    .expect("server ready before timeout");

    // Enforced, not open: a query with no bearer is 401 on the wire.
    let mut stream = tokio::net::TcpStream::connect(querier_addr)
        .await
        .expect("oidc-only querier accepts a connection");
    stream
        .write_all(
            b"POST /v1/query HTTP/1.1\r\nHost: 127.0.0.1\r\n\
              Content-Type: application/json\r\nContent-Length: 2\r\n\
              Connection: close\r\n\r\n{}",
        )
        .await
        .expect("write request");
    let mut response = String::new();
    timeout(
        Duration::from_secs(15),
        stream.read_to_string(&mut response),
    )
    .await
    .expect("response before timeout")
    .expect("read response");
    assert!(
        response.starts_with("HTTP/1.1 401 "),
        "oidc-only enforces (401), never open: {response}",
    );

    child.kill().await.expect("kill the server");
    let collected = timeout(Duration::from_secs(15), drain)
        .await
        .expect("stderr drains before timeout")
        .expect("drain task");
    assert!(
        !collected.iter().any(|l| l.contains("RFC 0026 open mode")),
        "auth is configured — no open-mode warning",
    );
}

/// Scenario RFC0029.2 — verification matrix.
/// See `docs/rfcs/0029-oidc-bearer-layer.md` §5.
#[test]
#[ignore = "RFC0029.2 discharged — `ourios_core::auth::oidc::tests::rfc0029_2_verification_matrix` is the oracle (every arm, one undifferentiated None); the pre-decode/nothing-reaches-the-WAL half is the served ingest_binding arm here"]
fn rfc0029_2_verification_matrix() {}

/// Scenario RFC0029.6 — JWKS rotation.
/// See `docs/rfcs/0029-oidc-bearer-layer.md` §5.
#[test]
#[ignore = "RFC0029.6 discharged — `ourios_core::auth::oidc::tests::rfc0029_6_jwks_rotation` is the oracle (unseen-kid refetch under the real throttle; withdrawn-kid rejection)"]
fn rfc0029_6_jwks_rotation() {}

// RFC0029.7 lives in `mod dex` below (CI-gated: needs Docker).

// --- RFC 0029 §3.3 ingest-binding arms (the verifier + tower-layer slice):
// a loopback fixture issuer, ES256 minting with a runtime-generated key
// (the ourios-core fixture policy — no committed private keys), and the
// spawned binary enforcing OIDC-resolved bindings on both listeners.

mod ingest_binding {
    use std::io::Write as _;
    use std::time::Duration;

    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use jsonwebtoken::EncodingKey;
    use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
    use opentelemetry_proto::tonic::collector::logs::v1::logs_service_client::LogsServiceClient;
    use opentelemetry_proto::tonic::common::v1::any_value::Value;
    use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue};
    use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
    use opentelemetry_proto::tonic::resource::v1::Resource;
    use p256::ecdsa::SigningKey;
    use p256::pkcs8::EncodePrivateKey as _;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::process::Command;
    use tokio::time::timeout;

    /// A fresh ES256 keypair (runtime-generated) and its public JWK.
    pub(super) fn make_key(kid: &str) -> (EncodingKey, serde_json::Value) {
        let signing = SigningKey::random(&mut rand::rngs::OsRng);
        let pem = signing
            .to_pkcs8_pem(p256::pkcs8::LineEnding::LF)
            .expect("pkcs8 pem");
        let encoding = EncodingKey::from_ec_pem(pem.as_bytes()).expect("encoding key");
        let point = signing.verifying_key().to_encoded_point(false);
        let jwk = serde_json::json!({
            "kty": "EC", "crv": "P-256", "use": "sig", "alg": "ES256", "kid": kid,
            "x": URL_SAFE_NO_PAD.encode(point.x().expect("x")),
            "y": URL_SAFE_NO_PAD.encode(point.y().expect("y")),
        });
        (encoding, jwk)
    }

    /// A loopback issuer serving discovery + a fixed JWKS.
    pub(super) async fn serve_issuer(jwk: serde_json::Value) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fixture issuer");
        let issuer = format!("http://{}", listener.local_addr().expect("addr"));
        let discovery = serde_json::json!({
            "issuer": issuer,
            "jwks_uri": format!("{issuer}/jwks"),
        });
        let jwks = serde_json::json!({ "keys": [jwk] });
        // Plain-string JSON responses: the server's axum feature set has
        // no `json` (the fixture doesn't need it).
        let json = |body: String| ([("content-type", "application/json")], body);
        let app = axum::Router::new()
            .route(
                "/.well-known/openid-configuration",
                axum::routing::get({
                    let discovery = discovery.to_string();
                    move || async move { json(discovery) }
                }),
            )
            .route(
                "/jwks",
                axum::routing::get({
                    let jwks = jwks.to_string();
                    move || async move { json(jwks) }
                }),
            );
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve issuer");
        });
        issuer
    }

    pub(super) fn mint(
        encoding: &EncodingKey,
        kid: &str,
        issuer: &str,
        tenants: &[&str],
    ) -> String {
        let now = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("epoch")
                .as_secs(),
        )
        .expect("fits");
        let claims = serde_json::json!({
            "iss": issuer, "aud": "ourios", "exp": now + 600,
            "sub": "edge-collector", "ourios_tenants": tenants,
        });
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::ES256);
        header.kid = Some(kid.to_string());
        jsonwebtoken::encode(&header, &claims, encoding).expect("mint")
    }

    /// One `ResourceLogs` batch whose tenant derives from `service.name`.
    pub(super) fn batch(tenant: &str) -> ExportLogsServiceRequest {
        ExportLogsServiceRequest {
            resource_logs: vec![ResourceLogs {
                resource: Some(Resource {
                    attributes: vec![KeyValue {
                        key: "service.name".to_string(),
                        value: Some(AnyValue {
                            value: Some(Value::StringValue(tenant.to_string())),
                        }),
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
                scope_logs: vec![ScopeLogs {
                    log_records: vec![LogRecord {
                        time_unix_nano: 1_775_127_480_000_000_000,
                        body: Some(AnyValue {
                            value: Some(Value::StringValue("user logged in".to_string())),
                        }),
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        }
    }

    /// The §3.3 ingest binding on the served binary: startup discovery
    /// against the fixture issuer; a bearer-less gRPC export is
    /// UNAUTHENTICATED before decode; a verified JWT ingests within its
    /// tenant claim and is whole-batch denied outside it; the HTTP
    /// listener 401s a bearer-less POST through the same resolver.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn oidc_ingest_binding_enforces_on_the_served_binary() {
        let (encoding, jwk) = make_key("key-1");
        let issuer = serve_issuer(jwk).await;

        let tmp = tempfile::TempDir::new().expect("temp");
        let wal = tmp.path().join("wal");
        std::fs::create_dir_all(&wal).expect("wal dir");
        let config_path = tmp.path().join("ourios.yaml");
        let mut file = std::fs::File::create(&config_path).expect("create config");
        write!(
            file,
            "storage:\n  local:\n    bucket_root: {}\n\
             receiver:\n  enabled: true\n  grpc_addr: 127.0.0.1:0\n  http_addr: 127.0.0.1:0\n  wal_root: {}\n\
             auth:\n  oidc:\n    issuer: {}\n    audience: ourios\n    tenant_claim: ourios_tenants\n",
            tmp.path().display(),
            wal.display(),
            issuer,
        )
        .expect("write config");

        let mut child = Command::new(env!("CARGO_BIN_EXE_ourios-server"))
            .arg("--config")
            .arg(&config_path)
            .env("RUST_LOG", "info")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn ourios-server");

        let stdout = child.stdout.take().expect("stdout piped");
        let mut out_lines = BufReader::new(stdout).lines();
        let (grpc_addr, http_addr) = timeout(Duration::from_secs(15), async {
            let mut grpc = None;
            let mut http = None;
            while let Some(line) = out_lines.next_line().await.expect("read stdout") {
                if let Some(addr) = line.strip_prefix("receiver gRPC listening on ") {
                    grpc = Some(addr.trim().to_string());
                }
                if let Some(addr) = line.strip_prefix("receiver HTTP listening on ") {
                    http = Some(addr.trim().to_string());
                }
                if let (Some(g), Some(h)) = (&grpc, &http) {
                    return (g.clone(), h.clone());
                }
            }
            panic!("receiver lines never appeared — discovery must succeed at startup");
        })
        .await
        .expect("server ready before timeout");

        let mut client = LogsServiceClient::connect(format!("http://{grpc_addr}"))
            .await
            .expect("grpc connect");

        // Bearer-less: UNAUTHENTICATED from the auth layer, pre-decode.
        let status = client
            .export(tonic::Request::new(batch("acme")))
            .await
            .expect_err("no bearer is rejected");
        assert_eq!(status.code(), tonic::Code::Unauthenticated);

        // A verified JWT bound to ["acme"]: in-set ingests…
        let token = mint(&encoding, "key-1", &issuer, &["acme"]);
        let authorization: tonic::metadata::MetadataValue<_> =
            format!("Bearer {token}").parse().expect("metadata");
        let mut request = tonic::Request::new(batch("acme"));
        request
            .metadata_mut()
            .insert("authorization", authorization.clone());
        client.export(request).await.expect("in-set batch acks");

        // …and an out-of-set tenant is whole-batch denied (§3.2 —
        // RFC 0026 semantics with the OIDC-resolved binding).
        let mut request = tonic::Request::new(batch("globex"));
        request
            .metadata_mut()
            .insert("authorization", authorization);
        let status = client
            .export(request)
            .await
            .expect_err("out-of-set tenant is denied");
        assert_eq!(status.code(), tonic::Code::PermissionDenied);

        // The HTTP listener runs the same resolver: bearer-less POST → 401.
        let mut stream = tokio::net::TcpStream::connect(&http_addr)
            .await
            .expect("http connect");
        stream
            .write_all(
                b"POST /v1/logs HTTP/1.1\r\nHost: 127.0.0.1\r\n\
                  Content-Type: application/json\r\nContent-Length: 2\r\n\
                  Connection: close\r\n\r\n{}",
            )
            .await
            .expect("write request");
        let mut response = String::new();
        timeout(
            Duration::from_secs(15),
            stream.read_to_string(&mut response),
        )
        .await
        .expect("response before timeout")
        .expect("read response");
        assert!(
            response.starts_with("HTTP/1.1 401 "),
            "bearer-less HTTP ingest is 401: {response}",
        );

        child.kill().await.expect("kill the server");
    }

    /// §3.2: OIDC discovery failure is a startup error, not a degraded
    /// mode — a receiver configured against an unreachable issuer exits
    /// nonzero naming `auth.oidc`.
    #[tokio::test]
    async fn oidc_unreachable_issuer_fails_receiver_startup() {
        // A held loopback port that deterministically fails every
        // connection (accept-then-close). Holding the listener for the
        // test's lifetime avoids the race where a dropped port is re-bound
        // by another local process before the child runs discovery; no
        // DNS or egress dependency either way.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let unreachable = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                drop(stream);
            }
        });
        let tmp = tempfile::TempDir::new().expect("temp");
        let wal = tmp.path().join("wal");
        std::fs::create_dir_all(&wal).expect("wal dir");
        let config_path = tmp.path().join("ourios.yaml");
        let mut file = std::fs::File::create(&config_path).expect("create config");
        write!(
            file,
            "storage:\n  local:\n    bucket_root: {}\n\
             receiver:\n  enabled: true\n  grpc_addr: 127.0.0.1:0\n  http_addr: 127.0.0.1:0\n  wal_root: {}\n\
             auth:\n  oidc:\n    issuer: {}\n    audience: ourios\n    tenant_claim: ourios_tenants\n",
            tmp.path().display(),
            wal.display(),
            unreachable,
        )
        .expect("write config");

        let output = Command::new(env!("CARGO_BIN_EXE_ourios-server"))
            .arg("--config")
            .arg(&config_path)
            .output()
            .await
            .expect("run ourios-server");
        assert!(
            !output.status.success(),
            "an unreachable issuer must fail startup"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("auth.oidc"),
            "names the failing key: {stderr}"
        );
    }
}

// --- RFC 0029 §5 .3/.4/.5 — the query/MCP binding slice: the OIDC-resolved
// binding drives the RFC 0026 contracts verbatim on the served binary.

mod claim_binding {
    use std::io::Write as _;
    use std::time::Duration;

    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::process::Command;
    use tokio::time::timeout;

    use super::ingest_binding::{batch, make_key, mint, serve_issuer};

    /// Spawn the binary with receiver+querier and the given `auth` YAML
    /// block; return (child, grpc, http-receiver, http-querier).
    async fn spawn_with_auth(
        tmp: &tempfile::TempDir,
        auth_yaml: &str,
        envs: &[(&str, &str)],
    ) -> (tokio::process::Child, String, String, std::net::SocketAddr) {
        let wal = tmp.path().join("wal");
        std::fs::create_dir_all(&wal).expect("wal dir");
        let config_path = tmp.path().join("ourios.yaml");
        let mut file = std::fs::File::create(&config_path).expect("create config");
        write!(
            file,
            "storage:\n  local:\n    bucket_root: {}\n\
             receiver:\n  enabled: true\n  grpc_addr: 127.0.0.1:0\n  http_addr: 127.0.0.1:0\n  wal_root: {}\n\
             querier:\n  enabled: true\n  http_addr: 127.0.0.1:0\n\
             {auth_yaml}",
            tmp.path().display(),
            wal.display(),
        )
        .expect("write config");

        let mut command = Command::new(env!("CARGO_BIN_EXE_ourios-server"));
        command
            .arg("--config")
            .arg(&config_path)
            .env("RUST_LOG", "info")
            .stdout(std::process::Stdio::piped())
            // Inherited so a pre-announcement startup failure lands the
            // child's actual error in the test output, not a bare timeout.
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true);
        for (key, value) in envs {
            command.env(key, value);
        }
        let mut child = command.spawn().expect("spawn ourios-server");
        let stdout = child.stdout.take().expect("stdout piped");
        let mut out_lines = BufReader::new(stdout).lines();
        let (grpc, http, querier) = timeout(Duration::from_secs(15), async {
            let (mut g, mut h, mut q) = (None, None, None);
            while let Some(line) = out_lines.next_line().await.expect("read stdout") {
                if let Some(a) = line.strip_prefix("receiver gRPC listening on ") {
                    g = Some(a.trim().to_string());
                }
                if let Some(a) = line.strip_prefix("receiver HTTP listening on ") {
                    h = Some(a.trim().to_string());
                }
                if let Some(a) = line.strip_prefix("querier HTTP listening on ") {
                    q = Some(a.trim().parse().expect("addr"));
                }
                if let (Some(g), Some(h), Some(q)) = (&g, &h, &q) {
                    return (g.clone(), h.clone(), *q);
                }
            }
            panic!("role announcements never appeared");
        })
        .await
        .expect("server ready before timeout");
        (child, grpc, http, querier)
    }

    /// Raw `POST /v1/query` with optional bearer + tenant headers; returns
    /// the status line.
    async fn query_status(
        addr: std::net::SocketAddr,
        bearer: Option<&str>,
        tenant: Option<&str>,
    ) -> String {
        use std::fmt::Write as _;
        let mut request = String::from("POST /v1/query HTTP/1.1\r\nHost: 127.0.0.1\r\n");
        if let Some(b) = bearer {
            write!(request, "Authorization: Bearer {b}\r\n").expect("write header");
        }
        if let Some(t) = tenant {
            write!(request, "x-ourios-tenant: {t}\r\n").expect("write header");
        }
        request.push_str(
            "Content-Type: text/plain\r\nContent-Length: 16\r\nConnection: close\r\n\r\ntemplate_id == 1",
        );
        let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
        stream.write_all(request.as_bytes()).await.expect("write");
        let mut response = String::new();
        timeout(
            Duration::from_secs(15),
            stream.read_to_string(&mut response),
        )
        .await
        .expect("response before timeout")
        .expect("read");
        response.lines().next().unwrap_or_default().to_string()
    }

    /// Scenario RFC0029.3 — claim binding drives unchanged enforcement:
    /// with `tenant_claim` = `["a", "b"]`, the RFC 0026 §5.3/§5.4
    /// contracts hold verbatim with the OIDC-resolved binding — in-set
    /// ingest acks, out-of-set is whole-batch denied before the WAL, and
    /// the query surface enforces 401 → 400 → 403 in order. (The
    /// `name_claim` → name-label arm is the resolver's one-line mapping,
    /// pinned by the core verifier tests.)
    /// See `docs/rfcs/0029-oidc-bearer-layer.md` §5.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rfc0029_3_claim_binding_enforcement() {
        use opentelemetry_proto::tonic::collector::logs::v1::logs_service_client::LogsServiceClient;

        let (encoding, jwk) = make_key("key-1");
        let issuer = serve_issuer(jwk).await;
        let tmp = tempfile::TempDir::new().expect("temp");
        let auth = format!(
            "auth:\n  oidc:\n    issuer: {issuer}\n    audience: ourios\n    tenant_claim: ourios_tenants\n"
        );
        let (mut child, grpc, _http, querier) = spawn_with_auth(&tmp, &auth, &[]).await;

        let token = mint(&encoding, "key-1", &issuer, &["a", "b"]);

        // Ingest: in-set acks; out-of-set is whole-batch denied.
        let mut client = LogsServiceClient::connect(format!("http://{grpc}"))
            .await
            .expect("grpc connect");
        let authorization: tonic::metadata::MetadataValue<_> =
            format!("Bearer {token}").parse().expect("metadata");
        let mut request = tonic::Request::new(batch("b"));
        request
            .metadata_mut()
            .insert("authorization", authorization.clone());
        client.export(request).await.expect("in-set batch acks");
        let mut request = tonic::Request::new(batch("c"));
        request
            .metadata_mut()
            .insert("authorization", authorization);
        let status = client
            .export(request)
            .await
            .expect_err("out-of-set tenant is denied");
        assert_eq!(status.code(), tonic::Code::PermissionDenied);

        // Query: 401 (no bearer) → 400 (bearer, no tenant) → 403
        // (bearer, out-of-set tenant) → 200 (in-set).
        assert!(
            query_status(querier, None, Some("a")).await.contains("401"),
            "authentication answers first"
        );
        assert!(
            query_status(querier, Some(&token), None)
                .await
                .contains("400"),
            "then the tenant contract"
        );
        assert!(
            query_status(querier, Some(&token), Some("c"))
                .await
                .contains("403"),
            "then the binding"
        );
        assert!(
            query_status(querier, Some(&token), Some("a"))
                .await
                .contains("200"),
            "in-set queries serve"
        );

        child.kill().await.expect("kill the server");
    }

    /// Scenario RFC0029.4 — wildcard claim: `["*"]` behaves as if every
    /// tenant were listed (RFC 0026 §5.5 parity) on ingest and query.
    /// See `docs/rfcs/0029-oidc-bearer-layer.md` §5.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rfc0029_4_wildcard_claim() {
        use opentelemetry_proto::tonic::collector::logs::v1::logs_service_client::LogsServiceClient;

        let (encoding, jwk) = make_key("key-1");
        let issuer = serve_issuer(jwk).await;
        let tmp = tempfile::TempDir::new().expect("temp");
        let auth = format!(
            "auth:\n  oidc:\n    issuer: {issuer}\n    audience: ourios\n    tenant_claim: ourios_tenants\n"
        );
        let (mut child, grpc, _http, querier) = spawn_with_auth(&tmp, &auth, &[]).await;

        let token = mint(&encoding, "key-1", &issuer, &["*"]);
        let mut client = LogsServiceClient::connect(format!("http://{grpc}"))
            .await
            .expect("grpc connect");
        for tenant in ["alpha", "beta", "entirely-new-tenant"] {
            let authorization: tonic::metadata::MetadataValue<_> =
                format!("Bearer {token}").parse().expect("metadata");
            let mut request = tonic::Request::new(batch(tenant));
            request
                .metadata_mut()
                .insert("authorization", authorization);
            client
                .export(request)
                .await
                .unwrap_or_else(|e| panic!("wildcard ingests {tenant}: {e}"));
            assert!(
                query_status(querier, Some(&token), Some(tenant))
                    .await
                    .contains("200"),
                "wildcard queries {tenant}"
            );
        }
        child.kill().await.expect("kill the server");
    }

    /// Scenario RFC0029.5 — coexistence and resolution order: one config
    /// with both `tokens` and `oidc`; a static bearer and a JWT each
    /// authenticate via their own path, carrying their own binding. (The
    /// static-only serving arm is the RFC 0026 suite; the oidc-only arm is
    /// `rfc0029_1_oidc_only_starts_and_enforces`; the no-`auth` open-mode
    /// parity arm is `rfc0026_6_open_mode_parity` — all unchanged.)
    /// See `docs/rfcs/0029-oidc-bearer-layer.md` §5.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rfc0029_5_coexistence_and_resolution_order() {
        use opentelemetry_proto::tonic::collector::logs::v1::logs_service_client::LogsServiceClient;

        let (encoding, jwk) = make_key("key-1");
        let issuer = serve_issuer(jwk).await;
        let tmp = tempfile::TempDir::new().expect("temp");
        // RFC 0026 §3.1 secret hygiene: the static token must be an
        // ${env:…} reference, resolved from the child's environment.
        let auth = format!(
            "auth:\n  tokens:\n    - name: edge-collector\n      token: ${{env:EDGE_TOK}}\n      tenants: [acme]\n\
             \x20\x20oidc:\n    issuer: {issuer}\n    audience: ourios\n    tenant_claim: ourios_tenants\n"
        );
        let (mut child, grpc, _http, querier) =
            spawn_with_auth(&tmp, &auth, &[("EDGE_TOK", "tok-edge")]).await;

        let jwt = mint(&encoding, "key-1", &issuer, &["globex"]);
        let mut client = LogsServiceClient::connect(format!("http://{grpc}"))
            .await
            .expect("grpc connect");

        // Each credential authenticates via its own path, each with its
        // own binding: the static token speaks for acme only, the JWT for
        // globex only — in-set acks, cross-binding denies.
        for (label, bearer, in_set, out_of_set) in [
            ("static", "tok-edge".to_string(), "acme", "globex"),
            ("oidc", jwt.clone(), "globex", "acme"),
        ] {
            let authorization: tonic::metadata::MetadataValue<_> =
                format!("Bearer {bearer}").parse().expect("metadata");
            let mut request = tonic::Request::new(batch(in_set));
            request
                .metadata_mut()
                .insert("authorization", authorization.clone());
            client
                .export(request)
                .await
                .unwrap_or_else(|e| panic!("{label}: in-set batch acks: {e}"));
            let mut request = tonic::Request::new(batch(out_of_set));
            request
                .metadata_mut()
                .insert("authorization", authorization);
            let status = client
                .export(request)
                .await
                .expect_err("cross-binding tenant is denied");
            assert_eq!(status.code(), tonic::Code::PermissionDenied, "{label}");
        }

        // And side by side on the query surface.
        assert!(
            query_status(querier, Some("tok-edge"), Some("acme"))
                .await
                .contains("200"),
            "static binding queries acme"
        );
        assert!(
            query_status(querier, Some(&jwt), Some("globex"))
                .await
                .contains("200"),
            "oidc binding queries globex"
        );

        child.kill().await.expect("kill the server");
    }
}

/// Scenario RFC0029.7 — Dex end-to-end with telemetry parity, against a
/// **real Dex container** (testcontainers; CI-gated like RFC 0019's
/// `s3 integration (localstack)` job — `#[ignore]`d in the default run).
///
/// Image note: the client-credentials grant and
/// `staticClients[].clientCredentialsClaims` are post-v2.45.1 (merged to
/// Dex master 2026-04; dexidp/dex#4691), so the test pins the `master`
/// image **by digest** for reproducibility. Bump to the release tag once
/// Dex v2.46 ships.
mod dex {
    use std::io::Write as _;
    use std::time::Duration;

    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::process::Command;
    use tokio::time::timeout;

    use super::ingest_binding::batch;

    const DEX_IMAGE: &str = "ghcr.io/dexidp/dex";
    const DEX_TAG: &str =
        "master@sha256:c382922b8f065f2f1ba142fde5b0ec1736b8fb7bc5bf18832f68c9aced95f243";
    /// The static client: its id is the token audience, its `name` is the
    /// `name_claim` label, its claims carry the tenant list.
    const CLIENT_ID: &str = "ourios-collector";
    const CLIENT_NAME: &str = "Edge Collector";
    const CLIENT_SECRET: &str = "dex-test-secret";

    /// Scenario RFC0029.7. See `docs/rfcs/0029-oidc-bearer-layer.md` §5.
    // One linear served scenario — container, mint, then the §5 arms in
    // their specified order; fragmenting into helpers would hide that
    // order (the cluster.rs precedent for this lint).
    #[allow(clippy::too_many_lines)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "RFC0029.7 — needs Docker (real Dex container); run by the dex-oidc CI job via --ignored"]
    async fn rfc0029_7_dex_end_to_end() {
        use opentelemetry_proto::tonic::collector::logs::v1::logs_service_client::LogsServiceClient;
        use testcontainers_modules::testcontainers::core::ContainerPort;
        use testcontainers_modules::testcontainers::runners::AsyncRunner;
        use testcontainers_modules::testcontainers::{GenericImage, ImageExt};

        // Reserve a host port up front: the verifier enforces issuer
        // equality against the discovery document, so the issuer URL must
        // be known before the container starts. Reservation is
        // inherently racy (the listener drops before Docker binds), so a
        // failed container start retries on a fresh port.
        fn reserve_port() -> u16 {
            let l = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve port");
            l.local_addr().expect("addr").port()
        }

        // Short-TTL tokens make the expiry arm real time rather than
        // clock-skew fiction: the pre-expiry arms complete in milliseconds
        // and 20 s leaves headroom for a slow runner, then the expiry wait
        // sleeps past the token's own `expires_in`. `enablePasswordDB` is
        // the inert built-in connector — Dex refuses to start with zero
        // connectors, though client-credentials never touches one.
        let dex_config_for = |issuer: &str| {
            format!(
                "issuer: {issuer}\n\
             storage:\n  type: memory\n\
             web:\n  http: 0.0.0.0:5556\n\
             enablePasswordDB: true\n\
             oauth2:\n  grantTypes: [\"client_credentials\"]\n\
             expiry:\n  idTokens: \"20s\"\n\
             staticClients:\n\
             \x20\x20- id: {CLIENT_ID}\n\
             \x20\x20\x20\x20name: {CLIENT_NAME}\n\
             \x20\x20\x20\x20secret: {CLIENT_SECRET}\n\
             \x20\x20\x20\x20clientCredentialsClaims:\n\
             \x20\x20\x20\x20\x20\x20groups: [\"acme\", \"globex\"]\n"
            )
        };
        let mut started = None;
        for attempt in 0..3 {
            let host_port = reserve_port();
            let issuer = format!("http://127.0.0.1:{host_port}/dex");
            match GenericImage::new(DEX_IMAGE, DEX_TAG)
                .with_copy_to(
                    "/etc/dex/config.docker.yaml",
                    dex_config_for(&issuer).into_bytes(),
                )
                .with_env_var("DEX_CLIENT_CREDENTIAL_GRANT_ENABLED_BY_DEFAULT", "true")
                .with_mapped_port(host_port, ContainerPort::Tcp(5556))
                .start()
                .await
            {
                Ok(container) => {
                    started = Some((container, issuer));
                    break;
                }
                Err(e) if attempt < 2 => {
                    eprintln!("dex start attempt {attempt} failed (port race?): {e}");
                }
                Err(e) => panic!("dex never started: {e}"),
            }
        }
        let (container, issuer) = started.expect("dex started");

        // Readiness: Dex's own discovery document, served under the
        // reserved issuer URL. On timeout, surface the container's own
        // logs — a config rejection otherwise reads as a bare timeout.
        let http = reqwest::Client::new();
        let discovery_url = format!("{issuer}/.well-known/openid-configuration");
        if timeout(Duration::from_secs(90), async {
            loop {
                if let Ok(response) = http.get(&discovery_url).send().await
                    && response.status().is_success()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        })
        .await
        .is_err()
        {
            let stdout = container
                .stdout_to_vec()
                .await
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .unwrap_or_default();
            let stderr = container
                .stderr_to_vec()
                .await
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .unwrap_or_default();
            panic!(
                "dex never became ready.\n--- dex stdout ---\n{stdout}\n--- dex stderr ---\n{stderr}"
            );
        }

        // The served instance verifies against Dex's real JWKS. Zero clock
        // skew so the expiry arm crosses at exactly the token TTL; MCP on.
        let tmp = tempfile::TempDir::new().expect("temp");
        let wal = tmp.path().join("wal");
        std::fs::create_dir_all(&wal).expect("wal dir");
        let config_path = tmp.path().join("ourios.yaml");
        let mut file = std::fs::File::create(&config_path).expect("create config");
        write!(
            file,
            "storage:\n  local:\n    bucket_root: {}\n\
             receiver:\n  enabled: true\n  grpc_addr: 127.0.0.1:0\n  http_addr: 127.0.0.1:0\n  wal_root: {}\n\
             querier:\n  enabled: true\n  http_addr: 127.0.0.1:0\n  mcp:\n    enabled: true\n\
             auth:\n  oidc:\n    issuer: {issuer}\n    audience: {CLIENT_ID}\n    tenant_claim: groups\n    name_claim: name\n    clock_skew_secs: 0\n",
            tmp.path().display(),
            wal.display(),
        )
        .expect("write config");

        let mut child = Command::new(env!("CARGO_BIN_EXE_ourios-server"))
            .arg("--config")
            .arg(&config_path)
            .env("RUST_LOG", "info")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn ourios-server");
        // Drain stderr into a buffer: the no-JWT-material arm scans it,
        // and an undrained pipe could block the child.
        let stderr = child.stderr.take().expect("stderr piped");
        let drain = tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            let mut collected = Vec::new();
            while let Some(line) = lines.next_line().await.expect("read stderr") {
                collected.push(line);
            }
            collected
        });
        let stdout = child.stdout.take().expect("stdout piped");
        let mut out_lines = BufReader::new(stdout).lines();
        let (grpc_addr, querier_addr) = timeout(Duration::from_secs(15), async {
            let (mut grpc, mut querier) = (None, None);
            while let Some(line) = out_lines.next_line().await.expect("read stdout") {
                if let Some(addr) = line.strip_prefix("receiver gRPC listening on ") {
                    grpc = Some(addr.trim().to_string());
                }
                if let Some(addr) = line.strip_prefix("querier HTTP listening on ") {
                    querier = Some(addr.trim().to_string());
                }
                if let (Some(g), Some(q)) = (&grpc, &querier) {
                    return (g.clone(), q.clone());
                }
            }
            panic!("role announcements never appeared — discovery against dex must succeed");
        })
        .await
        .expect("server ready before timeout");

        // Mint a real token from Dex's token endpoint — the OTel
        // Collector `oauth2client` flow, verbatim.
        let token_response: serde_json::Value = http
            .post(format!("{issuer}/token"))
            .basic_auth(CLIENT_ID, Some(CLIENT_SECRET))
            .form(&[
                ("grant_type", "client_credentials"),
                ("scope", "openid profile groups"),
            ])
            .send()
            .await
            .expect("token endpoint")
            .json()
            .await
            .expect("token json");
        let token = token_response["access_token"]
            .as_str()
            .expect("access_token in the response")
            .to_string();
        let expires_in = token_response["expires_in"]
            .as_u64()
            .expect("expires_in in the response");
        let minted_at = tokio::time::Instant::now();

        // Ingest: in-claim tenant acks; a cross-tenant batch is denied
        // (the audit arm below reads the denial event back).
        let mut client = LogsServiceClient::connect(format!("http://{grpc_addr}"))
            .await
            .expect("grpc connect");
        let authorization: tonic::metadata::MetadataValue<_> =
            format!("Bearer {token}").parse().expect("metadata");
        let mut request = tonic::Request::new(batch("acme"));
        request
            .metadata_mut()
            .insert("authorization", authorization.clone());
        client.export(request).await.expect("in-claim batch acks");
        let mut request = tonic::Request::new(batch("intruder"));
        request
            .metadata_mut()
            .insert("authorization", authorization.clone());
        let denied = client
            .export(request)
            .await
            .expect_err("cross-tenant batch is denied");
        assert_eq!(denied.code(), tonic::Code::PermissionDenied);
        assert!(
            !denied.message().contains(&token),
            "no JWT material in the denial"
        );

        // Query: the same token reads its own tenants.
        let query = |bearer: Option<String>, tenant: &'static str| {
            let http = http.clone();
            let url = format!("http://{querier_addr}/v1/query");
            async move {
                let mut req = http
                    .post(url)
                    .header("content-type", "text/plain")
                    .header("x-ourios-tenant", tenant)
                    .body("template_id == 1");
                if let Some(b) = bearer {
                    req = req.header("authorization", format!("Bearer {b}"));
                }
                req.send().await.expect("query send")
            }
        };
        assert_eq!(
            query(Some(token.clone()), "globex").await.status(),
            reqwest::StatusCode::OK,
            "in-claim query serves"
        );

        // MCP: the same bearer passes the transport gate; bearer-less is
        // the one undifferentiated 401.
        let mcp_url = format!("http://{querier_addr}/mcp");
        let initialize = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-03-26",
                        "capabilities": {},
                        "clientInfo": {"name": "rfc0029-7", "version": "0"}}
        });
        let unauthenticated = http
            .post(&mcp_url)
            .json(&initialize)
            .header("accept", "application/json, text/event-stream")
            .send()
            .await
            .expect("mcp send");
        assert_eq!(
            unauthenticated.status(),
            reqwest::StatusCode::UNAUTHORIZED,
            "mcp without a bearer is 401"
        );
        let authenticated = http
            .post(&mcp_url)
            .bearer_auth(&token)
            .json(&initialize)
            .header("accept", "application/json, text/event-stream")
            .send()
            .await
            .expect("mcp send");
        assert!(
            authenticated.status().is_success(),
            "the dex bearer passes the MCP gate: {}",
            authenticated.status()
        );

        // Expiry: sleep past the token's own `expires_in` (zero configured
        // skew) and the same token collapses to the undifferentiated 401.
        tokio::time::sleep_until(minted_at + Duration::from_secs(expires_in + 2)).await;
        let expired = query(Some(token.clone()), "globex").await;
        assert_eq!(
            expired.status(),
            reqwest::StatusCode::UNAUTHORIZED,
            "an expired dex token is the one undifferentiated 401"
        );
        assert!(
            !expired.text().await.expect("body").contains(&token),
            "no JWT material in the 401 body"
        );

        // Graceful shutdown (SIGTERM — what k8s sends) flushes the audit
        // sink, making the denial event durable and readable.
        let pid = child.id().expect("child pid").to_string();
        let signalled = Command::new("kill")
            .args(["-TERM", &pid])
            .status()
            .await
            .expect("run kill");
        assert!(signalled.success(), "SIGTERM delivered");
        timeout(Duration::from_secs(15), child.wait())
            .await
            .expect("exit before timeout")
            .expect("child exits");
        let stderr_lines = timeout(Duration::from_secs(15), drain)
            .await
            .expect("stderr drains")
            .expect("drain task");

        // §3.4 telemetry parity, the audit half: the denial emitted
        // `ingest_denied` carrying the `name_claim` value (the client's
        // display name), and nothing on the log surface carries the JWT.
        let mut denied_names = Vec::new();
        for entry in walkdir(&tmp.path().join("audit")) {
            let events = ourios_parquet::AuditReader::open_file(&entry)
                .expect("open audit file")
                .read_all()
                .expect("read audit file");
            for event in events {
                if let ourios_core::audit::AuditPayload::IngestDenied { token_name } = event.payload
                {
                    denied_names.push(token_name);
                }
            }
        }
        assert_eq!(
            denied_names,
            [CLIENT_NAME],
            "ingest_denied carries the name_claim value"
        );
        assert!(
            !stderr_lines.iter().any(|line| line.contains(&token)),
            "no JWT material on the log surface"
        );
    }

    /// All `.parquet` files under `root`, recursively (empty when the
    /// directory does not exist).
    fn walkdir(root: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut files = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == "parquet") {
                    files.push(path);
                }
            }
        }
        files
    }
}
