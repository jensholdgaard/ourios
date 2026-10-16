//! RFC0009.18 — a query whose resolved file set goes stale before its scan
//! (a compaction commits and deletes the files it named) resolves again and
//! returns every row once, or fails whole after a bounded number of attempts.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use ourios_core::record::{BodyKind, MinedRecord, Param};

use super::*;
use crate::test_support::{GetHook, HookStore};

/// 2026-04-02T10:58:00 UTC.
const TS0: u64 = 1_775_127_480_000_000_000;

fn record(ts_ns: u64) -> MinedRecord {
    MinedRecord {
        tenant_id: TenantId::new("acme"),
        template_id: 7,
        template_version: 1,
        severity_number: 9,
        severity_text: None,
        scope_name: None,
        scope_version: None,
        scope_attributes: Vec::new(),
        resource_schema_url: None,
        scope_schema_url: None,
        time_unix_nano: ts_ns,
        observed_time_unix_nano: None,
        attributes: Vec::new(),
        dropped_attributes_count: 0,
        resource_attributes: Vec::new(),
        trace_id: None,
        span_id: None,
        flags: 0,
        event_name: None,
        body_kind: BodyKind::String,
        params: vec![Param {
            type_tag: ourios_core::audit::ParamType::Num,
            value: "42".to_string(),
        }],
        separators: vec![String::new(), " ".to_string()],
        body: None,
        confidence: 1.0,
        lossy_flag: false,
    }
}

/// Write `rows` as one flush file; its key.
fn write_flush(store: &Store, rows: &[MinedRecord]) -> String {
    let partition = ourios_parquet::PartitionKey::derive(&rows[0]).expect("partition");
    let mut writer = ourios_parquet::Writer::open_in(store, partition).expect("writer");
    writer.append_records(rows).expect("append");
    writer.close().expect("close").key
}

fn name_of(key: &str) -> String {
    key.rsplit_once('/').expect("partition key").1.to_owned()
}

fn dir_of(key: &str) -> &str {
    key.rsplit_once('/').expect("partition key").0
}

/// A querier on the S3 path over the local store at `root`, with `hook`
/// run before each GET.
fn hooked_querier(root: &Path, hook: GetHook) -> Querier {
    let store = Store::local(root)
        .expect("local store")
        .wrap_backend(|inner| Arc::new(HookStore::new(inner, hook)));
    Querier {
        backend: Backend::Remote(store),
        promoted: ourios_parquet::PromotedAttributes::default(),
        template_maps: Arc::default(),
    }
}

fn every_row() -> QueryRequest {
    QueryRequest {
        tenant: TenantId::new("acme"),
        time_range: None,
        template_id: None,
        severity_text: None,
        limit: None,
    }
}

/// Whether `key` is a data file a scan reads.
fn is_data_file(key: &str) -> bool {
    key.starts_with("data/") && key.ends_with(".parquet")
}

/// A partition with no manifest is resolved; before its files are read, its
/// first commit creates the manifest and deletes the inputs. The query
/// re-resolves on the failed read and returns every row exactly once.
#[tokio::test]
async fn rfc0009_18_first_manifest_after_listing_re_resolves() {
    let tmp = tempfile::tempdir().expect("temp");
    let root = tmp.path().to_path_buf();
    let plain = Store::local(&root).expect("local store");
    let rows: Vec<MinedRecord> = (0..3).map(|i| record(TS0 + i * 1_000)).collect();
    let inputs = [
        write_flush(&plain, &rows[..2]),
        write_flush(&plain, &rows[2..]),
    ];
    let staged = write_flush(&plain, &rows);
    let output = staged.replace(".parquet", ".g1.parquet");
    std::fs::rename(root.join(&staged), root.join(&output)).expect("stage the output");
    let commit = ourios_parquet::Manifest::new(
        1,
        vec![name_of(&output)],
        inputs.iter().map(|k| name_of(k)).collect(),
    );
    let manifest_path: PathBuf = root
        .join(dir_of(&output))
        .join(ourios_parquet::MANIFEST_FILENAME);
    let mut pending = Some((commit, inputs.clone()));
    let hook: GetHook = Box::new(move |key| {
        if !is_data_file(key) {
            return;
        }
        if let Some((commit, inputs)) = pending.take() {
            std::fs::write(&manifest_path, commit.to_json().expect("json")).expect("manifest");
            for input in &inputs {
                std::fs::remove_file(root.join(input)).expect("delete input");
            }
        }
    });

    let result = hooked_querier(tmp.path(), hook)
        .run(every_row())
        .await
        .expect("query");

    assert_eq!(result.rows, 3, "every row exactly once");
}

