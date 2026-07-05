//! RFC 0024 §5 — calibration extraction + calibrated-generator sanity
//! (`.1`/`.2`), plus the adversarial umbrella (`.7`). The per-property
//! scenarios live with the crates that own each invariant:
//! `.3` (P1 round-trip) in `ourios-parquet`, `.4`/`.5` (P2/P3) in
//! `ourios-miner`, `.6` (P4 query oracle) in `ourios-querier`.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the
//! RFC is red; each names the green slice that discharges it.

/// Scenario RFC0024.1 — calibration extraction.
/// See `docs/rfcs/0024-otlp-envelope-property-testing.md` §5.
#[test]
#[ignore = "RFC0024.1 stub — implemented in the calibration green slice"]
fn rfc0024_1_calibration_extraction_is_deterministic() {
    todo!(
        "RFC0024.1 — `--calibrate` over a corpus produces a byte-identical \
         manifest on rerun, committed alongside the corpus tag"
    );
}

/// Scenario RFC0024.2 — calibrated generators are shaped by the manifest.
/// See `docs/rfcs/0024-otlp-envelope-property-testing.md` §5.
#[test]
#[ignore = "RFC0024.2 stub — implemented in the calibration green slice"]
fn rfc0024_2_calibrated_generators_match_manifest_moments() {
    todo!(
        "RFC0024.2 — gross distribution moments of N generated records fall \
         within documented tolerance of the manifest's"
    );
}

/// Scenario RFC0024.7 — adversarial mode finds nothing today.
/// See `docs/rfcs/0024-otlp-envelope-property-testing.md` §5.
#[test]
#[ignore = "RFC0024.7 stub — implemented in the oracle green slice (the umbrella runs last)"]
fn rfc0024_7_adversarial_mode_passes_the_full_property_set() {
    todo!(
        "RFC0024.7 — P1-P4 pass at an elevated case count on the adversarial \
         generators; any failure is a minimal reproducer by construction"
    );
}
