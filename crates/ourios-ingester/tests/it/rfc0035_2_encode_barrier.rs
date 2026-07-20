//! RFC0035.2 — snapshot/rotation stays coherent under in-flight encodes
//! (the §3.1 encode-drain-and-flush barrier). See
//! `docs/rfcs/0035-ingest-concurrency.md` §5.
//!
//! Injects slow in-flight encodes for frames ≤ the rotation mark (a
//! single-worker pool whose every emit crosses a sleeping audit
//! barrier), rotates the WAL, and asserts inside the rotation hook —
//! the `wal_high_water` stamping point — that every record at or below
//! the mark has **completed its sink emit and been durably flushed to
//! the store** before the mark is stamped. Reverting the pipeline's
//! pre-hook `quiesce_encodes()` (the mutation check) makes the hook
//! observe a partially-encoded store and the test fail.
//!
//! The restore arm then proves the stamped artefact is coherent: a
//! fresh miner restored from the rotation snapshot plus tail replay
//! equals a full WAL rebuild — no record loss at the mark.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::ingest_support::{coordinator, request, resource_logs, wal_config};
use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use ourios_config::MinerConfig;
use ourios_ingester::encode_pool::EncodePool;
use ourios_ingester::receiver::{IngestPipeline, TenantRule, fan_out};
use ourios_ingester::record_sink::{FlushConfig, ParquetRecordSink, SharedParquetSink};
use ourios_ingester::recovery;
use ourios_miner::cluster::MinerCluster;
use ourios_parquet::{Reader, Store};
use ourios_wal::{FrameKind, Wal, WalConfig};
use prost::Message;

const PRE_ROTATION_RECORDS: usize = 24;
/// Per-emit delay injected through the audit barrier — long enough that
/// the single worker is still mid-batch when the rotation fires (the
/// batch takes `24 × 100 ms ≈ 2.4 s` against the ~1.3 s rotation
/// point, a ≥ 1 s margin so the mutation check fails deterministically).
const EMIT_DELAY: Duration = Duration::from_millis(100);

/// Rows currently durable in the store (every `*.parquet` under `root`).
fn store_rows(root: &Path) -> usize {
    let mut rows = 0;
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
                rows += Reader::open_file(&path)
                    .expect("open_file")
                    .read_all()
                    .expect("read_all")
                    .len();
            }
        }
    }
    rows
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0035_2_high_water_is_stamped_only_after_drain_and_flush() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let wal_root = tmp.path().join("wal");
    let store_root = tmp.path().join("store");
    let snapshots_root = tmp.path().join("snapshots");
    std::fs::create_dir_all(&store_root).expect("store root");

    let wal = Wal::open(WalConfig {
        segment_age_secs: 1, // rotate on the §6.9 minimum age
        ..wal_config(&wal_root)
    })
    .expect("open WAL");

    // Every emit crosses the tiny size target, and the audit barrier in
    // front of each inline publish sleeps — a deterministic slow encode,
    // injected through a production seam rather than a test hook.
    let sink = SharedParquetSink::new(
        ParquetRecordSink::new(
            Store::local(&store_root).expect("local store"),
            FlushConfig {
                target_bytes: 1,
                max_buffer_age: Duration::from_secs(86_400),
                ceiling_bytes: usize::MAX,
            },
        )
        .with_audit_barrier(Box::new(|| {
            std::thread::sleep(EMIT_DELAY);
            true
        })),
    );

    // What the rotation hook — the wal_high_water stamping point —
    // observed: (buffered, durably-stored rows) at stamp time.
    let observed: Arc<Mutex<Vec<(usize, usize)>>> = Arc::new(Mutex::new(Vec::new()));
    let hook_observed = Arc::clone(&observed);
    let hook_sink = sink.clone();
    let hook_store = store_root.clone();
    let hook_snapshots = snapshots_root.clone();

    let miner = MinerCluster::new(MinerConfig::default()).with_record_sink(Box::new(sink.clone()));
    let pipeline = IngestPipeline::new(
        coordinator(Box::new(wal)),
        miner,
        TenantRule::service_name(),
    )
    .with_encode_pool(EncodePool::new(&sink, 1))
    .with_rotation_hook(Box::new(move |miner, mark| {
        hook_observed
            .lock()
            .expect("lock")
            .push((hook_sink.buffered_records(), store_rows(&hook_store)));
        recovery::write_snapshots(&hook_snapshots, miner, Some(mark)).expect("snapshot write");
    }));

    // Batch A: one frame, PRE_ROTATION_RECORDS records — acked long
    // before its encodes finish (the ack never waits on the pool).
    let bodies: Vec<String> = (0..PRE_ROTATION_RECORDS)
        .map(|i| format!("user {i} logged in"))
        .collect();
    let refs: Vec<&str> = bodies.iter().map(String::as_str).collect();
    pipeline
        .ingest(request(vec![resource_logs("svc", &refs)]))
        .await
        .expect("batch A acks");

    // Cross the segment age cap so batch B rotates while batch A's
    // encodes are still in flight on the single worker.
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    pipeline
        .ingest(request(vec![resource_logs("svc", &["payment 9 settled"])]))
        .await
        .expect("batch B acks");

    let observed = observed.lock().expect("lock").clone();
    assert_eq!(observed.len(), 1, "exactly one rotation fired");
    let (buffered_at_stamp, stored_at_stamp) = observed[0];
    assert_eq!(
        (buffered_at_stamp, stored_at_stamp),
        (0, PRE_ROTATION_RECORDS),
        "the barrier: when wal_high_water is stamped, every record ≤ the mark has \
         completed its encode (nothing buffered) AND is durably flushed (all \
         {PRE_ROTATION_RECORDS} rows in the store) — an unfinished encode or an \
         unflushed buffer must hold the stamp back",
    );

    // Restore arm: the artefact stamped under in-flight encodes is
    // coherent — snapshot at the mark + tail replay == full rebuild.
    pipeline.quiesce_encodes();
    drop(pipeline);
    let rule = TenantRule::service_name();
    let mut recovered = MinerCluster::new(MinerConfig::default());
    let mut wal = Wal::open(WalConfig {
        segment_age_secs: 1,
        ..wal_config(&wal_root)
    })
    .expect("reopen WAL");
    recovery::recover(&mut wal, &snapshots_root, &mut recovered, &rule).expect("recover");
    drop(wal);

    let mut control = MinerCluster::new(MinerConfig::default());
    for (kind, payload) in crate::ingest_support::replay_frames(&wal_root) {
        assert_eq!(kind, FrameKind::OtlpBatch);
        let request = ExportLogsServiceRequest::decode(payload.as_slice()).expect("decode frame");
        for record in fan_out(request, &rule).expect("fan out") {
            control.ingest(&record);
        }
    }
    assert_eq!(recovered.tenant_ids(), control.tenant_ids());
    for tenant in control.tenant_ids() {
        assert_eq!(
            recovered.snapshot_state(&tenant),
            control.snapshot_state(&tenant),
            "restore + tail replay equals the full rebuild — no loss at the mark",
        );
    }
}
