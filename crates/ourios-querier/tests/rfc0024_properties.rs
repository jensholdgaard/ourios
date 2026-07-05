//! RFC 0024 §5 — the querier-owned pipeline property: P4 (the query
//! oracle, `.6`) over generated OTLP batches and generated predicates
//! from `ourios-testgen`. See
//! `crates/ourios-bench/tests/rfc0024_calibration.rs` for the scenario
//! placement map.

/// Scenario RFC0024.6 — P4: the query oracle.
/// See `docs/rfcs/0024-otlp-envelope-property-testing.md` §5.
#[test]
#[ignore = "RFC0024.6 stub — implemented in the oracle green slice"]
fn rfc0024_6_querier_agrees_with_the_naive_oracle() {
    todo!(
        "RFC0024.6 — for generated batches and generated predicates (every \
         DSL operator class per field kind, incl. promoted and non-promoted \
         attribute equality), the querier's count equals a naive linear-scan \
         evaluator's"
    );
}
