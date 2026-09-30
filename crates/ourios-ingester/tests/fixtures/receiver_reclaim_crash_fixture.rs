//! Crash-under-reclamation fixture for `rfc0052_10_no_loss`.
//!
//! Not a product binary — declared as a `[[bin]]` only so the test can spawn
//! it as a real OS process and `SIGKILL` it. Extends
//! `receiver_sink_crash_fixture` (RFC0014.5) with what RFC 0052 adds: the
//! barrier that cuts, snapshots and stamps the checkpoint, and the
//! housekeeping pass that reclaims segments below it, both on a short
//! cadence over segments that age out after a second, with the default
//! rotation retry budget.
//!
//! Usage: `receiver_reclaim_crash_fixture <wal_root> <data_root> <audit_root>`.
//! Ingests one single-record batch after another, printing
//! `ACK <n> <segment> <byte>` once batch `n` is acknowledged (append +
//! fsync) at that frame offset, while a second thread runs the barrier and
//! the housekeeping pass, printing `RECLAIMED <segments>` whenever a pass
//! unlinks something. It never exits on its own; the parent kills it.

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::common::v1::any_value::Value;
use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue};
use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
use opentelemetry_proto::tonic::resource::v1::Resource;
use ourios_config::MinerConfig;
use ourios_core::tenant::TenantId;
use ourios_ingester::audit_sink::{BufferingAuditSink, SharedParquetAuditSink};
use ourios_ingester::barrier::Barrier;
use ourios_ingester::encode_pool::EncodePool;
use ourios_ingester::housekeeping::{Housekeeper, HousekeepingTick};
use ourios_ingester::publish::PublishCoordinator;
use ourios_ingester::receiver::{CommitCoordinator, IngestPipeline, SharedPipeline};
use ourios_ingester::record_sink::{FlushConfig, ParquetRecordSink, SharedParquetSink};
use ourios_miner::cluster::MinerCluster;
use ourios_parquet::Store;
use ourios_wal::{Wal, WalConfig};

const TENANT: &str = "checkout";
const CADENCE: Duration = Duration::from_millis(100);

/// The WAL knobs at their floors, so segments seal and become reclaimable
/// within a second of their first frame.
fn wal_config(root: String) -> WalConfig {
    WalConfig {
        root: root.into(),
        batch_window_ms: 20,
        segment_size_bytes: ourios_wal::MIN_SEGMENT_SIZE_BYTES,
        segment_age_secs: 1,
        housekeeping_secs: 1,
        max_unlinks_per_pass: ourios_wal::DEFAULT_MAX_UNLINKS_PER_PASS,
        rotation_retry_attempts: ourios_wal::DEFAULT_ROTATION_RETRY_ATTEMPTS,
        macos_full_fsync: false,
    }
}

/// The receiver minus its listeners, wired the way `serve` wires it: only
/// a cut flushes, so everything above the checkpoint sits in the buffers.
struct Node {
    pipeline: SharedPipeline,
    barrier: Arc<Barrier>,
    housekeeper: Housekeeper,
}

fn node(wal_root: String, data_root: &str, audit_root: &str) -> Node {
    let config = wal_config(wal_root);
    let snapshots_root = config.root.join("snapshots");
    let max_unlinks = usize::try_from(config.max_unlinks_per_pass).expect("fixture: cap fits");
    let wal = Wal::open(config).expect("fixture: Wal::open");
    let audit = SharedParquetAuditSink::new(BufferingAuditSink::new(
        Store::local(audit_root).expect("fixture: audit store"),
        100_000,
    ));
    let barrier_audit = audit.clone();
    let sink = SharedParquetSink::new(
        ParquetRecordSink::new(
            Store::local(data_root).expect("fixture: data store"),
            FlushConfig {
                target_bytes: usize::MAX,
                max_buffer_age: Duration::from_secs(86_400),
                ceiling_bytes: usize::MAX,
            },
        )
        .with_audit_barrier(Box::new(move || barrier_audit.flush())),
    );
    let miner = MinerCluster::with_audit_sink(MinerConfig::default(), Box::new(audit.clone()))
        .with_record_sink(Box::new(sink.clone()));
    let commits = CommitCoordinator::new(
        Box::new(wal),
        Duration::from_millis(20),
        ourios_wal::MIN_SEGMENT_SIZE_BYTES,
    );
    let publish = PublishCoordinator::new(sink, audit);
    let barrier = Arc::new(Barrier::new(
        publish.clone(),
        Arc::clone(&commits),
        snapshots_root,
        usize::MAX,
    ));
    let hook = Arc::clone(&barrier);
    let pipeline = IngestPipeline::new(Arc::clone(&commits), miner)
        .with_encode_pool(EncodePool::with_publisher(publish.publisher(), 2))
        .with_rotation_hook(Box::new(move |miner, mark| {
            hook.capture_rotation(miner, mark);
        }));
    let housekeeper = Housekeeper::new(commits, Arc::clone(&barrier), publish, max_unlinks);
    Node {
        pipeline: Arc::new(pipeline),
        barrier,
        housekeeper,
    }
}

/// One record whose `time_unix_nano` is `n`, the identity the parent reads
/// back out of Parquet.
fn batch(n: u64) -> ExportLogsServiceRequest {
    let text = |s: String| AnyValue {
        value: Some(Value::StringValue(s)),
    };
    ExportLogsServiceRequest {
        resource_logs: vec![ResourceLogs {
            resource: Some(Resource {
                attributes: vec![KeyValue {
                    key: "service.name".to_owned(),
                    value: Some(text(TENANT.to_owned())),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            scope_logs: vec![ScopeLogs {
                log_records: vec![LogRecord {
                    time_unix_nano: n,
                    body: Some(text(format!("order {n} shipped"))),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }],
    }
}

fn say(line: &str) {
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{line}").expect("fixture: write");
    stdout.flush().expect("fixture: flush");
}

/// The barrier and the housekeeping pass, each tick in turn, forever.
fn run_cadence(pipeline: &SharedPipeline, barrier: &Barrier, housekeeper: &Housekeeper) {
    loop {
        std::thread::sleep(CADENCE);
        barrier.tick(pipeline, true);
        if let HousekeepingTick::Completed(pass) = housekeeper.tick()
            && pass.removed_segments > 0
        {
            say(&format!("RECLAIMED {}", pass.removed_segments));
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mut args = std::env::args().skip(1);
    let mut arg = |name| {
        args.next()
            .unwrap_or_else(|| panic!("fixture: missing <{name}>"))
    };
    let (wal_root, data_root, audit_root) = (arg("wal_root"), arg("data_root"), arg("audit_root"));
    let Node {
        pipeline,
        barrier,
        housekeeper,
    } = node(wal_root, &data_root, &audit_root);

    let cadence = Arc::clone(&pipeline);
    std::thread::spawn(move || run_cadence(&cadence, &barrier, &housekeeper));

    for n in 1..=u64::MAX {
        pipeline
            .ingest(batch(n), TenantId::new(TENANT))
            .await
            .expect("fixture: ingest");
        let frame = pipeline.last_durable().expect("fixture: a durable frame");
        say(&format!("ACK {n} {} {}", frame.segment, frame.byte));
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}
