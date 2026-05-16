//! Per-tenant configuration for the Ourios miner.
//!
//! Defaults satisfy RFC 0001 §3.1.1 (similarity threshold) and
//! §3.2.1 (per-parameter byte limit). Values outside the
//! `CLAUDE.md` §3.2 ceiling are rejected by [`MinerConfig::try_new`]
//! per RFC 0001 §3.2.2 — initialisation fails before the miner
//! ever serves the offending tenant.

use std::error::Error;
use std::fmt;

/// Per-tenant miner configuration.
///
/// Construct with [`MinerConfig::default`] to get the
/// project-default values, or [`MinerConfig::try_new`] for an
/// explicit, validated pair. Field access is intentionally `pub`
/// — once a `MinerConfig` exists it has been validated, so
/// downstream code reads it as plain data.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MinerConfig {
    /// `simSeq` cutoff `s ∈ (0, 1]` for the clean-attach branch
    /// (RFC 0001 §3.3, §6.3). The project default is `0.7` per
    /// `CLAUDE.md` §3.1; lowering the *default* requires an RFC.
    /// Programmatic instantiation accepts any value in `(0, 1]`.
    pub similarity_threshold: f32,

    /// `simSeq` lower bound for the lossy-attach zone per RFC
    /// 0001 §6.3 three-zone model:
    ///
    /// - `simSeq ≥ similarity_threshold` → clean attach
    ///   (existing leaf, optional widen).
    /// - `similarity_floor ≤ simSeq < similarity_threshold` →
    ///   *lossy* zone: a new leaf is created (rather than
    ///   force-merging into a weaker match) and the line's body
    ///   is retained in the eventual data record. Counted by
    ///   `MinerCluster::body_retentions_total`.
    /// - `simSeq < similarity_floor` → parse failure: no
    ///   template is allocated, the line is dropped to the
    ///   parse-failure path, the body is still retained.
    ///
    /// The default is `0.5` (halfway to the project default
    /// threshold of `0.7`). The RFC does not pin a specific
    /// default; this value gives the lossy zone meaningful width
    /// without forcing arbitrary parses through.
    ///
    /// Must hold `0 < similarity_floor ≤ similarity_threshold`.
    /// Setting the floor equal to the threshold makes the lossy
    /// zone empty and reduces the three-zone model to the
    /// pre-§6.3 two-zone shape.
    pub similarity_floor: f32,

    /// Per-parameter byte limit (post-masking). Values above
    /// `CLAUDE.md` §3.2's 1 KiB ceiling are rejected by
    /// [`MinerConfig::try_new`].
    pub param_byte_limit: u32,
}

/// Per-`CLAUDE.md` §3.2: the project ceiling on configurable
/// parameter byte limits. Above this requires an RFC.
pub const PARAM_BYTE_LIMIT_CEILING: u32 = 1024;

/// Failure modes for [`MinerConfig::try_new`] /
/// [`MinerConfig::try_new_full`].
///
/// One variant per validated bound; each carries the offending
/// value so the operator can correlate the error with the
/// configuration source.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MinerConfigError {
    /// The supplied similarity threshold is outside `(0, 1]`.
    ThresholdOutOfRange(f32),
    /// The supplied similarity floor is outside `(0, threshold]`
    /// — the RFC §6.3 three-zone model requires
    /// `0 < floor ≤ threshold`.
    FloorOutOfRange { floor: f32, threshold: f32 },
    /// The supplied per-parameter byte limit exceeds the
    /// `CLAUDE.md` §3.2 ceiling of [`PARAM_BYTE_LIMIT_CEILING`].
    ParamByteLimitTooLarge(u32),
}

impl fmt::Display for MinerConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ThresholdOutOfRange(v) => {
                write!(f, "similarity_threshold must be in (0, 1], got {v}")
            }
            Self::FloorOutOfRange { floor, threshold } => {
                write!(
                    f,
                    "similarity_floor must be in (0, similarity_threshold] per RFC §6.3 \
                     (got floor={floor}, threshold={threshold})",
                )
            }
            Self::ParamByteLimitTooLarge(v) => {
                write!(
                    f,
                    "param_byte_limit exceeds the §3.2 ceiling of {PARAM_BYTE_LIMIT_CEILING} bytes (got {v})",
                )
            }
        }
    }
}

impl Error for MinerConfigError {}

impl Default for MinerConfig {
    /// RFC 0001 §3.1.1, §6.3, and §3.2.1 defaults.
    fn default() -> Self {
        Self {
            similarity_threshold: 0.7,
            similarity_floor: 0.5,
            param_byte_limit: 256,
        }
    }
}

impl MinerConfig {
    /// Validate a candidate configuration.
    ///
    /// Returns the config on success or a [`MinerConfigError`]
    /// naming the failed bound. RFC 0001 §3.2.2 requires that a
    /// caller wiring this into ingester startup propagate the
    /// error rather than serve the tenant; this function pins the
    /// rejection, the propagation contract lives at the call site.
    ///
    /// `similarity_floor` defaults to half of `threshold` so the
    /// two-arg shape stays compatible with pre-§6.3 callers.
    /// Setting the floor explicitly goes through
    /// [`MinerConfig::try_new_full`].
    ///
    /// # Errors
    ///
    /// - [`MinerConfigError::ThresholdOutOfRange`] when
    ///   `threshold` is not in `(0, 1]`.
    /// - [`MinerConfigError::ParamByteLimitTooLarge`] when
    ///   `byte_limit` exceeds [`PARAM_BYTE_LIMIT_CEILING`].
    pub fn try_new(threshold: f32, byte_limit: u32) -> Result<Self, MinerConfigError> {
        Self::try_new_full(threshold, threshold * 0.5, byte_limit)
    }

    /// Validate a candidate configuration with an explicit
    /// [`similarity_floor`][Self::similarity_floor]. See
    /// [`MinerConfig::try_new`] for the two-arg shape that
    /// derives a default floor.
    ///
    /// # Errors
    ///
    /// - [`MinerConfigError::ThresholdOutOfRange`] when
    ///   `threshold` is not in `(0, 1]`.
    /// - [`MinerConfigError::FloorOutOfRange`] when `floor` is
    ///   not in `(0, threshold]`.
    /// - [`MinerConfigError::ParamByteLimitTooLarge`] when
    ///   `byte_limit` exceeds [`PARAM_BYTE_LIMIT_CEILING`].
    pub fn try_new_full(
        threshold: f32,
        floor: f32,
        byte_limit: u32,
    ) -> Result<Self, MinerConfigError> {
        if !(threshold > 0.0 && threshold <= 1.0) {
            return Err(MinerConfigError::ThresholdOutOfRange(threshold));
        }
        if !(floor > 0.0 && floor <= threshold) {
            return Err(MinerConfigError::FloorOutOfRange { floor, threshold });
        }
        if byte_limit > PARAM_BYTE_LIMIT_CEILING {
            return Err(MinerConfigError::ParamByteLimitTooLarge(byte_limit));
        }
        Ok(Self {
            similarity_threshold: threshold,
            similarity_floor: floor,
            param_byte_limit: byte_limit,
        })
    }
}
