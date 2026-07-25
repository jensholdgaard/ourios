//! RFC0002.21 — a minimum-severity floor does not filter out records whose
//! severity is *unspecified* (`SeverityNumber = 0`).
//!
//! The `OTel` Logs SDK drops a record on `minimum_severity` only when its
//! `SeverityNumber` "is specified (i.e. not 0)"; unspecified records "bypass
//! minimum severity filtering". Ourios previously did the inverse — a
//! `severity >= trace` filter excluded them — so a source that emits
//! unspecified severity (Claude Code's `GenAI` events, ETW `LOG_ALWAYS`,
//! Google Cloud `DEFAULT`) looked like an empty backend.
//!
//! See `docs/rfcs/0002-query-dsl.md` §6.1 amendment.

use crate::common::{DEFAULT_WINDOW_NS, NOW, TS0, no_aliases, simple, write_all};
use ourios_core::record::MinedRecord;
use ourios_core::tenant::TenantId;
use ourios_querier::{Querier, QueryResult};

fn sev(n: u8, i: u64) -> MinedRecord {
    MinedRecord {
        severity_number: n,
        ..simple("t", 1, TS0 + i * 1_000)
    }
}

async fn run(bucket: &std::path::Path, dsl: &str) -> QueryResult {
    let q = Querier::new(bucket);
    let query = ourios_querier::dsl::parse(dsl).expect("parse");
    q.run_query(
        &query,
        &TenantId::new("t"),
        NOW,
        DEFAULT_WINDOW_NS,
        Some(&no_aliases()),
    )
    .await
    .expect("run_query")
}

/// A floor (`>=` / `>`) admits unspecified; a ceiling (`<` / `<=`) does not,
/// so a predicate and its negation still partition the rows.
#[tokio::test]
async fn rfc0002_21_unspecified_bypasses_a_minimum_severity_floor() {
    let bucket = tempfile::TempDir::new().expect("temp");
    write_all(
        bucket.path(),
        &[
            sev(0, 0), // unspecified — the OTel "no severity here"
            sev(0, 1),
            sev(9, 2),  // INFO
            sev(17, 3), // ERROR
        ],
    );

    // The query that used to return nothing at all against real agent
    // telemetry: every row is unspecified or above, so every row matches.
    let all = run(bucket.path(), "severity >= trace").await;
    assert_eq!(
        all.rows, 4,
        "`>= trace` must admit the two unspecified rows (SDK-aligned), plus INFO and ERROR"
    );

    // A floor above INFO still admits unspecified, and still excludes INFO.
    let high = run(bucket.path(), "severity >= error").await;
    assert_eq!(
        high.rows, 3,
        "`>= error` admits the two unspecified rows and ERROR, but not INFO"
    );

    // The complement excludes unspecified, so `p` / `not p` still partition:
    // 3 matched `>= error`, 1 matches `< error`, and 3 + 1 == 4 rows total.
    let low = run(bucket.path(), "severity < error").await;
    assert_eq!(
        low.rows, 1,
        "`< error` matches only INFO — a ceiling does not admit unspecified, \
         so `>= error` and `< error` partition the rows"
    );
}

/// An explicit threshold of `0` is a question *about* unspecified, not a
/// minimum-severity floor, so the bypass must not apply — otherwise
/// `severity > 0` ("has a severity") would absurdly match rows that have none.
#[tokio::test]
async fn rfc0002_21_explicit_zero_threshold_keeps_ordinary_semantics() {
    let bucket = tempfile::TempDir::new().expect("temp");
    write_all(bucket.path(), &[sev(0, 0), sev(9, 1)]);

    let specified = run(bucket.path(), "severity > 0").await;
    assert_eq!(
        specified.rows, 1,
        "`severity > 0` asks for rows that *have* a severity; the unspecified row is not one"
    );
}

/// The bypass is compiled into the predicate, not applied after the scan, so
/// row groups holding only unspecified rows must survive pruning. A
/// post-filter would leave the old min/max pruning in place and silently drop
/// whole files — the failure this test exists to catch.
#[tokio::test]
async fn rfc0002_21_unspecified_row_groups_are_not_pruned_away() {
    let bucket = tempfile::TempDir::new().expect("temp");
    // Written in one batch so the unspecified rows share row groups whose
    // severity max is 0 — exactly what a `>= error` floor would prune.
    write_all(bucket.path(), &[sev(0, 0), sev(0, 1), sev(0, 2)]);

    let result = run(bucket.path(), "severity >= error").await;
    assert_eq!(
        result.rows, 3,
        "a file whose severity range is entirely 0 must still be scanned for a `>= error` floor"
    );
    assert!(
        result.stats.row_groups_scanned > 0,
        "the row group must be scanned, not pruned: {:?}",
        result.stats
    );
}
