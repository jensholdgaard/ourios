//! Crash-with-a-non-empty-buffer fixture for `rfc0014_5_crash_no_loss`.
//!
//! Not a product binary — declared as a `[[bin]]` only so the test can spawn
//! it as a real OS process and `SIGKILL` it (the crate is
//! `#![deny(unsafe_code)]`, so a real child kill is the only faithful crash).
//! Mirrors `receiver_crash_fixture`, but the miner emits into a
//! [`ParquetRecordSink`] so the acknowledged records sit in the in-memory
//! flush buffer at crash time.
//!
//! Usage: `receiver_sink_crash_fixture <wal_root> <bucket_root>`. Builds an
//! `IngestPipeline` over a real `Wal` whose miner emits into a sink (a
//! never-flush [`FlushConfig`], so nothing reaches the store), ingests one
//! known batch (append + fsync = acknowledged), asserts the batch is buffered
//! but **un-flushed** (the store is still empty), prints `READY`, and parks.
//! The parent kills it after `READY` — the volatile buffer dies with the
//! process; only the WAL is durable, so recovery must reconstruct the batch
//! from it (RFC0014.5 / `CLAUDE.md` §3.4).

use std::io::Write;
use std::time::Duration;

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::common::v1::any_value::Value;
use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue};
use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
use opentelemetry_proto::tonic::resource::v1::Resource;
use ourios_config::MinerConfig;
use ourios_ingester::receiver::{CommitCoordinator, IngestPipeline, TenantRule};
use ourios_ingester::record_sink::{FlushConfig, ParquetRecordSink, SharedParquetSink};
use ourios_miner::cluster::MinerCluster;
use ourios_parquet::Store;
use ourios_wal::{Wal, WalConfig};

fn string_value(s: &str) -> AnyValue {
    AnyValue {
        value: Some(Value::StringValue(s.to_owned())),
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mut args = std::env::args().skip(1);
    let wal_root = args.next().expect("fixture: missing <wal_root> arg");
    let bucket_root = args.next().expect("fixture: missing <bucket_root> arg");

    let config = WalConfig {
        root: wal_root.into(),
        batch_window_ms: 100,
        segment_size_bytes: 128 * 1024 * 1024,
        segment_age_secs: 600,
        housekeeping_secs: 60,
        macos_full_fsync: false,
    };
    let window = Duration::from_millis(config.batch_window_ms);
    let segment_size_bytes = config.segment_size_bytes;
    let wal = Wal::open(config).expect("fixture: Wal::open");

    // Never-flush config: no size/age/ceiling trigger fires, so the batch
    // stays in the in-memory buffer and nothing reaches the store.
    let store = Store::local(&bucket_root).expect("fixture: Store::local");
    let sink = SharedParquetSink::new(ParquetRecordSink::new(
        store,
        FlushConfig {
            target_bytes: usize::MAX,
            max_buffer_age: Duration::from_secs(86_400),
            ceiling_bytes: usize::MAX,
        },
    ));

    let coordinator = CommitCoordinator::new(Box::new(wal), window, segment_size_bytes);
    // The production ingest shape (RFC 0035): the sink emit runs on the
    // concurrent encode pool, so the crash window covers the pooled path.
    let pipeline = IngestPipeline::new(
        coordinator,
        MinerCluster::new(MinerConfig::default()).with_record_sink(Box::new(sink.clone())),
        TenantRule::service_name(),
    )
    .with_encode_pool(ourios_ingester::encode_pool::EncodePool::new(&sink, 2));

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
                log_records: vec![
                    LogRecord {
                        body: Some(string_value("user 1 logged in")),
                        ..Default::default()
                    },
                    LogRecord {
                        body: Some(string_value("user 2 logged in")),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }],
            ..Default::default()
        }],
    };

    // Append + fsync the batch (durable) — the receiver would now ack.
    let ingested = pipeline.ingest(request).await.expect("fixture: ingest");
    assert_eq!(ingested, 2, "fixture ingested two records");

    // Drain the encode pool so the acked records have reached the buffer
    // before the assertions (the ack itself never waits on the encode).
    pipeline.quiesce_encodes();

    // The acknowledged records are durable in the WAL and sitting in the
    // volatile buffer, but NOT flushed: the store must still be empty.
    assert_eq!(sink.flushes(), 0, "fixture: no flush trigger fired");
    assert!(
        sink.buffered_records() >= 2,
        "fixture: the acked records are buffered, not yet in the store",
    );

    // Signal durability, then park so the parent kills us *after* the fsync
    // but before any flush — the crash-with-a-non-empty-buffer window.
    let mut stdout = std::io::stdout();
    writeln!(stdout, "READY").expect("fixture: write READY");
    stdout.flush().expect("fixture: flush READY");
    loop {
        std::thread::park();
    }
}
