//! RFC 0030 §5 — the server-owned scenarios: the plaintext-auth
//! startup warning (`.7`, implemented — it observes the spawned
//! binary, which only this crate can do), TLS on the querier surface
//! (`.3`), and the served end-to-end (`.8`). The receiver arms live in
//! `crates/ourios-ingester/tests/it/rfc0030_tls.rs` per §6.

use std::io::Write as _;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::time::timeout;

/// Scenario RFC0030.3 — querier + MCP over TLS.
/// See `docs/rfcs/0030-tls-mtls-listeners.md` §5.
#[test]
#[ignore = "RFC0030.3 stub — implemented in the querier green slice"]
fn rfc0030_3_querier_and_mcp_over_tls() {
    todo!(
        "RFC0030.3 — querier http_tls enabled, static bearer \
         configured: query (valid bearer + X-Ourios-Tenant) and MCP \
         initialize (valid bearer) succeed over TLS; a plaintext \
         request to the same port fails at the transport layer"
    );
}

/// Spawn the server with the given config file, collect stderr until
/// the querier readiness line appears on stdout, and return how many
/// stderr lines contained `needle`. The readiness line is the "warning
/// window" bound: `startup_guards` runs before any role announces
/// readiness, so a warning that exists is on stderr by then.
async fn warnings_before_ready(config_yaml: &str, tmp: &tempfile::TempDir, needle: &str) -> usize {
    let config_path = tmp.path().join("ourios.yaml");
    let mut file = std::fs::File::create(&config_path).expect("create config");
    write!(file, "{config_yaml}").expect("write config");

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
        while let Ok(Some(line)) = lines.next_line().await {
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
    // already written; kill the child to end the stderr stream.
    child.start_kill().expect("kill server");
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
