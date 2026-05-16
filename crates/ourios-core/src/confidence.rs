//! Three-zone confidence model per RFC 0001 §6.3.
//!
//! Every line that descends the Drain prefix tree to a non-empty
//! candidate set ends up in exactly one of three zones, decided
//! by `simSeq` against the best candidate's template:
//!
//! - **Clean** — `simSeq ≥ similarity_threshold`. The line
//!   attaches to the candidate leaf (optionally widening one or
//!   more positions per RFC §6.2 step 5). The emitted record
//!   does not need to retain the body.
//! - **Lossy** — `similarity_floor ≤ simSeq < similarity_threshold`.
//!   A new leaf is created rather than force-merging into a
//!   too-weak candidate (RFC §6.2 step 5b). The body is retained
//!   in the eventual data record so a reader can render the
//!   original line even though the template was "close enough"
//!   to suggest the candidate without being close enough to
//!   bind.
//! - **Parse failure** — `simSeq < similarity_floor`. No template
//!   is allocated; the line is dropped to the parse-failure path
//!   and the body is still retained for forensic inspection.
//!
//! Lines with no candidate at all (empty leaf list under the
//! `(severity, scope, length, prefix)` parent) take the
//! fresh-leaf branch directly, not classified by this enum —
//! see `MinerCluster::ingest_string`.
//!
//! `lossy_flag` (RFC §6.6) is a separate, orthogonal signal that
//! marks tokenizer/preprocessing failure. A row in the lossy
//! *zone* has body retention but `lossy_flag = false`; a row
//! flagged `lossy_flag = true` reflects a tokenize-time failure
//! independent of the §6.3 zone classification.

/// Three-zone classification per RFC 0001 §6.3.
///
/// The variants are pure data; the cluster owns the policy
/// decisions (leaf allocation, audit-event emission, counter
/// increments) that each zone implies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConfidenceZone {
    /// `simSeq ≥ similarity_threshold`. Attach to candidate
    /// (clean or widened); no body retention required.
    Clean,
    /// `similarity_floor ≤ simSeq < similarity_threshold`. New
    /// leaf created; body retained in the data record.
    Lossy,
    /// `simSeq < similarity_floor`. No template allocated;
    /// parse-failure path with body retention.
    ParseFailure,
}

impl ConfidenceZone {
    /// Classify a similarity score against a threshold/floor
    /// pair. Pure function — the cluster maps the returned zone
    /// to leaf-allocation and counter-bump decisions.
    ///
    /// Both `similarity` and `threshold`/`floor` are expected in
    /// `[0, 1]`; the function tolerates any `f32` input and
    /// reports the zone purely by comparison.
    #[must_use]
    pub fn classify(similarity: f32, threshold: f32, floor: f32) -> Self {
        debug_assert!(
            floor > 0.0 && floor <= threshold && threshold <= 1.0,
            "ConfidenceZone::classify expects 0 < floor ≤ threshold ≤ 1; \
             got floor={floor}, threshold={threshold}",
        );
        match similarity {
            s if s >= threshold => Self::Clean,
            s if s >= floor => Self::Lossy,
            _ => Self::ParseFailure,
        }
    }

    /// `true` iff the row at this zone should retain its
    /// original body bytes in the data record per RFC §6.3.
    /// Clean rows do not retain; lossy and parse-failure rows
    /// do. The §6.6 `lossy_flag` is independent of this.
    #[must_use]
    pub fn retains_body(self) -> bool {
        matches!(self, Self::Lossy | Self::ParseFailure)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Threshold 0.7, floor 0.5 — the project defaults.
    const T: f32 = 0.7;
    const F: f32 = 0.5;

    #[test]
    fn classify_above_threshold_is_clean() {
        assert_eq!(ConfidenceZone::classify(1.0, T, F), ConfidenceZone::Clean);
        assert_eq!(ConfidenceZone::classify(0.8, T, F), ConfidenceZone::Clean);
        // The boundary case: sim == threshold is clean per
        // RFC §6.3's ≥ comparison.
        assert_eq!(ConfidenceZone::classify(0.7, T, F), ConfidenceZone::Clean);
    }

    #[test]
    fn classify_between_floor_and_threshold_is_lossy() {
        assert_eq!(ConfidenceZone::classify(0.69, T, F), ConfidenceZone::Lossy);
        assert_eq!(ConfidenceZone::classify(0.6, T, F), ConfidenceZone::Lossy);
        // Boundary: sim == floor is lossy (still ≥ floor).
        assert_eq!(ConfidenceZone::classify(0.5, T, F), ConfidenceZone::Lossy);
    }

    #[test]
    fn classify_below_floor_is_parse_failure() {
        assert_eq!(
            ConfidenceZone::classify(0.49, T, F),
            ConfidenceZone::ParseFailure,
        );
        assert_eq!(
            ConfidenceZone::classify(0.0, T, F),
            ConfidenceZone::ParseFailure,
        );
    }

    #[test]
    fn retains_body_true_for_lossy_and_parse_failure_only() {
        assert!(!ConfidenceZone::Clean.retains_body());
        assert!(ConfidenceZone::Lossy.retains_body());
        assert!(ConfidenceZone::ParseFailure.retains_body());
    }

    #[test]
    fn classify_with_floor_equal_to_threshold_collapses_lossy_zone() {
        // When floor == threshold the lossy zone is empty;
        // every sim is either Clean (≥) or ParseFailure (<).
        assert_eq!(
            ConfidenceZone::classify(0.7, 0.7, 0.7),
            ConfidenceZone::Clean,
        );
        assert_eq!(
            ConfidenceZone::classify(0.69, 0.7, 0.7),
            ConfidenceZone::ParseFailure,
        );
    }
}
