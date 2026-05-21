//! Scenario RFC0005.1 — Round-trip preserves every §3.2 row-level column.
//! See `docs/rfcs/0005-parquet-storage.md` §5.
//!
//! Writes a populated `MinedRecord` through `Writer`, reads it
//! back through `Reader::open_partition`, and asserts every
//! row-level field round-trips per RFC0005.1's clauses:
//!
//! - Raw-byte columns (`trace_id`, `span_id`, `body`):
//!   byte-for-byte equality.
//! - Typed columns: Rust-level equality.
//! - Canonical-JSON `Vec<KeyValue>` columns
//!   (`attributes` / `resource_attributes`): empty-vec
//!   round-trips as empty per §3.2's `Vec::new()` ↔ `"[]"` rule.
//!   (Non-empty is deferred to the canonicalisation PR.)
//! - Partition columns are out of scope here — covered by
//!   RFC0005.5 / RFC0005.11.

use ourios_core::audit::ParamType;
use ourios_core::record::{BodyKind, MinedRecord, Param};
use ourios_core::tenant::TenantId;
use ourios_parquet::{PartitionKey, Reader, Writer};
use tempfile::TempDir;

/// A clean-attach String record populated with every row-level
/// column set to a non-default value (RFC0005.1's "every
/// OPTIONAL field set to Some" clause). All records share one
/// partition so the round-trip stays inside a single file.
fn populated_record(ts_ns: u64, template_id: u64) -> MinedRecord {
    MinedRecord {
        tenant_id: TenantId::new("tenant-a"),
        template_id,
        template_version: 7,
        severity_number: 9,
        severity_text: Some("INFO2".to_string()),
        scope_name: Some("lib.auth".to_string()),
        scope_version: Some("1.2.3".to_string()),
        time_unix_nano: ts_ns,
        observed_time_unix_nano: Some(ts_ns + 1_000),
        attributes: Vec::new(),
        dropped_attributes_count: 2,
        resource_attributes: Vec::new(),
        trace_id: Some([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]),
        span_id: Some([0xA, 0xB, 0xC, 0xD, 0xE, 0xF, 0x10, 0x11]),
        flags: 0x01,
        event_name: Some("login.success".to_string()),
        body_kind: BodyKind::String,
        params: vec![
            Param {
                type_tag: ParamType::Num,
                value: "42".to_string(),
            },
            Param {
                type_tag: ParamType::Ip,
                value: "10.0.0.1".to_string(),
            },
        ],
        // Three params would mean tokens.len() >= 2, separators
        // = tokens.len() + 1 >= 3. With 2 params and 1 literal
        // token, tokens.len() = 3, separators = 4. We use 3
        // (matching the lower-bound check) for simplicity —
        // params.len() + 1 = 3 ≤ separators.len() = 3.
        separators: vec![String::new(), " ".to_string(), " ".to_string()],
        body: None,
        confidence: 0.875,
        lossy_flag: false,
    }
}

/// Scenario RFC0005.1 — full row-level round-trip.
#[test]
fn rfc0005_1_round_trip_preserves_every_row_level_column() {
    let bucket = TempDir::new().unwrap();
    let bucket_path = bucket.path();

    // Same partition for every record so all go into one file.
    // 2026-04-02T10:58:00Z baseline.
    let ts0 = 1_775_127_480_000_000_000_u64;
    let originals = vec![
        populated_record(ts0, 1),
        populated_record(ts0 + 1_000_000, 2), // +1ms, same hour
        populated_record(ts0 + 60_000_000_000, 3), // +1min, same hour
    ];

    let partition = PartitionKey::derive(&originals[0]).expect("derive");
    let mut writer = Writer::open(bucket_path, partition.clone()).expect("open writer");
    writer.append_records(&originals).expect("append");
    let written = writer.close().expect("close");

    // Read back through the production query path.
    let reader = Reader::open_partition(&written.path, partition).expect("open_partition");
    let round_tripped = reader.read_all().expect("read_all");

    assert_eq!(round_tripped.len(), originals.len());
    for (i, (orig, rt)) in originals.iter().zip(round_tripped.iter()).enumerate() {
        assert_eq!(
            rt, orig,
            "row {i} mismatch — full struct equality covers every row-level §3.2 column at once"
        );
    }
}

/// RFC0005.1 sub-test — body raw-bytes round-trip preserved
/// byte-for-byte. Uses a lossy record so the writer's
/// `EmptySeparators` carve-out applies.
#[test]
fn rfc0005_1_body_raw_bytes_round_trip() {
    let bucket = TempDir::new().unwrap();

    let rec = MinedRecord {
        tenant_id: TenantId::new("tenant-x"),
        template_id: 0,
        template_version: 0,
        severity_number: 9,
        severity_text: None,
        scope_name: None,
        scope_version: None,
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
        body: Some("user 42 logged in from 10.0.0.1".to_string()),
        confidence: 0.0,
        lossy_flag: true,
    };

    let partition = PartitionKey::derive(&rec).expect("derive");
    let mut writer = Writer::open(bucket.path(), partition.clone()).expect("open");
    writer
        .append_records(std::slice::from_ref(&rec))
        .expect("append");
    let written = writer.close().expect("close");

    let reader = Reader::open_partition(&written.path, partition).expect("open_partition");
    let records = reader.read_all().expect("read_all");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].body, rec.body);
}
