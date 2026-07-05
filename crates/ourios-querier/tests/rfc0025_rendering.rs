//! RFC 0025 §5 — the query-owned scenario: rendering distinguishes
//! absent from empty (`.3`). See
//! `crates/ourios-parquet/tests/rfc0025_absent_body.rs` for the
//! scenario placement map.

mod common;

use common::{HOUR_NS, TS0, no_aliases, simple, write_all};
use ourios_core::record::{BodyKind, MinedRecord};
use ourios_core::tenant::TenantId;
use ourios_querier::{LogBody, Querier};
use tempfile::TempDir;

/// A lossy String row whose retained body is the **empty string** —
/// a legal record distinct from an absent body.
fn empty_string_row() -> MinedRecord {
    MinedRecord {
        template_id: 0,
        template_version: 0,
        body_kind: BodyKind::String,
        body: Some(String::new()),
        params: Vec::new(),
        separators: Vec::new(),
        lossy_flag: true,
        confidence: 0.0,
        ..simple("a", 1, TS0)
    }
}

/// An absent-body row as the miner emits it post-RFC 0025.
fn absent_row() -> MinedRecord {
    MinedRecord {
        template_id: 0,
        template_version: 0,
        body_kind: BodyKind::Absent,
        body: None,
        params: Vec::new(),
        separators: Vec::new(),
        lossy_flag: false,
        confidence: 0.0,
        time_unix_nano: TS0 + 1_000,
        ..simple("a", 1, TS0)
    }
}

/// Scenario RFC0025.3 — rendering distinguishes absent from empty.
/// See `docs/rfcs/0025-absent-body-representation.md` §5.
#[test]
fn rfc0025_3_rendering_distinguishes_absent_from_empty() {
    let bucket = TempDir::new().expect("temp dir");
    write_all(bucket.path(), &[empty_string_row(), absent_row()]);

    let querier = Querier::new(bucket.path());
    // The DSL `limit` doubles as the RFC 0017 row cap, so `records`
    // is populated.
    let query = ourios_querier::dsl::parse("severity >= 0 | limit 10").expect("parse");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let mut result = runtime
        .block_on(querier.run_query(
            &query,
            &TenantId::new("a"),
            TS0 + 24 * HOUR_NS,
            30 * 24 * HOUR_NS,
            Some(&no_aliases()),
        ))
        .expect("run_query");

    assert_eq!(result.rows, 2);
    result.records.sort_by_key(|row| row.time_unix_nano);

    match &result.records[0].body {
        LogBody::Rendered { line, .. } => {
            assert!(line.is_empty(), "the empty-string row carries \"\"");
        }
        other => panic!("empty-string row must render, got {other:?}"),
    }
    assert_eq!(
        result.records[1].body,
        LogBody::Absent,
        "the absent row carries no body at all — never an empty string",
    );
}
