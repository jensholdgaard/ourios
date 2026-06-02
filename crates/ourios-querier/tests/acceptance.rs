//! RFC 0007 §5 acceptance criteria — red gate.
//!
//! Each test enumerates one §5 scenario the querier must satisfy
//! before RFC 0007 moves `specified → red → green`. They are
//! `#[ignore]`'d `unimplemented!()` stubs until the execution
//! slice lands (which itself waits on RFC 0002's DSL branch, per
//! the crate docs). The scenario id is in each panic message so
//! the test ↔ criterion mapping is greppable (RFC 0007 §5 /
//! `docs/verification.md` §2).

// RFC0007.1 (B1 pushdown prunes row groups) is now a LIVE test —
// see `tests/execution.rs::rfc0007_1_pushdown_prunes_row_groups`.

#[ignore = "RFC 0007 red gate — execution pending (RFC0007.2)"]
#[test]
fn rfc0007_2_template_exact_latency_scales_with_result_not_corpus() {
    // B2: same query against corpora of increasing size with the
    // result-set size held ~constant — median latency bounded by
    // result size, not corpus size (the inverted-index-collapse
    // claim). Measured by criterion across corpus/otel-demo-v*.
    unimplemented!(
        "RFC0007.2 — assert template-exact latency tracks result \
         cardinality, not corpus size"
    );
}

#[ignore = "RFC 0007 red gate — execution pending (RFC0007.3)"]
#[test]
fn rfc0007_3_no_datafusion_or_sql_leakage() {
    // §4.6: no datafusion / arrow / SQL type appears in any
    // public signature or error Display. (Will be a
    // compile-/string-level boundary assertion once the engine
    // exists; stubbed here as the red-gate placeholder.)
    unimplemented!("RFC0007.3 — assert the public API leaks no DataFusion/arrow/SQL types");
}

#[ignore = "RFC 0007 red gate — execution pending (RFC0007.4)"]
#[test]
fn rfc0007_4_forward_compatible_reads() {
    // §3.5: Parquet files with unknown columns (future schema)
    // or missing optional columns (old schema) query without
    // error, honouring RFC 0005 §3.9 reader-contract defaults.
    unimplemented!("RFC0007.4 — assert unknown/missing columns honour RFC 0005 §3.9 defaults");
}

// RFC0007.5 (tenant isolation) is now a LIVE test — see
// `tests/execution.rs::rfc0007_5_tenant_isolation`.
