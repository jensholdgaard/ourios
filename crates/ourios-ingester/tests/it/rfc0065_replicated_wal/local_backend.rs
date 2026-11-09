//! The local backend is unchanged by the replicated one (RFC0065.16).

/// Scenario RFC0065.16 — the local backend is unchanged, the column arm.
/// Given `backend: local`, then written Parquet files have no `ingest_*`
/// columns. The suites arm is the RFC 0008, 0014 and 0052 suites passing
/// unchanged in CI.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0065_16_local_backend_files_have_no_ingest_columns() {
    use crate::ingest_support::{pooled_wal_pipeline, request, resource_logs};

    let tmp = tempfile::TempDir::new().expect("temp");
    let store = tmp.path().join("store");
    let (pipeline, sink) = pooled_wal_pipeline(&tmp.path().join("wal"), &store, 2);
    let tenant = ourios_core::tenant::TenantId::new("checkout");
    for export in [
        request(vec![resource_logs(
            "checkout",
            &["user 1 logged in", "user 2 logged in"],
        )]),
        request(vec![resource_logs(
            "billing",
            &["charge 9 EUR accepted", ""],
        )]),
    ] {
        pipeline
            .ingest(export, tenant.clone())
            .await
            .expect("local ingest acks");
    }
    pipeline.quiesce_encodes();
    sink.flush_all();
    drop(pipeline);

    let files = data_files(&store.join("data"));
    assert!(!files.is_empty(), "the local backend wrote Parquet");
    for path in files {
        assert_no_ingest_columns(&path);
    }
}

/// Every `*.parquet` file under `root`.
fn data_files(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let entries = std::fs::read_dir(&dir).expect("read dir").flatten();
        let (subdirs, plain): (Vec<_>, Vec<_>) =
            entries.map(|e| e.path()).partition(|p| p.is_dir());
        dirs.extend(subdirs);
        files.extend(
            plain
                .into_iter()
                .filter(|p| p.extension().is_some_and(|x| x == "parquet")),
        );
    }
    files
}

/// The data file at `path` has no `ingest_*` leaf, and its rows read back
/// with no position.
fn assert_no_ingest_columns(path: &std::path::Path) {
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    let file = std::fs::File::open(path).expect("open parquet");
    let builder = ParquetRecordBatchReaderBuilder::try_new(file).expect("metadata");
    let leaves: Vec<String> = builder
        .parquet_schema()
        .columns()
        .iter()
        .map(|c| c.path().string())
        .collect();
    assert!(
        leaves.iter().all(|leaf| !leaf.starts_with("ingest_")),
        "{} has an ingest column: {leaves:?}",
        path.display(),
    );
    let rows = ourios_parquet::Reader::open_file(path)
        .expect("open_file")
        .read_all()
        .expect("read_all");
    assert!(rows.iter().all(|r| r.ingest_position.is_none()));
}
