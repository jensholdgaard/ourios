//! Real OpenTelemetry Collector → Ourios interop (RFC 0026 / 0029 / 0030).
//!
//! The unit tests and the RFC0029.7 Dex test prove Ourios *validates* an
//! OIDC bearer the way the Collector's `oauth2client` extension mints one
//! ("the flow, verbatim"). What they do **not** prove is that a real
//! `otelcol` binary — its OTLP exporter, its `configtls` client, its
//! `oauth2client` extension — actually interoperates with our receiver over
//! the wire. This test closes that gap: a real `otelcol-contrib` container
//! reads log lines from a file, names the tenant on the exporter (`headers:
//! x-ourios-tenant`, RFC 0046 §3.6) and stamps `service.name` via a `resource` processor,
//! and exports them over gRPC to a served Ourios instance with **TLS**
//! (server auth) and an **OIDC** bearer fetched from the same Dex fixture
//! RFC0029.7 uses. We then flush and read the store back, asserting the
//! Collector's lines landed under the JWT-derived tenant and reconstruct
//! bit-for-bit.
//!
//! Networking: the Collector runs in a container and must dial *in* to
//! Ourios on the host, so the receiver binds `0.0.0.0` and the exporter
//! targets `host.docker.internal` (a host-gateway alias); the server cert
//! carries that SAN. Dex publishes to a reserved host port and is reached
//! from the container via the same alias.
//!
//! Like RFC0029.7 this needs a Docker-API runtime, so it is `#[ignore]`d and
//! run by the `collector-interop` CI job via `--ignored --exact`.
//!
//! Unix-only: graceful shutdown is driven by `kill -TERM`, and the server's
//! SIGTERM handling is itself Unix-only (the `rfc0003_16` / `rfc0008_10`
//! served-binary precedent).
#![cfg(unix)]

use std::io::Write as _;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::time::timeout;

const DEX_IMAGE: &str = "ghcr.io/dexidp/dex";
// Pinned by digest for the same reason as RFC0029.7 (client_credentials +
// clientCredentialsClaims are post-v2.45.1). Keep in lockstep with the const
// in `rfc0029_oidc.rs`.
const DEX_TAG: &str =
    "master@sha256:c382922b8f065f2f1ba142fde5b0ec1736b8fb7bc5bf18832f68c9aced95f243";

// The contrib distribution carries `filelog`, `resource`, the
// `oauth2client` extension, and (from 0.159.0) the `drain` processor
// (the core distro carries none of them). Pinned by digest (the
// multi-arch manifest-list index for the release named below) so this
// required check never depends on a mutable tag — same posture as the
// Dex pin above.
const COLLECTOR_IMAGE: &str = "otel/opentelemetry-collector-contrib";
// 0.159.0 — the first contrib release whose distribution manifest carries
// the `drain` processor (RFC 0050's upstream templating source); the
// TLS + OIDC scenario rides the same pin so the file keeps one collector
// version. Digest-pinned (multi-arch index) like the Dex pin above.
const COLLECTOR_TAG: &str =
    "0.159.0@sha256:1f2c54a30e713fac6b3ae77a1ec84010c2007e29ced8ec666214fc2f6739c1cc";

/// Dex static client: id is the token audience, `name` is the `name_claim`
/// value, and its `groups` claim carries the tenant list.
const CLIENT_ID: &str = "ourios-collector";
const CLIENT_SECRET: &str = "dex-test-secret";
/// The tenant the Collector stamps onto every record (via a `resource`
/// processor) — Ourios derives the tenant from `service.name`, and this value
/// is in the token's `groups` claim so the ingest is authorised.
const TENANT: &str = "acme";

/// The lines the Collector reads from the mounted file and ships to Ourios.
/// They share one template ("user … logged in") yet must reconstruct
/// bit-for-bit out of the store (§3.3).
const APP_LINES: &[&str] = &[
    "user alice logged in",
    "user bob logged in",
    "user carol logged in",
];

