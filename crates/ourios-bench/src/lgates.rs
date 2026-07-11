//! RFC 0031 L-gate math — the comparative pass/fail rules.
//!
//! Pure ratio logic over the §3.6 measurements (no IO): the bytes-read
//! **must-win** rule for the L1–L4 classes, with the §7 calibration
//! values carried as configuration. Mirrors the a1/c1/c2 gate-math
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

/// Outcome of one bytes-read must-win gate (RFC 0031 §5, RFC0031.2–.5):
/// pass iff `ourios_bytes × margin ≤ loki_bytes`.
///
/// A zero byte-count on **either** side is [`Invalid`](Self::Invalid),
/// never a pass: a must-win gate only passes on a *demonstrated* win over
/// valid measurements, and a stray zero (a broken channel, an empty
/// result) would otherwise fake an infinite advantage — the same honesty
/// rule that makes a missing Loki stats block an error rather than a 0.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum BytesGateOutcome {
    /// Both measurements valid; the gate is decided.
    Decided {
        /// `ourios_bytes × margin ≤ loki_bytes`.
        pass: bool,
        /// `loki_bytes / ourios_bytes` — how many times fewer bytes
        /// Ourios read (the headline ratio; ≥ `margin` ⇒ `pass`).
        advantage: f64,
    },
    /// A zero byte-count made the comparison meaningless.
    Invalid {
        /// Which side(s) reported zero.
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
    // Saturating: an astronomically large ourios×margin can only make the
    // comparison *harder* to pass, never wrap into a false pass.
    let pass = ourios_bytes.saturating_mul(margin) <= loki_bytes;
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
        // ourios×margin would overflow; saturation keeps the comparison
        // failing (u64::MAX > loki) instead of wrapping small.
        let out = bytes_must_win(u64::MAX / 2, 1_000, 10);
        assert!(!out.passed(), "{out:?}");
    }

    #[test]
    fn margins_flow_into_the_decision() {
        // The §6 calibration-wiring pin: the same measurements decide
        // differently under different configured margins.
        let m = ComparativeMargins::default();
        assert!(bytes_must_win(100, 1_000, m.m_l1).passed());
        assert!(!bytes_must_win(100, 1_000, 20).passed());
    }
}
