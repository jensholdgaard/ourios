//! Drain similarity (`simSeq`) and Ourios confidence ratio.
//!
//! Per RFC 0001 §3.2 — the math primitive that decides whether
//! a candidate line attaches to an existing template, gets a
//! new leaf, or is rejected as a parse failure. Per §6.3 — the
//! ratio reframing that makes the clean-attach decision
//! boundary land at `confidence == 1.0` regardless of the
//! configured threshold.
//!
//! This module ships only the pure math. The cluster-side wiring
//! that consumes these (best-candidate selection in
//! [`crate::cluster::MinerCluster::ingest`], the §6.3 three-zone
//! branch, the RFC §6.4 widening + audit emission) is a future
//! PR; flipping `RFC0001.4`, `H1.1`, and `H5.1` waits for that
//! integration.
//!
//! The wildcard distinction is encoded as a [`Token`] enum
//! rather than a `"<*>"` sentinel string. RFC §3.1 describes
//! wildcards using the literal `<*>` notation, but a sentinel
//! string risks collision (a log line containing the literal
//! token `<*>` would be silently treated as a wildcard match)
//! and obscures the type-level distinction between
//! masking-time tags (`<UUID>`, `<NUM>`, ...) and
//! widening-time wildcards. The `Token::Wildcard` variant is
//! the unambiguous representation; future widening code will
//! produce `Vec<Token<'a>>` directly.

/// One position in a Drain template: either a fixed string
/// (matched literally against the candidate line) or a wildcard
/// that matches any token at that position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Token<'a> {
    Fixed(&'a str),
    Wildcard,
}

