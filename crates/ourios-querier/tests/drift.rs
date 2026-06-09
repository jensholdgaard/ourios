//! RFC 0010 — `drift` audit-stream query acceptance tests (RFC0010.1–.8), and
//! the relocated RFC 0001 scenario H5.3 (the miner crate can't run a querier,
//! so the drift query path's home is here — cf. RFC0001.5/.6 in
//! `rfc0001_query_semantics.rs`).
//!
//! Each test seeds the RFC 0005 `audit/` Parquet stream via the production
//! `ParquetAuditSink` (real Parquet, the path drift reads) and runs the
//! compiled `drift from <t1> to <t2>` query through the public
//! [`Querier::run_drift`] surface — no `DataFusion`/SQL appears in the test or
//! the result (RFC0010.8).

mod common;

use common::{compaction, rejected_degenerate, type_expanded, widened, write_audit};
use ourios_core::tenant::TenantId;
use ourios_querier::dsl::{DriftQuery, Statement, parse_statement};
use ourios_querier::{DriftRow, Querier};

/// 2026-06-01T00:00:00Z in nanos — the window lower bound the tests use.
const T1: u64 = 1_780_272_000_000_000_000;
/// 2026-06-01T12:00:00Z — midway through the window (where most events land).
const MID: u64 = 1_780_315_200_000_000_000;
/// 2026-06-02T00:00:00Z — the window upper bound (excluded, half-open).
const T2: u64 = 1_780_358_400_000_000_000;
/// A `now` reference comfortably after the absolute windows; the absolute
/// RFC 3339 bounds make the tests independent of it, but `run_drift` requires
/// one for relative-bound resolution.
const NOW: u64 = 1_780_963_200_000_000_000; // 2026-06-09T00:00:00Z

/// The fixed-window drift query the tests share: `[2026-06-01, 2026-06-02)`.
fn drift_one_day() -> DriftQuery {
    match parse_statement("drift from 2026-06-01T00:00:00Z to 2026-06-02T00:00:00Z")
        .expect("parse drift")
    {
        Statement::Drift(d) => d,
        Statement::Logs(_) => panic!("expected a drift statement"),
    }
}

async fn run(bucket: &std::path::Path, tenant: &str, query: &DriftQuery) -> Vec<DriftRow> {
    Querier::new(bucket)
        .run_drift(query, &TenantId::new(tenant), NOW)
        .await
        .expect("run_drift")
        .rows
}

/// Scenario RFC0010.1 — Drift returns templates that gained a version, with
/// counts (discharges RFC 0001 H5.3).
/// See `docs/rfcs/0010-audit-stream-queries.md` §5 and
/// `docs/rfcs/0001-template-miner.md` §5.
///
/// Templates A and B each have qualifying widening / type-expansion events in
/// `[t1, t2)`; the drift query returns exactly one row each, with
/// `widening_count` equal to the number of qualifying events.
#[tokio::test]
async fn rfc0010_1_drift_returns_drifted_templates_with_counts() {
    // Arrange — A: two widenings; B: one widening + one type-expansion.
    const A: u64 = 100;
    const B: u64 = 200;
    let bucket = tempfile::TempDir::new().expect("temp");
    write_audit(
        bucket.path(),
        &[
            widened("acme", A, 1, MID),
            widened("acme", A, 2, MID + 1_000),
            widened("acme", B, 1, MID + 2_000),
            type_expanded("acme", B, 2, MID + 3_000),
        ],
    );

    // Act
    let rows = run(bucket.path(), "acme", &drift_one_day()).await;

    // Assert — one row per template, counts match.
    assert_eq!(rows.len(), 2, "exactly A and B drifted");
    let by_id: std::collections::HashMap<u64, &DriftRow> =
        rows.iter().map(|r| (r.template_id, r)).collect();
    assert_eq!(by_id[&A].widening_count, 2);
    assert_eq!(by_id[&B].widening_count, 2);
}

/// Scenario H5.3 — Drift query returns templates that gained a version in the
/// window (RFC 0001 §6.7; relocated from `ourios-miner/tests/hazards.rs` since
/// the miner crate can't run a querier).
/// See `docs/rfcs/0001-template-miner.md` §5.
///
/// The same mechanism as RFC0010.1, asserted through the public drift surface:
/// this is the criterion that flips the RFC 0001 H5.3 stub.
#[tokio::test]
async fn h5_3_drift_query_returns_templates_that_gained_a_version() {
    // Arrange — leaf A widened once, leaf B type-expanded once, both in window.
    const A: u64 = 7;
    const B: u64 = 9;
    let bucket = tempfile::TempDir::new().expect("temp");
    write_audit(
        bucket.path(),
        &[
            widened("acme", A, 1, MID),
            type_expanded("acme", B, 3, MID + 5_000),
        ],
    );

    // Act
    let rows = run(bucket.path(), "acme", &drift_one_day()).await;

    // Assert — both leaves are reported as having gained a version.
    let ids: std::collections::BTreeSet<u64> = rows.iter().map(|r| r.template_id).collect();
    assert_eq!(ids, std::collections::BTreeSet::from([A, B]));
    for r in &rows {
        assert_eq!(r.widening_count, 1, "one qualifying event each");
    }
}

