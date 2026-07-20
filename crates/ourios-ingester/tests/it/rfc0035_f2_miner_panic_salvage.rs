//! RFC 0035 review F2 — a miner panic mid-batch on the pooled path must
//! not lose the records mined before it, and must leave the pipeline
//! (capture slot, ingest gate, miner mutex) usable for the next batch.
//!
//! Pre-split, records 1..k of a panicking batch had already been emitted
//! inline by the time the panic fired; the ordered/concurrent split
//! buffered them in a local that unwound before `pool.submit`. The fix
//! submits what was mined before re-panicking (pipeline.rs) and settles
//! the capture slot across the unwind (`ingest_mined`). This drives a
//! real batch whose third record panics the miner (an injected
//! audit-sink panic — audit emission is in the ordered phase) and
//! asserts the pre-panic records still reach Parquet, then that a clean
//! follow-up batch acks and lands.

use ourios_config::MinerConfig;
use ourios_core::audit::{AuditEvent, AuditPayload, AuditSink, TemplateChange};
use ourios_ingester::encode_pool::EncodePool;
use ourios_ingester::receiver::{IngestPipeline, TenantRule};
use ourios_ingester::record_sink::{ParquetRecordSink, SharedParquetSink};
use ourios_miner::cluster::MinerCluster;
use ourios_parquet::{Reader, Store};
use ourios_wal::Wal;
use std::path::Path;
use std::sync::Arc;

use crate::ingest_support::{coordinator, never_flush, request, resource_logs, wal_config};

/// Panics on the second `Created` template event, once. The batch's
/// first template mints fine; the second mint unwinds mid-batch.
struct PanicOnSecondCreated {
    created_seen: usize,
    fired: bool,
}

impl AuditSink for PanicOnSecondCreated {
    fn emit(&mut self, event: AuditEvent) {
        if let AuditPayload::Template {
            change: TemplateChange::Created { .. },
            ..
        } = &event.payload
        {
            self.created_seen += 1;
            if self.created_seen == 2 && !self.fired {
                self.fired = true;
                panic!("injected mid-batch miner panic");
            }
        }
    }
}

/// Decoded rows (as `template id + params` descriptors) in every
/// `*.parquet` under `root`.
fn stored_rows(root: &Path) -> Vec<String> {
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
                for record in Reader::open_file(&path)
                    .expect("open_file")
                    .read_all()
                    .expect("read_all")
                {
                    rows.push(format!(
                        "template {} params {:?}",
                        record.template_id,
                        record.params.iter().map(|p| &p.value).collect::<Vec<_>>(),
                    ));
                }
            }
        }
    }
    rows.sort();
    rows
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0035_f2_pre_panic_records_reach_parquet_and_the_next_batch_acks() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let store_root = tmp.path().join("store");
    std::fs::create_dir_all(&store_root).expect("store root");
    let wal = Wal::open(wal_config(&tmp.path().join("wal"))).expect("open WAL");
    let sink = SharedParquetSink::new(ParquetRecordSink::new(
        Store::local(&store_root).expect("local store"),
        never_flush(),
    ));
    let miner = MinerCluster::with_audit_sink(
        MinerConfig::default(),
        Box::new(PanicOnSecondCreated {
            created_seen: 0,
            fired: false,
        }),
    )
    .with_record_sink(Box::new(sink.clone()));
    let pipeline = Arc::new(
        IngestPipeline::new(
            coordinator(Box::new(wal)),
            miner,
            TenantRule::service_name(),
        )
        .with_encode_pool(EncodePool::new(&sink, 2)),
    );

    // Records 1–2 share the batch's first template (one Created + one
    // attach); record 3 mints the second template and panics the miner;
    // record 4 is never mined.
    let poisoned = pipeline.clone();
    let outcome = tokio::spawn(async move {
        poisoned
            .ingest(request(vec![resource_logs(
                "svc",
                &["alpha 1 done", "alpha 2 done", "beta started", "gamma seen"],
            )]))
            .await
    })
    .await;
    assert!(
        outcome.is_err(),
        "the injected miner panic propagates out of the ingest task",
    );

    // The records mined before the panic were submitted to the pool and
    // must reach durable Parquet.
    pipeline.quiesce_encodes();
    sink.flush_all();
    let salvaged = stored_rows(&store_root);
    assert_eq!(
        salvaged.len(),
        2,
        "records mined before the mid-batch panic reach Parquet (got {salvaged:?})",
    );

    // The pipeline survives: gate released, miner mutex recovered,
    // capture slot clean — a clean follow-up batch acks and lands.
    let ingested = pipeline
        .ingest(request(vec![resource_logs("svc", &["delta 7 sent"])]))
        .await
        .expect("the next batch acks after the panic");
    assert_eq!(ingested, 1);
    pipeline.quiesce_encodes();
    sink.flush_all();
    assert_eq!(
        stored_rows(&store_root).len(),
        3,
        "the follow-up batch's record landed too",
    );
    assert_eq!(
        pipeline.with_miner(ourios_miner::cluster::MinerCluster::mined_capture_salvages_total),
        0,
        "the panic fired before a capture, so nothing needed salvaging",
    );
}
