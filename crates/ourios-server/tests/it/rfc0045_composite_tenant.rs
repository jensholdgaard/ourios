//! RFC 0045 — composite tenant derivation at the process boundary:
//! RFC0045.2 (the S2 scenario end-to-end), .3 (strict missing-key
//! rejection), .4 (join injectivity), .5 (epoch semantics across a rule
//! change), and .8 (auth binding unchanged).
//!
//! Three server lifetimes over one store + WAL root, each driven through
//! `--config`, OTLP/HTTP export, SIGTERM (which flushes to Parquet), and
//! the querier:
//!
//! 1. default rule — one `fluxcd` export lands in tenant `fluxcd`;
//! 2. composite rule `[k8s.cluster.name, service.name]` — the S2 pair,
//!    the injectivity pair, and a missing-key rejection; then queries prove
//!    every tenant sees only its own rows and the phase-1 files are
//!    byte-untouched;
//! 3. composite rule + a static token bound to `cluster1/fluxcd` — a
//!    `cluster2/fluxcd` export under that token is refused (403), a
//!    `cluster1/fluxcd` one is accepted.
//!
//! Unix-only: shutdown is driven with `kill -TERM` (as in
//! `rfc0003_16_served_binary`).
#![cfg(unix)]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::common::v1::any_value::Value;
use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue};
use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
use opentelemetry_proto::tonic::resource::v1::Resource;
use prost::Message;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::time::timeout;

fn now_ns() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    u64::try_from(nanos).unwrap_or(0)
}

fn string_value(s: &str) -> AnyValue {
    AnyValue {
        value: Some(Value::StringValue(s.to_owned())),
    }
}