/// Scenario RFC0010.2 — Half-open window `[from, to)` excludes out-of-window
/// events.
/// See `docs/rfcs/0010-audit-stream-queries.md` §5.
///
/// An event before `t1`, one exactly at `t2` (the excluded upper bound), and
/// one exactly at `t1` (the included lower bound). Only the lower-boundary and
/// the in-window events count.
#[tokio::test]
async fn rfc0010_2_half_open_window_excludes_out_of_window_events() {
    const A: u64 = 1;
    let bucket = tempfile::TempDir::new().expect("temp");
    write_audit(
        bucket.path(),
        &[
            widened("acme", A, 1, T1 - 1_000), // before t1 — excluded
            widened("acme", A, 2, T1),         // exactly t1 — included (lower bound)
            widened("acme", A, 3, MID),        // in window — included
            widened("acme", A, 4, T2),         // exactly t2 — excluded (upper bound)
        ],
    );

    // Act
    let rows = run(bucket.path(), "acme", &drift_one_day()).await;

    // Assert — only the t1-boundary event and the in-window event count.
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].template_id, A);
    assert_eq!(
        rows[0].widening_count, 2,
        "lower bound included, upper bound excluded",
    );
}

/// Scenario RFC0010.3 — `event_type` scoping excludes non-widenings.
/// See `docs/rfcs/0010-audit-stream-queries.md` §5.
///
/// Template C has only `rejected_degenerate` / `compaction`-adjacent events
/// (no qualifying ones) and must not appear; template D mixes a qualifying
/// widening with a `rejected_degenerate`, and its count includes only the
/// qualifying one.
#[tokio::test]
async fn rfc0010_3_event_type_scoping_excludes_non_widenings() {
    const C: u64 = 30;
    const D: u64 = 40;
    let bucket = tempfile::TempDir::new().expect("temp");
    write_audit(
        bucket.path(),
        &[
            rejected_degenerate("acme", C, 1, MID),
            compaction("acme", MID + 1_000), // carries no template_id
            widened("acme", D, 1, MID + 2_000),
            rejected_degenerate("acme", D, 2, MID + 3_000),
        ],
    );

    // Act
    let rows = run(bucket.path(), "acme", &drift_one_day()).await;

    // Assert — C absent; D present with count 1 (only the widening).
    assert_eq!(rows.len(), 1, "only D drifted");
    assert_eq!(rows[0].template_id, D);
    assert_eq!(rows[0].widening_count, 1);
}

/// Scenario RFC0010.4 — Tenant isolation (CLAUDE.md §3.7).
/// See `docs/rfcs/0010-audit-stream-queries.md` §5.
///
/// Tenant X and tenant Y both have qualifying events in the same window;
/// running drift in X's context never surfaces Y's events (enforced at the
/// `tenant_id` Hive partition root, not a post-scan filter).
#[tokio::test]
async fn rfc0010_4_tenant_isolation() {
    const X_TEMPLATE: u64 = 11;
    const Y_TEMPLATE: u64 = 22;
    let bucket = tempfile::TempDir::new().expect("temp");
    write_audit(
        bucket.path(),
        &[
            widened("tenant-x", X_TEMPLATE, 1, MID),
            widened("tenant-y", Y_TEMPLATE, 1, MID),
        ],
    );

    // Act — drift in tenant X's context.
    let rows = run(bucket.path(), "tenant-x", &drift_one_day()).await;

    // Assert — only X's template; Y is unreachable.
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].template_id, X_TEMPLATE);
    assert!(
        rows.iter().all(|r| r.template_id != Y_TEMPLATE),
        "tenant Y's drift must never appear in tenant X's result",
    );
}

/// Scenario RFC0010.5 — An empty window is an empty result, not an error.
/// See `docs/rfcs/0010-audit-stream-queries.md` §5.
///
/// Two cases: a tenant with no audit files at all, and a tenant whose only
/// events are excluded types. Both return an empty row set, not an error.
#[tokio::test]
async fn rfc0010_5_empty_window_is_empty_not_error() {
    // Case 1 — no audit files for the tenant.
    let bucket = tempfile::TempDir::new().expect("temp");
    let rows = run(bucket.path(), "nobody", &drift_one_day()).await;
    assert!(rows.is_empty(), "no audit files ⇒ empty drift result");

    // Case 2 — only excluded event types in the window.
    let bucket2 = tempfile::TempDir::new().expect("temp");
    write_audit(
        bucket2.path(),
        &[
            rejected_degenerate("acme", 1, 1, MID),
            compaction("acme", MID + 1_000),
        ],
    );
    let rows2 = run(bucket2.path(), "acme", &drift_one_day()).await;
    assert!(
        rows2.is_empty(),
        "only excluded events ⇒ empty drift result"
    );
}

