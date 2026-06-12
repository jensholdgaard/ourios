//! RFC 0001 — `time_unix_nano` preserved verbatim (RFC0001.10).
//!
//! This §5 criterion is an end-to-end ingest → Parquet → query scenario,
//! not miner behaviour: it asserts that a record's wire `time_unix_nano`
//! survives the write into Parquet bit-for-bit AND that a time-range query
//! over the matching window returns the row. The miner crate can run
//! neither the Parquet write nor a query, so the scenario lives here (the
//! same relocation as RFC0001.5/.6 in `rfc0001_query_semantics.rs`) and
//! reuses the RFC 0005 store fixtures shared via `tests/common`
//! (`simple`, `write_all`).

mod common;

/// Scenario RFC0001.10 — `time_unix_nano` is preserved verbatim from the wire.
/// See `docs/rfcs/0001-template-miner.md` §5.
///
/// `time_unix_nano` is a pass-through field (`MinedRecord.time_unix_nano`),
/// so the obligation is twofold: the value stored in Parquet equals the input
/// byte-for-byte (read back via `ourios_parquet::Reader`), and a DSL
/// time-range query whose window straddles that instant returns the row
/// (gates benchmarks B1). The fixture value is the RFC §5 literal
/// `1_715_700_000_000_000_000` (= 2024-05-14T15:20:00Z); the `range(...)`
/// window `[2024-05-13T11:33:20Z, 2024-05-15T19:06:40Z)`
/// (= `1_715_600_000_000_000_000 .. 1_715_800_000_000_000_000`) brackets it.
/// An explicit `range(...)` fully replaces the default look-back, so the
/// 2026-based `NOW`/window fixtures don't gate the 2024 instant.
#[tokio::test]
async fn rfc0001_10_time_unix_nano_preserved_verbatim_from_wire() {
    use common::{NOW, no_aliases, simple, write_all};
    use ourios_core::record::MinedRecord;
    use ourios_core::tenant::TenantId;
    use ourios_parquet::{PartitionKey, Reader};
    use ourios_querier::Querier;

    // Arrange — one record at the exact RFC §5 instant, plus two controls
    // that must NOT match: `BEFORE` an hour before the window (its hour
    // partition is dropped by the prune) and `AT_HI` exactly at the
    // exclusive upper bound — whose hour partition *overlaps* the window,
    // so only the row-level half-open `ts < end` predicate (not the
    // partition prune) can exclude it. Together they exercise both the
    // partition prune and the row-level range filter.
    const TS: u64 = 1_715_700_000_000_000_000;
    const BEFORE: u64 = 1_715_600_000_000_000_000 - common::HOUR_NS;
    const AT_HI: u64 = 1_715_800_000_000_000_000; // == WINDOW_HI; excluded (half-open)
    // RFC 3339 forms of the §5 window bounds (whole-second instants, so they
    // round-trip to the exact nanos the column stores — RFC 0002 §7 `time`).
    const WINDOW_LO: &str = "2024-05-13T11:33:20Z";
    const WINDOW_HI: &str = "2024-05-15T19:06:40Z";

    let bucket = tempfile::TempDir::new().expect("temp");
    let target = simple("a", 1, TS);
    write_all(
        bucket.path(),
        &[
            target.clone(),
            simple("a", 1, BEFORE),
            simple("a", 1, AT_HI),
        ],
    );
    let q = Querier::new(bucket.path());
    let tenant = TenantId::new("a");

    // Act (query) — a `range(...)` straddling the instant. The explicit range
    // overrides the default look-back, so membership is decided by these
    // bounds, not by the 2026-based `NOW`/`window` fixtures.
    let query = ourios_querier::dsl::parse(&format!("true | range({WINDOW_LO}, {WINDOW_HI})"))
        .expect("parse");
    let result = q
        .run_query(
            &query,
            &tenant,
            NOW,
            common::DEFAULT_WINDOW_NS,
            Some(&no_aliases()),
        )
        .await
        .expect("run_query");

    // Assert (query) — exactly the in-window target row is returned: the
    // `BEFORE` row is partition-pruned, and the `AT_HI` row (in an
    // overlapping partition) is excluded by the half-open row-level
    // `ts < end` predicate — so the row filter, not just the prune, is
    // exercised.
    assert_eq!(
        result.rows, 1,
        "the time-range query returns the single in-window row",
    );

    // Act (storage) — read the target's Parquet file back through the
    // production reader and recover the row.
    let part = PartitionKey::derive(&target).expect("derive partition");
    let file = sole_parquet(&part.data_path(bucket.path()));
    let stored: Vec<MinedRecord> = Reader::open_partition(&file, part)
        .expect("open partition")
        .read_all()
        .expect("read records");

    // Assert (storage) — the stored `time_unix_nano` equals the wire input
    // verbatim (a pass-through field; no rounding, no truncation).
    assert_eq!(stored.len(), 1, "the target file holds exactly the one row");
    assert_eq!(
        stored[0].time_unix_nano, TS,
        "time_unix_nano is stored byte-for-byte as it arrived on the wire",
    );
}

/// The single `*.parquet` file in `dir` (the fixture writes one file per
/// partition). Panics if the directory does not hold exactly one.
fn sole_parquet(dir: &std::path::Path) -> std::path::PathBuf {
    let mut parquets: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .expect("read partition dir")
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "parquet"))
        .collect();
    assert_eq!(parquets.len(), 1, "fixture writes one file per partition");
    parquets.pop().expect("one parquet file")
}
