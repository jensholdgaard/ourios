//! Scenario RFC0005.11 — Row-vs-path validation on partition mismatch.
//! See `docs/rfcs/0005-parquet-storage.md` §5.
//!
//! Writes a file under one partition, then opens it via
//! `Reader::open_partition` with a *different* `PartitionKey`
//! and asserts the reader returns a `PartitionMismatch` hard
//! error. Also verifies the §3.4 time-fallback rule (a record
//! with `time_unix_nano = 0` and a non-zero
//! `observed_time_unix_nano` validates cleanly when the
//! supplied partition matches the observed-time bucket).

use ourios_core::record::{BodyKind, MinedRecord};
use ourios_core::tenant::TenantId;
use ourios_parquet::{PartitionKey, Reader, ReaderError, Writer};
use tempfile::TempDir;

fn lossy_record(tenant: &str, time_ns: u64, observed: Option<u64>) -> MinedRecord {
    MinedRecord {
        tenant_id: TenantId::new(tenant),
        template_id: 0,
        template_version: 0,
        severity_number: 9,
        severity_text: None,
        scope_name: None,
        scope_version: None,
        scope_attributes: Vec::new(),
        resource_schema_url: None,
        scope_schema_url: None,
        time_unix_nano: time_ns,
        observed_time_unix_nano: observed,
        attributes: Vec::new(),
        dropped_attributes_count: 0,
        resource_attributes: Vec::new(),
        trace_id: None,
        span_id: None,
        flags: 0,
        event_name: None,
        body_kind: BodyKind::String,
        params: Vec::new(),
        separators: Vec::new(),
        body: Some("placeholder".to_string()),
        confidence: 0.0,
        lossy_flag: true,
    }
}

/// Scenario RFC0005.11 — open with a deliberately mismatching
/// `tenant_id`: reader errors.
#[test]
fn rfc0005_11_tenant_mismatch_returns_partition_mismatch_error() {
    let bucket = TempDir::new().unwrap();
    let rec = lossy_record("tenant-a", 1_775_127_480_000_000_000, None);
    let written_partition = PartitionKey::derive(&rec).unwrap();

    let mut writer = Writer::open(bucket.path(), written_partition.clone()).unwrap();
    writer.append_records(&[rec]).unwrap();
    let written = writer.close().unwrap();

    // Construct a different partition with the same time
    // bucket but wrong tenant id.
    let wrong_partition = PartitionKey {
        tenant_id: "tenant-b".to_string(),
        ..written_partition
    };

    let reader = Reader::open_partition(&written.path, wrong_partition.clone()).unwrap();
    let err = reader
        .read_all()
        .expect_err("mismatching tenant must error");
    match err {
        ReaderError::PartitionMismatch {
            row_index,
            expected,
            actual,
            ..
        } => {
            assert_eq!(row_index, 0);
            assert_eq!(expected.tenant_id, "tenant-b");
            assert_eq!(actual.tenant_id, "tenant-a");
        }
        other => panic!("expected PartitionMismatch, got {other:?}"),
    }
}

/// Scenario RFC0005.11 — open with the wrong hour: reader
/// errors. Verifies the time-bucket axis of the validation.
#[test]
fn rfc0005_11_hour_mismatch_returns_partition_mismatch_error() {
    let bucket = TempDir::new().unwrap();
    let rec = lossy_record("tenant-x", 1_775_127_480_000_000_000, None);
    let written_partition = PartitionKey::derive(&rec).unwrap();

    let mut writer = Writer::open(bucket.path(), written_partition.clone()).unwrap();
    writer.append_records(&[rec]).unwrap();
    let written = writer.close().unwrap();

    let wrong_partition = PartitionKey {
        hour: (written_partition.hour + 1) % 24,
        ..written_partition
    };

    let reader = Reader::open_partition(&written.path, wrong_partition).unwrap();
    let err = reader.read_all().expect_err("mismatching hour must error");
    assert!(matches!(err, ReaderError::PartitionMismatch { .. }));
}

/// Scenario RFC0005.11 — the §3.4 time-fallback "And" clause:
/// a row with `time_unix_nano = 0` and a non-zero
/// `observed_time_unix_nano` validates cleanly when the
/// supplied partition matches the observed-time bucket.
#[test]
fn rfc0005_11_time_fallback_validates_cleanly() {
    let bucket = TempDir::new().unwrap();
    let rec = lossy_record("tenant-fallback", 0, Some(1_775_127_480_000_000_000));
    let partition = PartitionKey::derive(&rec).unwrap();
    // Sanity: the partition derives from observed_time, so
    // it's in 2026 not 1970.
    assert_eq!(partition.year, 2026);

    let mut writer = Writer::open(bucket.path(), partition.clone()).unwrap();
    writer.append_records(&[rec]).unwrap();
    let written = writer.close().unwrap();

    // Reader uses the same §3.4 derivation; validation passes.
    let reader = Reader::open_partition(&written.path, partition).unwrap();
    let records = reader
        .read_all()
        .expect("fallback partition validates cleanly");
    assert_eq!(records.len(), 1);
}

/// Diagnostic `open_file` mode skips row-vs-path validation —
/// records that would error under `open_partition` surface
/// as-stored.
#[test]
fn open_file_skips_partition_validation() {
    let bucket = TempDir::new().unwrap();
    let rec = lossy_record("tenant-a", 1_775_127_480_000_000_000, None);
    let partition = PartitionKey::derive(&rec).unwrap();

    let mut writer = Writer::open(bucket.path(), partition).unwrap();
    writer.append_records(std::slice::from_ref(&rec)).unwrap();
    let written = writer.close().unwrap();

    // No partition supplied — reader doesn't validate.
    let reader = Reader::open_file(&written.path).unwrap();
    let records = reader
        .read_all()
        .expect("open_file never partition-validates");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].tenant_id.as_str(), rec.tenant_id.as_str());
}