/// Scenario: real Collector → Ourios over TLS + OIDC, logs land and
/// reconstruct — the composition of RFC 0026 (auth), RFC 0029 (OIDC
/// bearer), and RFC 0030 (TLS listeners) exercised by a real `otelcol`.
#[allow(clippy::too_many_lines)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Docker (real Dex + otelcol-contrib containers); run by the collector-interop CI job via --ignored"]
async fn collector_exports_over_tls_and_oidc() {
    use testcontainers_modules::testcontainers::core::{ContainerPort, Host};
    use testcontainers_modules::testcontainers::runners::AsyncRunner;
    use testcontainers_modules::testcontainers::{GenericImage, ImageExt};

    // Reserve the Dex host port up front — the issuer URL is baked into the
    // Dex config and enforced by the verifier, so it must be known before the
    // container starts (RFC0029.7 precedent).
    fn reserve_port() -> u16 {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve port");
        l.local_addr().expect("addr").port()
    }

    // --- Dex (OIDC provider) -------------------------------------------------
    let dex_config_for = |issuer: &str| {
        format!(
            "issuer: {issuer}\n\
             storage:\n  type: memory\n\
             web:\n  http: 0.0.0.0:5556\n\
             enablePasswordDB: true\n\
             oauth2:\n  grantTypes: [\"client_credentials\"]\n\
             expiry:\n  idTokens: \"120s\"\n\
             staticClients:\n\
             \x20\x20- id: {CLIENT_ID}\n\
             \x20\x20\x20\x20name: Edge Collector\n\
             \x20\x20\x20\x20secret: {CLIENT_SECRET}\n\
             \x20\x20\x20\x20clientCredentialsClaims:\n\
             \x20\x20\x20\x20\x20\x20groups: [\"{TENANT}\", \"globex\"]\n"
        )
    };
    let mut started = None;
    for attempt in 0..3 {
        let dex_port = reserve_port();
        let issuer = format!("http://127.0.0.1:{dex_port}/dex");
        match GenericImage::new(DEX_IMAGE, DEX_TAG)
            .with_copy_to(
                "/etc/dex/config.docker.yaml",
                dex_config_for(&issuer).into_bytes(),
            )
            .with_env_var("DEX_CLIENT_CREDENTIAL_GRANT_ENABLED_BY_DEFAULT", "true")
            .with_mapped_port(dex_port, ContainerPort::Tcp(5556))
            .start()
            .await
        {
            Ok(container) => {
                started = Some((container, issuer, dex_port));
                break;
            }
            Err(e) if attempt < 2 => eprintln!("dex start attempt {attempt} failed: {e}"),
            Err(e) => panic!("dex never started: {e}"),
        }
    }
    let (dex, issuer, dex_port) = started.expect("dex started");

    let http = reqwest::Client::new();
    let discovery_url = format!("{issuer}/.well-known/openid-configuration");
    if timeout(Duration::from_secs(90), async {
        loop {
            if let Ok(r) = http.get(&discovery_url).send().await
                && r.status().is_success()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .is_err()
    {
        panic!("dex never became ready:\n{}", container_logs(&dex).await);
    }

    // --- TLS material --------------------------------------------------------
    // The cert carries `host.docker.internal` so the Collector (in a
    // container) verifies the host it dials, plus loopback SANs for the
    // host-side query.
    let tmp = tempfile::TempDir::new().expect("temp");
    let signed = rcgen::generate_simple_self_signed(vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "host.docker.internal".to_string(),
    ])
    .expect("mint server cert");
    let ca_pem = signed.cert.pem();
    let cert_path = tmp.path().join("server.crt");
    let key_path = tmp.path().join("server.key");
    std::fs::write(&cert_path, &ca_pem).expect("write cert");
    std::fs::write(&key_path, signed.signing_key.serialize_pem()).expect("write key");

    // --- Ourios (receiver + querier, TLS + OIDC) -----------------------------
    // The gRPC receiver binds 0.0.0.0 so the container can reach it; the
    // querier stays on loopback (queried from the host).
    let wal = tmp.path().join("wal");
    std::fs::create_dir_all(&wal).expect("wal dir");
    let config_path = tmp.path().join("ourios.yaml");
    let mut file = std::fs::File::create(&config_path).expect("create config");
    write!(
        file,
        "storage:\n  local:\n    bucket_root: {bucket}\n\
         receiver:\n  enabled: true\n  grpc_addr: 0.0.0.0:0\n\
         \x20\x20grpc_tls:\n    cert_file: {cert}\n    key_file: {key}\n\
         \x20\x20http_addr: 127.0.0.1:0\n  wal_root: {wal}\n\
         querier:\n  enabled: true\n  http_addr: 127.0.0.1:0\n\
         \x20\x20http_tls:\n    cert_file: {cert}\n    key_file: {key}\n\
         auth:\n  oidc:\n    issuer: {issuer}\n    audience: {CLIENT_ID}\n\
         \x20\x20\x20\x20tenant_claim: groups\n    name_claim: name\n    clock_skew_secs: 60\n",
        bucket = tmp.path().display(),
        cert = cert_path.display(),
        key = key_path.display(),
        wal = wal.display(),
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
    let stderr = child.stderr.take().expect("stderr piped");
    let drain = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        let mut out = Vec::new();
        while let Some(line) = lines.next_line().await.expect("read stderr") {
            out.push(line);
        }
        out
    });
    let stdout = child.stdout.take().expect("stdout piped");
    let mut out_lines = BufReader::new(stdout).lines();
    let (grpc_addr, querier_addr) = timeout(Duration::from_secs(15), async {
        let (mut grpc, mut querier) = (None, None);
        while let Some(line) = out_lines.next_line().await.expect("read stdout") {
            if let Some(a) = line.strip_prefix("receiver gRPC listening on ") {
                grpc = Some(a.trim().to_string());
            }
            if let Some(a) = line.strip_prefix("querier HTTP listening on ") {
                querier = Some(a.trim().to_string());
            }
            if let (Some(g), Some(q)) = (&grpc, &querier) {
                return (g.clone(), q.clone());
            }
        }
        panic!("ourios role announcements never appeared");
    })
    .await
    .expect("ourios ready before timeout");
    // "0.0.0.0:PORT" — the container reaches the same port on the host gateway.
    let grpc_port: u16 = grpc_addr
        .rsplit_once(':')
        .and_then(|(_, p)| p.parse().ok())
        .expect("grpc port");

    // --- The OTel Collector --------------------------------------------------
    let collector_config = format!(
        "extensions:\n\
         \x20\x20oauth2client:\n\
         \x20\x20\x20\x20client_id: {CLIENT_ID}\n\
         \x20\x20\x20\x20client_secret: {CLIENT_SECRET}\n\
         \x20\x20\x20\x20token_url: http://host.docker.internal:{dex_port}/dex/token\n\
         \x20\x20\x20\x20scopes: [openid, profile, groups]\n\
         receivers:\n\
         \x20\x20filelog:\n\
         \x20\x20\x20\x20include: [/etc/otelcol/app.log]\n\
         \x20\x20\x20\x20start_at: beginning\n\
         processors:\n\
         \x20\x20resource:\n\
         \x20\x20\x20\x20attributes:\n\
         \x20\x20\x20\x20\x20\x20- key: service.name\n\
         \x20\x20\x20\x20\x20\x20\x20\x20value: {TENANT}\n\
         \x20\x20\x20\x20\x20\x20\x20\x20action: upsert\n\
         exporters:\n\
         \x20\x20otlp:\n\
         \x20\x20\x20\x20endpoint: host.docker.internal:{grpc_port}\n\
         \x20\x20\x20\x20headers:\n\
         \x20\x20\x20\x20\x20\x20x-ourios-tenant: {TENANT}\n\
         \x20\x20\x20\x20tls:\n\
         \x20\x20\x20\x20\x20\x20ca_file: /etc/otelcol/ca.crt\n\
         \x20\x20\x20\x20auth:\n\
         \x20\x20\x20\x20\x20\x20authenticator: oauth2client\n\
         service:\n\
         \x20\x20extensions: [oauth2client]\n\
         \x20\x20pipelines:\n\
         \x20\x20\x20\x20logs:\n\
         \x20\x20\x20\x20\x20\x20receivers: [filelog]\n\
         \x20\x20\x20\x20\x20\x20processors: [resource]\n\
         \x20\x20\x20\x20\x20\x20exporters: [otlp]\n",
    );
    let app_log = format!("{}\n", APP_LINES.join("\n"));

    // Baseline the WAL before the Collector can export, so a segment header
    // written at startup isn't mistaken for a delivered batch.
    let wal_bytes = || dir_bytes(&wal);
    let wal_baseline = wal_bytes();

    let collector = GenericImage::new(COLLECTOR_IMAGE, COLLECTOR_TAG)
        .with_copy_to(
            "/etc/otelcol-contrib/config.yaml",
            collector_config.into_bytes(),
        )
        .with_copy_to("/etc/otelcol/ca.crt", ca_pem.clone().into_bytes())
        .with_copy_to("/etc/otelcol/app.log", app_log.into_bytes())
        .with_host("host.docker.internal", Host::HostGateway)
        .start()
        .await
        .expect("otelcol started");

    // --- Wait for the export to be acked (WAL-before-ack) --------------------
    // The served sink only flushes to Parquet on graceful shutdown (no config
    // knob), so a live query can't see the rows yet. The WAL, however, is
    // written before the receiver acks the Collector — so a WAL that has grown
    // past its baseline and then gone quiet means the batch landed. Then we
    // flush and read back.
    let acked = timeout(Duration::from_secs(120), async {
        let (mut last, mut stable) = (wal_baseline, 0u32);
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let cur = wal_bytes();
            if cur > wal_baseline && cur == last {
                stable += 1;
                if stable >= 2 {
                    return;
                }
            } else {
                stable = 0;
            }
            last = cur;
        }
    })
    .await;
    assert!(
        acked.is_ok(),
        "collector never delivered a batch (WAL stayed empty at {} bytes).\n\
         --- otelcol logs ---\n{}",
        wal_bytes(),
        container_logs(&collector).await,
    );

    // The query surface serves over the full TLS + OIDC stack with the
    // Collector's own token (empty store pre-flush → 200, zero rows). This
    // pins that the read path composes with the same auth the Collector used.
    let token = mint_token(&http, &issuer).await;
    let tls_client = reqwest::Client::builder()
        .add_root_certificate(reqwest::Certificate::from_pem(ca_pem.as_bytes()).expect("root"))
        .build()
        .expect("tls client");
    let query = tls_client
        .post(format!("https://{querier_addr}/v1/query"))
        .header("content-type", "text/plain")
        .header("x-ourios-tenant", TENANT)
        .header("authorization", format!("Bearer {token}"))
        .body("template_id == 1")
        .send()
        .await
        .expect("query over TLS + OIDC");
    assert_eq!(
        query.status(),
        reqwest::StatusCode::OK,
        "the TLS querier serves the Collector's OIDC token",
    );

    // --- Flush and read the store back ---------------------------------------
    // SIGTERM (what k8s sends) drains the sink; the querier dies with it, so
    // we assert landing by reading the flushed Parquet directly.
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
    let stderr_lines = timeout(Duration::from_secs(15), drain)
        .await
        .expect("stderr drains")
        .expect("drain task");

    let tenant = ourios_core::tenant::TenantId::new(TENANT);
    let registry = ourios_querier::derive_template_registry(
        ourios_querier::StoreRef::Local(tmp.path()),
        &tenant,
    )
    .expect("derive registry");
    assert!(
        !registry.is_empty(),
        "the receiver persisted the miner's template audit for the Collector's lines",
    );

    let mut rendered = Vec::new();
    for f in data_parquet_files(&tmp.path().join("data")) {
        let records = ourios_parquet::Reader::open_file(&f)
            .expect("open data file")
            .read_all()
            .expect("read records");
        for record in records {
            assert_eq!(
                record.tenant_id, tenant,
                "every Collector-shipped record binds to the JWT-derived tenant",
            );
            let ourios_querier::LogBody::Rendered { line, .. } =
                ourios_querier::render_log_body(&record, &registry)
            else {
                panic!("a string body renders to a line");
            };
            rendered.push(String::from_utf8(line).expect("utf8 line"));
        }
    }
    rendered.sort();
    let mut want: Vec<String> = APP_LINES.iter().map(|s| (*s).to_owned()).collect();
    want.sort();
    assert_eq!(
        rendered, want,
        "every line the real Collector exported over TLS + OIDC landed under \
         the JWT tenant and reconstructs bit-for-bit",
    );
    assert!(
        !stderr_lines.iter().any(|l| l.contains(&token)),
        "no JWT material on the log surface",
    );
}

