//! RFC 0001 — template-id query semantics (RFC0001.5, RFC0001.6).
//!
//! These two §5 criteria are query-semantics, not miner behaviour: they
//! assert what a `where template_id = X` predicate returns over a written
//! store, which only the querier can exercise. They live here (not in
//! `ourios-miner/tests`, where the miner crate cannot run queries) and reuse
//! the RFC 0005 store fixtures shared with `tests/rfc0002_dsl.rs` via
//! `tests/common` (`simple`, `write_all`) plus the RFC0002.9 operator-built
//! `AliasMap` pattern.

/// Scenario RFC0001.5 — Bare `template_id = X` spans all versions of leaf X.
/// See `docs/rfcs/0001-template-miner.md` §5.
///
/// `template_id` is stable across a leaf's widenings, so rows attached against
/// `(X, 1)`, `(X, 2)`, `(X, 3)` all carry the same `template_id` and a bare
/// `template_id == X` returns all three by construction — no alias resolution
/// (an empty `AliasMap`). A control row with a different `template_id` is
/// excluded.
#[tokio::test]
async fn rfc0001_5_bare_template_id_spans_all_versions_of_leaf() {
    use crate::common::{DEFAULT_WINDOW_NS, HOUR_NS, NOW, TS0, no_aliases, simple, write_all};
    use ourios_core::tenant::TenantId;
    use ourios_querier::Querier;

    // Arrange — leaf X (= 1) with three rows differing only in
    // `template_version` (1, 2, 3), modelling the same leaf widened twice over
    // time; plus a control row at a different `template_id` (= 2).
    const X: u64 = 1;
    let bucket = tempfile::TempDir::new().expect("temp");
    let versioned = |version: u32, i: u64| ourios_core::record::MinedRecord {
        template_version: version,
        ..simple("a", X, TS0 + i * 1_000)
    };
    write_all(
        bucket.path(),
        &[
            versioned(1, 0),
            versioned(2, 1),
            versioned(3, 2),
            // Control: a different leaf, must NOT match `template_id == X`.
            simple("a", 2, TS0 + HOUR_NS),
        ],
    );
    let q = Querier::new(bucket.path());
    let tenant = TenantId::new("a");

    // Act — a bare `template_id == X` against an EMPTY alias map (no alias
    // resolution is involved — this is by-construction).
    let query = ourios_querier::dsl::parse(&format!("template_id == {X}")).expect("parse");
    let result = q
        .run_query(&query, &tenant, NOW, DEFAULT_WINDOW_NS, Some(&no_aliases()))
        .await
        .expect("run_query");

    // Assert — all three (X, v) rows match; the different-id control is
    // excluded. `template_id` is version-stable, so the bare equality spans
    // every version of the leaf.
    assert_eq!(
        result.rows, 3,
        "template_id == X spans all of (X,1),(X,2),(X,3); the different-id control is excluded",
    );
}

/// Scenario RFC0001.6 — Bare `template_id = X` does NOT follow alias chains.
/// See `docs/rfcs/0001-template-miner.md` §5.
///
/// Given two leaves A and B that the alias index records as equivalent (B ≡ A,
/// asserted via the ourios-core operator API), a bare `template_id == A`
/// returns ONLY A's rows — it never follows the alias chain to B. The explicit
/// `resolves_to(A)` (RFC 0002 §5.4) is the form that includes B. This is the
/// RFC-0001-labelled assertion of the contract RFC0002.9 covers from the DSL
/// side.
#[tokio::test]
async fn rfc0001_6_bare_template_id_does_not_follow_alias_chains() {
    use crate::common::{DEFAULT_WINDOW_NS, HOUR_NS, NOW, TS0, simple, write_all};
    use ourios_core::alias::{ActorId, AliasMap, Operator};
    use ourios_core::audit::InMemoryAuditSink;
    use ourios_core::tenant::TenantId;
    use ourios_querier::Querier;

    // Arrange — two distinct leaves A and B in tenant T, each in its own hour
    // so a `template_id` filter prunes by row-group statistics.
    const A: u64 = 10;
    const B: u64 = 20;
    let bucket = tempfile::TempDir::new().expect("temp");
    write_all(
        bucket.path(),
        &[
            simple("T", A, TS0),
            simple("T", A, TS0 + 1_000),
            simple("T", B, TS0 + HOUR_NS),
        ],
    );
    let q = Querier::new(bucket.path());
    let tenant = TenantId::new("T");

    // The alias index records B ≡ A, built via the ourios-core operator API
    // (the RFC0002.9 pattern).
    let mut aliases = AliasMap::new();
    let mut sink = InMemoryAuditSink::new();
    let by = Operator::now(ActorId::new("op-test").expect("actor"), "merge drift");
    aliases
        .assert(&mut sink, &tenant, A, vec![B], by)
        .expect("assert B ≡ A");

    let rows = async |text: &str| {
        let query = ourios_querier::dsl::parse(text).expect("parse");
        q.run_query(&query, &tenant, NOW, DEFAULT_WINDOW_NS, Some(&aliases))
            .await
            .expect("run_query")
            .rows
    };

    // Act / Assert — bare `template_id == A` returns ONLY A's two rows, never
    // following the alias chain to B …
    assert_eq!(
        rows(&format!("template_id == {A}")).await,
        2,
        "bare template_id == A returns only A's rows, not aliased B",
    );
    // … while the explicit `resolves_to(A)` form (RFC 0002 §5.4) includes B.
    assert_eq!(
        rows(&format!("resolves_to({A})")).await,
        3,
        "resolves_to(A) is the explicit form that follows the alias chain to B",
    );
}
