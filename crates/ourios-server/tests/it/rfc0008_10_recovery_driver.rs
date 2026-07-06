//! RFC0008.10 — Startup recovery driver, end to end through the
//! served binary. See `docs/rfcs/0008-wal.md` §5.
//!
//! Pre-populates a WAL root with two durable frames and a per-tenant
//! snapshot covering only the first, spawns `ourios-server` with the
//! receiver role (recovery runs before the listeners bind), sends one
//! more export over HTTP, then SIGTERMs. The shutdown-written
//! snapshot artefacts must reflect restored + tail-replayed + live
//! state: equal, per tenant, to a control miner fed the same records
//! from scratch — proving the driver restored the snapshot,
//! suppressed the covered frame, replayed the tail, served live
//! traffic, and wrote coherent snapshots at the shutdown cadence
//! point (RFC 0001 §6.9).
//!
//! Unix-only: graceful shutdown is driven by `kill -TERM`, and the
//! server's SIGTERM handling is itself Unix-only.
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
use ourios_config::MinerConfig;
use ourios_ingester::receiver::{TenantRule, fan_out};
use ourios_ingester::{recovery, snapshot_store};
use ourios_miner::cluster::MinerCluster;
use ourios_miner::snapshot::RecoveryOutcome;
use ourios_wal::{FrameKind, Wal, WalConfig};
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

/// A batch for `service` carrying one string-body record per entry
/// in `bodies`.
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
                    .map(|body| LogRecord {
                        body: Some(string_value(body)),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            }],
            ..Default::default()
        }],
    }
}

fn wal_config(root: &Path) -> WalConfig {
    WalConfig {
        root: root.to_path_buf(),
        batch_window_ms: 100,
        segment_size_bytes: 128 * 1024 * 1024,
        segment_age_secs: 600,
        housekeeping_secs: 60,
        macos_full_fsync: false,
    }
}

/// Hand-rolled OTLP/HTTP POST (no HTTP-client dependency) — returns
/// the raw response so the caller can check the status line.
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

/// Scenario RFC0008.10 — Startup recovery driver: per-consumer
/// horizons, observed through the served binary.
/// See `docs/rfcs/0008-wal.md` §5.
#[tokio::test]
async fn rfc0008_10_recovery_runs_before_serving_and_shutdown_snapshots_are_coherent() {
    // Arrange: a WAL with two durable frames — `covered` (checkout,
    // inside the snapshot) and `tail` (billing, above the snapshot's
    // high-water mark S) — plus the snapshot artefact at S.
    let tmp = tempfile::TempDir::new().expect("temp");
    let wal_root: PathBuf = tmp.path().join("wal");
    let snapshots_root = wal_root.join("snapshots");
    let rule = TenantRule::service_name();

    let covered = export_request("checkout", &["user 1 logged in", "user 2 logged in"]);
    let tail = export_request("billing", &["charge 9 EUR accepted"]);
    let live_batch = export_request("checkout", &["user 3 logged in"]);

    let s = {
        let mut wal = Wal::open(wal_config(&wal_root)).expect("open WAL");
        wal.append(FrameKind::OtlpBatch, &covered.encode_to_vec())
            .expect("append covered");
        let s = wal.sync().expect("sync covered");
        wal.append(FrameKind::OtlpBatch, &tail.encode_to_vec())
            .expect("append tail");
        wal.sync().expect("sync tail");
        s
    };

    let mut snap_miner = MinerCluster::new(MinerConfig::default());
    for record in fan_out(covered.clone(), &rule).expect("fan out covered") {
        snap_miner.ingest(&record);
    }
    recovery::write_snapshots(&snapshots_root, &snap_miner, Some(s)).expect("snapshot at S");

    // Act: spawn the server (recovery runs to completion before the
    // listeners bind — the reported addresses are the proof the bind
    // happened, and a recovery failure would abort startup).
    let mut child = Command::new(env!("CARGO_BIN_EXE_ourios-server"))
        .env("OURIOS_BUCKET_ROOT", tmp.path())
        .env("OURIOS_RECEIVER_ENABLED", "1")
        .env("OURIOS_RECEIVER_GRPC_ADDR", "127.0.0.1:0")
        .env("OURIOS_RECEIVER_HTTP_ADDR", "127.0.0.1:0")
        .env("OURIOS_WAL_ROOT", &wal_root)
        .stdout(Stdio::piped())
        // Reap the server if the test returns early (timeout / panic)
        // so a failing run can't leak the process.
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

    // One live export on top of the recovered state.
    let response = http_post_logs(http_addr, &live_batch.encode_to_vec()).await;
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "live export returns 200, got status line {:?}",
        response.lines().next(),
    );

    // Graceful shutdown — the second snapshot cadence point.
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

    // Assert: the shutdown-written artefacts equal a control miner
    // fed covered → tail → live from scratch — restored snapshot,
    // suppressed covered frame, replayed tail, live ingest, all
    // folded into one coherent per-tenant state.
    let mut control = MinerCluster::new(MinerConfig::default());
    for request in [&covered, &tail, &live_batch] {
        for record in fan_out(request.clone(), &rule).expect("fan out") {
            control.ingest(&record);
        }
    }

    let artefacts = snapshot_store::load_all(&snapshots_root).expect("load shutdown snapshots");
    let tenants: Vec<&str> = artefacts.iter().map(|(t, _)| t.as_str()).collect();
    assert_eq!(tenants, vec!["billing", "checkout"]);
    for (tenant, bytes) in &artefacts {
        let (state, outcome) = ourios_miner::snapshot::recover(Some(bytes));
        assert_eq!(outcome, RecoveryOutcome::Restored);
        let mut state = state.expect("known-version artefact decodes");
        assert!(
            state.wal_high_water.is_some(),
            "the shutdown snapshot records the durable high-water mark",
        );
        state.wal_high_water = None;
        assert_eq!(
            state,
            control.snapshot_state(tenant),
            "tenant {:?} shutdown snapshot diverges from the control",
            tenant.as_str(),
        );
    }
}