/// Mint an access token from Dex's token endpoint — the `oauth2client`
/// client-credentials flow, from the host side.
async fn mint_token(http: &reqwest::Client, issuer: &str) -> String {
    let response: serde_json::Value = http
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
    response["access_token"]
        .as_str()
        .expect("access_token")
        .to_string()
}

/// Total bytes of all regular files under `dir` (0 if absent).
fn dir_bytes(dir: &std::path::Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(meta) = path.metadata() {
                total += meta.len();
            }
        }
    }
    total
}

/// All `.parquet` files under `root`, recursively (empty when absent).
fn data_parquet_files(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "parquet") {
                files.push(path);
            }
        }
    }
    files
}

/// A container's stdout + stderr, for surfacing a config rejection that would
/// otherwise read as a bare timeout.
async fn container_logs<I>(
    container: &testcontainers_modules::testcontainers::ContainerAsync<I>,
) -> String
where
    I: testcontainers_modules::testcontainers::Image,
{
    let out = container
        .stdout_to_vec()
        .await
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    let err = container
        .stderr_to_vec()
        .await
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    format!("--- stdout ---\n{out}\n--- stderr ---\n{err}")
}

// --- RFC 0050: a real drainprocessor pass ---------------------------------

/// The repetitive stream the `drain` processor converges on:
/// `user <*> logged in from <*>` after a handful of lines.
const DRAIN_LINES: &[&str] = &[
    "user 1 logged in from 10.0.0.1",
    "user 2 logged in from 10.0.0.2",
    "user 3 logged in from 10.0.0.3",
    "user 4 logged in from 10.0.0.4",
    "user 5 logged in from 10.0.0.5",
    "user 6 logged in from 10.0.0.6",
    "user 7 logged in from 10.0.0.7",
    "user 8 logged in from 10.0.0.8",
];

