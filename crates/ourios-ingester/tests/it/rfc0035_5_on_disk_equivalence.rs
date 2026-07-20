//! RFC0035.5 — no on-disk or query change (the Design A scope guard).
//! See `docs/rfcs/0035-ingest-concurrency.md` §5.
//!
//! One fixed corpus through both ingest shapes — the pre-RFC fully
//! synchronous path (`miner.ingest` emits under the gate) and the
//! pooled ordered/concurrent split — then, via the existing read path,
//! assert the Parquet **schema**, the **`template_id` values**, and the
//! decoded rows as a **multiset** are identical, and that a
//! `template_id == N` query differential returns identical result sets.
//! Byte-for-byte file identity is explicitly NOT claimed (§5: row order
//! within a partition may differ; never file-hash).

use std::path::{Path, PathBuf};

use crate::ingest_support::{never_flush, pooled_wal_pipeline, request, resource_logs, wal_config};
use ourios_config::MinerConfig;
use ourios_core::record::MinedRecord;
use ourios_ingester::receiver::{IngestPipeline, TenantRule};
use ourios_ingester::record_sink::{ParquetRecordSink, SharedParquetSink};
use ourios_miner::cluster::MinerCluster;
use ourios_parquet::{Reader, Store};
use ourios_wal::Wal;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

/// The fixed multi-tenant corpus: clean templates (id-assignment
/// order-sensitive), numeric params, a parameter-overflow line (spills
/// to the body column, §3.2), an empty body (parse failure), and a
/// widening mix — the paths whose encoded columns differ.
fn corpus() -> Vec<opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest> {
    let overflow = format!("blob {} attached", "x".repeat(400));
    vec![
        request(vec![
            resource_logs("checkout", &["user 1 logged in", "user 2 logged in"]),
            resource_logs("billing", &["charge 9 EUR accepted"]),
        ]),
        request(vec![resource_logs(
            "checkout",
            &["user alpha logged in", "user 3 logged out"],
        )]),
        request(vec![resource_logs("billing", &[overflow.as_str(), ""])]),
        request(vec![
            resource_logs("checkout", &["cache warmed"]),
            resource_logs("billing", &["charge 12 EUR accepted"]),
        ]),
    ]
}

/// Every `*.parquet` file under `root`.
fn parquet_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
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
                files.push(path);
            }
        }
    }
    files
}

/// Decode every row under `root` through the existing read path, plus
/// the distinct Arrow schemas of the files (rendered, for equality).
fn read_side(root: &Path) -> (Vec<MinedRecord>, Vec<String>) {
    let mut rows = Vec::new();
    let mut schemas = Vec::new();
    for path in parquet_files(root) {
        let file = std::fs::File::open(&path).expect("open parquet file");
        let schema = format!(
            "{:?}",
            ParquetRecordBatchReaderBuilder::try_new(file)
                .expect("parquet metadata")
                .schema()
        );
        if !schemas.contains(&schema) {
            schemas.push(schema);
        }
        rows.extend(
            Reader::open_file(&path)
                .expect("open_file")
                .read_all()
                .expect("read_all"),
        );
    }
    schemas.sort();
    (rows, schemas)
}

/// Order `rows` canonically (total order via the derived `Debug`
/// rendering, which covers every field) — multiset comparison, never
/// row-order or file-hash comparison (§5).
fn canonicalize(mut rows: Vec<MinedRecord>) -> Vec<MinedRecord> {
    rows.sort_by_cached_key(|r| format!("{r:?}"));
    rows
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0035_5_pooled_and_sync_paths_write_equivalent_parquet() {
    let tmp = tempfile::TempDir::new().expect("temp");

    // Sync side: the pre-RFC path — no pool, emit under the gate.
    let sync_store = tmp.path().join("sync-store");
    std::fs::create_dir_all(&sync_store).expect("store root");
    let sync_sink = SharedParquetSink::new(ParquetRecordSink::new(
        Store::local(&sync_store).expect("local store"),
        never_flush(),
    ));
    {
        let wal = Wal::open(wal_config(&tmp.path().join("sync-wal"))).expect("open WAL");
        let pipeline = IngestPipeline::new(
            crate::ingest_support::coordinator(Box::new(wal)),
            MinerCluster::new(MinerConfig::default()).with_record_sink(Box::new(sync_sink.clone())),
            TenantRule::service_name(),
        );
        for export in corpus() {
            pipeline.ingest(export).await.expect("sync ingest acks");
        }
        sync_sink.flush_all();
    }

    // Pooled side: the RFC 0035 shape, same corpus in the same WAL order
    // (sequential awaits pin the append order to the corpus order, so
    // template-id assignment is comparable).
    let pooled_store = tmp.path().join("pooled-store");
    let (pipeline, pooled_sink) =
        pooled_wal_pipeline(&tmp.path().join("pooled-wal"), &pooled_store, 4);
    for export in corpus() {
        pipeline.ingest(export).await.expect("pooled ingest acks");
    }
    pipeline.quiesce_encodes();
    pooled_sink.flush_all();
    drop(pipeline);

    let (sync_rows, sync_schemas) = read_side(&sync_store);
    let (pooled_rows, pooled_schemas) = read_side(&pooled_store);

    assert_eq!(
        sync_schemas, pooled_schemas,
        "identical Parquet schema — Design A introduces no migration (§3.5)",
    );

    let sync_rows = canonicalize(sync_rows);
    let pooled_rows = canonicalize(pooled_rows);
    assert!(!sync_rows.is_empty(), "the corpus produced rows");
    assert_eq!(
        sync_rows, pooled_rows,
        "decoded rows are multiset-equal — template_id values, params, bodies, \
         and every other column unchanged",
    );

    // Query differential: for every assigned template id, a
    // `template_id == N` filter over the read path returns the same
    // result set on both sides.
    let mut ids: Vec<u64> = sync_rows.iter().map(|r| r.template_id).collect();
    ids.sort_unstable();
    ids.dedup();
    assert!(ids.len() > 1, "the corpus spans several templates");
    for id in ids {
        let sync_hits: Vec<&MinedRecord> =
            sync_rows.iter().filter(|r| r.template_id == id).collect();
        let pooled_hits: Vec<&MinedRecord> =
            pooled_rows.iter().filter(|r| r.template_id == id).collect();
        assert_eq!(
            sync_hits, pooled_hits,
            "template_id == {id}: identical query results on both paths",
        );
    }
}
