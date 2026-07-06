//! Scenario RFC0005.5 — Partition layout follows §3.4.
//! See `docs/rfcs/0005-parquet-storage.md` §5.
//!
//! Drives the writer with a multi-tenant, multi-hour record
//! stream (including a non-`ASCII` tenant id) and asserts files
//! land under
//! `data/tenant_id=<percent-encoded>/year=YYYY/month=MM/day=DD/hour=HH/<flush_uuid>.parquet`.
//! The flush identifier is verified to be a `UUIDv7` (per §3.4's
//! normative "writer MUST emit `UUIDv7`" clause).

use std::path::{Path, PathBuf};

use ourios_core::record::{BodyKind, MinedRecord};
use ourios_core::tenant::TenantId;
use ourios_parquet::{PartitionKey, Writer};
use tempfile::TempDir;
use uuid::Uuid;

fn empty_record(tenant: &str, ts_unix_nano: u64) -> MinedRecord {
    MinedRecord {
        tenant_id: TenantId::new(tenant),
        template_id: 1,
        template_version: 1,
        severity_number: 9,
        severity_text: None,
        scope_name: None,
        scope_version: None,
        scope_attributes: Vec::new(),
        resource_schema_url: None,
        scope_schema_url: None,
        time_unix_nano: ts_unix_nano,
        observed_time_unix_nano: None,
        attributes: Vec::new(),
        dropped_attributes_count: 0,
        resource_attributes: Vec::new(),
        trace_id: None,
        span_id: None,
        flags: 0,
        event_name: None,
        body_kind: BodyKind::String,
        params: Vec::new(),
        // `separators.len() = params.len() + 1 = 1` — the
        // minimum-valid shape for a clean-attach String row
        // with no wildcards.
        separators: vec![String::new()],
        body: None,
        confidence: 1.0,
        lossy_flag: false,
    }
}

fn write_one_record(bucket: &Path, record: &MinedRecord) -> std::path::PathBuf {
    let partition = PartitionKey::derive(record).expect("derive partition");
    let mut writer = Writer::open(bucket, partition).expect("open writer");
    writer
        .append_records(std::slice::from_ref(record))
        .expect("append");
    let written = writer.close().expect("close writer");
    written.path
}

/// Scenario RFC0005.5 — multi-tenant, multi-hour layout.
#[test]
fn rfc0005_5_files_land_at_expected_partition_paths() {
    let bucket = TempDir::new().unwrap();
    let bucket_path = bucket.path();

    // Three timestamps, three different UTC hours within a day.
    // 2026-04-02T10:58:00Z, 11:15:00Z, 12:00:00Z.
    let ts_hour_10 = 1_775_127_480_000_000_000_u64;
    let ts_hour_11 = ts_hour_10 + 17 * 60 * 1_000_000_000; // +17 min
    let ts_hour_12 = ts_hour_10 + (3600 + 2 * 60) * 1_000_000_000; // +1h 2m

    let p1 = write_one_record(bucket_path, &empty_record("tenant-a", ts_hour_10));
    let p2 = write_one_record(bucket_path, &empty_record("tenant-a", ts_hour_11));
    let p3 = write_one_record(bucket_path, &empty_record("tenant-b", ts_hour_12));

    for path in [&p1, &p2, &p3] {
        assert!(path.exists(), "writer must produce a real file at {path:?}");
    }

    // Component-wise prefix comparison stays portable across
    // path-separator conventions (Unix `/` vs Windows `\`).
    let rel = |p: &Path| -> PathBuf { p.strip_prefix(bucket_path).unwrap().to_path_buf() };
    let prefix = |tenant: &str, hour: &str| -> PathBuf {
        [
            "data",
            &format!("tenant_id={tenant}"),
            "year=2026",
            "month=04",
            "day=02",
            &format!("hour={hour}"),
        ]
        .iter()
        .collect()
    };
    assert!(
        rel(&p1).starts_with(prefix("tenant-a", "10")),
        "{:?}",
        rel(&p1),
    );
    assert!(
        rel(&p2).starts_with(prefix("tenant-a", "11")),
        "{:?}",
        rel(&p2),
    );
    assert!(
        rel(&p3).starts_with(prefix("tenant-b", "12")),
        "{:?}",
        rel(&p3),
    );

    // RFC 0005 §3.4: filename is `<flush_uuid>.parquet` where
    // flush_uuid is a UUIDv7.
    for path in [&p1, &p2, &p3] {
        let stem = path.file_stem().unwrap().to_string_lossy();
        let parsed = Uuid::parse_str(&stem).unwrap_or_else(|e| panic!("bad uuid {stem:?}: {e}"));
        assert_eq!(
            parsed.get_version_num(),
            7,
            "writer MUST emit UUIDv7 per §3.4 (got v{} for {stem})",
            parsed.get_version_num(),
        );
    }
}

