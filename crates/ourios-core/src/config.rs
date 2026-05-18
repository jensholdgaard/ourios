//! Per-tenant configuration for the Ourios miner.
//!
//! Defaults satisfy RFC 0001 §3.1.1 (similarity threshold) and
//! §3.2.1 (per-parameter byte limit). Values outside the
//! `CLAUDE.md` §3.2 ceiling are rejected by [`MinerConfig::try_new`]
//! per RFC 0001 §3.2.2 — initialisation fails before the miner
//! ever serves the offending tenant.
//!
//! **Tunables vs invariants (RFC 0004).** Every field on
//! [`MinerConfig`] is a *tunable* — globally settable, overridable
//! per tenant, validated at construction. The set is closed: each
//! field below sits *inside* a `CLAUDE.md` §3 invariant rather
//! than against one. The non-tunable invariants (widening + audit,
//! `severity_number` in the template key, body retention on the
//! §6.3 lossy zone, bit-identical reconstruction, per-tenant
//! mining) live in the algorithm itself and never appear as
//! fields here. Adding a field that touches those areas requires
//! a `meta:` RFC against `CLAUDE.md` §3 — see RFC 0004 §3.5.

use std::error::Error;
use std::fmt;

/// Per-tenant miner configuration.
///
/// Construct with [`MinerConfig::default`] to get the
/// project-default values, or [`MinerConfig::try_new`] for an
/// explicit, validated pair. Field access is intentionally `pub`
/// — once a `MinerConfig` exists it has been validated, so
/// downstream code reads it as plain data.
///
/// # The tunable surface (RFC 0004 §3.2)
///
/// | Field | Default | Validated range | §3 invariant it lives inside |
/// |---|---|---|---|
/// | [`similarity_threshold`](Self::similarity_threshold) | `0.7` | `(0, 1]` | §3.1 — strict-by-default |
/// | [`similarity_floor`](Self::similarity_floor) | `0.4` | `(0, threshold]` | §3.1 — bounds the §6.3 lossy zone; body retention in that zone is invariant |
/// | [`prefix_depth`](Self::prefix_depth) | `2` | `0..=8` | §3.1 — affects tree quality, not safety |
/// | [`param_byte_limit`](Self::param_byte_limit) | `256` | `1..=1024` | §3.2 — bounds cardinality; overflow spilling is invariant |
///
/// The struct is `Clone + Copy + 'static`, so the cluster can
/// hold a default and a per-tenant override map without
/// allocation overhead on the hot path.
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
    /// The default is `0.4` per RFC §6.3 *Defaults*: the floor
    /// matches the threshold from the original Drain paper, on
    /// the reasoning that lines below the paper's own bar are
    /// likely genuinely different events. The lossy-zone floor
    /// is a tuning knob between `0` and `similarity_threshold`;
    /// it is not load-bearing for any §3 invariant.
    ///
    /// Must hold `0 < similarity_floor ≤ similarity_threshold`.
    /// Setting the floor equal to the threshold collapses the
    /// lossy zone to zero width — every below-threshold line
    /// routes straight to the parse-failure path. This is
    /// **stricter** than the pre-§6.3 "create a fresh leaf for
    /// every below-threshold line" shape, not equivalent to it.
    pub similarity_floor: f32,

    /// Per-parameter byte limit (post-masking). Values above
    /// `CLAUDE.md` §3.2's 1 KiB ceiling are rejected by
    /// [`MinerConfig::try_new`].
    pub param_byte_limit: u32,

    /// Drain prefix-tree depth (RFC 0001 §6.2 step 3 — the number
    /// of leading masked tokens the tree partitions by before
    /// reaching a leaf list). Higher = more precise grouping at
    /// the cost of slightly more memory per tenant; lower = more
    /// candidate-scan work but the §6.4 widening / type-expansion
    /// paths become reachable for more line shapes (RFC0001.2's
    /// degenerate-template guard test, for instance, runs with
    /// `prefix_depth = 0`).
    ///
    /// The default is `2` per the Drain paper §3.2 (Drain3's
    /// `depth = 4` total → 2 prefix-token levels). RFC 0001 §6.1
    /// names `~8` as the realistic ceiling; values above
    /// [`PREFIX_DEPTH_CEILING`] are rejected by
    /// [`MinerConfig::with_prefix_depth`].
    ///
    /// Stored as [`u8`] because the realistic ceiling is small and
    /// the cluster up-casts to [`usize`] at the tree-call boundary;
    /// the type rejects nonsensical "tens of thousands" values at
    /// the API surface.
    pub prefix_depth: u8,
}

