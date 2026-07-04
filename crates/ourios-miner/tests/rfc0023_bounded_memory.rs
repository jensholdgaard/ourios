//! RFC 0023 §5 — bounded template memory (RFC 0001 amendment).
//!
//! Scenarios RFC0023.1/.3/.4/.5/.6 live here (miner-level bounds +
//! corpus invariance + telemetry). RFC0023.2 (overflow bodies
//! round-trip through the Parquet body column) is an ingest-path
//! integration and lives in
//! `crates/ourios-ingester/tests/rfc0023_overflow_roundtrip.rs`.
//! RFC0023.7 (the 16 GiB `HDFS_v2` scale rerun under 8 GiB peak RSS)
//! is a bench-hardware criterion discharged by the
//! `docs/benchmarks.md` §9 record, not a `cargo test` — the runner
//! lives in the maintainer's `scratch/baseline/` tooling.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the
//! RFC is red; each names the green slice that discharges it.

/// Scenario RFC0023.1 — the ceiling holds and never merges.
/// See `docs/rfcs/0023-bounded-template-memory.md` §5.
#[test]
#[ignore = "RFC0023.1 stub — implemented in the miner-bounds green slice"]
fn rfc0023_1_template_ceiling_holds_and_never_merges() {
    todo!(
        "RFC0023.1 — with a small max_templates, the tenant's template count \
         plateaus at the ceiling, every would-mint line takes the \
         parse-failure path with its body retained, and no overflow line is \
         attached to any existing template"
    );
}

/// Scenario RFC0023.3 — node fan-out caps via wildcard routing.
/// See `docs/rfcs/0023-bounded-template-memory.md` §5.
#[test]
#[ignore = "RFC0023.3 stub — implemented in the miner-bounds green slice"]
fn rfc0023_3_node_fanout_caps_via_wildcard_routing() {
    todo!(
        "RFC0023.3 — a prefix level presenting more than max_node_children \
         distinct tokens never exceeds the cap; later tokens route through \
         the wildcard child and attach stays threshold-gated (a below-floor \
         line still fails parse rather than merging)"
    );
}

/// Scenario RFC0023.4 — the long-line guard.
/// See `docs/rfcs/0023-bounded-template-memory.md` §5.
#[test]
#[ignore = "RFC0023.4 stub — implemented in the miner-bounds green slice"]
fn rfc0023_4_long_lines_fail_parse_with_body_retained() {
    todo!(
        "RFC0023.4 — a line tokenizing past max_line_tokens takes the \
         parse-failure path, its body round-trips bit-identically, and no \
         template of that width exists in the tree"
    );
}

/// Scenario RFC0023.5 — defaults are invisible on healthy corpora.
/// See `docs/rfcs/0023-bounded-template-memory.md` §5.
#[test]
#[ignore = "RFC0023.5 stub — implemented in the miner-bounds green slice"]
fn rfc0023_5_default_bounds_are_invisible_on_healthy_corpora() {
    todo!(
        "RFC0023.5 — under default bounds the seed-corpus template set is \
         identical to an unbounded run (the corpus/C1/C2 suites remain the \
         full-strength oracle in CI)"
    );
}

/// Scenario RFC0023.6 — saturation is observable.
/// See `docs/rfcs/0023-bounded-template-memory.md` §5.
#[test]
#[ignore = "RFC0023.6 stub — implemented in the telemetry green slice"]
fn rfc0023_6_ceiling_saturation_is_observable() {
    todo!(
        "RFC0023.6 — a ceiling-saturated tenant shows \
         ourios.miner.parse_failures increments with \
         reason = template_ceiling and ourios.miner.template.count at the \
         ceiling value"
    );
}