/// Writer fail-fast: appending a record whose derived partition
/// disagrees with the writer's open partition errors at write
/// time (the §3.9 row-vs-path contract enforced from the write
/// side too, not just the read side).
#[test]
fn writer_rejects_records_outside_its_partition() {
    use ourios_parquet::WriterError;

    let bucket = TempDir::new().unwrap();
    let bucket_path = bucket.path();

    // Open a writer scoped to tenant-a at hour 10.
    let opening = empty_record("tenant-a", 1_775_127_480_000_000_000);
    let partition = PartitionKey::derive(&opening).unwrap();
    let mut writer = Writer::open(bucket_path, partition.clone()).unwrap();

    // First, an in-partition record — writes cleanly.
    writer.append_records(&[opening]).unwrap();

    // Now append a record from tenant-b: same hour, different
    // tenant_id. The writer must reject before touching the
    // RecordBatch builder.
    let foreign = empty_record("tenant-b", 1_775_127_480_000_000_000);
    let err = writer.append_records(&[foreign]).unwrap_err();
    match err {
        WriterError::PartitionMismatch {
            row_index,
            expected,
            actual,
        } => {
            assert_eq!(row_index, 0);
            assert_eq!(expected.tenant_id, "tenant-a");
            assert_eq!(actual.tenant_id, "tenant-b");
            assert_eq!(expected.hour, actual.hour); // same hour, only tenant differs
        }
        other => panic!("expected PartitionMismatch, got {other:?}"),
    }

    // A record one hour later from the right tenant — same
    // tenant, derived hour differs.
    let later = empty_record("tenant-a", 1_775_127_480_000_000_000 + 3600 * 1_000_000_000);
    let err = writer.append_records(&[later]).unwrap_err();
    match err {
        WriterError::PartitionMismatch {
            row_index,
            expected,
            actual,
        } => {
            assert_eq!(row_index, 0);
            assert_eq!(expected.tenant_id, "tenant-a");
            assert_eq!(actual.tenant_id, "tenant-a");
            assert_eq!(expected.hour, 10);
            assert_eq!(actual.hour, 11);
        }
        other => panic!("expected PartitionMismatch, got {other:?}"),
    }
}

/// Atomic-publish contract (RFC 0005 §7, RFC 0013 buffer-and-put):
/// the writer buffers rows in memory and publishes the finished file
/// to the object store only on [`Writer::close`]. Nothing lands under
/// the partition path before close, and a writer dropped without
/// close publishes nothing — there is **no on-disk temp artifact**.
///
/// Contract change from the former temp-file scheme: before RFC 0013
/// the writer streamed to a `<uuid>.parquet.tmp` and renamed it on
/// close, leaving a `.tmp` on disk while open (and the `Drop` impl
/// cleaned it up). Buffer-and-put removes the temp file entirely —
/// bytes live in memory until the store `put`, which is itself atomic
/// (the local backend stages + renames internally), preserving the
/// "readers see a complete file or nothing" guarantee.
#[test]
fn writer_publishes_only_on_close_and_drop_publishes_nothing() {
    let bucket = TempDir::new().unwrap();
    let bucket_path = bucket.path();

    let opening = empty_record("tenant-a", 1_775_127_480_000_000_000);
    let partition = PartitionKey::derive(&opening).expect("derive partition");

    // 1. While open — even after appending — nothing is published:
    //    neither the final `<uuid>.parquet` nor any `.parquet.tmp`.
    {
        let mut writer = Writer::open(bucket_path, partition.clone()).unwrap();
        writer
            .append_records(std::slice::from_ref(&opening))
            .unwrap();
        let final_path = writer.final_path().to_path_buf();
        assert!(
            !final_path.exists(),
            "buffer-and-put publishes nothing before close, found {final_path:?}",
        );
        assert!(
            !final_path.with_extension("parquet.tmp").exists(),
            "buffer-and-put leaves no `.parquet.tmp` on disk",
        );
        // 2. Drop without close → still nothing published.
        drop(writer);
        assert!(
            !final_path.exists(),
            "dropping without close must publish nothing, found {final_path:?}",
        );
    }

    // 3. Open a fresh writer, write, close. The final file is
    //    published atomically at the `<uuid>.parquet` name; no
    //    `.parquet.tmp` is left behind.
    let mut writer = Writer::open(bucket_path, partition).unwrap();
    writer.append_records(&[opening]).unwrap();
    let written = writer.close().expect("close");
    assert!(written.path.exists(), "final path must exist after close");
    assert_eq!(
        written.path.extension().and_then(|s| s.to_str()),
        Some("parquet"),
        "published file MUST have the .parquet extension, got {:?}",
        written.path,
    );
    assert!(
        !written.path.with_extension("parquet.tmp").exists(),
        "no `.parquet.tmp` must exist after a buffer-and-put close",
    );
}

/// RFC0005.5 sub-test — non-ASCII tenant ids percent-encode per §3.4.
#[test]
fn rfc0005_5_non_ascii_tenant_id_percent_encodes() {
    let bucket = TempDir::new().unwrap();
    let bucket_path = bucket.path();

    // "tenant-å" — å is U+00E5, UTF-8 = 0xC3 0xA5 → "%C3%A5".
    let rec = empty_record("tenant-å", 1_775_127_480_000_000_000);
    let path = write_one_record(bucket_path, &rec);

    let rel = path.strip_prefix(bucket_path).unwrap().to_path_buf();
    let expected_prefix: PathBuf = ["data", "tenant_id=tenant-%C3%A5"].iter().collect();
    assert!(
        rel.starts_with(&expected_prefix),
        "non-ASCII tenant must percent-encode per §3.4, got: {rel:?}",
    );
}
