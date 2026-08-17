//! RFC0046.4 — The WAL frame carries the tenant. See
//! `docs/rfcs/0046-out-of-band-tenancy.md` §5.
//!
//! Exports acknowledged under two different selectors — whose records carry
//! **no** `service.name` at all, so nothing could be derived — are fsynced
//! into the WAL and then lost from process state before any Parquet flush
//! (the pipeline and its never-flush sink are dropped, the RFC0014.5 shape
//! without the SIGKILL, which that scenario already proves). Recovery must
//! land every record in the tenant it was acknowledged under, from the
//! `0x03` frame alone.

use std::path::Path;
use std::time::Duration;

use ourios_config::MinerConfig;
use ourios_core::record::MinedRecord;
use ourios_core::tenant::TenantId;
use ourios_ingester::record_sink::{FlushConfig, ParquetRecordSink, SharedParquetSink};
use ourios_ingester::recovery;
use ourios_miner::cluster::MinerCluster;
use ourios_parquet::{Reader, Store};
use ourios_wal::{FrameKind, Wal};

use crate::ingest_support::{
    open_pipeline, replay_frames, request, resource_logs_with_attrs, wal_config,
};

fn never_flush() -> FlushConfig {
    FlushConfig {
        target_bytes: usize::MAX,
        max_buffer_age: Duration::from_secs(86_400),
        ceiling_bytes: usize::MAX,
    }
}

fn all_rows(root: &Path) -> Vec<MinedRecord> {
    let mut rows = Vec::new();
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
                rows.extend(
                    Reader::open_file(&path)
                        .expect("open_file")
                        .read_all()
                        .expect("read_all"),
                );
            }
        }
    }
    rows
}

/// Scenario RFC0046.4 — WAL frame carries the tenant.
/// See `docs/rfcs/0046-out-of-band-tenancy.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0046_4_replay_lands_records_in_the_acknowledged_tenant() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let wal_root = tmp.path().join("wal");
    let bucket_root = tmp.path().join("store");
    std::fs::create_dir_all(&bucket_root).expect("store root");

    // Acknowledge under two selectors; no resource carries service.name.
    {
        let pipeline = open_pipeline(&wal_root);
        for (tenant, body) in [
            ("acme", "acme one"),
            ("globex", "globex one"),
            ("acme", "acme two"),
        ] {
            pipeline
                .ingest(
                    request(vec![resource_logs_with_attrs(
                        &[("host.name", "n1")],
                        &[body],
                    )]),
                    TenantId::new(tenant),
                )
                .await
                .expect("acked");
        }
        // Dropped without any flush: the WAL is the only durable copy.
    }
    assert!(all_rows(&bucket_root).is_empty(), "nothing flushed");
    let frames = replay_frames(&wal_root);
    assert_eq!(frames.len(), 3);
    assert!(
        frames
            .iter()
            .all(|(kind, _)| *kind == FrameKind::TenantOtlpBatch),
        "every ingest frame is kind 0x03"
    );

    // Recover into a fresh miner + sink and flush.
    let mut wal = Wal::open(wal_config(&wal_root)).expect("reopen WAL");
    let store = Store::local(&bucket_root).expect("store");
    let sink = SharedParquetSink::new(ParquetRecordSink::new(store, never_flush()));
    let mut miner =
        MinerCluster::new(MinerConfig::default()).with_record_sink(Box::new(sink.clone()));
    let report = recovery::recover(&mut wal, &wal_root.join("snapshots"), &mut miner)
        .expect("startup recovery");
    assert_eq!(report.records_fed_to_miner, 3);
    sink.flush_all();

    let rows = all_rows(&bucket_root);
    let mut by_tenant: Vec<(String, String)> = rows
        .iter()
        .map(|r| {
            (
                r.tenant_id.as_str().to_owned(),
                r.body.clone().unwrap_or_default(),
            )
        })
        .collect();
    by_tenant.sort();
    assert_eq!(
        by_tenant
            .iter()
            .map(|(t, _)| t.as_str())
            .collect::<Vec<_>>(),
        ["acme", "acme", "globex"],
        "each record is in the tenant it was acknowledged under: {by_tenant:?}"
    );
}
