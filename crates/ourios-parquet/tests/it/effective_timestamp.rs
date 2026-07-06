//! Scenario RFC0005.13 (storage half) — effective-timestamp
//! fallback, amendment 2026-06-11.
//! See `docs/rfcs/0005-parquet-storage.md` §3.2 / §5.
//!
//! A record with `time_unix_nano = 0` and a non-zero
//! `observed_time_unix_nano = T`:
//!
//! - stores `effective_time_unix_nano = T` (writer-derived);
//! - lands under the partition tuple derived from `T` (§3.4 — the
//!   partition tuple and the stored column never disagree);
//! - keeps the wire `time_unix_nano = 0` verbatim (RFC0001.10 —
//!   derived, never overwriting).
//!
//! The query half (the RFC 0002 §6.2 window over the column, plus
//! the §3.9 pre-amendment-file fallback) lives in
//! `crates/ourios-querier/tests/rfc0005_13.rs`.

use std::fs::File;
use std::path::{Path, PathBuf};

use arrow_array::Array;
use arrow_array::cast::AsArray;
use arrow_array::types::TimestampNanosecondType;
use ourios_core::record::{BodyKind, MinedRecord};
use ourios_core::tenant::TenantId;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

use ourios_parquet::{PartitionKey, Writer, columns, effective_time_unix_nano};

/// 2026-04-02T10:58:00 UTC (hour=10) — same anchor as the other
/// storage tests.
const T: u64 = 1_775_127_480_000_000_000;

fn rec(time_unix_nano: u64, observed: Option<u64>) -> MinedRecord {
    MinedRecord {
        tenant_id: TenantId::new("a"),
        template_id: 1,
        template_version: 1,
        severity_number: 9,
        severity_text: None,
        scope_name: None,
        scope_version: None,
        scope_attributes: Vec::new(),
        resource_schema_url: None,
        scope_schema_url: None,
        time_unix_nano,
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
        separators: vec![String::new()],
        body: None,
        confidence: 1.0,
        lossy_flag: false,
    }
}

/// Write `record` through the production `Writer` and return the
/// single emitted `*.parquet` path.
fn write_one(bucket: &Path, record: &MinedRecord) -> PathBuf {
    let part = PartitionKey::derive(record).expect("derive partition");
    let mut w = Writer::open(bucket, part.clone()).expect("open writer");
    w.append_records(std::slice::from_ref(record))
        .expect("append");
    w.close().expect("close");

    let dir = part.data_path(bucket);
    let mut parquets: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("read partition dir")
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "parquet"))
        .collect();
    assert_eq!(parquets.len(), 1, "one file per flush");
    parquets.pop().expect("one parquet file")
}

/// The single row's `(time_unix_nano, effective_time_unix_nano)`
/// as stored, read raw through the `parquet` crate (not the
/// project reader — `MinedRecord` deliberately has no effective
/// field, so the column is only visible at the Parquet level).
fn stored_timestamps(file: &Path) -> (i64, Option<i64>) {
    let reader = ParquetRecordBatchReaderBuilder::try_new(File::open(file).expect("open file"))
        .expect("parquet builder")
        .build()
        .expect("parquet reader");
    let batches: Vec<_> = reader.collect::<Result<_, _>>().expect("read batches");
    assert_eq!(batches.len(), 1, "single tiny row group");
    let batch = &batches[0];
    assert_eq!(batch.num_rows(), 1, "single row");

    let time_idx = batch
        .schema()
        .index_of(columns::TIME_UNIX_NANO)
        .expect("time column present");
    let time = batch
        .column(time_idx)
        .as_primitive::<TimestampNanosecondType>()
        .value(0);

    let eff_idx = batch
        .schema()
        .index_of(columns::EFFECTIVE_TIME_UNIX_NANO)
        .expect("effective column present in post-amendment files");
    let eff_col = batch
        .column(eff_idx)
        .as_primitive::<TimestampNanosecondType>();
    let eff = (!eff_col.is_null(0)).then(|| eff_col.value(0));

    (time, eff)
}

/// RFC0005.13 — observed-only record: the stored effective column
/// equals `T`, the partition tuple derives from `T`, and the wire
/// `time_unix_nano` stays `0` verbatim.
#[test]
fn rfc0005_13_observed_only_record_stores_effective_and_keeps_wire_zero() {
    // Arrange
    let bucket = tempfile::TempDir::new().expect("temp");
    let record = rec(0, Some(T));

    // Act
    let file = write_one(bucket.path(), &record);

    // Assert — partition tuple derived from T (2026-04-02T10 UTC),
    // not the 1970 epoch the zero wire value would land under.
    let part = PartitionKey::derive(&record).expect("derive");
    assert_eq!(
        (part.year, part.month, part.day, part.hour),
        (2026, 4, 2, 10),
        "partition derives from the observed fallback",
    );

    // Assert — stored column: wire zero verbatim, effective = T.
    let (time, eff) = stored_timestamps(&file);
    assert_eq!(time, 0, "wire time_unix_nano is never overwritten");
    assert_eq!(
        eff,
        Some(i64::try_from(T).expect("T fits i64")),
        "effective_time_unix_nano stores the observed fallback",
    );
}

/// The stored column always equals what the §3.4 partition
/// derivation chose — both run [`effective_time_unix_nano`], so
/// asserting the stored value against the shared function pins the
/// "never disagree" rule across all three derivation shapes.
#[test]
fn stored_effective_always_matches_the_partition_derivation() {
    for record in [rec(T, Some(T + 1_000)), rec(0, Some(T)), rec(0, None)] {
        // Arrange
        let bucket = tempfile::TempDir::new().expect("temp");
        let expected = effective_time_unix_nano(&record).expect("derive effective");

        // Act
        let file = write_one(bucket.path(), &record);

        // Assert
        let (_, eff) = stored_timestamps(&file);
        assert_eq!(
            eff,
            Some(expected),
            "stored column equals the shared derivation for \
             time={} observed={:?}",
            record.time_unix_nano,
            record.observed_time_unix_nano,
        );
    }
}