/// The independent oracle: drain's converged template for
/// [`DRAIN_LINES`], known a priori. Every stored claim must be either
/// this exact string or (for the records annotated before drain
/// converged) the record's own body — anything else means a stored
/// value was rewritten somewhere.
const CONVERGED: &str = "user <*> logged in from <*>";

/// RFC 0050 — an **actual drainprocessor pass** (the §6 corpus-arm
/// source): otelcol-contrib runs the `drain` processor (defaults —
/// it emits `log.record.template`) over a repetitive log file and
/// exports the annotated stream to a served Ourios in `adopt` mode,
/// over plaintext OTLP in open mode — this scenario isolates the
/// RFC 0050 integration; the TLS + OIDC composition is the sibling
/// test's job. Asserted on the flushed store:
///
/// - the drainprocessor's `log.record.template` attribute is stored
///   verbatim on every record (RFC 0018 fidelity);
/// - converged records (their claim carries `<*>`) were **adopted**:
///   the registry tokens at their `(template_id, template_version)`
///   render exactly the claimed string (RFC0050.2, real source);
/// - every body reconstructs byte-identically (RFC0050.4 —
///   adoption is alignment-gated).
#[allow(clippy::too_many_lines)] // one linear end-to-end scenario, like the sibling
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Docker (real otelcol-contrib container); run by the collector-interop CI job via --ignored"]
async fn drainprocessor_annotates_and_ourios_adopts() {
    use testcontainers_modules::testcontainers::core::Host;
    use testcontainers_modules::testcontainers::runners::AsyncRunner as _;
    use testcontainers_modules::testcontainers::{GenericImage, ImageExt};

    let tmp = tempfile::TempDir::new().expect("temp");
    let wal = tmp.path().join("wal");
    std::fs::create_dir_all(&wal).expect("wal dir");
    let config_path = tmp.path().join("ourios.yaml");
    let mut file = std::fs::File::create(&config_path).expect("create config");
    write!(
        file,
        "storage:\n  local:\n    bucket_root: \"{bucket}\"\n\
         receiver:\n  enabled: true\n  grpc_addr: 0.0.0.0:0\n\
         \x20\x20http_addr: 127.0.0.1:0\n  wal_root: \"{wal}\"\n\
         miner:\n  upstream_templates: adopt\n",
        bucket = tmp.path().display(),
        wal = wal.display(),
    )
    .expect("write config");

    let mut child = Command::new(env!("CARGO_BIN_EXE_ourios-server"))
        .arg("--config")
        .arg(&config_path)
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn ourios-server");
    let stdout = child.stdout.take().expect("stdout piped");
    let mut out_lines = BufReader::new(stdout).lines();
    let grpc_addr = timeout(Duration::from_secs(15), async {
        while let Some(line) = out_lines.next_line().await.expect("read stdout") {
            if let Some(a) = line.strip_prefix("receiver gRPC listening on ") {
                return a.trim().to_string();
            }
        }
        panic!("receiver announcement never appeared");
    })
    .await
    .expect("ourios ready before timeout");
    let grpc_port: u16 = grpc_addr
        .rsplit_once(':')
        .and_then(|(_, p)| p.parse().ok())
        .expect("grpc port");

    let collector_config = format!(
        "receivers:\n\
         \x20\x20filelog:\n\
         \x20\x20\x20\x20include: [/etc/otelcol/app.log]\n\
         \x20\x20\x20\x20start_at: beginning\n\
         processors:\n\
         \x20\x20resource:\n\
         \x20\x20\x20\x20attributes:\n\
         \x20\x20\x20\x20\x20\x20- key: service.name\n\
         \x20\x20\x20\x20\x20\x20\x20\x20value: {TENANT}\n\
         \x20\x20\x20\x20\x20\x20\x20\x20action: upsert\n\
         \x20\x20drain: {{}}\n\
         exporters:\n\
         \x20\x20otlp:\n\
         \x20\x20\x20\x20endpoint: host.docker.internal:{grpc_port}\n\
         \x20\x20\x20\x20headers:\n\
         \x20\x20\x20\x20\x20\x20x-ourios-tenant: {TENANT}\n\
         \x20\x20\x20\x20tls:\n\
         \x20\x20\x20\x20\x20\x20insecure: true\n\
         service:\n\
         \x20\x20pipelines:\n\
         \x20\x20\x20\x20logs:\n\
         \x20\x20\x20\x20\x20\x20receivers: [filelog]\n\
         \x20\x20\x20\x20\x20\x20processors: [resource, drain]\n\
         \x20\x20\x20\x20\x20\x20exporters: [otlp]\n",
    );
    let app_log = format!("{}\n", DRAIN_LINES.join("\n"));

    let wal_bytes = || dir_bytes(&wal);
    let wal_baseline = wal_bytes();

    let collector = GenericImage::new(COLLECTOR_IMAGE, COLLECTOR_TAG)
        .with_copy_to(
            "/etc/otelcol-contrib/config.yaml",
            collector_config.into_bytes(),
        )
        .with_copy_to("/etc/otelcol/app.log", app_log.into_bytes())
        .with_host("host.docker.internal", Host::HostGateway)
        .start()
        .await
        .expect("otelcol started");

    // WAL-before-ack: growth past the baseline, then quiet, means the
    // annotated batch landed (the sibling test's ack heuristic).
    let acked = timeout(Duration::from_secs(120), async {
        let (mut last, mut stable) = (wal_baseline, 0u32);
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let cur = wal_bytes();
            if cur > wal_baseline && cur == last {
                stable += 1;
                if stable >= 2 {
                    return;
                }
            } else {
                stable = 0;
            }
            last = cur;
        }
    })
    .await;
    assert!(
        acked.is_ok(),
        "collector never delivered a batch (WAL stayed at {} bytes).\n{}",
        wal_bytes(),
        container_logs(&collector).await,
    );

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

    let claim_of = |record: &ourios_core::record::MinedRecord| -> Option<String> {
        record.attributes.iter().find_map(|kv| {
            (kv.key == "log.record.template")
                .then(|| kv.value.as_ref()?.value.as_ref())
                .flatten()
                .and_then(|v| match v {
                    ourios_core::otlp::any_value::Value::StringValue(s) => Some(s.clone()),
                    _ => None,
                })
        })
    };

    // The adopt discriminator: only the RFC 0050 adopt path emits
    // `template_adopted` audit events. Without this, "the row's
    // template renders the claim" would be circular — the built-in
    // miner converges on the same canonical for this corpus even
    // under `ignore`.
    let mut adopted_events: Vec<(u64, String)> = Vec::new();
    for f in data_parquet_files(&tmp.path().join("audit")) {
        let events = ourios_parquet::AuditReader::open_file(&f)
            .expect("open audit file")
            .read_all()
            .expect("read audit events");
        for event in events {
            if let ourios_core::audit::AuditPayload::Template {
                template_id,
                change: ourios_core::audit::TemplateChange::Adopted { new_template, .. },
                ..
            } = event.payload
            {
                adopted_events.push((template_id, new_template));
            }
        }
    }
    assert!(
        adopted_events
            .iter()
            .any(|(_, template)| template == CONVERGED),
        "the adopt path audited the drainprocessor's converged template: {adopted_events:?}",
    );

    let mut converged_adoptions = 0usize;
    let mut rendered = Vec::new();
    for f in data_parquet_files(&tmp.path().join("data")) {
        let records = ourios_parquet::Reader::open_file(&f)
            .expect("open data file")
            .read_all()
            .expect("read records");
        for record in records {
            // RFC 0018: the drainprocessor's annotation is stored
            // verbatim — checked against the a-priori oracle, not
            // against the store's own contents.
            let claim = claim_of(&record)
                .unwrap_or_else(|| panic!("drain annotated every record: {record:?}"));
            let ourios_querier::LogBody::Rendered { line, .. } =
                ourios_querier::render_log_body(&record, &registry)
            else {
                panic!("a string body renders to a line");
            };
            let line = String::from_utf8(line).expect("utf8 line");
            // Pre-convergence, drain annotates a line with itself.
            assert!(
                claim == CONVERGED || claim == line,
                "every stored claim is drain's known output for this corpus, \
                 got {claim:?} (line {line:?})",
            );
            // A converged claim was adopted: the audit stream carries
            // its template_adopted event for this row's id, and the
            // registry tokens at the row's key render the claim.
            if claim == CONVERGED {
                assert!(
                    adopted_events
                        .iter()
                        .any(|(id, _)| *id == record.template_id),
                    "row {} adopted via the audited adopt path",
                    record.template_id,
                );
                let tokens = registry
                    .get(&(record.template_id, record.template_version))
                    .expect("adopted row's (id, version) resolves in the registry");
                assert_eq!(
                    ourios_miner::tree::format_template(tokens),
                    claim,
                    "the row's template IS the drainprocessor's claim",
                );
                converged_adoptions += 1;
            }
            rendered.push(line);
        }
    }
    assert!(
        converged_adoptions >= DRAIN_LINES.len() / 2,
        "most records arrive after drain converges and adopt its template \
         (got {converged_adoptions} of {})",
        DRAIN_LINES.len(),
    );
    rendered.sort();
    let mut want: Vec<String> = DRAIN_LINES.iter().map(|s| (*s).to_owned()).collect();
    want.sort();
    assert_eq!(
        rendered, want,
        "every line reconstructs byte-identically (RFC0050.4)",
    );
}
