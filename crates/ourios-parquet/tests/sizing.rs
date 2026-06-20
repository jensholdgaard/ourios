//! Scenario RFC0005.6 — row-group size lands inside the H4 target.
//! See `docs/rfcs/0005-parquet-storage.md` §5 and §6.
//!
//! Heavyweight integration test: generates more than 256 MiB of
//! mined records, flushes them through the production writer
//! (`Writer::append_records` / `Writer::close` — not the corpus-mode
//! single-file path), parses the emitted file's Parquet footer, and
//! asserts that every row group's uncompressed `total_byte_size`
//! lands inside the §3.5 / H4 range [128 MiB, 1 GiB] — except the
//! final row group of the file, which carries the remainder and may
//! be smaller.
//!
//! Marked `#[ignore]` (slow + ~hundreds of MiB of allocation); run it
//! by hand via `cargo test -p ourios-parquet --ignored`. Scheduling
//! it on a CI cadence is RFC 0005 §7's open question — the project's
//! CI has no `schedule:` trigger today, so this stays manual.

use std::fs::File;

use ourios_core::record::{BodyKind, MinedRecord};
use ourios_core::tenant::TenantId;
use ourios_parquet::{PartitionKey, Writer};
use parquet::file::reader::{FileReader, SerializedFileReader};
use tempfile::TempDir;

/// Per-record body size. The `body` column is the §3.2 overflow /
/// retention column (no per-value cardinality limit), so a large
/// body is the cheapest way to push real uncompressed bytes through
/// the writer's row-group sizing path.
const BODY_BYTES: usize = 16 * 1024;

/// Total input volume: comfortably past RFC0005.6's ">256 MiB" floor
/// and large enough that the 128 MiB flush threshold seals at least
/// two row groups (i.e. at least one non-final group to assert on).
const TARGET_BYTES: usize = 384 * 1024 * 1024;
const NUM_RECORDS: usize = TARGET_BYTES / BODY_BYTES;

/// Append in bounded sub-batches so peak memory stays small; the
/// writer seals row groups internally as the in-progress buffer
/// crosses the 128 MiB threshold.
const BATCH_ROWS: usize = 512;

const MIB_128: i64 = 128 * 1024 * 1024;
const GIB_1: i64 = 1024 * 1024 * 1024;

/// ~16 KiB of pseudo-random printable ASCII, unique per row. The
/// entropy is load-bearing: a repetitive body compresses away in the
/// writer's in-progress buffer, so its size estimate never crosses the
/// 128 MiB flush threshold and the whole run lands in one row group —
/// which is *not* what the sizing rule is about. High-entropy bodies
/// make the buffered uncompressed size track the real byte volume.
/// (Same generator shape as `no_body_dict.rs`.)
fn high_entropy_body(seed: usize) -> String {
    let mut s = String::with_capacity(BODY_BYTES);
    let mut x: u64 = (seed as u64)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(1);
    for _ in 0..BODY_BYTES {
        x = x
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let c = ((x >> 56) as u8 & 0x3F) + b' '; // printable ASCII
        s.push(c as char);
    }
    s
}

fn sized_record(seed: usize) -> MinedRecord {
    MinedRecord {
        tenant_id: TenantId::new("tenant-sizing"),
        // A parse-failure row: no template matched (id/version 0,
        // confidence 0.0), so the original line is retained in `body`
        // and the row is flagged lossy (RFC 0001 §6.6). This keeps the
        // populated `body`, the empty `separators`, and `lossy_flag`
        // mutually consistent.
        template_id: 0,
        template_version: 0,
        severity_number: 9,
        severity_text: None,
        scope_name: None,
        scope_version: None,
        scope_attributes: Vec::new(),
        resource_schema_url: None,
        scope_schema_url: None,
        time_unix_nano: 1_775_127_480_000_000_000,
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
        separators: Vec::new(),
        body: Some(high_entropy_body(seed)),
        confidence: 0.0,
        lossy_flag: true,
    }
}

/// Scenario RFC0005.6 — every non-final row group's uncompressed
/// size lands inside the §3.5 / H4 [128 MiB, 1 GiB] band.
#[test]
#[ignore = "heavyweight: generates >256 MiB; run via `cargo test -p ourios-parquet --ignored`"]
fn rfc0005_6_row_group_size_lands_inside_h4_target() {
    let bucket = TempDir::new().unwrap();
    let partition = PartitionKey::derive(&sized_record(0)).expect("derive partition");
    let mut writer = Writer::open(bucket.path(), partition).expect("open writer");

    let mut next = 0;
    while next < NUM_RECORDS {
        let end = (next + BATCH_ROWS).min(NUM_RECORDS);
        let batch: Vec<MinedRecord> = (next..end).map(sized_record).collect();
        writer.append_records(&batch).expect("append");
        next = end;
    }
    let written = writer.close().expect("close writer");
    assert_eq!(written.num_rows, i64::try_from(NUM_RECORDS).unwrap());

    let file = File::open(&written.path).expect("open parquet file");
    let reader = SerializedFileReader::new(file).expect("open parquet reader");
    let metadata = reader.metadata();
    let num_rg = metadata.num_row_groups();

    // A meaningful sizing test must have actually split: >256 MiB at a
    // 128 MiB flush threshold yields at least two row groups, so there
    // is at least one non-final group whose lower bound we can assert.
    assert!(
        num_rg >= 2,
        "expected >=2 row groups from >256 MiB of input, got {num_rg}",
    );

    for rg_idx in 0..num_rg {
        let bytes = metadata.row_group(rg_idx).total_byte_size();
        let is_final = rg_idx == num_rg - 1;

        // Upper bound applies to every row group.
        assert!(
            bytes <= GIB_1,
            "row group {rg_idx}: total_byte_size {bytes} exceeds 1 GiB ({GIB_1})",
        );
        // Lower bound applies to every row group except the file's
        // final one, which carries the remainder.
        if !is_final {
            assert!(
                bytes >= MIB_128,
                "row group {rg_idx} (non-final): total_byte_size {bytes} below 128 MiB ({MIB_128})",
            );
        }
    }
}
