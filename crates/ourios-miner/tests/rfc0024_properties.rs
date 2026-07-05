//! RFC 0024 §5 — the miner-owned pipeline properties: P2 (no silent
//! merge, `.4`) and P3 (RFC 0023 bounds, `.5`) over generated OTLP
//! batches from `ourios-testgen`. See
//! `crates/ourios-bench/tests/rfc0024_calibration.rs` for the scenario
//! placement map.

/// Scenario RFC0024.4 — P2: no silent merge over generated batches.
/// See `docs/rfcs/0024-otlp-envelope-property-testing.md` §5.
#[test]
#[ignore = "RFC0024.4 stub — implemented in the pipeline-properties green slice"]
fn rfc0024_4_no_silent_merge_over_generated_batches() {
    todo!(
        "RFC0024.4 — every generated record's row carries a template its \
         line attached to under §6.3, or NO_TEMPLATE with body retained — \
         never another line's template"
    );
}

/// Scenario RFC0024.5 — P3: RFC 0023 bounds hold over generated streams.
/// See `docs/rfcs/0024-otlp-envelope-property-testing.md` §5.
#[test]
#[ignore = "RFC0024.5 stub — implemented in the pipeline-properties green slice"]
fn rfc0024_5_bounds_hold_over_generated_streams() {
    todo!(
        "RFC0024.5 — with deliberately tiny bounds, template count / node \
         fan-out / line length never exceed their caps mid-stream"
    );
}
