//! RFC0016.5 / RFC0016.7 — the querier role at the process boundary.
//!
//! These are the only querier scenarios that cross a real socket and drive a
//! real shutdown signal. They spawn the `ourios-server` binary, read the
//! OS-assigned port it prints, and:
//!
//! - **.5** asserts the role is env-gated (no `querier HTTP listening` line
//!   when `OURIOS_QUERIER_ENABLED` is unset) and that an enabled querier binds
//!   and then drains cleanly on SIGTERM.
//! - **.7** asserts the receiver and querier compose in one binary over one
//!   `OURIOS_BUCKET_ROOT`: both listeners bind, the querier serves a real DSL
//!   query against a seeded store, and SIGTERM drains both with a clean exit.
//!
//! Unix-only: shutdown is driven with `kill -TERM`, and the server's SIGTERM
//! handling is itself Unix-only (mirrors `rfc0003_16_served_binary`).
#![cfg(unix)]

use std::net::SocketAddr;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ourios_core::record::{BodyKind, MinedRecord, Param};
use ourios_core::tenant::TenantId;
use ourios_parquet::{PartitionKey, Writer};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::time::timeout;

/// The current wall clock in unix nanos — a fixture timestamp that always
/// falls inside the server's default look-back window `[now - W, now]`.
fn now_ns() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    u64::try_from(nanos).unwrap_or(0)
}

/// A minimal one-record `template_id == 1` fixture for `tenant`.
fn mined(tenant: &str, template_id: u64) -> MinedRecord {
    MinedRecord {
        tenant_id: TenantId::new(tenant),
        template_id,
        template_version: 1,
        severity_number: 9,
        severity_text: Some("INFO".to_string()),
        scope_name: Some("lib.cart".to_string()),
        scope_version: Some("1.0.0".to_string()),
        scope_attributes: Vec::new(),
        resource_schema_url: None,
        scope_schema_url: None,
        time_unix_nano: now_ns(),
        observed_time_unix_nano: None,
        attributes: Vec::new(),
        dropped_attributes_count: 0,
        resource_attributes: Vec::new(),
        trace_id: None,
        span_id: None,
        flags: 0,
        event_name: None,
        body_kind: BodyKind::String,
        params: vec![Param {
            type_tag: ourios_core::audit::ParamType::Num,
            value: "42".to_string(),
        }],
        separators: vec![String::new(), " ".to_string()],
        body: None,
        confidence: 1.0,
        lossy_flag: false,
    }
}

fn write_records(bucket: &Path, recs: &[MinedRecord]) {
    let part = PartitionKey::derive(&recs[0]).expect("derive partition");
    let mut w = Writer::open(bucket, part).expect("open writer");
    w.append_records(recs).expect("append");
    w.close().expect("close");
}

/// Hand-rolled `POST /v1/query` (no HTTP-client dependency): a `text/plain`
/// DSL body with the tenant header. Returns the raw response so the caller can
/// check the status line + JSON body.
async fn http_post_query(addr: SocketAddr, tenant: &str, dsl: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.expect("connect querier");
    let head = format!(
        "POST /v1/query HTTP/1.1\r\nHost: {addr}\r\nX-Ourios-Tenant: {tenant}\r\n\
         Content-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        dsl.len(),
    );
    stream.write_all(head.as_bytes()).await.expect("write head");
    stream.write_all(dsl.as_bytes()).await.expect("write body");
    stream.flush().await.ok();
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .expect("read HTTP response");
    String::from_utf8_lossy(&response).into_owned()
}

/// SIGTERM `child` (what k8s / `nerdctl stop` send) and assert a clean exit.
async fn terminate_and_assert_clean(mut child: Child) {
    let pid = child.id().expect("server pid");
    let kill_status = Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status()
        .await
        .expect("run kill -TERM");
    assert!(
        kill_status.success(),
        "kill -TERM succeeded, got {kill_status:?}"
    );
    let status = timeout(Duration::from_secs(15), child.wait())
        .await
        .expect("server exits before timeout")
        .expect("await server exit");
    assert!(
        status.success(),
        "graceful shutdown exits cleanly, got {status:?}"
    );
}

