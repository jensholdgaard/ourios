//! RFC 0026 §5 — the server-owned scenarios: token-store
//! configuration (`.1`), the query-path status contract (`.4`), the
//! query half of wildcard binding (`.5`), and open-mode parity
//! (`.6`). The ingest-side scenarios (`.2`/`.3`, the ingest half of
//! `.5`, and `.7`) live in
//! `crates/ourios-ingester/tests/rfc0026_auth.rs`.
//!
//! Remaining stubs are `#[ignore]`d so the default run stays green
//! while the RFC is red; each names the green slice that discharges
//! it. `.1` is green: the schema/substitution/redaction arms live in
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
/// exposure (RFC 0026 §3.1). The warning reaches stderr through the tracing
/// mirror; its `OTel` event name (`ourios.server.auth.open_mode`) is
/// registry-backed like every dogfooded log event.
/// See `docs/rfcs/0026-authentication-tenant-binding.md` §5.
#[tokio::test]
async fn rfc0026_1_missing_auth_section_starts_open_with_a_warning() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let mut child = Command::new(env!("CARGO_BIN_EXE_ourios-server"))
        .env("OURIOS_BUCKET_ROOT", tmp.path())
        .env("OURIOS_QUERIER_ENABLED", "1")
        .env("OURIOS_QUERIER_HTTP_ADDR", "127.0.0.1:0")
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

/// Scenario RFC0026.4 — query enforcement and status contract.
/// See `docs/rfcs/0026-authentication-tenant-binding.md` §5.
#[test]
#[ignore = "RFC0026.4 stub — implemented in the query green slice"]
fn rfc0026_4_query_status_contract() {
    todo!(
        "RFC0026.4 — 401 missing/unknown bearer; 400 missing/empty \
         x-ourios-tenant (unchanged); 403 out-of-set tenant; 200 with \
         correct results in-set; drift endpoint under the same gate"
    );
}

/// Scenario RFC0026.5 (query half) — wildcard binding.
/// See `docs/rfcs/0026-authentication-tenant-binding.md` §5.
#[test]
#[ignore = "RFC0026.5 stub — implemented in the query green slice"]
fn rfc0026_5_wildcard_binding_query() {
    todo!(
        "RFC0026.5 — a tenants: [\"*\"] token queries arbitrary tenants as \
         if every tenant were listed"
    );
}

/// Scenario RFC0026.6 — open-mode parity.
/// See `docs/rfcs/0026-authentication-tenant-binding.md` §5.
#[test]
#[ignore = "RFC0026.6 stub — implemented in the query green slice (parity is asserted by the existing suites plus this warning check)"]
fn rfc0026_6_open_mode_parity() {
    todo!(
        "RFC0026.6 — with no auth section the existing ingest + query \
         acceptance suites pass unchanged and the startup warning is \
         emitted exactly once"
    );
}
