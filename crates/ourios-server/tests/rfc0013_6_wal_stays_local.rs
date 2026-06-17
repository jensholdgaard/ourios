//! RFC0013.6 — WAL stays local, end to end through the served binary.
//! See `docs/rfcs/0013-object-storage.md` §5.
//!
//! Spawns `ourios-server` with the receiver role, a data store rooted at
//! `bucket_root` and a WAL at a **disjoint** `wal_root`, ingests one batch
//! over OTLP/HTTP, and SIGTERMs (graceful shutdown drains the RFC 0014 sink).
//! The assertion is the separation the scenario names: only Parquet (and, when
//! compaction runs, `manifest.json`) objects land under `bucket_root`; the WAL
//! `*.wal` segments stay under `wal_root` and never reach the store
//! (`CLAUDE.md` §3.4 — local disk is cache + WAL; §3.6 — object storage is the
//! source of truth for the Parquet, not the WAL).
//!
//! Unix-only: graceful shutdown is driven by `kill -TERM`, like the colocated
//! RFC0008.10 harness this mirrors.
#![cfg(unix)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::common::v1::any_value::Value;
use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue};
use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
use opentelemetry_proto::tonic::resource::v1::Resource;
use ourios_parquet::Reader;
use prost::Message;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::Command;
use tokio::time::timeout;

fn string_value(s: &str) -> AnyValue {
    AnyValue {
        value: Some(Value::StringValue(s.to_owned())),
    }
}

fn export_request(service: &str, bodies: &[&str]) -> ExportLogsServiceRequest {
    ExportLogsServiceRequest {
        resource_logs: vec![ResourceLogs {
            resource: Some(Resource {
                attributes: vec![KeyValue {
                    key: "service.name".to_owned(),
                    value: Some(string_value(service)),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            scope_logs: vec![ScopeLogs {
                log_records: bodies
                    .iter()
                    .map(|b| LogRecord {
                        body: Some(string_value(b)),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            }],
            ..Default::default()
        }],
    }
}

/// Hand-rolled OTLP/HTTP POST (no HTTP-client dependency).
async fn http_post_logs(addr: SocketAddr, body: &[u8]) -> String {
    let mut stream = TcpStream::connect(addr).await.expect("connect HTTP");
    let head = format!(
        "POST /v1/logs HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/x-protobuf\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len(),
    );
    stream.write_all(head.as_bytes()).await.expect("write head");
    stream.write_all(body).await.expect("write body");
    stream.flush().await.ok();
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .expect("read HTTP response");
    String::from_utf8_lossy(&response).into_owned()
}

/// Every file (not directory) under `root`, recursively.
fn files_under(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out
}

fn has_extension(path: &Path, ext: &str) -> bool {
    path.extension().is_some_and(|e| e == ext)
}

/// Scenario RFC0013.6 — with an object-storage backend, only data/audit/manifest
/// objects reach the store; the WAL stays on local disk (`CLAUDE.md` §3.4).
/// See `docs/rfcs/0013-object-storage.md` §5.
#[tokio::test]
async fn rfc0013_6_wal_stays_local() {
    // Arrange: disjoint store and WAL roots so "in the store" vs "on local
    // disk" is unambiguous.
    let tmp = tempfile::TempDir::new().expect("temp");
    let bucket_root = tmp.path().join("store");
    let wal_root = tmp.path().join("wal");
    std::fs::create_dir_all(&bucket_root).expect("create store root");

    let batch = export_request("checkout", &["user 1 logged in", "user 2 logged in"]);

    // Act: spawn the server, ingest one batch over HTTP, then SIGTERM so the
    // graceful-shutdown drain flushes the sink to Parquet.
    let mut child = Command::new(env!("CARGO_BIN_EXE_ourios-server"))
        .env("OURIOS_BUCKET_ROOT", &bucket_root)
        .env("OURIOS_RECEIVER_ENABLED", "1")
        .env("OURIOS_RECEIVER_GRPC_ADDR", "127.0.0.1:0")
        .env("OURIOS_RECEIVER_HTTP_ADDR", "127.0.0.1:0")
        .env("OURIOS_WAL_ROOT", &wal_root)
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn ourios-server");

    let stdout = child.stdout.take().expect("server stdout piped");
    let mut lines = BufReader::new(stdout).lines();
    let mut http_addr: Option<SocketAddr> = None;
    let read_addr = async {
        while http_addr.is_none() {
            let line = lines
                .next_line()
                .await
                .expect("read server stdout")
                .expect("server stdout closed before reporting addresses");
            if let Some(rest) = line.strip_prefix("receiver HTTP listening on ") {
                http_addr = Some(rest.trim().parse().expect("parse HTTP addr"));
            }
        }
    };
    timeout(Duration::from_secs(15), read_addr)
        .await
        .expect("server reports its bound address before timeout");
    let http_addr = http_addr.expect("HTTP addr");

    let response = http_post_logs(http_addr, &batch.encode_to_vec()).await;
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "export returns 200, got status line {:?}",
        response.lines().next(),
    );

    let pid = child.id().expect("server pid");
    let kill_status = Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status()
        .await
        .expect("run kill -TERM");
    assert!(kill_status.success(), "kill -TERM, got {kill_status:?}");
    let status = timeout(Duration::from_secs(15), child.wait())
        .await
        .expect("server exits before timeout")
        .expect("await server exit");
    assert!(status.success(), "clean exit, got {status:?}");

    // Assert: the store holds the Parquet data and nothing WAL-ish; the WAL
    // segments stayed under the local WAL root.
    let store_files = files_under(&bucket_root);
    let parquet: Vec<&PathBuf> = store_files
        .iter()
        .filter(|p| has_extension(p, "parquet"))
        .collect();
    assert!(
        !parquet.is_empty(),
        "the shutdown drain landed at least one Parquet object in the store; saw {store_files:?}",
    );
    for path in &store_files {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        assert!(
            has_extension(path, "parquet") || name == "manifest.json",
            "only Parquet/manifest objects reach the store, found {path:?}",
        );
        assert!(
            !has_extension(path, "wal"),
            "no WAL segment may reach the store, found {path:?}",
        );
    }

    let wal_files = files_under(&wal_root);
    assert!(
        wal_files.iter().any(|p| has_extension(p, "wal")),
        "the WAL segments stay on local disk under the WAL root; saw {wal_files:?}",
    );
    assert!(
        !wal_files.iter().any(|p| has_extension(p, "parquet")),
        "no Parquet object may live under the WAL root; saw {wal_files:?}",
    );

    // The store's Parquet is the real mined data (the separation isn't
    // vacuous): the checkout records round-trip back out.
    let rows: usize = parquet
        .iter()
        .map(|p| {
            Reader::open_file(p)
                .expect("open flushed parquet")
                .read_all()
                .expect("read flushed parquet")
                .len()
        })
        .sum();
    assert_eq!(rows, 2, "both ingested records were flushed to the store");
}