/// One-record export whose `Resource` carries `attrs`, stamped now so the
/// querier's default look-back window includes it.
fn export(attrs: &[(&str, &str)], body: &str) -> Vec<u8> {
    ExportLogsServiceRequest {
        resource_logs: vec![ResourceLogs {
            resource: Some(Resource {
                attributes: attrs
                    .iter()
                    .map(|(key, value)| KeyValue {
                        key: (*key).to_owned(),
                        value: Some(string_value(value)),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            }),
            scope_logs: vec![ScopeLogs {
                log_records: vec![LogRecord {
                    time_unix_nano: now_ns(),
                    body: Some(string_value(body)),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }],
    }
    .encode_to_vec()
}

struct Server {
    child: Child,
    http: SocketAddr,
    querier: SocketAddr,
}

/// Spawn `ourios-server --config` with receiver + querier on ephemeral
/// ports over `tmp` (`store/`, `wal/`), returning the announced addresses.
async fn start(tmp: &Path, tenant_yaml: &str, auth_yaml: &str) -> Server {
    let config = format!(
        "storage:\n  local:\n    bucket_root: {store}\n\
         receiver:\n  enabled: true\n  grpc_addr: 127.0.0.1:0\n  http_addr: 127.0.0.1:0\n\
         \x20\x20wal_root: {wal}\n{tenant_yaml}\
         querier:\n  enabled: true\n  http_addr: 127.0.0.1:0\n{auth_yaml}",
        store = tmp.join("store").display(),
        wal = tmp.join("wal").display(),
    );
    let path = tmp.join(format!("config-{}.yaml", now_ns()));
    std::fs::write(&path, config).expect("write config");
    let mut child = Command::new(env!("CARGO_BIN_EXE_ourios-server"))
        .arg("--config")
        .arg(&path)
        .env("RFC0045_TOKEN", "tok-cluster-one")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn ourios-server");
    let stdout = child.stdout.take().expect("stdout piped");
    let mut stderr = child.stderr.take().expect("stderr piped");
    let mut lines = BufReader::new(stdout).lines();
    let mut http = None;
    let mut querier = None;
    let read = async {
        while http.is_none() || querier.is_none() {
            let Some(line) = lines.next_line().await.expect("read stdout") else {
                let mut err = String::new();
                stderr.read_to_string(&mut err).await.ok();
                panic!("server exited before announcing its addresses; stderr:\n{err}");
            };
            if let Some(rest) = line.strip_prefix("receiver HTTP listening on ") {
                http = Some(rest.trim().parse().expect("http addr"));
            } else if let Some(rest) = line.strip_prefix("querier HTTP listening on ") {
                querier = Some(rest.trim().parse().expect("querier addr"));
            }
        }
    };
    timeout(Duration::from_secs(20), read)
        .await
        .expect("server announces its addresses");
    Server {
        child,
        http: http.expect("http"),
        querier: querier.expect("querier"),
    }
}

async fn stop(mut server: Server) {
    let pid = server.child.id().expect("pid");
    Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status()
        .await
        .expect("kill -TERM");
    let status = timeout(Duration::from_secs(20), server.child.wait())
        .await
        .expect("exit before timeout")
        .expect("await exit");
    assert!(status.success(), "clean shutdown, got {status:?}");
}

async fn raw_post(addr: SocketAddr, head: String, body: &[u8]) -> String {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    stream.write_all(head.as_bytes()).await.expect("write head");
    stream.write_all(body).await.expect("write body");
    stream.flush().await.ok();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.expect("read");
    String::from_utf8_lossy(&response).into_owned()
}

/// OTLP/HTTP export; returns the HTTP status code.
async fn post_logs(addr: SocketAddr, body: &[u8], bearer: Option<&str>) -> u16 {
    let auth = bearer.map_or(String::new(), |t| format!("Authorization: Bearer {t}\r\n"));
    let head = format!(
        "POST /v1/logs HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/x-protobuf\r\n\
         {auth}Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len(),
    );
    status_of(&raw_post(addr, head, body).await)
}

fn status_of(response: &str) -> u16 {
    response
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no status line in {response:?}"))
}

/// `true` over `tenant` — the row count the querier reports.
async fn rows_for(addr: SocketAddr, tenant: &str) -> u64 {
    let dsl = "true";
    let head = format!(
        "POST /v1/query HTTP/1.1\r\nHost: {addr}\r\nX-Ourios-Tenant: {tenant}\r\n\
         Content-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        dsl.len(),
    );
    let response = raw_post(addr, head, dsl.as_bytes()).await;
    assert_eq!(status_of(&response), 200, "query {tenant}: {response}");
    let body = response.split("\r\n\r\n").nth(1).expect("body");
    let json: serde_json::Value = serde_json::from_str(body).expect("json");
    json["rows"].as_u64().expect("rows")
}

/// Every Parquet object under `root` with its size + mtime — the "was
/// anything rewritten" fingerprint.
fn parquet_fingerprint(root: &Path) -> BTreeMap<PathBuf, (u64, SystemTime)> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|x| x == "parquet") {
                let meta = entry.metadata().expect("metadata");
                out.insert(path, (meta.len(), meta.modified().expect("mtime")));
            }
        }
    }
    out
}

const COMPOSITE: &str = "  tenant:\n    rule: [k8s.cluster.name, service.name]\n";

/// Scenarios RFC0045.2 / .3 / .4 / .5 / .8.
/// See `docs/rfcs/0045-composite-tenant-derivation.md` §5.
#[tokio::test]
async fn rfc0045_composite_tenant_end_to_end() {
    let tmp = tempfile::TempDir::new().expect("temp");
    std::fs::create_dir_all(tmp.path().join("store")).expect("store root");

    // Phase 1 — default rule: fluxcd (with a cluster attribute the default
    // rule ignores) lands in tenant `fluxcd`.
    let s1 = start(tmp.path(), "", "").await;
    assert_eq!(
        post_logs(
            s1.http,
            &export(
                &[("service.name", "fluxcd"), ("k8s.cluster.name", "cluster1")],
                "epoch one"
            ),
            None
        )
        .await,
        200
    );
    stop(s1).await;
    let before = parquet_fingerprint(&tmp.path().join("store"));
    assert!(!before.is_empty(), "phase 1 flushed to Parquet on shutdown");

    // Phase 2 — composite rule.
    let s2 = start(tmp.path(), COMPOSITE, "").await;
    let cases: [(&str, &str, &str); 4] = [
        // RFC0045.2 — the S2 pair.
        ("cluster1", "fluxcd", "from cluster one"),
        ("cluster2", "fluxcd", "from cluster two"),
        // RFC0045.4 — the injectivity pair.
        ("a", "b/c", "left tuple"),
        ("a/b", "c", "right tuple"),
    ];
    for (cluster, service, body) in cases {
        assert_eq!(
            post_logs(
                s2.http,
                &export(
                    &[("service.name", service), ("k8s.cluster.name", cluster)],
                    body
                ),
                None
            )
            .await,
            200,
            "{cluster}/{service} accepted"
        );
    }
    // RFC0045.3 — a group lacking / emptying the cluster key rejects the
    // whole export, same posture as a missing service.name.
    for attrs in [
        vec![("service.name", "fluxcd")],
        vec![("service.name", "fluxcd"), ("k8s.cluster.name", "")],
    ] {
        assert_eq!(
            post_logs(s2.http, &export(&attrs, "must not land"), None).await,
            400,
            "missing/empty rule key is rejected: {attrs:?}"
        );
    }
    stop(s2).await;

    // RFC0045.5 — phase-1 files are untouched (nothing rewritten, no
    // repartitioning); the old-epoch tenant still answers.
    let after = parquet_fingerprint(&tmp.path().join("store"));
    for (path, fingerprint) in &before {
        assert_eq!(
            after.get(path),
            Some(fingerprint),
            "{} was rewritten or removed by the rule change",
            path.display()
        );
    }
    assert!(after.len() > before.len(), "phase 2 added its own files");

    // Query every tenant: each sees only its own rows.
    let s3 = start(tmp.path(), COMPOSITE, "").await;
    let expected = [
        ("fluxcd", 1),
        ("cluster1/fluxcd", 1),
        ("cluster2/fluxcd", 1),
        ("a/b%2Fc", 1),
        ("a%2Fb/c", 1),
        ("a/b/c", 0),
        ("cluster1", 0),
    ];
    for (tenant, rows) in expected {
        assert_eq!(
            rows_for(s3.querier, tenant).await,
            rows,
            "tenant {tenant}; store: {:#?}",
            parquet_fingerprint(&tmp.path().join("store"))
                .keys()
                .collect::<Vec<_>>()
        );
    }
    stop(s3).await;

    auth_binding_unchanged(tmp.path()).await;
}

/// Phase 3 / RFC0045.8 — composite rule + a token bound to
/// `cluster1/fluxcd`: a `cluster2/fluxcd` export under it is refused whole,
/// a `cluster1/fluxcd` export is accepted, and no token is still 401.
async fn auth_binding_unchanged(tmp: &Path) {
    let auth = "auth:\n  tokens:\n    - name: cluster-one\n      token: ${env:RFC0045_TOKEN}\n\
                \x20\x20\x20\x20\x20\x20tenants: [cluster1/fluxcd]\n";
    let s4 = start(tmp, COMPOSITE, auth).await;
    assert_eq!(
        post_logs(
            s4.http,
            &export(
                &[("service.name", "fluxcd"), ("k8s.cluster.name", "cluster2")],
                "wrong cluster"
            ),
            Some("tok-cluster-one")
        )
        .await,
        403
    );
    assert_eq!(
        post_logs(
            s4.http,
            &export(
                &[("service.name", "fluxcd"), ("k8s.cluster.name", "cluster1")],
                "right cluster"
            ),
            Some("tok-cluster-one")
        )
        .await,
        200
    );
    assert_eq!(
        post_logs(
            s4.http,
            &export(
                &[("service.name", "fluxcd"), ("k8s.cluster.name", "cluster1")],
                "no token"
            ),
            None
        )
        .await,
        401
    );
    stop(s4).await;
}
