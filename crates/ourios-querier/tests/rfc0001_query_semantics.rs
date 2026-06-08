//! RFC 0001 — template-id query semantics (RFC0001.5, RFC0001.6).
//!
//! These two §5 criteria are query-semantics, not miner behaviour: they
//! assert what a `where template_id = X` predicate returns over a written
//! store, which only the querier can exercise. They live here (not in
//! `ourios-miner/tests`, where the miner crate cannot run queries) and reuse
//! the RFC 0005 store fixtures from `tests/rfc0002_dsl.rs` (`simple`,
//! `write_all`) plus the RFC0002.9 operator-built `AliasMap` pattern.

/// Shared fixtures: a real RFC 0005 store written by `ourios-parquet`, the
/// same way `tests/rfc0002_dsl.rs` and `tests/execution.rs` build one, so the
/// compiled DSL runs against genuine Parquet (predicate pushdown + statistics,
/// not a mock).
#[cfg(test)]
mod fixtures {
    use std::collections::HashMap;
    use std::path::Path;

    use ourios_core::audit::ParamType;
    use ourios_core::otlp::any_value::Value as AvValue;
    use ourios_core::otlp::{AnyValue, KeyValue};
    use ourios_core::record::{BodyKind, MinedRecord, Param};
    use ourios_core::tenant::TenantId;
    use ourios_parquet::{PartitionKey, Writer};

    /// 2026-04-02T10:58:00 UTC — the same base instant the execution and
    /// RFC 0002 tests use, so all fixture rows land in one `hour=` partition
    /// unless bumped.
    pub const TS0: u64 = 1_775_127_480_000_000_000;
    /// One hour in nanoseconds.
    pub const HOUR_NS: u64 = 3_600_000_000_000;

    pub fn kv(key: &str, value: &str) -> KeyValue {
        KeyValue {
            key: key.to_string(),
            value: Some(AnyValue {
                value: Some(AvValue::StringValue(value.to_string())),
            }),
            ..Default::default()
        }
    }

    /// A fully-populated fixture record (same shape as the RFC 0002 test
    /// fixtures): `template_version` defaults to 1, overridable via
    /// struct-update syntax.
    #[allow(clippy::too_many_arguments)]
    pub fn rec(
        tenant: &str,
        template_id: u64,
        ts_ns: u64,
        severity_number: u8,
        service: &str,
        scope: &str,
        trace_id: Option<[u8; 16]>,
        span_id: Option<[u8; 8]>,
    ) -> MinedRecord {
        MinedRecord {
            tenant_id: TenantId::new(tenant),
            template_id,
            template_version: 1,
            severity_number,
            severity_text: None,
            scope_name: Some(scope.to_string()),
            scope_version: Some("1.0.0".to_string()),
            time_unix_nano: ts_ns,
            observed_time_unix_nano: Some(ts_ns + 1_000),
            attributes: Vec::new(),
            dropped_attributes_count: 0,
            resource_attributes: vec![kv("service.name", service)],
            trace_id,
            span_id,
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

    /// A minimal record: template 1, INFO, service "api", scope "lib.cart".
    pub fn simple(tenant: &str, template_id: u64, ts_ns: u64) -> MinedRecord {
        rec(tenant, template_id, ts_ns, 9, "api", "lib.cart", None, None)
    }

    pub fn write_all(bucket: &Path, recs: &[MinedRecord]) {
        let mut by_part: HashMap<PartitionKey, Vec<MinedRecord>> = HashMap::new();
        for r in recs {
            by_part
                .entry(PartitionKey::derive(r).expect("derive partition"))
                .or_default()
                .push(r.clone());
        }
        for (part, rs) in by_part {
            let mut w = Writer::open(bucket, part).expect("open writer");
            w.append_records(&rs).expect("append");
            w.close().expect("close");
        }
    }

    /// A window wide enough that a query with no `range(...)` still covers all
    /// fixture rows.
    pub const DEFAULT_WINDOW_NS: u64 = 30 * 24 * HOUR_NS;
    /// A `now` reference comfortably after the fixture instants.
    pub const NOW: u64 = TS0 + 24 * HOUR_NS;

    /// An empty alias projection: no operator has aliased anything, so a bare
    /// `template_id == n` resolves with no alias chain to follow.
    pub fn no_aliases() -> ourios_core::alias::AliasMap {
        ourios_core::alias::AliasMap::new()
    }
}

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
    use fixtures::{DEFAULT_WINDOW_NS, NOW, TS0, simple, write_all};
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
            simple("a", 2, TS0 + fixtures::HOUR_NS),
        ],
    );
    let q = Querier::new(bucket.path());
    let tenant = TenantId::new("a");

    // Act — a bare `template_id == X` against an EMPTY alias map (no alias
    // resolution is involved — this is by-construction).
    let query = ourios_querier::dsl::parse("template_id == 1").expect("parse");
    let result = q
        .run_query(
            &query,
            &tenant,
            NOW,
            DEFAULT_WINDOW_NS,
            &fixtures::no_aliases(),
        )
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
    use fixtures::{DEFAULT_WINDOW_NS, NOW, TS0, simple, write_all};
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
            simple("T", B, TS0 + fixtures::HOUR_NS),
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
        q.run_query(&query, &tenant, NOW, DEFAULT_WINDOW_NS, &aliases)
            .await
            .expect("run_query")
            .rows
    };

    // Act / Assert — bare `template_id == A` returns ONLY A's two rows, never
    // following the alias chain to B …
    assert_eq!(
        rows("template_id == 10").await,
        2,
        "bare template_id == A returns only A's rows, not aliased B",
    );
    // … while the explicit `resolves_to(A)` form (RFC 0002 §5.4) includes B.
    assert_eq!(
        rows("resolves_to(10)").await,
        3,
        "resolves_to(A) is the explicit form that follows the alias chain to B",
    );
}
