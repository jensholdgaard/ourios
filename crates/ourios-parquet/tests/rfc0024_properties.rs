//! RFC 0024 §5 — the storage-owned pipeline property: P1 (round-trip
//! fidelity, `.3`) over generated OTLP batches from `ourios-testgen` (the dev-only
//! generator crate the calibration green slice introduces).
//! See `crates/ourios-bench/tests/rfc0024_calibration.rs` for the
//! scenario placement map.

/// Scenario RFC0024.3 — P1: round-trip fidelity over generated batches.
/// See `docs/rfcs/0024-otlp-envelope-property-testing.md` §5.
#[test]
#[ignore = "RFC0024.3 stub — implemented in the pipeline-properties green slice"]
fn rfc0024_3_round_trip_fidelity_over_generated_batches() {
    todo!(
        "RFC0024.3 — every generated record round-trips per the RFC 0017/0018 \
         fidelity contract: string bodies bit-identical, structured bodies \
         canonical-JSON equal, envelope fields preserved"
    );
}