/// A file set that goes stale on every attempt fails the query whole with a
/// storage error after the bounded attempts, never with a short result.
#[tokio::test]
async fn rfc0009_18_stale_three_times_is_a_storage_error() {
    let tmp = tempfile::tempdir().expect("temp");
    let root = tmp.path().to_path_buf();
    let plain = Store::local(&root).expect("local store");
    write_flush(&plain, &[record(TS0)]);
    let moves = Arc::new(AtomicU64::new(0));
    let counted = Arc::clone(&moves);
    let hook: GetHook = Box::new(move |key| {
        if !is_data_file(key) {
            return;
        }
        let n = counted.fetch_add(1, Ordering::SeqCst) + 1;
        let renamed = format!("{}/{n:08}-0000-7000-8000-000000000000.parquet", dir_of(key));
        std::fs::rename(root.join(key), root.join(renamed)).expect("move the file");
    });

    let err = hooked_querier(tmp.path(), hook)
        .run(every_row())
        .await
        .expect_err("a file set that never settles fails the query");

    assert!(matches!(err, QueryError::Storage { .. }), "{err:?}");
    assert_eq!(
        moves.load(Ordering::SeqCst),
        u64::from(SCAN_ATTEMPTS),
        "one stale read per attempt"
    );
}

/// A commit lands between the store resolver's listing and its manifest
/// read, and deletes the inputs: the newer manifest consumes them and names
/// the output, so the query reads every row exactly once, with no retry.
#[tokio::test]
async fn rfc0009_18_list_then_manifest_does_not_tear() {
    let tmp = tempfile::tempdir().expect("temp");
    let root = tmp.path().to_path_buf();
    let plain = Store::local(&root).expect("local store");
    let rows: Vec<MinedRecord> = (0..3).map(|i| record(TS0 + i * 1_000)).collect();
    let inputs = [
        write_flush(&plain, &rows[..2]),
        write_flush(&plain, &rows[2..]),
    ];
    let staged = write_flush(&plain, &rows);
    let output = staged.replace(".parquet", ".g2.parquet");
    std::fs::rename(root.join(&staged), root.join(&output)).expect("stage the output");
    let manifest_path: PathBuf = root
        .join(dir_of(&output))
        .join(ourios_parquet::MANIFEST_FILENAME);
    let earlier = ourios_parquet::Manifest::new(1, Vec::new(), Vec::new());
    std::fs::write(&manifest_path, earlier.to_json().expect("json")).expect("manifest");
    let commit = ourios_parquet::Manifest::new(
        2,
        vec![name_of(&output)],
        inputs.iter().map(|k| name_of(k)).collect(),
    );
    let mut pending = Some((commit, inputs.clone()));
    let hook: GetHook = Box::new(move |key| {
        if !key.ends_with(ourios_parquet::MANIFEST_FILENAME) {
            return;
        }
        if let Some((commit, inputs)) = pending.take() {
            std::fs::write(&manifest_path, commit.to_json().expect("json")).expect("manifest");
            for input in &inputs {
                std::fs::remove_file(root.join(input)).expect("delete input");
            }
        }
    });

    let result = hooked_querier(tmp.path(), hook)
        .run(every_row())
        .await
        .expect("query");

    assert_eq!(result.rows, 3, "every row exactly once");
}