/// Per-`CLAUDE.md` §3.2: the project ceiling on configurable
/// parameter byte limits. Above this requires an RFC.
pub const PARAM_BYTE_LIMIT_CEILING: u32 = 1024;

/// Per-RFC 0001 §6.1: the realistic ceiling on the Drain prefix
/// depth. Above this requires an RFC.
pub const PREFIX_DEPTH_CEILING: u8 = 8;

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
    /// The supplied prefix depth exceeds the RFC 0001 §6.1
    /// ceiling of [`PREFIX_DEPTH_CEILING`].
    PrefixDepthTooLarge(u8),
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
            Self::PrefixDepthTooLarge(v) => {
                write!(
                    f,
                    "prefix_depth exceeds the RFC 0001 §6.1 ceiling of {PREFIX_DEPTH_CEILING} (got {v})",
                )
            }
        }
    }
}

impl Error for MinerConfigError {}

impl Default for MinerConfig {
    /// RFC 0001 §3.1.1 (`threshold = 0.7`), §6.3
    /// (`floor = 0.4`), §3.2.1 (`param_byte_limit = 256`), and
    /// §6.2 step 3 (`prefix_depth = 2` per the Drain paper §3.2).
    fn default() -> Self {
        Self {
            similarity_threshold: 0.7,
            similarity_floor: 0.4,
            param_byte_limit: 256,
            prefix_depth: 2,
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
    /// `similarity_floor` defaults to the RFC §6.3 value of
    /// `0.4` when the supplied threshold permits it
    /// (`threshold ≥ 0.4`); for sub-`0.4` thresholds the floor
    /// degrades to the threshold itself (collapsing the lossy
    /// zone — the smallest valid configuration). Callers that
    /// need an explicit floor go through
    /// [`MinerConfig::try_new_full`].
    ///
    /// At `threshold = 0.7` (project default), `try_new` and
    /// [`MinerConfig::default`] produce the same triple
    /// `(0.7, 0.4, byte_limit)`.
    ///
    /// # Errors
    ///
    /// - [`MinerConfigError::ThresholdOutOfRange`] when
    ///   `threshold` is not in `(0, 1]`.
    /// - [`MinerConfigError::ParamByteLimitTooLarge`] when
    ///   `byte_limit` exceeds [`PARAM_BYTE_LIMIT_CEILING`].
    pub fn try_new(threshold: f32, byte_limit: u32) -> Result<Self, MinerConfigError> {
        Self::try_new_full(threshold, threshold.min(0.4), byte_limit)
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
            // Picks up `MinerConfig::default()`'s prefix_depth (2,
            // per the Drain paper). Override with
            // [`Self::with_prefix_depth`].
            prefix_depth: Self::default().prefix_depth,
        })
    }

    /// Return a copy of `self` with [`prefix_depth`][Self::prefix_depth]
    /// replaced. Validates against the RFC 0001 §6.1 ceiling
    /// [`PREFIX_DEPTH_CEILING`].
    ///
    /// # Errors
    ///
    /// [`MinerConfigError::PrefixDepthTooLarge`] when `depth >
    /// PREFIX_DEPTH_CEILING`.
    pub fn with_prefix_depth(mut self, depth: u8) -> Result<Self, MinerConfigError> {
        if depth > PREFIX_DEPTH_CEILING {
            return Err(MinerConfigError::PrefixDepthTooLarge(depth));
        }
        self.prefix_depth = depth;
        Ok(self)
    }
}
