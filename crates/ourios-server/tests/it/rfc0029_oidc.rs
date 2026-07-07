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
    let tmp = tempfile::TempDir::new().expect("temp");
    let config_path = tmp.path().join("ourios.yaml");
    let mut file = std::fs::File::create(&config_path).expect("create config");
    write!(
        file,
        "storage:\n  local:\n    bucket_root: {}\n\
         querier:\n  enabled: true\n  http_addr: 127.0.0.1:0\n\
         auth:\n  oidc:\n    issuer: https://dex.internal.example\n    audience: ourios\n    tenant_claim: ourios_tenants\n",
        tmp.path().display(),
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
#[ignore = "RFC0029.2 stub — implemented in the verifier green slice"]
fn rfc0029_2_verification_matrix() {
    todo!(
        "RFC0029.2 — fixture-issuer valid token accepted; expired / \
         nbf-beyond-skew / wrong-aud / wrong-iss / bad-sig / alg:none \
         / HMAC-downgrade / non-JWT all one undifferentiated 401 \
         before wire decode, nothing reaching the WAL"
    );
}

/// Scenario RFC0029.3 — claim binding drives unchanged enforcement.
/// See `docs/rfcs/0029-oidc-bearer-layer.md` §5.
#[test]
#[ignore = "RFC0029.3 stub — implemented in the binding green slice"]
fn rfc0029_3_claim_binding_enforcement() {
    todo!(
        "RFC0029.3 — tenant_claim [a, b]: RFC 0026 §5.3/§5.4 verbatim \
         with the OIDC-resolved binding — in-set ingest acks, any \
         out-of-set batch whole-batch 403 with no WAL append, \
         401→400→403 on query + MCP, name_claim as the name label"
    );
}

/// Scenario RFC0029.4 — wildcard claim.
/// See `docs/rfcs/0029-oidc-bearer-layer.md` §5.
#[test]
#[ignore = "RFC0029.4 stub — implemented in the binding green slice"]
fn rfc0029_4_wildcard_claim() {
    todo!(
        "RFC0029.4 — tenant_claim [\"*\"]: ingest and query to \
         arbitrary tenants as if every tenant were listed \
         (RFC 0026 §5.5 parity)"
    );
}

/// Scenario RFC0029.5 — coexistence and resolution order.
/// See `docs/rfcs/0029-oidc-bearer-layer.md` §5.
#[test]
#[ignore = "RFC0029.5 stub — implemented in the binding green slice"]
fn rfc0029_5_coexistence_and_resolution_order() {
    todo!(
        "RFC0029.5 — static + oidc side by side, each with its own \
         binding; static-only and oidc-only both serve; no auth \
         section passes the RFC 0026 §5.6 open-mode parity arm \
         unchanged"
    );
}

/// Scenario RFC0029.6 — JWKS rotation.
/// See `docs/rfcs/0029-oidc-bearer-layer.md` §5.
#[test]
#[ignore = "RFC0029.6 stub — implemented in the verifier green slice"]
fn rfc0029_6_jwks_rotation() {
    todo!(
        "RFC0029.6 — issuer rotates mid-run: unseen kid triggers a \
         JWKS re-fetch and the new-key token verifies without \
         restart; the withdrawn key's tokens are rejected once the \
         refreshed set drops it"
    );
}

/// Scenario RFC0029.7 — Dex end-to-end with telemetry parity.
/// See `docs/rfcs/0029-oidc-bearer-layer.md` §5.
#[test]
#[ignore = "RFC0029.7 stub — implemented in the dex acceptance green slice"]
fn rfc0029_7_dex_end_to_end() {
    todo!(
        "RFC0029.7 — real Dex container (testcontainers, CI-gated): \
         client-credentials mint drives ingest/query/MCP; short-TTL \
         expiry rejected as the undifferentiated 401; unchanged \
         error.type values; ingest_denied carries the name_claim \
         value; no JWT material on any surface"
    );
}

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
    fn make_key(kid: &str) -> (EncodingKey, serde_json::Value) {
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
    async fn serve_issuer(jwk: serde_json::Value) -> String {
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

    fn mint(encoding: &EncodingKey, kid: &str, issuer: &str, tenants: &[&str]) -> String {
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
    fn batch(tenant: &str) -> ExportLogsServiceRequest {
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
        // A just-closed loopback port (no DNS/egress dependency).
        let unreachable = {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            format!("http://{}", l.local_addr().expect("addr"))
        };
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
