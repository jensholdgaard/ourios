//! RFC 0018 §5 — RFC0018.6 monotonicity arm.
//!
//! Out-of-range `SeverityNumber` is preserved (not clamped) at ingest, so a
//! `severity >= ERROR` query must still match a preserved `25` — the
//! `SeverityNumber` is monotone, and `25 >= 17 (ERROR)`. Proves the querier's
//! severity comparison treats the preserved value correctly end-to-end.
//!
//! See `docs/rfcs/0018-otlp-log-spec-compliance.md` §5; the receiver-preserve
//! + `error.type` arms live in `ourios-ingester/tests/rfc0018_otlp_compliance.rs`.

mod common;

use common::{DEFAULT_WINDOW_NS, NOW, TS0, no_aliases, simple, write_all};
use ourios_core::record::MinedRecord;
use ourios_core::tenant::TenantId;
use ourios_querier::Querier;

/// Scenario RFC0018.6 — a `severity >= ERROR` query matches a preserved
/// out-of-range `SeverityNumber` (monotonicity).
/// See `docs/rfcs/0018-otlp-log-spec-compliance.md` §5.
#[tokio::test]
async fn rfc0018_6_severity_compare_matches_preserved_out_of_range() {
    let bucket = tempfile::TempDir::new().expect("temp");
    let sev = |n: u8, i: u64| MinedRecord {
        severity_number: n,
        ..simple("t", 1, TS0 + i * 1_000)
    };
    write_all(
        bucket.path(),
        &[
            sev(25, 0),  // out-of-named-range, preserved — must match `>= error` (25 >= 17)
            sev(200, 1), // also preserved, also >= error
            sev(9, 2),   // INFO — must NOT match `>= error`
        ],
    );

    let q = Querier::new(bucket.path());
    let tenant = TenantId::new("t");
    let query = ourios_querier::dsl::parse("severity >= error").expect("parse");
    let result = q
        .run_query(&query, &tenant, NOW, DEFAULT_WINDOW_NS, Some(&no_aliases()))
        .await
        .expect("run_query");

    assert_eq!(
        result.rows, 2,
        "the preserved 25 and 200 match `severity >= error` (monotonic); INFO (9) does not",
    );
}
