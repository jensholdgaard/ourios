//! RFC 0031 L-gate math — the comparative pass/fail rules.
//!
//! Pure ratio logic over the §3.6 measurements (no IO): the bytes-read
//! **must-win** rule for the L1–L4 classes and the **floor** rule for
//! the L6/L7 family, with the §7 calibration values carried as
//! configuration. Mirrors the a1/c1/c2 gate-math
//! pattern: the arithmetic is unit-tested here, the measurements arrive
//! from the comparative harness (`ourios_query_answer` /
//! `parse_loki_bytes_processed`), and the wiring into the §3.6 results
//! file lands with the dispatch-run slice.

/// The RFC 0031 §7 calibration values, as configuration.
///
/// The `Default` carries the §7 **proposed** starting points — margins
/// `M ≥ 10` (mirroring B1's 10× framing on the honest metric) and the
/// floor/parity factors `F_L6 = 3`, `F_L7 = 2`. They are provisional by
/// design: §7 freezes them only after a calibration look at the first
/// indicative run, so nothing here should be treated as accepted until
/// `docs/rfcs/0031-comparative-evaluation-loki.md` §7 says so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComparativeMargins {
    /// L1 (template-exact lookup) must-win margin.
    pub m_l1: u64,
    /// L2 (attribute predicate) must-win margin.
    pub m_l2: u64,
    /// L3 (trace correlation) must-win margin.
    pub m_l3: u64,
    /// L4 (frequency aggregation) must-win margin.
    pub m_l4: u64,
    /// L6 broad-scan latency floor factor.
    pub f_l6: u64,
    /// L7 ingest-throughput parity factor.
    pub f_l7: u64,
}

impl Default for ComparativeMargins {
    fn default() -> Self {
        Self {
            m_l1: 10,
            m_l2: 10,
            m_l3: 10,
            m_l4: 10,
            f_l6: 3,
            f_l7: 2,
        }
    }
}

/// Outcome of one bytes-read gate (RFC 0031 §5): the must-win rule
/// ([`bytes_must_win`], RFC0031.2–.5) or the floor rule
/// ([`bytes_within_floor`], the RFC0031.7–.8 direction).
///
/// A zero byte-count on **either** side is [`Invalid`](Self::Invalid),
/// never a pass: a gate only decides over valid measurements, and a
/// stray zero (a broken channel, an empty result) would otherwise fake
/// an infinite advantage — the same honesty rule that makes a missing
/// Loki stats block an error rather than a 0.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum BytesGateOutcome {
    /// Both measurements valid; the gate is decided.
    Decided {
        /// The evaluating gate's rule held (see [`bytes_must_win`] /
        /// [`bytes_within_floor`] for the two rules).
        pass: bool,
        /// The headline ratio `loki_bytes / ourios_bytes`: values above
        /// `1.0` mean Ourios read that many times fewer bytes, below
        /// `1.0` that it read more. Same orientation for both gates;
        /// must-win passes at `≥ margin`, the floor gate at
        /// `≥ 1/factor`.
        advantage: f64,
    },
    /// The comparison was meaningless: a zero byte-count on either side,
    /// a zero `margin`/`factor` (a misconfigured gate), or a
    /// measurement so large the floor budget overflows.
    Invalid {
        /// What made it meaningless: which side(s) reported zero, that
        /// the margin/factor itself was zero, or that the floor budget
        /// overflowed.
        reason: String,
    },
}

impl BytesGateOutcome {
    /// `true` iff the gate is decided *and* passed.
    #[must_use]
    pub fn passed(&self) -> bool {
        matches!(self, Self::Decided { pass: true, .. })
    }
}

/// Evaluate one bytes-read must-win gate: does Ourios read at least
/// `margin`× fewer bytes than Loki for the same (equivalence-checked)
/// query? Integer domain — no float in the pass decision; the reported
/// `advantage` ratio is derived afterwards for the results table.
#[must_use]
pub fn bytes_must_win(ourios_bytes: u64, loki_bytes: u64, margin: u64) -> BytesGateOutcome {
    // margin == 0 would make `ourios × 0 ≤ loki` pass unconditionally —
    // a misconfiguration must be loud, not a free win.
    if margin == 0 {
        return BytesGateOutcome::Invalid {
            reason: "margin is 0 — a must-win gate with no margin passes everything, \
                     which demonstrates nothing"
                .to_string(),
        };
    }
    if ourios_bytes == 0 || loki_bytes == 0 {
        return BytesGateOutcome::Invalid {
            reason: format!(
                "zero byte-count (ourios={ourios_bytes}, loki={loki_bytes}) — a must-win \
                 gate needs both measurements non-zero to demonstrate anything"
            ),
        };
    }
    // checked_mul, not saturating: saturation to u64::MAX would FALSELY
    // pass against loki_bytes == u64::MAX. Overflow means the true product
    // exceeds u64::MAX ≥ loki_bytes, so overflow ⇒ fail, exactly.
    let pass = match ourios_bytes.checked_mul(margin) {
        Some(product) => product <= loki_bytes,
        None => false,
    };
    #[allow(clippy::cast_precision_loss)] // reporting ratio only; the pass
    // decision above is exact integer math.
    let advantage = loki_bytes as f64 / ourios_bytes as f64;
    BytesGateOutcome::Decided { pass, advantage }
}

