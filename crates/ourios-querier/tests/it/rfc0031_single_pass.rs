//! RFC 0031 §3.6 — single-pass execution (`QueryOptions::elide_count_scan`).
//!
//! A limited query whose materialized result did not hit the limit is
//! complete, so the count is derivable from the returned rows and the
//! separate count scan is redundant IO. These tests pin the opt-in's
//! contract: `rows` is the full matching total in every case (complete,
//! truncated, exactly-at-limit), the elided `stats` keep the count scan's
//! row-group pruning counts with an honest `bytes_read = 0`, and the
//! default (no opt-in) path is byte-identical to the two-pass behavior
//! the RFC 0017 §3.4 tests pin.

use crate::common::{DEFAULT_WINDOW_NS, NOW, TS0, no_aliases, rec, write_all};

use ourios_core::tenant::TenantId;
use ourios_querier::{Querier, QueryOptions, QueryResult};
use tempfile::TempDir;

/// Two files in one partition — one all-INFO, one all-ERROR — so a
/// `severity >= error` query has a statistics-prunable row group and the
/// pruning counts compared across execution modes are non-trivial.
fn severity_split_store() -> TempDir {
    let bucket = TempDir::new().expect("bucket dir");
    let info: Vec<_> = (0..4)
        .map(|i| rec("acme", 1, TS0 + i, 9, "api", "lib.cart", None, None))
        .collect();
    let errors: Vec<_> = (0..2)
        .map(|i| rec("acme", 2, TS0 + 100 + i, 17, "api", "lib.cart", None, None))
        .collect();
    // Separate `write_all` calls open separate writers, so the two severity
    // populations land in distinct files (distinct row groups).
    write_all(bucket.path(), &info);
    write_all(bucket.path(), &errors);
    bucket
}

async fn run(bucket: &std::path::Path, dsl: &str, options: QueryOptions) -> QueryResult {
    let query = ourios_querier::dsl::parse(dsl).expect("parse DSL");
    Querier::new(bucket)
        .run_query_with(
            &query,
            &TenantId::new("acme"),
            NOW,
            DEFAULT_WINDOW_NS,
            Some(&no_aliases()),
            options,
        )
        .await
        .expect("query")
}

/// Complete result (returned < limit): the count scan is elided — `rows`
/// equals the returned row count, `stats.bytes_read` is an honest 0, the
/// row-group pruning counts are exactly the count scan's, and the total
/// bytes for the query drop by exactly the elided scan.
#[tokio::test]
async fn complete_result_elides_the_count_scan() {
    let bucket = severity_split_store();
    let counted = run(bucket.path(), "severity >= error", QueryOptions::default()).await;
    assert_eq!(counted.rows, 2);
    assert!(counted.stats.bytes_read > 0, "the count scan read data");
    assert!(
        counted.stats.row_groups_pruned >= 1,
        "the INFO-only file's row group must be statistics-prunable \
         (scanned {}, pruned {})",
        counted.stats.row_groups_scanned,
        counted.stats.row_groups_pruned,
    );

    let dsl = "severity >= error | limit 100";
    let two_pass = run(bucket.path(), dsl, QueryOptions::default()).await;
    let elided = run(bucket.path(), dsl, QueryOptions::single_pass()).await;

    assert_eq!(elided.rows, 2, "the count is the returned row count");
    assert_eq!(elided.records.len(), 2, "every matching row rendered");
    assert_eq!(
        elided.stats.bytes_read, 0,
        "the count scan never ran, so its byte figure is an honest 0",
    );
    assert_eq!(
        (
            elided.stats.row_groups_scanned,
            elided.stats.row_groups_pruned,
        ),
        (
            counted.stats.row_groups_scanned,
            counted.stats.row_groups_pruned,
        ),
        "the materialize plan prunes by the same predicate over the same \
         files, so the elided stats keep the count-scan pruning counts",
    );
    assert!(elided.materialize_bytes_read > 0);
    assert_eq!(
        elided.materialize_bytes_read, two_pass.materialize_bytes_read,
        "elision changes which scans run, not what materialization reads",
    );

    let total =
        |r: &QueryResult| r.stats.bytes_read + r.materialize_bytes_read + r.registry_bytes_read;
    assert_eq!(
        total(&two_pass) - total(&elided),
        two_pass.stats.bytes_read,
        "the saving is exactly the elided count scan's bytes",
    );
}

/// Truncated result (returned == limit, more rows exist): elision cannot
/// prove completeness, so the count scan runs — `rows` is the full
/// matching total and `stats` keep the pinned two-pass shape (equal to a
/// count-only query's, RFC 0017 §3.4).
#[tokio::test]
async fn truncated_result_falls_back_to_the_count_scan() {
    let bucket = severity_split_store();
    let counted = run(bucket.path(), "severity >= error", QueryOptions::default()).await;

    let truncated = run(
        bucket.path(),
        "severity >= error | limit 1",
        QueryOptions::single_pass(),
    )
    .await;
    assert_eq!(
        truncated.rows, 2,
        "rows is the full matching total despite the elision opt-in",
    );
    assert_eq!(
        truncated.records.len(),
        1,
        "records stay capped at the limit"
    );
    assert_eq!(
        truncated.stats, counted.stats,
        "the fallback ran the count scan, so the pinned stats equality holds",
    );
}

/// Exactly-at-limit result (returned == limit and exactly limit rows
/// exist): indistinguishable from truncation before counting, so the
/// count scan runs and confirms the result was in fact complete.
#[tokio::test]
async fn exact_limit_result_falls_back_and_counts_correctly() {
    let bucket = severity_split_store();
    let counted = run(bucket.path(), "severity >= error", QueryOptions::default()).await;

    let exact = run(
        bucket.path(),
        "severity >= error | limit 2",
        QueryOptions::single_pass(),
    )
    .await;
    assert_eq!(exact.rows, 2);
    assert_eq!(exact.records.len(), 2, "the full result was returned");
    assert_eq!(
        exact.stats, counted.stats,
        "returned == limit ⇒ count scan ran (bytes_read > 0 among them)",
    );
}

/// An empty match under the opt-in elides too (0 returned < limit): zero
/// rows, no count-scan bytes.
#[tokio::test]
async fn empty_result_elides_the_count_scan() {
    let bucket = severity_split_store();
    let none = run(
        bucket.path(),
        "severity >= fatal | limit 10",
        QueryOptions::single_pass(),
    )
    .await;
    assert_eq!(none.rows, 0);
    assert!(none.records.is_empty());
    assert_eq!(none.stats.bytes_read, 0);
}

/// Without the opt-in, a complete limited result still runs both scans —
/// the RFC 0017 §3.4 pinned shape (`limited.stats == counted.stats`)
/// holds beyond the truncated case its own test exercises.
#[tokio::test]
async fn default_options_keep_the_two_pass_stats_shape() {
    let bucket = severity_split_store();
    let counted = run(bucket.path(), "severity >= error", QueryOptions::default()).await;
    let limited = run(
        bucket.path(),
        "severity >= error | limit 100",
        QueryOptions::default(),
    )
    .await;
    assert_eq!(limited.rows, 2);
    assert_eq!(limited.records.len(), 2);
    assert_eq!(limited.stats, counted.stats);
    assert!(limited.stats.bytes_read > 0);
}
