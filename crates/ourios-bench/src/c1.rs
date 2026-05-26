//! C1 — Bit-identical reconstruction rate.
//!
//! Per RFC 0006 §3.4.2 the C1 measurement is:
//!
//! ```text
//! C1 = count(records WHERE !lossy_flag AND reconstruct == bytes)
//!    / count(records WHERE !lossy_flag)
//! ```
//!
//! Equality is byte-for-byte
//! `reconstruct(record, template) == line_bytes`, where
//! `line_bytes` come from the input OTLP record's body. The
//! target is `1.000000` (six-decimal precision) on every
//! corpus.
//!
//! `lossy_flag = true` rows are excluded from **both**
//! numerator and denominator — the definition of "non-lossy
//! reconstruction rate". The bench also reports
//! `lossy_flag_ratio = count(lossy) / count(all)` as a
//! quality signal per `docs/benchmarks.md` C1, with the
//! ≤ 5% / ≤ 20% targets surfaced but **not** gating.
//!
//! The accumulator is **streaming** — fed one record at a
//! time by the harness loop rather than receiving a buffered
//! `Vec<MinedRecord>` after the fact. Keeps memory bounded
//! at `O(snapshots)` on RFC-sized corpora regardless of line
//! count.

use ourios_core::otlp::OtlpLogRecord;
use ourios_core::record::MinedRecord;
use ourios_miner::reconstruct::reconstruct;
use ourios_miner::tree::OwnedToken;

use crate::C1Result;
use crate::corpus::line_bytes;

/// Streaming accumulator for the §3.4.2 C1 measurement.
///
/// The harness loop calls [`Self::record`] once per ingested
/// line; [`Self::finalize`] computes the [`C1Result`] from
/// the accumulated counters at the end.
#[derive(Debug, Default)]
pub(crate) struct C1Accumulator {
    non_lossy_total: u64,
    non_lossy_reconstruct_ok: u64,
    lossy_count: u64,
    all_total: u64,
}

impl C1Accumulator {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Consume one `(input, emitted, snapshot)` triple per
    /// the harness callback contract. `snapshot` is the
    /// emit-time `(template_id, template_version)` template
    /// tokens; `None` for lossy / `NO_TEMPLATE` records
    /// (which §3.4.2 excludes from the rate anyway).
    pub(crate) fn record(
        &mut self,
        input: &OtlpLogRecord,
        emitted: &MinedRecord,
        snapshot: Option<&[OwnedToken]>,
    ) {
        self.all_total = self.all_total.saturating_add(1);
        if emitted.lossy_flag {
            self.lossy_count = self.lossy_count.saturating_add(1);
            return;
        }
        // Non-lossy. §3.4.2 says we count this record in the
        // denominator regardless of whether the snapshot
        // lookup succeeds; an absent snapshot for a non-lossy
        // record is the harness's "RFC 0001 §6.1 contract
        // violation" hard error (and the harness has already
        // returned `BenchError::Pipeline` before we get
        // here), so in practice we always see `Some(...)`
        // for non-lossy.
        self.non_lossy_total = self.non_lossy_total.saturating_add(1);
        let Some(template) = snapshot else {
            // Defensive — should be unreachable per the
            // harness contract.
            return;
        };
        let Some(line) = line_bytes(input) else {
            // Bench corpus is always `Body::String`; a
            // non-string body would also be a contract
            // violation. Skip silently in C1's denominator
            // would be wrong, so we already counted it
            // above. Mismatch is reported via `pass = false`
            // at finalize time.
            return;
        };
        if reconstruct(emitted, template) == line {
            self.non_lossy_reconstruct_ok = self.non_lossy_reconstruct_ok.saturating_add(1);
        }
    }

