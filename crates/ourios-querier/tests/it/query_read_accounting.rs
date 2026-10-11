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
    run_with(bucket, dsl, Some(&no_aliases())).await
}

/// `dsl` with `aliases` as the caller-held alias projection; `None` makes
/// a `resolves_to` query acquire the template map at compile time.
async fn run_with(
    bucket: &std::path::Path,
    dsl: &str,
    aliases: Option<&ourios_core::alias::AliasMap>,
) -> QueryResult {
    let query = ourios_querier::dsl::parse(dsl).expect("parse DSL");
    Querier::new(bucket)
        .run_query(
            &query,
            &TenantId::new("acme"),
            NOW,
            DEFAULT_WINDOW_NS,
            aliases,
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

/// Write a manifest naming every committed file into each data partition,
/// returning the manifests' total bytes.
fn write_manifests(bucket: &std::path::Path) -> u64 {
    let mut stack = vec![bucket.join("data")];
    let mut written = 0;
    while let Some(dir) = stack.pop() {
        let mut files = Vec::new();
        for entry in std::fs::read_dir(&dir).expect("list data dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "parquet") {
                let name = path.file_name().expect("file name").to_string_lossy();
                files.push(name.into_owned());
            }
        }
        if !files.is_empty() {
            let manifest = ourios_parquet::Manifest {
                generation: 1,
                files,
            };
            manifest.write_atomic(&dir).expect("write manifest");
            let path = dir.join(ourios_parquet::MANIFEST_FILENAME);
            written += std::fs::metadata(path).expect("stat manifest").len();
        }
    }
    written
}

/// File-set resolution reads each partition's manifest before any table
/// exists; those bytes are fetched too. The manifests name exactly the
/// files the glob fallback would, so the scan is unchanged and the
/// difference is the manifests alone.
#[tokio::test]
async fn manifest_reads_are_counted() {
    let bucket = store();
    let without = run(bucket.path(), "template_id == 1").await;
    let manifests = write_manifests(bucket.path());
    assert!(manifests > 0, "every partition got a manifest");

    let with = run(bucket.path(), "template_id == 1").await;

    assert_eq!(with.rows, without.rows);
    assert_eq!(with.stats.files_opened, without.stats.files_opened);
    assert_eq!(
        with.bytes_fetched,
        without.bytes_fetched + manifests,
        "the manifests' bytes are the only difference",
    );
}

/// The store plus an audit stream for its templates, so a `resolves_to`
/// query has a template map to acquire.
fn store_with_audit() -> (TempDir, u64) {
    let bucket = store();
    let events: Vec<_> = (0..FILES)
        .map(|k| crate::common::widened("acme", 1 + k, 1, TS0 + k))
        .collect();
    crate::common::write_audit(bucket.path(), &events);
    let (_, audit_bytes, _) = parquet_files(&bucket.path().join("audit"));
    (bucket, audit_bytes)
}

/// A `resolves_to` query acquires the template map at compile time. When it
/// renders no row, `registry_bytes_read` stays `0` (it reports the map
/// behind rendered rows), so the acquisition is in `bytes_fetched` and the
/// total still includes it.
#[tokio::test]
async fn an_unrendered_acquisition_is_still_counted() {
    let (bucket, audit_bytes) = store_with_audit();
    // The same query with a caller-held projection acquires nothing.
    let without = run(bucket.path(), "resolves_to(99) | limit 10").await;
    assert_eq!(without.bytes_fetched, without.total_bytes_read());

    let result = run_with(bucket.path(), "resolves_to(99) | limit 10", None).await;

    assert_eq!(result.rows, 0);
    assert_eq!(result.registry_bytes_read, 0, "no row was rendered");
    assert_eq!(
        result.bytes_fetched,
        without.bytes_fetched + audit_bytes,
        "the cold fold's audit bytes are counted on top of the table reads",
    );
    assert_eq!(result.total_bytes_read(), result.bytes_fetched);
}

/// When the compile-time acquisition's map renders rows, the renderer
/// reuses it and reports it in `registry_bytes_read`: the table reads in
/// `bytes_fetched` are exactly those of the same query run without a
/// compile-time acquisition, so the map is not counted twice.
#[tokio::test]
async fn a_rendered_acquisition_is_counted_once() {
    let (bucket, _) = store_with_audit();
    // Warm the artifact so both runs below acquire the same way (a hit).
    run_with(bucket.path(), "resolves_to(1) | limit 10", None).await;

    let compiled = run_with(bucket.path(), "resolves_to(1) | limit 10", None).await;
    let lazy = run(bucket.path(), "resolves_to(1) | limit 10").await;

    assert_eq!(compiled.rows, 1);
    assert_eq!(compiled.records.len(), 1);
    assert!(compiled.registry_bytes_read > 0, "the rendered map's bytes");
    assert_eq!(compiled.registry_bytes_read, lazy.registry_bytes_read);
    assert_eq!(
        compiled.bytes_fetched, lazy.bytes_fetched,
        "the acquisition is in registry_bytes_read only",
    );
    assert_eq!(
        compiled.total_bytes_read(),
        compiled.bytes_fetched + compiled.registry_bytes_read,
    );
}
