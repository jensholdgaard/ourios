//! Scenario RFC0005.13 (query half) — the time window filters the
//! effective timestamp, with the §3.9 pre-amendment fallback.
//! See `docs/rfcs/0005-parquet-storage.md` §3.2 / §3.9 / §5 and
//! `docs/rfcs/0002-query-dsl.md` §6.2 (amendment 2026-06-11).
//!
//! Three obligations, each through the real querier path:
//!
//! 1. A record with `time_unix_nano = 0` and
//!    `observed_time_unix_nano = T` is returned by a `range(...)`
//!    window containing `T` — observed-only records are addressable
//!    by time (the B1 unblock).
//! 2. A **pre-amendment** file (no `effective_time_unix_nano`
//!    column, built with the raw `ArrowWriter` per the RFC0007.4
//!    pattern) answers the same window as `effective :=
//!    time_unix_nano` — old rows are still found, alone *and* mixed
//!    with post-amendment files (where `DataFusion`'s schema union
//!    fills the column with NULL, which would otherwise fail both
//!    window bounds and silently hide the file — the §3.9-forbidden
//!    outcome).
//! 3. The window stays *prunable* on the stored column: one file
//!    with two row groups in one hour partition (so neither the
//!    directory prune nor plan-time file pruning can hide the skip),
//!    and a sub-hour window skips the out-of-window row group via
//!    statistics (RFC 0005 §3.2 rule 3 — the B1 mechanism).
//!
//! The storage half (stored column value, partition tuple, verbatim
//! wire zero) lives in
//! `crates/ourios-parquet/tests/effective_timestamp.rs`.

mod common;

use std::fs::File;
use std::path::Path;

use arrow_array::RecordBatch;
use ourios_core::record::MinedRecord;
use ourios_core::tenant::TenantId;
use parquet::arrow::ArrowWriter;

use common::{DEFAULT_WINDOW_NS, HOUR_NS, NOW, TS0, no_aliases, simple, write_all};
use ourios_parquet::{PartitionKey, columns, mined_records_to_batch};
use ourios_querier::Querier;

