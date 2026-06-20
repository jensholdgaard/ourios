//! `Writer::open_with_zstd_level` — the configurable compression
//! level used by `ourios-bench`'s A1 codec sweep.
//!
//! The level is a physical-encoding knob, not an RFC 0005 §3.5
//! schema change: a file written at any level reads back through
//! the same `Reader` (Parquet records the codec per column chunk).
//! These tests pin (a) a non-default level round-trips, (b) an
//! out-of-range level is rejected, (c) the documented default.

use ourios_core::audit::ParamType;
use ourios_core::record::{BodyKind, MinedRecord, Param};
use ourios_core::tenant::TenantId;
use ourios_parquet::{DEFAULT_ZSTD_LEVEL, PartitionKey, Reader, Writer, WriterError};
use tempfile::TempDir;

fn rec(ts_ns: u64, template_id: u64) -> MinedRecord {
    MinedRecord {
        tenant_id: TenantId::new("tenant-z"),
        template_id,
        template_version: 1,
        severity_number: 9,
        severity_text: Some("INFO".to_string()),
        scope_name: Some("lib.cart".to_string()),
        scope_version: Some("1.0.0".to_string()),
        scope_attributes: Vec::new(),
        resource_schema_url: None,
        scope_schema_url: None,
        time_unix_nano: ts_ns,
        observed_time_unix_nano: Some(ts_ns + 1_000),
        attributes: Vec::new(),
        dropped_attributes_count: 0,
        resource_attributes: Vec::new(),
        trace_id: None,
        span_id: None,
        flags: 0x01,
        event_name: None,
        body_kind: BodyKind::String,
        params: vec![Param {
            type_tag: ParamType::Num,
            value: "42".to_string(),
        }],
        separators: vec![String::new(), " ".to_string()],
        body: None,
        confidence: 1.0,
        lossy_flag: false,
    }
}

/// A file written at a non-default level reads back unchanged —
/// the level never alters the logical contract, only the bytes.
#[test]
fn non_default_zstd_level_round_trips() {
    let bucket = TempDir::new().unwrap();
    let ts0 = 1_775_127_480_000_000_000_u64;
    let originals = vec![
        rec(ts0, 1),
        rec(ts0 + 1_000_000, 2),
        rec(ts0 + 2_000_000, 1),
    ];
    let partition = PartitionKey::derive(&originals[0]).expect("derive");

    let mut writer = Writer::open_with_zstd_level(bucket.path(), partition.clone(), 19)
        .expect("open at level 19");
    writer.append_records(&originals).expect("append");
    let written = writer.close().expect("close");

    let reader = Reader::open_partition(&written.path, partition).expect("open_partition");
    let round_tripped = reader.read_all().expect("read_all");
    assert_eq!(
        round_tripped, originals,
        "level-19 file round-trips identically"
    );
}

/// An out-of-range level is rejected at open as
/// [`WriterError::Parquet`] — ZSTD accepts roughly 1..=22, so 99
/// is firmly invalid.
#[test]
fn out_of_range_zstd_level_is_rejected() {
    let bucket = TempDir::new().unwrap();
    let partition = PartitionKey::derive(&rec(1_775_127_480_000_000_000, 1)).expect("derive");
    // `matches!` rather than a `match` arm so we don't require
    // `Writer: Debug` for the Ok variant (it holds an `ArrowWriter`).
    let rejected = matches!(
        Writer::open_with_zstd_level(bucket.path(), partition, 99),
        Err(WriterError::Parquet(_))
    );
    assert!(
        rejected,
        "level 99 must be rejected as WriterError::Parquet"
    );
}

/// The documented RFC 0005 §3.6 default that `Writer::open` uses.
/// Pinned so a change to the production default is a deliberate,
/// reviewed edit (and an RFC 0005 §3.6 decision).
#[test]
fn default_zstd_level_is_three() {
    assert_eq!(DEFAULT_ZSTD_LEVEL, 3);
}
