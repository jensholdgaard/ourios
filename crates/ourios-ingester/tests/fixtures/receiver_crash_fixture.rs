//! Crash-before-ack fixture for `rfc0003_2_crash_before_ack`.
//!
//! Not a product binary — declared as a `[[bin]]` only so the test can
//! spawn it as a real OS process and `SIGKILL` it. The crate is
//! `#![deny(unsafe_code)]` (workspace lint), so a `fork()` harness is
//! out; a child driven by `Child::kill()` is the no-`unsafe` way to
//! exercise a genuine crash. Mirrors `ourios-wal`'s `wal_crash_fixture`.
//!
//! Usage: `receiver_crash_fixture <wal_root>`. Builds an `IngestPipeline`
//! over a real `Wal` at that root, ingests one known batch (append +
//! fsync), prints `READY`, then parks. The parent kills it after `READY`
//! — i.e. after the batch is durable but before any transport ack — so a
//! restart's `replay` must still find the frame (at-least-once).

use std::io::Write;
use std::time::Duration;

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::common::v1::any_value::Value;
use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue};
use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
use opentelemetry_proto::tonic::resource::v1::Resource;
use ourios_config::MinerConfig;
use ourios_ingester::receiver::{CommitCoordinator, IngestPipeline, TenantRule};
use ourios_miner::cluster::MinerCluster;
use ourios_wal::{Wal, WalConfig};

fn string_value(s: &str) -> AnyValue {
    AnyValue {
        value: Some(Value::StringValue(s.to_owned())),
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let root = std::env::args()
        .nth(1)
        .expect("fixture: missing <wal_root> arg");
    let config = WalConfig {
        root: root.into(),
        batch_window_ms: 100,
        segment_size_bytes: 128 * 1024 * 1024,
        segment_age_secs: 600,
        housekeeping_secs: 60,
        macos_full_fsync: false,
    };
    let window = Duration::from_millis(config.batch_window_ms);
    let segment_size_bytes = config.segment_size_bytes;
    let wal = Wal::open(config).expect("fixture: Wal::open");
    let coordinator = CommitCoordinator::new(Box::new(wal), window, segment_size_bytes);
    let pipeline = IngestPipeline::new(
        coordinator,
        MinerCluster::new(MinerConfig::default()),
        TenantRule::service_name(),
    );

    let request = ExportLogsServiceRequest {
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
                    body: Some(string_value("user 1 logged in")),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }],
    };

    // Append + fsync the batch (durable) — the receiver would now ack.
    let ingested = pipeline.ingest(request).await.expect("fixture: ingest");
    assert_eq!(ingested, 1, "fixture ingested one record");

    // Signal durability, then park so the parent kills us *after* the
    // fsync but before the (transport) ack — the crash-before-ack window.
    let mut stdout = std::io::stdout();
    writeln!(stdout, "READY").expect("fixture: write READY");
    stdout.flush().expect("fixture: flush READY");
    loop {
        std::thread::park();
    }
}