/// A process that serves zero requests must still stamp its shutdown
/// snapshots with the replay high-water mark (the recovery-seeded
/// `last_durable`) — an unstamped artefact is discarded at the next
/// start, degrading every restart-without-traffic to a full replay.
#[tokio::test]
async fn rfc0008_10_shutdown_without_live_traffic_stamps_the_recovered_high_water() {
    // Arrange: a WAL with one durable frame, no snapshot.
    let tmp = tempfile::TempDir::new().expect("temp");
    let wal_root: PathBuf = tmp.path().join("wal");
    let snapshots_root = wal_root.join("snapshots");

    let batch = export_request("checkout", &["user 1 logged in"]);
    let durable = {
        let mut wal = Wal::open(wal_config(&wal_root)).expect("open WAL");
        wal.append(FrameKind::OtlpBatch, &batch.encode_to_vec())
            .expect("append");
        wal.sync().expect("sync")
    };

    // Act: spawn, wait for the bound-address report (recovery is
    // complete by then), SIGTERM without sending a single request.
    let mut child = Command::new(env!("CARGO_BIN_EXE_ourios-server"))
        .env("OURIOS_BUCKET_ROOT", tmp.path())
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
    let read_addr = async {
        loop {
            let line = lines
                .next_line()
                .await
                .expect("read server stdout")
                .expect("server stdout closed before reporting addresses");
            if line.starts_with("receiver HTTP listening on ") {
                break;
            }
        }
    };
    timeout(Duration::from_secs(15), read_addr)
        .await
        .expect("server reports its bound address before timeout");

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

    // Assert: the shutdown-written artefact carries the replayed
    // frame's offset as its high-water mark.
    let artefacts = snapshot_store::load_all(&snapshots_root).expect("load shutdown snapshots");
    assert_eq!(artefacts.len(), 1);
    let (state, outcome) = ourios_miner::snapshot::recover(Some(&artefacts[0].1));
    assert_eq!(outcome, RecoveryOutcome::Restored);
    let high_water = state
        .expect("known-version artefact decodes")
        .wal_high_water
        .expect("the zero-request shutdown snapshot still records a high-water mark");
    assert_eq!(high_water.segment, durable.segment.to_string());
    assert_eq!(high_water.byte, durable.byte);
}
