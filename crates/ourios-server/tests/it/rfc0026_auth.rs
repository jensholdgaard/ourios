//! RFC 0026 §5 — the server-owned scenarios: token-store
//! configuration (`.1`), the query-path status contract (`.4`), the
//! query half of wildcard binding (`.5`), and open-mode parity
//! (`.6`). The ingest-side scenarios (`.2`/`.3`, the ingest half of
//! `.5`, and `.7`) live in
//! `crates/ourios-ingester/tests/rfc0026_auth.rs`.
//!
//! All four server-owned scenarios are green. `.1`: the schema/substitution/redaction arms live in
//! `ourios_server::config::file`, the store-validation matrix in
//! `ourios_server::auth`, the file→store mapping in `src/main.rs`
//! (`rfc0026_1_*`), and the startup-observable arms — the empty-list
//! startup error and the open-mode warning — here, against the spawned
//! binary (the `rfc0016_5_7_served_querier` pattern).

use std::io::Write as _;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::time::timeout;

/// Scenario RFC0026.1 (startup error) — an *empty* `auth.tokens` list is a
/// startup configuration error (a locked-out server is never the intent,
/// RFC 0026 §3.1): the process exits non-zero, naming the key.
/// See `docs/rfcs/0026-authentication-tenant-binding.md` §5.
#[tokio::test]
async fn rfc0026_1_empty_token_list_is_a_startup_error() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let config_path = tmp.path().join("ourios.yaml");
    let mut file = std::fs::File::create(&config_path).expect("create config");
    write!(
        file,
        "storage:\n  local:\n    bucket_root: {}\nauth:\n  tokens: []\n",
        tmp.path().display(),
    )
    .expect("write config");

    let output = timeout(
        Duration::from_secs(15),
        Command::new(env!("CARGO_BIN_EXE_ourios-server"))
            .arg("--config")
            .arg(&config_path)
            // If the startup error ever regressed into a running server, the
            // timeout would drop this future — don't leave that child behind.
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("server exits before timeout")
    .expect("run ourios-server");

    assert!(
        !output.status.success(),
        "an empty auth.tokens list must fail startup, got {:?}",
        output.status,
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("auth.tokens"),
        "the error names the key: {stderr}"
    );
}

/// Scenario RFC0026.1 (open mode) — a missing `auth` section starts in open
/// mode: the role comes up, and a structured startup warning names the
/// exposure (RFC 0026 §3.1). The human-readable copy asserted here reaches
/// stderr through the tracing `fmt` mirror (which renders the target and
/// message, not the event name); the registry-backed event name
/// (`ourios.server.auth.open_mode`) travels the `OTel` Logs signal, where
/// `weaver registry live-check` enforces it in CI.
/// See `docs/rfcs/0026-authentication-tenant-binding.md` §5.
#[tokio::test]
async fn rfc0026_1_missing_auth_section_starts_open_with_a_warning() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let mut child = Command::new(env!("CARGO_BIN_EXE_ourios-server"))
        .env("OURIOS_BUCKET_ROOT", tmp.path())
        .env("OURIOS_QUERIER_ENABLED", "1")
        .env("OURIOS_QUERIER_HTTP_ADDR", "127.0.0.1:0")
        // Deterministic regardless of the harness environment: an inherited
        // RUST_LOG=error would filter the warning off stderr.
        .env("RUST_LOG", "info")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn ourios-server");

    // The warning precedes the role start, so it appears promptly; the
    // timeout bounds the scan if it never does.
    let stderr = child.stderr.take().expect("server stderr piped");
    let mut lines = BufReader::new(stderr).lines();
    let saw_warning = timeout(Duration::from_secs(15), async {
        while let Some(line) = lines.next_line().await.expect("read stderr") {
            if line.contains("RFC 0026 open mode") {
                return true;
            }
        }
        false
    })
    .await
    .expect("warning appears before timeout");
    assert!(saw_warning, "the open-mode warning names the exposure");

    // Keep draining stderr: the server logs there for its lifetime, and an
    // undrained pipe can fill and block it before the stdout line below.
    tokio::spawn(async move { while lines.next_line().await.ok().flatten().is_some() {} });

    // Open mode is *open*: the role still starts (the listener binds).
    let stdout = child.stdout.take().expect("server stdout piped");
    let mut out_lines = BufReader::new(stdout).lines();
    let bound = timeout(Duration::from_secs(15), async {
        while let Some(line) = out_lines.next_line().await.expect("read stdout") {
            if line.contains("querier HTTP listening on") {
                return true;
            }
        }
        false
    })
    .await
    .expect("querier binds before timeout");
    assert!(bound, "open mode still serves");

    child.kill().await.expect("kill the server");
}

/// A one-token store for the query-gate arms.
fn query_store(tenants: &[&str]) -> std::sync::Arc<ourios_core::auth::TokenStore> {
    std::sync::Arc::new(
        ourios_core::auth::build_token_store(Some(&[ourios_core::auth::TokenSpec {
            name: Some("query-cli".to_string()),
            token: Some("tok-query".to_string()),
            tenants: tenants.iter().map(|t| (*t).to_string()).collect(),
        }]))
        .expect("valid")
        .expect("enabled"),
    )
}

/// `POST /v1/query` against an authed router. Returns the status.
async fn post_query(
    router: axum::Router,
    bearer: Option<&str>,
    tenant: Option<&str>,
    body: &str,
) -> axum::http::StatusCode {
    use tower::ServiceExt as _;
    let mut req = axum::http::Request::builder()
        .method("POST")
        .uri("/v1/query")
        .header(axum::http::header::CONTENT_TYPE, "text/plain");
    if let Some(value) = bearer {
        req = req.header(axum::http::header::AUTHORIZATION, value);
    }
    if let Some(t) = tenant {
        req = req.header("x-ourios-tenant", t);
    }
    router
        .oneshot(
            req.body(axum::body::Body::from(body.to_owned()))
                .expect("build request"),
        )
        .await
        .expect("oneshot")
        .status()
}