    /// Compute the §3.4.2 [`C1Result`] from the accumulator.
    ///
    /// `c1.pass = false` lands in the results JSON when any
    /// non-lossy row failed to reconstruct; `main.rs` maps that
    /// to a non-zero process exit per §3.4.2.
    ///
    /// The two `u64 → f64` casts (for `rate` and
    /// `lossy_flag_ratio`) lose precision above `2^52` ≈
    /// 4.5 × 10¹⁵ records; the bench will never see corpora
    /// that large (RFC 0006 §3.4.3 puts the upper end at low
    /// millions), so the allow is safe.
    #[allow(clippy::cast_precision_loss)]
    pub(crate) fn finalize(&self) -> C1Result {
        // §3.4.2 fraction. Defined as `1.0` (vacuously
        // perfect) when there are zero non-lossy rows —
        // surfaces a single-record all-lossy corpus as "no
        // reconstruction failures observed" rather than
        // `NaN`. The gate still passes (no failing rows) so
        // a future H7.1 regression that turns every row
        // lossy would surface via the `lossy_flag_ratio`
        // quality signal, not via C1.
        let rate = if self.non_lossy_total > 0 {
            (self.non_lossy_reconstruct_ok as f64) / (self.non_lossy_total as f64)
        } else {
            1.0
        };
        let lossy_flag_ratio = if self.all_total > 0 {
            (self.lossy_count as f64) / (self.all_total as f64)
        } else {
            0.0
        };
        C1Result {
            non_lossy_total: self.non_lossy_total,
            non_lossy_reconstruct_ok: self.non_lossy_reconstruct_ok,
            rate,
            lossy_flag_ratio,
            pass: self.non_lossy_reconstruct_ok == self.non_lossy_total,
        }
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

    /// End-to-end: load the seed corpus, run the harness
    /// against a `C1Accumulator`, finalize. Asserts the
    /// RFC0006.2 target — every non-lossy row reconstructs
    /// byte-for-byte. Same property the H7.1 unit-scale test
    /// pins in `crates/ourios-miner/tests/hazards.rs`, only
    /// here it flows through the bench's own corpus →
    /// streaming harness → C1 pipeline.
    #[test]
    fn c1_is_100_percent_on_seed_corpus() {
        let load = corpus::load(&seed_corpus_dir()).expect("seed corpus loads");
        let mut acc = C1Accumulator::new();
        harness::run(&load, false, |input, emitted, snap| {
            acc.record(input, emitted, snap);
        })
        .expect("harness runs");
        let c1 = acc.finalize();
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

    /// Empty accumulator (no records observed) finalises as
    /// the vacuously-perfect `rate = 1.0` / `pass = true` per
    /// the §3.4.2 carve-out for zero non-lossy rows.
    #[test]
    fn empty_accumulator_finalises_vacuously() {
        let c1 = C1Accumulator::new().finalize();
        assert_eq!(c1.non_lossy_total, 0);
        assert!((c1.rate - 1.0).abs() < f64::EPSILON);
        assert!(c1.pass, "empty corpus is vacuously perfect");
    }

    /// RFC0006.2 (mismatch sub-criterion) — a non-lossy row
    /// whose `reconstruct` disagrees with the input bytes is
    /// counted as a failure (`pass = false`, `rate < 1`). The
    /// real miner never produces this (it's the H7.1 property),
    /// so the mismatch path is only reachable via a hand-forged
    /// fixture: a `[Fixed("alpha")]` template reconstructs to
    /// "alpha" while the input line is "beta". End-to-end
    /// forcing through the live pipeline would need a
    /// fault-injection hook the harness deliberately doesn't
    /// have, so this unit test is the home of the contract;
    /// `main.rs` turns `pass = false` into a non-zero exit
    /// (§3.4.2).
    #[test]
    fn reconstruction_mismatch_is_counted_as_failure() {
        use ourios_core::otlp::{Body, OtlpLogRecord};
        use ourios_core::record::{BodyKind, MinedRecord};
        use ourios_core::tenant::TenantId;
        use ourios_miner::tree::OwnedToken;

        let template = vec![OwnedToken::Fixed("alpha".to_string())];
        let emitted = MinedRecord {
            tenant_id: TenantId::new("bench-tenant"),
            template_id: 1,
            template_version: 0,
            severity_number: 9,
            severity_text: None,
            scope_name: None,
            scope_version: None,
            time_unix_nano: 0,
            observed_time_unix_nano: None,
            attributes: Vec::new(),
            dropped_attributes_count: 0,
            resource_attributes: Vec::new(),
            trace_id: None,
            span_id: None,
            flags: 0,
            event_name: None,
            body_kind: BodyKind::String,
            // 0 params + 2 separators satisfies the §6.6
            // template-shape invariant for a single Fixed
            // token, so `reconstruct` uses the template
            // (yielding "alpha") rather than falling back to
            // the retained body.
            params: Vec::new(),
            separators: vec![String::new(), String::new()],
            body: None,
            confidence: 1.0,
            lossy_flag: false,
        };
        let input = OtlpLogRecord {
            tenant_id: TenantId::new("bench-tenant"),
            body: Some(Body::String("beta".to_string())),
            ..Default::default()
        };

        let mut acc = C1Accumulator::new();
        acc.record(&input, &emitted, Some(&template));
        let c1 = acc.finalize();

        assert_eq!(
            c1.non_lossy_total, 1,
            "the non-lossy row is in the denominator"
        );
        assert_eq!(
            c1.non_lossy_reconstruct_ok, 0,
            "the mismatch is not counted as a success",
        );
        assert!(
            (c1.rate - 0.0).abs() < f64::EPSILON,
            "rate is 0 on a sole mismatch"
        );
        assert!(
            !c1.pass,
            "RFC 0006 §3.4.2: a reconstruction mismatch must fail the C1 gate",
        );
    }
}
