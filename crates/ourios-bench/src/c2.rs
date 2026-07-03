//! C2 — Template-count convergence.
//!
//! Per RFC 0006 §3.4.3 the gate is: "template count grows
//! sub-linearly and plateaus within 2× of its steady-state
//! value by 1 M lines", operationalised as **`count(1M) ≥
//! SS / 2`** where SS is the template count at end of corpus.
//! Since template count is monotonic non-decreasing (the
//! miner never unmerges), `count(1M) ≥ SS/2` means the curve
//! can't have more than doubled between 1 M lines and the
//! end — i.e. it's within 2× of steady state.
//!
//! The template count at any point is the number of distinct
//! non-[`NO_TEMPLATE`] `template_id`s seen so far. The
//! cluster's id allocator is **monotonic** — it hands out
//! `1, 2, 3, …` in creation order (RFC 0001 §6.1:
//! "per-tenant monotonic"; the bench is single-tenant), so a
//! template's id first appears on the line that created it and
//! is strictly greater than every id seen before. That lets
//! C2 count distinct templates in **O(1) memory**: track the
//! max id seen and bump a counter only when an id exceeds it.
//! This matters because a *non-converging* corpus — the very
//! thing C2 is built to flag — can mint millions of templates;
//! a `HashSet` of ids would balloon (and could OOM) on exactly
//! those inputs, whereas the max-plus-counter stays flat. C2
//! is otherwise a pure stream accumulator over the harness
//! callback, like C1.
//!
//! Pinned definitions (§3.4.3):
//!
//! - **Sample cadence** `N = max(1, ceil(total_lines / 1024))`
//!   — bounds the curve to ≤ 1024 samples. The count is
//!   recorded after line indices `N-1, 2N-1, …` (1-based:
//!   every N-th line), with the final line always sampled.
//!   Sample count is `ceil(total_lines / N)`.
//! - **Steady-state (SS)**: the count at the last sample.
//! - **Count at 1 M lines**: the count at the sample whose
//!   1-based line number is closest to `1_000_000`, floor
//!   tie-break. Defined only on corpora ≥ 1 M lines.
//! - **Convergence ratio** = `count_1m / SS`, in `(0, 1]`.
//! - **Pass**: `ratio ≥ 0.5` on a ≥ 1 M-line corpus; corpora
//!   below 1 M lines abstain (`pass = None`).

use ourios_core::record::MinedRecord;
use ourios_miner::cluster::NO_TEMPLATE;

use crate::{C2Result, ConvergenceSample};

/// Curve-size cap: the cadence is chosen so a corpus of any
/// size yields at most this many samples (§3.4.3).
const SAMPLE_BUDGET: u64 = 1024;

/// The "1 M lines" mark the convergence ratio is measured at.
const ONE_MILLION: u64 = 1_000_000;

/// Streaming accumulator for the §3.4.3 C2 measurement. Fed one
/// emitted record per ingested line by the harness loop;
/// [`Self::finalize`] computes the [`C2Result`].
pub(crate) struct C2Accumulator {
    total_lines: u64,
    cadence: u64,
    /// Distinct templates seen so far. Bumped whenever an id
    /// exceeds `max_template_id` (a newly-created template,
    /// given monotonic allocation).
    template_count: u64,
    /// Largest `template_id` observed. A larger id means a
    /// freshly-created template; `≤` means a reuse already
    /// counted.
    max_template_id: u64,
    curve: Vec<ConvergenceSample>,
    processed: u64,
}

impl C2Accumulator {
    /// Create an accumulator for a corpus of `total_lines`
    /// lines. The cadence is fixed up front from the line
    /// count per §3.4.3.
    pub(crate) fn new(total_lines: u64) -> Self {
        let cadence = total_lines.div_ceil(SAMPLE_BUDGET).max(1);
        Self {
            total_lines,
            cadence,
            template_count: 0,
            max_template_id: 0,
            curve: Vec::new(),
            processed: 0,
        }
    }

    /// Observe one emitted record. Only `template_id` matters
    /// to C2; the rest of the record is ignored.
    pub(crate) fn record(&mut self, emitted: &MinedRecord) {
        self.observe(emitted.template_id);
    }

    /// Core of [`Self::record`], split out so the colocated
    /// tests can drive the sampling + convergence math at
    /// scale (millions of synthetic ids) without constructing
    /// `MinedRecord`s or running the miner.
    fn observe(&mut self, template_id: u64) {
        // A non-`NO_TEMPLATE` id larger than any seen before is
        // a freshly-created template (monotonic allocation);
        // a smaller-or-equal id is a reuse already counted.
        if template_id != NO_TEMPLATE && template_id > self.max_template_id {
            self.max_template_id = template_id;
            self.template_count += 1;
        }
        self.processed += 1;

        // Sample after every N-th line (1-based `processed`
        // divisible by the cadence) and always on the final
        // line. The guard avoids a duplicate final sample when
        // the last line happens to fall on a cadence boundary.
        let on_cadence = self.processed.is_multiple_of(self.cadence);
        let is_last = self.processed == self.total_lines;
        if (on_cadence || is_last) && self.curve.last().map(|s| s.lines) != Some(self.processed) {
            self.curve.push(ConvergenceSample {
                lines: self.processed,
                template_count: self.template_count,
            });
        }
    }

