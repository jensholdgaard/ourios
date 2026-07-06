//! RFC 0018 — OTLP log-spec compliance acceptance scenario (§5), the DSL arm
//! (`.4`).
//!
//! **Status: `green` (this arm).** `event_name` is a first-class DSL field
//! (parsed, compiled to the `event_name` column, and rendered), exercised
//! end-to-end through [`Querier::run_query`] against a real RFC 0005 store.
//!
//! See `docs/rfcs/0018-otlp-log-spec-compliance.md` §5/§6.

use crate::common::{DEFAULT_WINDOW_NS, NOW, TS0, no_aliases, simple, write_all};
use ourios_core::record::MinedRecord;
use ourios_core::tenant::TenantId;
use ourios_querier::Querier;

/// Scenario RFC0018.4 — `event_name` is filterable in the DSL: a query filtering
/// on `event_name` compiles to the `event_name` column and returns exactly the
/// matching rows, with no DataFusion/SQL surface leaking to the user (H6 — the
/// `QueryResult` it returns is Ourios-owned).
/// See `docs/rfcs/0018-otlp-log-spec-compliance.md` §5.
#[tokio::test]
async fn rfc0018_4_event_name_is_filterable() {
    let bucket = tempfile::TempDir::new().expect("temp");
    let named = |name: &str, i: u64| MinedRecord {
        event_name: Some(name.to_string()),
        ..simple("t", 1, TS0 + i * 1_000)
    };
    write_all(
        bucket.path(),
        &[
            named("checkout", 0),
            named("checkout", 1),
            // Controls: a different event_name and an absent one must not match.
            named("login", 2),
            simple("t", 1, TS0 + 3_000),
        ],
    );

    let q = Querier::new(bucket.path());
    let tenant = TenantId::new("t");
    let query = ourios_querier::dsl::parse("event_name == \"checkout\"").expect("parse");
    let result = q
        .run_query(&query, &tenant, NOW, DEFAULT_WINDOW_NS, Some(&no_aliases()))
        .await
        .expect("run_query");

    assert_eq!(
        result.rows, 2,
        "only the two `checkout` rows match the event_name filter (login + absent excluded)"
    );
}
