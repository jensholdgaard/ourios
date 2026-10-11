//! What a query read (#924): the Parquet files it opened and every byte it
//! fetched, the footers schema inference reads before any plan exists
//! included.

use crate::common::{DEFAULT_WINDOW_NS, HOUR_NS, NOW, TS0, no_aliases, rec, write_all};

use ourios_core::tenant::TenantId;
use ourios_querier::{Querier, QueryResult};
use tempfile::TempDir;

const FILES: u64 = 3;

/// One file per hour partition, each holding one row of its own template.
fn store() -> TempDir {
    let bucket = TempDir::new().expect("bucket dir");
    let recs: Vec<_> = (0..FILES)
        .map(|k| {
            rec(
                "acme",
                1 + k,
                TS0 + k * HOUR_NS,
                9,
                "api",
                "lib.cart",
                None,
                None,
            )
        })
        .collect();
    write_all(bucket.path(), &recs);
    bucket
}

/// The data files' total size and total footer size (the Thrift metadata
/// plus the 8-byte length-and-magic trailer).
fn parquet_files(root: &std::path::Path) -> (u64, u64, u64) {
    let mut stack = vec![root.to_path_buf()];
    let (mut files, mut bytes, mut footers) = (0, 0, 0);
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("list data dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "parquet") {
                let content = std::fs::read(&path).expect("read data file");
                let len = content.len();
                let trailer: [u8; 4] = content[len - 8..len - 4].try_into().expect("4 bytes");
                files += 1;
                bytes += len as u64;
                footers += u64::from(u32::from_le_bytes(trailer)) + 8;
            }
        }
    }
    (files, bytes, footers)
}

async fn run(bucket: &std::path::Path, dsl: &str) -> QueryResult {
    let query = ourios_querier::dsl::parse(dsl).expect("parse DSL");
    Querier::new(bucket)
        .run_query(
            &query,
            &TenantId::new("acme"),
            NOW,
            DEFAULT_WINDOW_NS,
            Some(&no_aliases()),
        )
        .await
        .expect("query")
}

/// A provably-empty filter plans no scan, but schema inference has already
/// opened every file and read its footer: both are reported.
#[tokio::test]
async fn a_provably_empty_query_still_reports_the_footers_it_read() {
    let bucket = store();
    let (files, bytes, footers) = parquet_files(&bucket.path().join("data"));
    assert_eq!(files, FILES);

    let result = run(bucket.path(), "false").await;

    assert_eq!(result.rows, 0);
    assert_eq!(result.stats.bytes_read, 0, "no scan ran");
    assert_eq!(result.stats.files_opened, FILES);
    let fetched = result.bytes_fetched;
    assert!(
        fetched >= footers && fetched <= bytes,
        "fetched {fetched}: at least the {footers} footer bytes, at most the {bytes} file bytes",
    );
    assert_eq!(result.total_bytes_read(), fetched);
}

/// A scanning query reports its scan bytes and the inference footers on
/// top: more than the scan metric alone sees.
#[tokio::test]
async fn a_scanning_query_reports_inference_and_scan_reads() {
    let bucket = store();
    let (_, bytes, footers) = parquet_files(&bucket.path().join("data"));

    let result = run(bucket.path(), "template_id == 1").await;

    assert_eq!(result.rows, 1);
    assert_eq!(result.stats.files_opened, FILES);
    let fetched = result.bytes_fetched;
    assert!(
        fetched >= result.stats.bytes_read + footers,
        "fetched {fetched} covers the scan's {} bytes and the {footers} inference footer bytes",
        result.stats.bytes_read,
    );
    assert!(
        fetched <= 3 * bytes,
        "fetched {fetched} of {bytes} file bytes"
    );
    assert_eq!(
        result.total_bytes_read(),
        fetched,
        "no template map acquired"
    );
}

/// A drift query runs two audit scans (the window and the adoption
/// lookup), each with its own schema inference: both are reported.
#[tokio::test]
async fn a_drift_query_reports_both_scans_reads() {
    // 2026-06-01T12:00:00Z, inside the drift window below.
    const MID: u64 = 1_780_315_200_000_000_000;
    let bucket = TempDir::new().expect("bucket dir");
    crate::common::write_audit(
        bucket.path(),
        &[crate::common::widened("acme", 100, 1, MID)],
    );
    let (files, _, footers) = parquet_files(&bucket.path().join("audit"));
    assert!(files > 0, "the fixture wrote audit files");
    let ourios_querier::dsl::Statement::Drift(query) = ourios_querier::dsl::parse_statement(
        "drift from 2026-06-01T00:00:00Z to 2026-06-02T00:00:00Z",
    )
    .expect("parse drift") else {
        panic!("expected a drift statement");
    };

    let result = Querier::new(bucket.path())
        .run_drift(&query, &TenantId::new("acme"), MID + HOUR_NS)
        .await
        .expect("run_drift");

    assert_eq!(result.rows.len(), 1, "the widened template drifted");
    assert_eq!(
        result.stats.files_opened,
        2 * files,
        "two scans, each over every file"
    );
    assert!(
        result.bytes_fetched >= result.stats.bytes_read + 2 * footers,
        "fetched {} covers both scans' {} bytes and two inference passes over {footers} \
         footer bytes",
        result.bytes_fetched,
        result.stats.bytes_read,
    );
}