/// Render nanoseconds-since-epoch as the RFC 3339 instant the DSL
/// `time` grammar takes (nanosecond precision, so bounds are exact).
fn rfc3339(ns: u64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_nanos(i64::try_from(ns).expect("fits i64"))
        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

/// Run `range(<lo>, <hi>)` (half-open, nanosecond bounds) for tenant
/// "a" and return the matching row count.
async fn rows_in_window(bucket: &Path, lo: u64, hi: u64) -> u64 {
    let query =
        ourios_querier::dsl::parse(&format!("true | range({}, {})", rfc3339(lo), rfc3339(hi)))
            .expect("parse");
    Querier::new(bucket)
        .run_query(
            &query,
            &TenantId::new("a"),
            NOW,
            DEFAULT_WINDOW_NS,
            &no_aliases(),
        )
        .await
        .expect("run_query")
        .rows
}

/// A `simple` fixture row reshaped to the RFC0005.13 trigger: wire
/// `time_unix_nano = 0`, `observed_time_unix_nano = Some(ts_ns)`.
fn observed_only(ts_ns: u64) -> MinedRecord {
    MinedRecord {
        time_unix_nano: 0,
        observed_time_unix_nano: Some(ts_ns),
        ..simple("a", 1, 0)
    }
}

/// Write `record` as a committed `*.parquet` in **pre-amendment**
/// shape: the writer's batch with the `effective_time_unix_nano`
/// column projected away, laid down with the raw `ArrowWriter` at
/// the record's RFC 0005 partition directory (the RFC0007.4 /
/// RFC0005.2 old-writer pattern).
fn write_pre_amendment(bucket: &Path, record: &MinedRecord) {
    let base = mined_records_to_batch(std::slice::from_ref(record)).expect("base batch");
    let keep: Vec<usize> = base
        .schema()
        .fields()
        .iter()
        .enumerate()
        .filter(|(_, f)| f.name() != columns::EFFECTIVE_TIME_UNIX_NANO)
        .map(|(i, _)| i)
        .collect();
    let old: RecordBatch = base.project(&keep).expect("project out effective column");

    let dir = PartitionKey::derive(record)
        .expect("derive partition")
        .data_path(bucket);
    std::fs::create_dir_all(&dir).expect("mkdir partition");
    let file = File::create(dir.join("pre_amendment.parquet")).expect("create parquet");
    let mut w = ArrowWriter::try_new(file, old.schema(), None).expect("arrow writer");
    w.write(&old).expect("write batch");
    w.close().expect("close writer");
}

/// RFC0005.13 — a `range(...)` window containing `T` returns the
/// observed-only record (`time_unix_nano = 0`,
/// `observed_time_unix_nano = T`), and a window over the epoch (where
/// the zero wire value would sit) does NOT — the window filters the
/// effective timestamp, not the wire one.
#[tokio::test]
async fn rfc0005_13_window_returns_observed_only_record() {
    // Arrange
    let bucket = tempfile::TempDir::new().expect("temp");
    write_all(bucket.path(), &[observed_only(TS0)]);

    // Act / Assert — a window around T finds the row.
    assert_eq!(
        rows_in_window(bucket.path(), TS0 - 1_000, TS0 + 1_000).await,
        1,
        "the observed-only record is addressable by time via its effective timestamp",
    );
    // A window over the epoch (covering the wire `0`) finds nothing:
    // the effective value replaced the zero for windowing purposes.
    assert_eq!(
        rows_in_window(bucket.path(), 0, 1_000).await,
        0,
        "the wire zero is not what the window filters",
    );
}

/// RFC0005.13 (second half) — a pre-amendment file (no
/// `effective_time_unix_nano` column) answers a time window as
/// `effective := time_unix_nano` (RFC 0005 §3.9 rule 2): the old row
/// is found both when the file stands alone and when it is mixed with
/// a post-amendment file in the same scan (the schema-union NULL
/// case), and an out-of-window query still excludes it.
#[tokio::test]
async fn rfc0005_13_pre_amendment_file_windows_on_time_unix_nano() {
    // Arrange — hour 10: a pre-amendment file alone in the store.
    let bucket = tempfile::TempDir::new().expect("temp");
    write_pre_amendment(bucket.path(), &simple("a", 1, TS0));

    // Act / Assert — alone: the union schema lacks the column, the
    // window compiles over `time_unix_nano` exactly as before.
    assert_eq!(
        rows_in_window(bucket.path(), TS0 - 1_000, TS0 + 1_000).await,
        1,
        "a pre-amendment-only store answers the window unchanged",
    );
    assert_eq!(
        rows_in_window(bucket.path(), TS0 + 1_000, TS0 + 2_000).await,
        0,
        "out-of-window rows in a pre-amendment file stay excluded",
    );

    // Arrange — hour 11: add a post-amendment file, making the scan
    // mixed: the union schema now has the column and the
    // pre-amendment file's rows read it as NULL.
    write_all(bucket.path(), &[simple("a", 2, TS0 + HOUR_NS)]);

    // Act / Assert — a window covering both hours returns both rows;
    // the NULL-filled old row is NOT silently hidden (§3.9 rule 2's
    // explicit carve-out from absent-OPTIONAL ⇒ predicate-false).
    assert_eq!(
        rows_in_window(bucket.path(), TS0 - 1_000, TS0 + HOUR_NS + 1_000).await,
        2,
        "a mixed scan returns pre- and post-amendment rows alike",
    );
    // And a window covering only the old row still finds exactly it.
    assert_eq!(
        rows_in_window(bucket.path(), TS0 - 1_000, TS0 + 1_000).await,
        1,
        "the pre-amendment row is individually addressable in a mixed scan",
    );
}

/// The effective-timestamp window still prunes row groups via the
/// stored column's statistics (RFC 0005 §3.2 rule 3 — a real column,
/// not a query-time fallback expression, is what keeps B1's pruning
/// mechanism alive). One file with two row groups (so neither the
/// directory-level prune nor `DataFusion`'s plan-time file-level
/// prune can hide the skip from the row-group metrics): a sub-hour
/// window must skip the out-of-window row group by min/max
/// statistics.
#[tokio::test]
async fn rfc0005_13_effective_window_prunes_row_groups() {
    // Arrange — one hour-10 file holding two single-row row groups:
    // one at the start of the hour, one 30 minutes in. Written with
    // the raw `ArrowWriter` (the production writer rotates row
    // groups by size, far above two rows) from the production
    // batch, so the effective column and its statistics are real.
    let bucket = tempfile::TempDir::new().expect("temp");
    let records = [observed_only(TS0), observed_only(TS0 + HOUR_NS / 2)];
    let batch = mined_records_to_batch(&records).expect("batch");
    let dir = PartitionKey::derive(&records[0])
        .expect("derive partition")
        .data_path(bucket.path());
    std::fs::create_dir_all(&dir).expect("mkdir partition");
    let file = File::create(dir.join("two_row_groups.parquet")).expect("create parquet");
    let props = parquet::file::properties::WriterProperties::builder()
        .set_max_row_group_size(1)
        .build();
    let mut w = ArrowWriter::try_new(file, batch.schema(), Some(props)).expect("arrow writer");
    w.write(&batch).expect("write batch");
    w.close().expect("close writer");

    let query = ourios_querier::dsl::parse(&format!(
        "true | range({}, {})",
        rfc3339(TS0 - 1_000),
        rfc3339(TS0 + 1_000)
    ))
    .expect("parse");
    // Act — a window covering only the first file's instant.
    let result = Querier::new(bucket.path())
        .run_query(
            &query,
            &TenantId::new("a"),
            NOW,
            DEFAULT_WINDOW_NS,
            &no_aliases(),
        )
        .await
        .expect("run_query");

    // Assert — only the in-window row matches, and the other file's
    // row group was pruned by statistics, not scanned.
    assert_eq!(result.rows, 1, "only the in-window row matches");
    assert!(
        result.stats.row_groups_pruned >= 1,
        "the out-of-window row group must be pruned via the effective \
         column's statistics; stats={:?}",
        result.stats,
    );
}