/// Drain similarity per RFC 0001 §3.2:
///
/// `simSeq(L, T) = (count of positions i where t_i == τ_i or τ_i is <*>) / N`
///
/// Returns a value in `[0.0, 1.0]`. Wildcards in the template
/// always match the corresponding line position.
///
/// # Panics
///
/// - If `line.len() != template.len()`. The Drain tree's
///   length-N node selection guarantees lengths match by
///   construction; a stray caller bypassing the tree gets a
///   loud failure rather than silent truncation-by-`zip`.
/// - If `line.is_empty()`. RFC §3.2 implies `N ≥ 1`; the
///   empty case has no defined similarity (`0 / 0`).
#[must_use]
pub fn sim_seq(line: &[&str], template: &[Token<'_>]) -> f32 {
    assert_eq!(
        line.len(),
        template.len(),
        "sim_seq precondition: line and template must be equal length \
         (got {} vs {})",
        line.len(),
        template.len(),
    );
    assert!(
        !line.is_empty(),
        "sim_seq precondition: empty inputs have no defined similarity (N ≥ 1 per RFC §3.2)",
    );

    let n = line.len();
    let mut matches = 0_usize;
    for i in 0..n {
        let position_matches = match template[i] {
            Token::Wildcard => true,
            Token::Fixed(s) => line[i] == s,
        };
        if position_matches {
            matches += 1;
        }
    }

    // Cast widths: matches and n are both bounded by line.len()
    // (typically a small log-line token count, never close to
    // 2^24); f32 represents every integer in [0, 2^24] exactly,
    // so no precision loss in the division.
    #[allow(clippy::cast_precision_loss)]
    let result = (matches as f32) / (n as f32);
    result
}

/// Ourios confidence ratio per RFC 0001 §6.3:
///
/// `confidence = simSeq / threshold`
///
/// The ratio reframes scale-invariantly across tenants —
/// `confidence == 1.0` is always the clean-attach decision
/// boundary, regardless of the configured threshold. RFC0001.4's
/// worked example: `simSeq = 0.7, threshold = 0.7 → confidence
/// = 1.0`; the same `simSeq = 0.7` under `threshold = 0.5 →
/// confidence = 1.4`.
///
/// # Panics
///
/// If `threshold <= 0.0`. [`MinerConfig::try_new`] enforces
/// `threshold ∈ (0, 1]`, so any threshold sourced from a
/// validated [`MinerConfig`] cannot trigger this; a stray caller
/// bypassing config validation gets a loud failure rather than
/// `+inf` or `NaN`.
///
/// [`MinerConfig`]: ourios_core::config::MinerConfig
/// [`MinerConfig::try_new`]: ourios_core::config::MinerConfig::try_new
#[must_use]
pub fn confidence_ratio(sim_seq: f32, threshold: f32) -> f32 {
    assert!(
        threshold > 0.0,
        "confidence_ratio precondition: threshold must be > 0 (got {threshold})",
    );
    sim_seq / threshold
}

#[cfg(test)]
mod tests {
    use super::*;

    // sim_seq

    #[test]
    fn sim_seq_identical_sequences_return_one() {
        // Arrange — every position is a fixed literal and matches.
        let line = ["user", "42", "logged"];
        let template = [
            Token::Fixed("user"),
            Token::Fixed("42"),
            Token::Fixed("logged"),
        ];

        // Act
        let r = sim_seq(&line, &template);

        // Assert
        assert!(
            (r - 1.0).abs() < f32::EPSILON,
            "identical sequences must yield 1.0, got {r}",
        );
    }

    #[test]
    fn sim_seq_wildcard_template_matches_anything() {
        // Arrange — template is all wildcards; the line content
        // is irrelevant.
        let line = ["whatever", "you", "want"];
        let template = [Token::Wildcard, Token::Wildcard, Token::Wildcard];

        // Act
        let r = sim_seq(&line, &template);

        // Assert
        assert!(
            (r - 1.0).abs() < f32::EPSILON,
            "all-wildcard template must yield 1.0, got {r}",
        );
    }

    #[test]
    fn sim_seq_partial_match_returns_correct_ratio() {
        // Arrange — 2 of 3 positions match; middle position
        // differs (line "42", template "17").
        let line = ["user", "42", "logged"];
        let template = [
            Token::Fixed("user"),
            Token::Fixed("17"),
            Token::Fixed("logged"),
        ];

        // Act
        let r = sim_seq(&line, &template);

        // Assert
        assert!(
            (r - 2.0_f32 / 3.0_f32).abs() < f32::EPSILON,
            "2/3 match must yield 0.667, got {r}",
        );
    }

    #[test]
    fn sim_seq_matches_rfc_section_3_5_worked_example_c() {
        // Arrange — RFC 0001 §3.5 worked example, line C vs
        // template T_A. T_A is "user <NUM> logged in from <IP>"
        // (fixed at every position after masking). Line C is
        // "user <NUM> logged out from <IP>" — differs at
        // position 3 ("in" vs "out"). The RFC asserts the result
        // is 5/6 ≈ 0.833.
        let line = ["user", "<NUM>", "logged", "out", "from", "<IP>"];
        let template = [
            Token::Fixed("user"),
            Token::Fixed("<NUM>"),
            Token::Fixed("logged"),
            Token::Fixed("in"),
            Token::Fixed("from"),
            Token::Fixed("<IP>"),
        ];

        // Act
        let r = sim_seq(&line, &template);

        // Assert
        assert!(
            (r - 5.0_f32 / 6.0_f32).abs() < f32::EPSILON,
            "RFC §3.5 case C must yield 5/6 ≈ 0.833, got {r}",
        );
    }

    #[test]
    #[should_panic(expected = "equal length")]
    fn sim_seq_panics_on_length_mismatch() {
        // Arrange — line length 2, template length 3.
        let line = ["a", "b"];
        let template = [Token::Fixed("a"), Token::Fixed("b"), Token::Fixed("c")];

        // Act + Assert — should_panic catches the precondition.
        let _ = sim_seq(&line, &template);
    }

    #[test]
    #[should_panic(expected = "N ≥ 1")]
    fn sim_seq_panics_on_empty_inputs() {
        // Arrange — both empty.
        let line: [&str; 0] = [];
        let template: [Token<'_>; 0] = [];

        // Act + Assert
        let _ = sim_seq(&line, &template);
    }

    // confidence_ratio

    #[test]
    fn confidence_ratio_at_threshold_returns_one() {
        // Arrange — RFC §6.3 boundary: simSeq == threshold.
        let sim = 0.7_f32;
        let threshold = 0.7_f32;

        // Act
        let r = confidence_ratio(sim, threshold);

        // Assert — exact 1.0 for x/x with finite non-zero x.
        assert!(
            (r - 1.0_f32).abs() < f32::EPSILON,
            "simSeq == threshold must yield exactly 1.0, got {r}",
        );
    }

    #[test]
    fn confidence_ratio_is_scale_invariant_across_tenants() {
        // Arrange — RFC 0001 §6.3 / RFC0001.4: the same
        // `simSeq = 0.7` under two different thresholds yields
        // different confidence values, but `confidence == 1.0`
        // is always the boundary regardless of threshold.
        let sim = 0.7_f32;

        // Act — tenant A on the project default (0.7) sits at
        // the boundary; tenant B on a looser 0.5 sits comfortably
        // above it.
        let conf_a = confidence_ratio(sim, 0.7);
        let conf_b = confidence_ratio(sim, 0.5);

        // Assert
        assert!(
            (conf_a - 1.0_f32).abs() < f32::EPSILON,
            "tenant A (threshold 0.7) must hit the boundary, got {conf_a}",
        );
        assert!(
            (conf_b - 1.4_f32).abs() < f32::EPSILON,
            "tenant B (threshold 0.5) must yield 1.4 for the same simSeq, got {conf_b}",
        );
    }

    #[test]
    #[should_panic(expected = "threshold must be > 0")]
    fn confidence_ratio_panics_on_zero_threshold() {
        // Arrange
        let sim = 0.5_f32;
        let threshold = 0.0_f32;

        // Act + Assert
        let _ = confidence_ratio(sim, threshold);
    }
}