/// Evaluate one bytes-read **floor** gate (RFC 0031 §2's L6/L7
/// dispositions, scenarios RFC0031.7–.8, factors `F_L6`/`F_L7` from §7):
/// Ourios is allowed to be *worse* than Loki here, but only within the
/// committed factor — pass iff `ourios_bytes ≤ factor × loki_bytes`.
/// The inverse question of [`bytes_must_win`]: broad scans (L6) and
/// ingest (L7) are bounded-loss classes, not wins to demonstrate.
///
/// The reported `advantage` keeps [`bytes_must_win`]'s orientation
/// (`loki_bytes / ourios_bytes`, above `1.0` means Ourios read fewer
/// bytes) so both gates' tables read the same way; only the pass rule
/// differs — the floor passes at `advantage ≥ 1/factor`.
///
/// Same honesty guards as [`bytes_must_win`], with one twist on the
/// overflow arm: `factor × loki_bytes` overflowing would be a
/// *mathematically true* pass (the budget exceeds anything a `u64` can
/// measure) — but an exabyte-scale Loki figure is a broken measurement,
/// so it fails closed as [`Invalid`](BytesGateOutcome::Invalid) rather
/// than passing on garbage.
#[must_use]
pub fn bytes_within_floor(ourios_bytes: u64, loki_bytes: u64, factor: u64) -> BytesGateOutcome {
    // factor == 0 would make `ourios ≤ 0` fail unconditionally — a
    // misconfiguration must be loud, not a silent permanent fail.
    if factor == 0 {
        return BytesGateOutcome::Invalid {
            reason: "factor is 0 — a floor gate with no budget fails everything, \
                     which demonstrates nothing"
                .to_string(),
        };
    }
    if ourios_bytes == 0 || loki_bytes == 0 {
        return BytesGateOutcome::Invalid {
            reason: format!(
                "zero byte-count (ourios={ourios_bytes}, loki={loki_bytes}) — a floor \
                 gate needs both measurements non-zero to demonstrate anything"
            ),
        };
    }
    // checked_mul, not saturating: a saturated budget of u64::MAX would
    // pass ANY ourios figure — the same false-pass trap as must-win's,
    // reached from the other side. Overflow ⇒ Invalid, never a pass.
    let Some(budget) = loki_bytes.checked_mul(factor) else {
        return BytesGateOutcome::Invalid {
            reason: format!(
                "loki_bytes × factor overflows u64 (loki={loki_bytes}, factor={factor}) \
                 — a budget past u64::MAX is a broken measurement, not a pass"
            ),
        };
    };
    let pass = ourios_bytes <= budget;
    #[allow(clippy::cast_precision_loss)] // reporting ratio only; the pass
    // decision above is exact integer math.
    let advantage = loki_bytes as f64 / ourios_bytes as f64;
    BytesGateOutcome::Decided { pass, advantage }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_carry_the_section_7_proposals() {
        // Pinned so the provisional §7 values can't drift silently; when
        // §7 freezes calibrated values, this test changes WITH the RFC.
        let m = ComparativeMargins::default();
        assert_eq!((m.m_l1, m.m_l2, m.m_l3, m.m_l4), (10, 10, 10, 10));
        assert_eq!((m.f_l6, m.f_l7), (3, 2));
    }

    #[test]
    fn passes_at_and_below_the_margin_boundary() {
        // Exactly at the boundary: ourios×10 == loki ⇒ pass.
        let at = bytes_must_win(100, 1_000, 10);
        assert!(at.passed(), "{at:?}");
        // Well below: decisive win, advantage reported.
        let BytesGateOutcome::Decided { pass, advantage } = bytes_must_win(10, 1_000, 10) else {
            panic!("expected decided");
        };
        assert!(pass);
        assert!((advantage - 100.0).abs() < f64::EPSILON, "{advantage}");
    }

    #[test]
    fn fails_just_above_the_boundary() {
        // One byte over: ourios×10 == loki+10 > loki ⇒ fail, but still a
        // decided (reportable) outcome with its ratio.
        let BytesGateOutcome::Decided { pass, advantage } = bytes_must_win(101, 1_000, 10) else {
            panic!("expected decided");
        };
        assert!(!pass);
        assert!(advantage < 10.0, "{advantage}");
    }

    #[test]
    fn zero_on_either_side_is_invalid_never_a_pass() {
        // A broken channel reporting 0 must not fake an infinite win.
        assert!(!bytes_must_win(0, 1_000, 10).passed());
        assert!(!bytes_must_win(100, 0, 10).passed());
        assert!(!bytes_must_win(0, 0, 10).passed());
        assert!(matches!(
            bytes_must_win(0, 1_000, 10),
            BytesGateOutcome::Invalid { .. }
        ));
        // A zero MARGIN would pass unconditionally (0×anything ≤ loki) —
        // it must be Invalid, not a free win.
        assert!(matches!(
            bytes_must_win(100, 1_000, 0),
            BytesGateOutcome::Invalid { .. }
        ));
    }

    #[test]
    fn huge_inputs_cannot_wrap_into_a_false_pass() {
        // ourios×margin would overflow; overflow must fail regardless of
        // how large loki is.
        let out = bytes_must_win(u64::MAX / 2, 1_000, 10);
        assert!(!out.passed(), "{out:?}");
        // The saturation trap: with loki == u64::MAX, a saturating product
        // (u64::MAX ≤ u64::MAX) would FALSELY pass even though the true
        // product exceeds it — checked_mul must fail this.
        let trap = bytes_must_win(u64::MAX / 2, u64::MAX, 10);
        assert!(!trap.passed(), "{trap:?}");
        // Sanity: a huge-but-non-overflowing product still decides exactly.
        assert!(bytes_must_win(u64::MAX / 16, u64::MAX, 10).passed());
    }

    #[test]
    fn margins_flow_into_the_decision() {
        // The §6 calibration-wiring pin: the same measurements decide
        // differently under different configured margins.
        let m = ComparativeMargins::default();
        assert!(bytes_must_win(100, 1_000, m.m_l1).passed());
        assert!(!bytes_must_win(100, 1_000, 20).passed());
    }

    #[test]
    fn floor_passes_at_and_below_the_factor_boundary() {
        // Exactly at the boundary: ourios == 3×loki ⇒ pass.
        let at = bytes_within_floor(300, 100, 3);
        assert!(at.passed(), "{at:?}");
        // Ourios reading FEWER bytes trivially satisfies the floor; the
        // advantage keeps the must-win loki/ourios orientation.
        let BytesGateOutcome::Decided { pass, advantage } = bytes_within_floor(10, 1_000, 3) else {
            panic!("expected decided");
        };
        assert!(pass);
        assert!((advantage - 100.0).abs() < f64::EPSILON, "{advantage}");
    }

    #[test]
    fn floor_fails_just_above_the_boundary() {
        // One byte over the budget: still a decided (reportable) outcome.
        let BytesGateOutcome::Decided { pass, advantage } = bytes_within_floor(301, 100, 3) else {
            panic!("expected decided");
        };
        assert!(!pass);
        assert!(advantage < 1.0, "{advantage}");
    }

    #[test]
    fn floor_zero_on_either_side_is_invalid_never_a_pass() {
        assert!(!bytes_within_floor(0, 1_000, 3).passed());
        assert!(!bytes_within_floor(100, 0, 3).passed());
        assert!(!bytes_within_floor(0, 0, 3).passed());
        assert!(matches!(
            bytes_within_floor(0, 1_000, 3),
            BytesGateOutcome::Invalid { .. }
        ));
        // A zero FACTOR would fail unconditionally — a misconfiguration
        // must be Invalid, not a silent permanent fail.
        assert!(matches!(
            bytes_within_floor(100, 1_000, 0),
            BytesGateOutcome::Invalid { .. }
        ));
    }

    #[test]
    fn floor_overflowing_budget_is_invalid_never_a_pass() {
        // loki×factor overflows: the bound would hold mathematically for
        // ANY ourios figure (a saturated budget of u64::MAX passes
        // everything), but only because the measurement is implausible —
        // checked_mul must refuse, not pass.
        let trap = bytes_within_floor(u64::MAX / 2, u64::MAX, 3);
        assert!(!trap.passed(), "{trap:?}");
        assert!(matches!(trap, BytesGateOutcome::Invalid { .. }));
        // Sanity: a huge-but-non-overflowing budget still decides exactly,
        // on both sides of the bound.
        assert!(bytes_within_floor(u64::MAX / 2, u64::MAX / 4, 3).passed());
        assert!(!bytes_within_floor(u64::MAX / 2, u64::MAX / 8, 3).passed());
    }

    #[test]
    fn floor_factors_flow_into_the_decision() {
        // The same measurements decide differently under F_L6 vs F_L7.
        let m = ComparativeMargins::default();
        assert!(bytes_within_floor(250, 100, m.f_l6).passed());
        assert!(!bytes_within_floor(250, 100, m.f_l7).passed());
    }
}
