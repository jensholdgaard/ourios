//! C1 — Bit-identical reconstruction rate.
//!
//! Per RFC 0006 §3.4.2 the C1 measurement is:
//!
//! ```text
//! C1 = count(records WHERE !lossy_flag AND reconstruct == bytes)
//!    / count(records WHERE !lossy_flag)
//! ```
//!
//! Equality is byte-for-byte `Vec<u8> == line.as_bytes()`
//! after looking up the emit-time `(template_id,
//! template_version)` snapshot via the harness's
//! [`crate::harness::HarnessOutput::snapshots`] map. The
//! target is `1.000000` (six-decimal precision) on every
//! corpus.
//!
//! `lossy_flag = true` rows are excluded from **both**
//! numerator and denominator — that's the definition of
//! "non-lossy reconstruction rate". The bench also reports
//! `lossy_flag_ratio = count(lossy) / count(all)` as a
//! quality signal per `docs/benchmarks.md` C1, with the ≤ 5%
//! / ≤ 20% targets surfaced but **not** gating.

use ourios_miner::reconstruct::reconstruct;

use crate::C1Result;
use crate::harness::HarnessOutput;

/// Compute the C1 result for one harness run. Returns the
/// populated [`C1Result`] regardless of whether the gate
/// passes; the caller surfaces `c1.pass = false` via the
/// results JSON, and `main.rs` translates that into a
/// non-zero process exit per §3.4.2.
///
/// The two `u64 → f64` casts (for `rate` and
/// `lossy_flag_ratio`) lose precision above `2^52` ≈ 4.5 × 10¹⁵
/// records; the bench will never see corpora that large
/// (RFC0006.3 puts the upper end at low millions), so the
/// allow is safe.
#[allow(clippy::cast_precision_loss)]
pub(crate) fn compute(harness: &HarnessOutput) -> C1Result {
    let mut non_lossy_total = 0u64;
    let mut non_lossy_ok = 0u64;
    let mut lossy_count = 0u64;

    for (cline, record) in harness.lines.iter().zip(harness.records.iter()) {
        if record.lossy_flag {
            lossy_count += 1;
            continue;
        }
        non_lossy_total += 1;
        // The H7.1 contract guarantees a snapshot for every
        // emitted (id, version) pair; an absent snapshot is a
        // bench bug (the §3.4.2 "key not in map → bench exits
        // non-zero" rule from the RFC0006.2 docstring). We
        // panic here because the snapshots map is built
        // alongside the records in a single harness loop, so
        // an absence is impossible by construction — the
        // panic catches future refactors that break that
        // construction.
        let template = harness
            .snapshots
            .get(&(record.template_id, record.template_version))
            .unwrap_or_else(|| {
                panic!(
                    "RFC 0006 §3.4.2: emitted record (template_id={}, template_version={}) \
                     has no matching snapshot — harness invariant violated",
                    record.template_id, record.template_version,
                )
            });
        if reconstruct(record, template) == cline.line.as_bytes() {
            non_lossy_ok += 1;
        }
    }

    // §3.4.2 fraction. Defined as `1.0` (vacuously perfect)
    // when there are zero non-lossy rows — surfaces a
    // single-record all-lossy corpus as "no reconstruction
    // failures observed" rather than `NaN`. The gate still
    // passes (no failing rows) so a future H7.1 regression
    // that turns every row lossy would surface via the
    // `lossy_flag_ratio` quality signal, not via C1.
    let all_total = u64::try_from(harness.records.len()).unwrap_or(u64::MAX);
    let rate = if non_lossy_total > 0 {
        (non_lossy_ok as f64) / (non_lossy_total as f64)
    } else {
        1.0
    };
    let lossy_flag_ratio = if all_total > 0 {
        (lossy_count as f64) / (all_total as f64)
    } else {
        0.0
    };

    C1Result {
        non_lossy_total,
        non_lossy_reconstruct_ok: non_lossy_ok,
        rate,
        lossy_flag_ratio,
        pass: non_lossy_ok == non_lossy_total,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corpus;
    use crate::harness;
    use std::path::{Path, PathBuf};

    fn seed_corpus_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("workspace root")
            .join("testdata/corpus")
    }

    /// End-to-end: load the seed corpus, run the harness,
    /// compute C1. Asserts the RFC0006.2 target — every
    /// non-lossy row reconstructs byte-for-byte. Same
    /// property the H7.1 unit-scale test pins in
    /// `crates/ourios-miner/tests/hazards.rs`, only here it
    /// flows through the bench's own corpus → harness → C1
    /// pipeline.
    #[test]
    fn c1_is_100_percent_on_seed_corpus() {
        let load = corpus::load(&seed_corpus_dir()).expect("seed corpus loads");
        let harness = harness::run(load).expect("harness runs");
        let c1 = compute(&harness);
        assert_eq!(
            c1.non_lossy_reconstruct_ok, c1.non_lossy_total,
            "RFC 0006 §3.4.2: every non-lossy row must reconstruct byte-for-byte",
        );
        assert!(
            (c1.rate - 1.0).abs() < 1e-7,
            "rate must equal 1.000000, got {}",
            c1.rate,
        );
        assert!(c1.pass, "c1.pass must be true when rate = 1.000000");
    }
}
