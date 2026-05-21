//! Scenario RFC0005.5 — Partition layout follows §3.4.
//! See `docs/rfcs/0005-parquet-storage.md` §5.
//!
//! Drives the writer with a multi-tenant, multi-hour record
//! stream (including a non-`ASCII` tenant id) and asserts files
//! land under
//! `data/tenant_id=<percent-encoded>/year=YYYY/month=MM/day=DD/hour=HH/<flush_uuid>.parquet`.
//! The flush identifier is verified to be a `UUIDv7` (per §3.4's
//! normative "writer MUST emit `UUIDv7`" clause).

use std::path::Path;

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
        separators: vec![String::new(), String::new()],
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

    // Strip the bucket root so the asserted prefix is partition-relative.
    let rel = |p: &Path| -> String {
        p.strip_prefix(bucket_path)
            .unwrap()
            .to_string_lossy()
            .into_owned()
    };
    assert!(
        rel(&p1).starts_with("data/tenant_id=tenant-a/year=2026/month=04/day=02/hour=10/"),
        "{}",
        rel(&p1),
    );
    assert!(
        rel(&p2).starts_with("data/tenant_id=tenant-a/year=2026/month=04/day=02/hour=11/"),
        "{}",
        rel(&p2),
    );
    assert!(
        rel(&p3).starts_with("data/tenant_id=tenant-b/year=2026/month=04/day=02/hour=12/"),
        "{}",
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

/// RFC0005.5 sub-test — non-ASCII tenant ids percent-encode per §3.4.
#[test]
fn rfc0005_5_non_ascii_tenant_id_percent_encodes() {
    let bucket = TempDir::new().unwrap();
    let bucket_path = bucket.path();

    // "tenant-å" — å is U+00E5, UTF-8 = 0xC3 0xA5 → "%C3%A5".
    let rec = empty_record("tenant-å", 1_775_127_480_000_000_000);
    let path = write_one_record(bucket_path, &rec);

    let rel = path
        .strip_prefix(bucket_path)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert!(
        rel.starts_with("data/tenant_id=tenant-%C3%A5/"),
        "non-ASCII tenant must percent-encode per §3.4, got: {rel}",
    );
}