    /// Compute the §3.4.3 [`C2Result`] from the accumulated
    /// curve.
    ///
    /// The `u64 → f64` casts for the ratio lose precision only
    /// above `2^52` distinct templates, which no real corpus
    /// approaches.
    #[allow(clippy::cast_precision_loss)]
    pub(crate) fn finalize(self) -> C2Result {
        let template_count_at_end = self.curve.last().map_or(0, |s| s.template_count);
        let corpus_at_least_1m = self.total_lines >= ONE_MILLION;

        let (template_count_at_1m_lines, convergence_ratio, pass) = if corpus_at_least_1m {
            // Sample whose 1-based line number is closest to
            // 1 M; on a tie the earlier (smaller `lines`)
            // sample wins — the `(distance, lines)` key makes
            // that the strict minimum.
            let count_1m = self
                .curve
                .iter()
                .min_by_key(|s| (s.lines.abs_diff(ONE_MILLION), s.lines))
                .map(|s| s.template_count);
            let ratio = count_1m.and_then(|c| {
                (template_count_at_end > 0).then(|| (c as f64) / (template_count_at_end as f64))
            });
            let pass = ratio.map(|r| r >= 0.5);
            (count_1m, ratio, pass)
        } else {
            (None, None, None)
        };

        C2Result {
            sample_cadence: self.cadence,
            total_lines: self.total_lines,
            template_count_at_1m_lines,
            template_count_at_end,
            convergence_ratio,
            convergence_curve: self.curve,
            pass,
            corpus_at_least_1m,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive `observe` over `total_lines` lines whose
    /// `template_id`s cycle through `1..=distinct` (a bounded,
    /// stable alphabet), then finalize. Pure counter work — no
    /// miner, no disk — so even a 1 M-line run is milliseconds.
    fn run_stable(total_lines: u64, distinct: u64) -> C2Result {
        let mut acc = C2Accumulator::new(total_lines);
        for i in 0..total_lines {
            // ids in 1..=distinct (0 would be NO_TEMPLATE).
            acc.observe((i % distinct) + 1);
        }
        acc.finalize()
    }

    /// Cadence bounds the curve to ≤ 1024 samples, and the
    /// curve length is exactly `ceil(total_lines / cadence)`
    /// (the RFC0006.3 assertion).
    #[test]
    fn cadence_bounds_curve_length() {
        for total in [1_u64, 100, 1024, 1025, 50_000, 1_000_000] {
            let r = run_stable(total, 4);
            assert!(r.sample_cadence >= 1);
            assert!(
                r.convergence_curve.len() as u64 <= SAMPLE_BUDGET,
                "curve exceeds 1024 samples for total={total}",
            );
            assert_eq!(
                r.convergence_curve.len() as u64,
                total.div_ceil(r.sample_cadence),
                "curve length must equal ceil(total / cadence) for total={total}",
            );
            // The final sample always covers the last line.
            assert_eq!(
                r.convergence_curve.last().unwrap().lines,
                total,
                "final sample is the last line for total={total}",
            );
        }
    }

    /// A ≥ 1 M-line corpus with a bounded alphabet plateaus
    /// immediately, so `count_1m == SS` → ratio 1.0 → pass.
    /// Exercises the full ≥ 1 M gate math at scale without the
    /// miner.
    #[test]
    fn stable_corpus_passes_the_gate() {
        let r = run_stable(1_000_000, 8);
        assert!(r.corpus_at_least_1m);
        assert_eq!(r.template_count_at_end, 8);
        assert_eq!(r.template_count_at_1m_lines, Some(8));
        assert_eq!(r.convergence_ratio, Some(1.0));
        assert_eq!(r.pass, Some(true));
    }

    /// A corpus below 1 M lines abstains: no 1 M count, no
    /// ratio, `pass = None`.
    #[test]
    fn short_corpus_abstains() {
        let r = run_stable(10_000, 5);
        assert!(!r.corpus_at_least_1m);
        assert_eq!(r.template_count_at_1m_lines, None);
        assert_eq!(r.convergence_ratio, None);
        assert_eq!(r.pass, None);
        // The curve is still produced (a diagnostic), and SS is
        // the bounded alphabet size.
        assert_eq!(r.template_count_at_end, 5);
    }

    /// A corpus whose template count is still climbing steeply
    /// at 1 M lines (no plateau) fails the gate: `count_1m` is
    /// far under half the end count.
    #[test]
    fn non_converging_curve_fails_the_gate() {
        // Phase 1 (first 1 M lines): a bounded alphabet of 10
        // templates, so the count at 1 M is ~10. Phase 2
        // (next 1 M lines): a brand-new template every line, so
        // the end count is ~1 M. count_1m / SS ≈ 10 / 1_000_010
        // ≪ 0.5 → the gate fails unambiguously. (Distinct
        // phase-2 ids start at 2 M so they never collide with
        // the 1..=10 alphabet.)
        let total = 2_000_000u64;
        let mut acc = C2Accumulator::new(total);
        for i in 0..total {
            let id = if i < 1_000_000 {
                (i % 10) + 1
            } else {
                2_000_000 + i
            };
            acc.observe(id);
        }
        let r = acc.finalize();
        assert!(r.corpus_at_least_1m);
        let ratio = r.convergence_ratio.expect("ratio on ≥1M corpus");
        assert!(
            ratio < 0.5,
            "templates still climbing at 1 M → ratio {ratio} must be < 0.5",
        );
        assert_eq!(
            r.pass,
            Some(false),
            "a non-converged corpus must fail the C2 gate",
        );
    }
}