/// Scenario RFC0026.4 — query enforcement and status contract.
/// See `docs/rfcs/0026-authentication-tenant-binding.md` §5.
#[tokio::test]
async fn rfc0026_4_query_status_contract() {
    use axum::http::StatusCode;

    let bucket = tempfile::tempdir().expect("temp");
    let auth = query_store(&["acme"]);
    let router = || {
        ourios_server::querier::router_with_auth(
            bucket.path().to_path_buf(),
            3_600_000_000_000,
            Some(auth.clone()),
        )
    };

    // 401: missing and unknown bearer — before the tenant contract.
    for bearer in [None, Some("Bearer tok-wrong")] {
        let status = post_query(router(), bearer, Some("acme"), "template_id == 1").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{bearer:?}");
    }

    // 400: missing/empty tenant with a VALID bearer — today's contract,
    // unchanged by the gate.
    for tenant in [None, Some("")] {
        let status = post_query(
            router(),
            Some("Bearer tok-query"),
            tenant,
            "template_id == 1",
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{tenant:?}");
    }

    // 403: a well-formed tenant outside the token's set.
    let status = post_query(
        router(),
        Some("Bearer tok-query"),
        Some("globex"),
        "template_id == 1",
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // 200: in-set tenant serves the query (an empty store answers with an
    // empty, well-formed result).
    let status = post_query(
        router(),
        Some("Bearer tok-query"),
        Some("acme"),
        "template_id == 1",
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // The drift endpoint sits under the same gate: 401 / 403 / 200.
    let drift = "drift from 2026-06-01T00:00:00Z to 2026-06-02T00:00:00Z";
    assert_eq!(
        post_query(router(), None, Some("acme"), drift).await,
        StatusCode::UNAUTHORIZED,
    );
    assert_eq!(
        post_query(router(), Some("Bearer tok-query"), Some("globex"), drift).await,
        StatusCode::FORBIDDEN,
    );
    assert_eq!(
        post_query(router(), Some("Bearer tok-query"), Some("acme"), drift).await,
        StatusCode::OK,
    );
}

/// Scenario RFC0026.5 (query half) — wildcard binding.
/// See `docs/rfcs/0026-authentication-tenant-binding.md` §5.
#[tokio::test]
async fn rfc0026_5_wildcard_binding_query() {
    let bucket = tempfile::tempdir().expect("temp");
    let auth = query_store(&["*"]);
    for tenant in ["alpha", "beta", "entirely-new-tenant"] {
        let status = post_query(
            ourios_server::querier::router_with_auth(
                bucket.path().to_path_buf(),
                3_600_000_000_000,
                Some(auth.clone()),
            ),
            Some("Bearer tok-query"),
            Some(tenant),
            "template_id == 1",
        )
        .await;
        assert_eq!(
            status,
            axum::http::StatusCode::OK,
            "wildcard serves {tenant}"
        );
    }
}

/// Scenario RFC0026.6 — open-mode parity.
///
/// The suites half of the scenario is the CI suite itself: every other
/// acceptance test in this workspace drives auth-less routers/configs, so
/// their continued green **is** the parity assertion. This test pins the
/// remaining observable: with no `auth` configured and both network roles
/// enabled, the startup warning is emitted exactly once, and requests are
/// served (open really is open).
/// See `docs/rfcs/0026-authentication-tenant-binding.md` §5.
#[tokio::test]
async fn rfc0026_6_open_mode_parity() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let wal = tmp.path().join("wal");
    std::fs::create_dir_all(&wal).expect("wal dir");
    let mut child = Command::new(env!("CARGO_BIN_EXE_ourios-server"))
        .env("OURIOS_BUCKET_ROOT", tmp.path())
        .env("OURIOS_QUERIER_ENABLED", "1")
        .env("OURIOS_QUERIER_HTTP_ADDR", "127.0.0.1:0")
        .env("OURIOS_RECEIVER_ENABLED", "1")
        .env("OURIOS_RECEIVER_GRPC_ADDR", "127.0.0.1:0")
        .env("OURIOS_RECEIVER_HTTP_ADDR", "127.0.0.1:0")
        .env("OURIOS_WAL_ROOT", &wal)
        .env("RUST_LOG", "info")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn ourios-server");

    // Readiness bound: both roles announce on stdout; collect until the
    // querier line (printed last).
    let stdout = child.stdout.take().expect("stdout piped");
    let mut out_lines = BufReader::new(stdout).lines();
    timeout(Duration::from_secs(15), async {
        while let Some(line) = out_lines.next_line().await.expect("read stdout") {
            if line.contains("querier HTTP listening on") {
                return;
            }
        }
        panic!("querier line never appeared");
    })
    .await
    .expect("server ready before timeout");

    // Exactly one open-mode warning: scan stderr for a bounded window
    // after readiness (the warning precedes the role start, so it has
    // already been written).
    let stderr = child.stderr.take().expect("stderr piped");
    let mut err_lines = BufReader::new(stderr).lines();
    let mut count = 0;
    let _ = timeout(Duration::from_secs(2), async {
        while let Some(line) = err_lines.next_line().await.expect("read stderr") {
            if line.contains("RFC 0026 open mode") {
                count += 1;
            }
        }
    })
    .await;
    assert_eq!(count, 1, "the open-mode warning is emitted exactly once");

    child.kill().await.expect("kill the server");
}