/// Scenario RFC0016.5 — role gating + graceful shutdown.
/// See `docs/rfcs/0016-query-serving-endpoint.md` §5.
#[tokio::test]
async fn rfc0016_5_role_gating_and_graceful_shutdown() {
    // Arrange: a compactor-only server (no querier role).
    let tmp = tempfile::TempDir::new().expect("temp");
    let mut disabled = Command::new(env!("CARGO_BIN_EXE_ourios-server"))
        .env("OURIOS_BUCKET_ROOT", tmp.path())
        // OURIOS_QUERIER_ENABLED deliberately unset.
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn ourios-server");

    // Assert: no querier listener is announced. The compactor prints nothing
    // on startup, so we positively confirm a window passes without the line.
    let stdout = disabled.stdout.take().expect("server stdout piped");
    let mut lines = BufReader::new(stdout).lines();
    let saw_querier_line = timeout(Duration::from_secs(2), async {
        while let Some(line) = lines.next_line().await.expect("read stdout") {
            if line.contains("querier HTTP listening on") {
                return true;
            }
        }
        false
    })
    .await;
    assert!(
        matches!(saw_querier_line, Err(_) | Ok(false)),
        "no querier listener is bound when the role is disabled, saw {saw_querier_line:?}",
    );
    terminate_and_assert_clean(disabled).await;

    // Act: now enable the querier role on an ephemeral port.
    let mut child = Command::new(env!("CARGO_BIN_EXE_ourios-server"))
        .env("OURIOS_BUCKET_ROOT", tmp.path())
        .env("OURIOS_QUERIER_ENABLED", "1")
        .env("OURIOS_QUERIER_HTTP_ADDR", "127.0.0.1:0")
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn ourios-server");
    let addr = read_querier_addr(&mut child).await;

    // Assert: the listener is live (a connection succeeds), then SIGTERM
    // drains it and the process exits cleanly.
    TcpStream::connect(addr)
        .await
        .expect("querier listener accepts a connection");
    terminate_and_assert_clean(child).await;
}

/// Scenario RFC0016.7 — receiver and querier compose in one binary.
/// See `docs/rfcs/0016-query-serving-endpoint.md` §5.
#[tokio::test]
async fn rfc0016_7_receiver_and_querier_compose_in_one_binary() {
    // Arrange: seed one queryable row, then start both roles on distinct
    // ephemeral ports over the one bucket root.
    let tmp = tempfile::TempDir::new().expect("temp");
    let wal_root = tmp.path().join("wal");
    write_records(tmp.path(), &[mined("acme", 1)]);

    let mut child = Command::new(env!("CARGO_BIN_EXE_ourios-server"))
        .env("OURIOS_BUCKET_ROOT", tmp.path())
        .env("OURIOS_RECEIVER_ENABLED", "1")
        .env("OURIOS_RECEIVER_GRPC_ADDR", "127.0.0.1:0")
        .env("OURIOS_RECEIVER_HTTP_ADDR", "127.0.0.1:0")
        .env("OURIOS_WAL_ROOT", &wal_root)
        .env("OURIOS_QUERIER_ENABLED", "1")
        .env("OURIOS_QUERIER_HTTP_ADDR", "127.0.0.1:0")
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn ourios-server");

    // Read all three announced addresses (both receiver transports + querier).
    let stdout = child.stdout.take().expect("server stdout piped");
    let mut lines = BufReader::new(stdout).lines();
    let mut grpc_addr: Option<SocketAddr> = None;
    let mut http_addr: Option<SocketAddr> = None;
    let mut querier_addr: Option<SocketAddr> = None;
    let read_addrs = async {
        while grpc_addr.is_none() || http_addr.is_none() || querier_addr.is_none() {
            let line = lines
                .next_line()
                .await
                .expect("read server stdout")
                .expect("server stdout closed before reporting addresses");
            if let Some(rest) = line.strip_prefix("receiver gRPC listening on ") {
                grpc_addr = Some(rest.trim().parse().expect("parse gRPC addr"));
            } else if let Some(rest) = line.strip_prefix("receiver HTTP listening on ") {
                http_addr = Some(rest.trim().parse().expect("parse HTTP addr"));
            } else if let Some(rest) = line.strip_prefix("querier HTTP listening on ") {
                querier_addr = Some(rest.trim().parse().expect("parse querier addr"));
            }
        }
    };
    timeout(Duration::from_secs(15), read_addrs)
        .await
        .expect("server reports all bound addresses before timeout");
    let querier_addr = querier_addr.expect("querier addr");

    // Act + Assert: the querier serves the seeded row over the shared store.
    let response = http_post_query(querier_addr, "acme", "template_id == 1").await;
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "querier returns 200, got status line {:?}",
        response.lines().next(),
    );
    let body = response
        .split("\r\n\r\n")
        .nth(1)
        .expect("response has a body");
    let json: serde_json::Value = serde_json::from_str(body).expect("body is JSON");
    assert_eq!(json["rows"], 1, "the seeded row is read: {json}");

    // Shutdown drains both roles (the receiver release frees the `Wal`).
    terminate_and_assert_clean(child).await;
}

/// Read the `querier HTTP listening on {addr}` line the server prints on
/// startup, with a timeout.
async fn read_querier_addr(child: &mut Child) -> SocketAddr {
    let stdout = child.stdout.take().expect("server stdout piped");
    let mut lines = BufReader::new(stdout).lines();
    let read = async {
        loop {
            let line = lines
                .next_line()
                .await
                .expect("read server stdout")
                .expect("server stdout closed before reporting the querier address");
            if let Some(rest) = line.strip_prefix("querier HTTP listening on ") {
                return rest.trim().parse().expect("parse querier addr");
            }
        }
    };
    timeout(Duration::from_secs(15), read)
        .await
        .expect("server reports the querier address before timeout")
}
