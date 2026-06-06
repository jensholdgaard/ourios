//! RFC0003.16 — Served binary: both transports bind, a client export
//! round-trips, graceful shutdown.
//!
//! The only receiver scenario that crosses a real socket: it spawns the
//! `ourios-server` binary with the receiver role enabled on `127.0.0.1:0`
//! (both transports), reads the OS-assigned ports it prints, exports a
//! batch over each with a real gRPC + HTTP client, then sends SIGTERM and
//! waits for a clean exit. Only after the process is gone — freeing the
//! single-writer `Wal` — does it replay the WAL to assert both batches
//! are durable (WAL-before-ack end-to-end), with no acked batch lost. No
//! dedup is asserted (at-least-once).

use std::net::SocketAddr;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::collector::logs::v1::logs_service_client::LogsServiceClient;
use opentelemetry_proto::tonic::common::v1::any_value::Value;
use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue};
use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
use opentelemetry_proto::tonic::resource::v1::Resource;
use ourios_ingester::receiver::decode_protobuf;
use ourios_wal::{FrameKind, FrameSink, RecoveryError, Wal, WalConfig};
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

/// A one-record `checkout` batch carrying `body`.
fn export_request(body: &str) -> ExportLogsServiceRequest {
    ExportLogsServiceRequest {
        resource_logs: vec![ResourceLogs {
            resource: Some(Resource {
                attributes: vec![KeyValue {
                    key: "service.name".to_owned(),
                    value: Some(string_value("checkout")),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            scope_logs: vec![ScopeLogs {
                log_records: vec![LogRecord {
                    body: Some(string_value(body)),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }],
    }
}

/// Hand-rolled OTLP/HTTP POST (no HTTP-client dependency) — returns the
/// raw response so the caller can check the status line.
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

#[derive(Default)]
struct CollectingSink(Vec<(FrameKind, Vec<u8>)>);
impl FrameSink for CollectingSink {
    fn consume(&mut self, kind: FrameKind, payload: &[u8]) -> Result<(), RecoveryError> {
        self.0.push((kind, payload.to_vec()));
        Ok(())
    }
}

fn replay_frames(wal_root: &Path) -> Vec<(FrameKind, Vec<u8>)> {
    let mut sink = CollectingSink::default();
    Wal::open(WalConfig {
        root: wal_root.to_path_buf(),
        batch_window_ms: 100,
        segment_size_bytes: 128 * 1024 * 1024,
        segment_age_secs: 600,
        housekeeping_secs: 60,
        macos_full_fsync: false,
    })
    .expect("reopen WAL")
    .replay(&mut sink)
    .expect("replay");
    sink.0
}

fn body_of(request: &ExportLogsServiceRequest) -> Option<String> {
    match request.resource_logs[0].scope_logs[0].log_records[0]
        .body
        .as_ref()?
        .value
        .as_ref()?
    {
        Value::StringValue(s) => Some(s.clone()),
        _ => None,
    }
}

/// Scenario RFC0003.16 — Served binary: both transports bind, a client
/// export round-trips, graceful shutdown.
/// See `docs/rfcs/0003-otlp-receiver.md` §5.
#[tokio::test]
async fn rfc0003_16_served_binary_binds_round_trips_and_shuts_down() {
    // Arrange: a temp store + WAL root, and the server spawned with the
    // receiver role enabled on ephemeral ports.
    let tmp = tempfile::TempDir::new().expect("temp");
    let wal_root = tmp.path().join("wal");

    let mut child = Command::new(env!("CARGO_BIN_EXE_ourios-server"))
        .env("OURIOS_BUCKET_ROOT", tmp.path())
        .env("OURIOS_RECEIVER_ENABLED", "1")
        .env("OURIOS_RECEIVER_GRPC_ADDR", "127.0.0.1:0")
        .env("OURIOS_RECEIVER_HTTP_ADDR", "127.0.0.1:0")
        .env("OURIOS_WAL_ROOT", &wal_root)
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn ourios-server");

    // Read the OS-assigned addresses the server reports on stdout.
    let stdout = child.stdout.take().expect("server stdout piped");
    let mut lines = BufReader::new(stdout).lines();
    let mut grpc_addr: Option<SocketAddr> = None;
    let mut http_addr: Option<SocketAddr> = None;
    let read_addrs = async {
        while grpc_addr.is_none() || http_addr.is_none() {
            let line = lines
                .next_line()
                .await
                .expect("read server stdout")
                .expect("server stdout closed before reporting addresses");
            if let Some(rest) = line.strip_prefix("receiver gRPC listening on ") {
                grpc_addr = Some(rest.trim().parse().expect("parse gRPC addr"));
            } else if let Some(rest) = line.strip_prefix("receiver HTTP listening on ") {
                http_addr = Some(rest.trim().parse().expect("parse HTTP addr"));
            }
        }
    };
    timeout(Duration::from_secs(15), read_addrs)
        .await
        .expect("server reports its bound addresses before timeout");
    let grpc_addr = grpc_addr.expect("gRPC addr");
    let http_addr = http_addr.expect("HTTP addr");

    // Act: export a batch over each transport with a real client.
    let mut grpc = LogsServiceClient::connect(format!("http://{grpc_addr}"))
        .await
        .expect("connect gRPC");
    grpc.export(export_request("grpc batch"))
        .await
        .expect("gRPC export succeeds");

    let http_response =
        http_post_logs(http_addr, &export_request("http batch").encode_to_vec()).await;
    assert!(
        http_response.starts_with("HTTP/1.1 200"),
        "HTTP export returns 200, got status line {:?}",
        http_response.lines().next(),
    );

    // SIGTERM the server (what k8s / `nerdctl stop` send) and assert a
    // clean exit — graceful shutdown, no panic.
    let pid = child.id().expect("server pid");
    std::process::Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status()
        .expect("send SIGTERM");
    let status = timeout(Duration::from_secs(15), child.wait())
        .await
        .expect("server exits before timeout")
        .expect("await server exit");
    assert!(
        status.success(),
        "graceful shutdown exits cleanly, got {status:?}"
    );

    // Assert: both batches are durable in the WAL (the process is gone, so
    // the single-writer handle is free), recovering each body. No dedup.
    let frames = replay_frames(&wal_root);
    assert_eq!(frames.len(), 2, "one durable OtlpBatch frame per export");
    let bodies: Vec<String> = frames
        .iter()
        .map(|(kind, payload)| {
            assert_eq!(*kind, FrameKind::OtlpBatch);
            let request = decode_protobuf(payload).expect("frame decodes");
            body_of(&request).expect("frame carries a string body")
        })
        .collect();
    assert!(
        bodies.contains(&"grpc batch".to_owned()) && bodies.contains(&"http batch".to_owned()),
        "both transports' batches are durable, got {bodies:?}",
    );
}