/// Scenario RFC0010.6 — Result ordering is `widening_count` DESC, then
/// `template_id` ASC.
/// See `docs/rfcs/0010-audit-stream-queries.md` §5.
///
/// Three templates with counts 3, 1, 1 (two tied) — the result is ordered by
/// count descending, ties broken by ascending template id (the deterministic
/// pin for golden tests).
#[tokio::test]
async fn rfc0010_6_ordering_count_desc_then_template_id_asc() {
    const HIGH: u64 = 50; // 3 events
    const TIE_LO: u64 = 10; // 1 event, lower id
    const TIE_HI: u64 = 20; // 1 event, higher id
    let bucket = tempfile::TempDir::new().expect("temp");
    write_audit(
        bucket.path(),
        &[
            widened("acme", HIGH, 1, MID),
            widened("acme", HIGH, 2, MID + 1_000),
            widened("acme", HIGH, 3, MID + 2_000),
            widened("acme", TIE_HI, 1, MID + 3_000),
            widened("acme", TIE_LO, 1, MID + 4_000),
        ],
    );

    // Act
    let rows = run(bucket.path(), "acme", &drift_one_day()).await;

    // Assert — HIGH first (count 3), then the tie broken by ascending id.
    let order: Vec<(u64, u64)> = rows
        .iter()
        .map(|r| (r.template_id, r.widening_count))
        .collect();
    assert_eq!(order, vec![(HIGH, 3), (TIE_LO, 1), (TIE_HI, 1)]);
}

/// Scenario RFC0010.7 — Aggregate version/time bounds per template.
/// See `docs/rfcs/0010-audit-stream-queries.md` §5.
///
/// Template A has qualifying events spanning versions and timestamps; its row
/// carries `min(old_version)`, `max(new_version)`, `min(timestamp)` /
/// `max(timestamp)` over the window's qualifying events.
#[tokio::test]
async fn rfc0010_7_aggregate_version_and_time_bounds() {
    const A: u64 = 1;
    let bucket = tempfile::TempDir::new().expect("temp");
    // Versions: old 1→new 2, old 4→new 5, old 2→new 3. Timestamps span MID..MID+2_000.
    write_audit(
        bucket.path(),
        &[
            widened("acme", A, 1, MID),
            widened("acme", A, 4, MID + 2_000),
            widened("acme", A, 2, MID + 1_000),
        ],
    );

    // Act
    let rows = run(bucket.path(), "acme", &drift_one_day()).await;

    // Assert — bounds aggregate across all qualifying events.
    assert_eq!(rows.len(), 1);
    let r = &rows[0];
    assert_eq!(r.min_old_version, 1, "min(old_version)");
    assert_eq!(r.max_new_version, 5, "max(new_version)");
    assert_eq!(r.first_seen, common::at(MID), "min(timestamp)");
    assert_eq!(r.last_seen, common::at(MID + 2_000), "max(timestamp)");
}

/// Scenario RFC0010.8 — No DataFusion/SQL leakage (hazard H6).
/// See `docs/rfcs/0010-audit-stream-queries.md` §5.
///
/// The drift surface is the DSL only: a SQL-shaped or malformed drift query is
/// rejected by the parser with an Ourios-owned error whose `Display` names no
/// DataFusion/SQL construct (the same denylist technique as RFC0002.8 /
/// RFC0007.3). The accepted grammar exposes no engine type either — the public
/// surface is `parse_statement` → `DriftQuery` → `DriftRow`.
#[test]
fn rfc0010_8_no_sql_or_datafusion_leakage() {
    // A SQL-shaped drift attempt is not accepted by the DSL grammar.
    for sql_shaped in [
        "drift from now to now SELECT *",
        "SELECT * FROM audit GROUP BY template_id",
        "drift GROUP BY template_id",
    ] {
        let err = parse_statement(sql_shaped).expect_err("SQL-shaped query must be rejected");
        let shown = err.to_string().to_ascii_lowercase();
        for token in [
            "datafusion",
            "arrow",
            "logicalplan",
            "logical plan",
            "physical_plan",
            "recordbatch",
            "group by",
        ] {
            assert!(
                !shown.contains(token),
                "drift parse error leaked engine token {token:?}: {shown:?}",
            );
        }
    }
}
