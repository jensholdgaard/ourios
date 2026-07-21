//! Crash-with-a-non-empty-buffer fixture for `rfc0014_5_crash_no_loss`.
//!
//! Not a product binary — declared as a `[[bin]]` only so the test can spawn
//! it as a real OS process and `SIGKILL` it (the crate is
//! `#![deny(unsafe_code)]`, so a real child kill is the only faithful crash).
//! Mirrors `receiver_crash_fixture`, but the miner emits into a
//! [`ParquetRecordSink`] so the acknowledged records sit in the in-memory
//! flush buffer at crash time.
//!
//! Usage: `receiver_sink_crash_fixture <wal_root> <bucket_root> [window]`.
//! Builds an `IngestPipeline` over a real `Wal` whose miner emits into a sink
//! (nothing reaches the store), ingests one known batch (append + fsync =
//! acknowledged), asserts the batch is **un-flushed** (the store is still
//! empty), prints `READY`, and parks. The parent kills it after `READY`;
//! only the WAL is durable, so recovery must reconstruct the batch from it
//! (RFC0014.5 / `CLAUDE.md` §3.4).
//!
//! `window` selects where the acked records sit at crash time:
//! - `buffer` (default): in the in-memory flush buffer (RFC0014.5).
//! - `sweep`: drained **out** of the buffers by the age sweep's atomic
//!   drain, with the off-lock `write_ordered` (the slow S3 PUT) still in
//!   flight — the issue #578 window, where the records' only copies are
//!   this process's memory and the WAL.

use std::io::Write;
use std::time::Duration;

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::common::v1::any_value::Value;
use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue};
use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
use opentelemetry_proto::tonic::resource::v1::Resource;
use ourios_config::MinerConfig;
use ourios_ingester::audit_sink::{BufferingAuditSink, SharedParquetAuditSink};
use ourios_ingester::publish::PublishCoordinator;
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
    let crash_window = args.next().unwrap_or_else(|| "buffer".to_owned());

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

    // No size/age/ceiling trigger fires on emit, so the batch stays in the
    // in-memory buffer and nothing reaches the store. The `sweep` window uses
    // a zero buffer age so the age sweep's `drain_aged` takes the partition.
    let store = Store::local(&bucket_root).expect("fixture: Store::local");
    let max_buffer_age = match crash_window.as_str() {
        "buffer" => Duration::from_secs(86_400),
        "sweep" => Duration::ZERO,
        other => panic!("fixture: unknown window {other:?}"),
    };
    let sink = SharedParquetSink::new(ParquetRecordSink::new(
        store.clone(),
        FlushConfig {
            target_bytes: usize::MAX,
            max_buffer_age,
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

    // The issue #578 window: run the age sweep's first half — the atomic
    // drain under the miner lock — and hold the drained snapshot without
    // ever starting its off-lock `write_ordered`, as if the store PUT hung.
    // The records are now out of the buffers too; their only copies are this
    // process's memory (killed below) and the WAL.
    let _in_flight = match crash_window.as_str() {
        "sweep" => {
            let audit = SharedParquetAuditSink::new(BufferingAuditSink::new(store, 1024));
            let coordinator = PublishCoordinator::new(sink.clone(), audit);
            let drained = pipeline.with_miner(|_miner| coordinator.drain_aged());
            assert!(!drained.is_empty(), "fixture: the sweep drained the batch");
            assert_eq!(
                sink.buffered_records(),
                0,
                "fixture: the acked records left the buffers — in flight",
            );
            Some(drained)
        }
        _ => None,
    };

    // Signal durability, then park so the parent kills us *after* the fsync
    // but before any flush — the crash-with-a-non-empty-buffer (or
    // crash-mid-sweep-publish) window.
    let mut stdout = std::io::stdout();
    writeln!(stdout, "READY").expect("fixture: write READY");
    stdout.flush().expect("fixture: flush READY");
    loop {
        std::thread::park();
    }
}
