//! Per-tenant template-tree snapshot format + v1 recovery
//! (RFC 0001 §6.9, §3.5.1 / §3.5.2).
//!
//! A snapshot is a **rebuildable recovery-acceleration cache, not
//! durable state** — the WAL is the durable truth (`[CLAUDE.md
//! §3.4]`). It exists only to shorten cold-start replay; a lost,
//! absent, or corrupt snapshot is never a data-loss event, it
//! degrades to a full WAL replay.
//!
//! # Format
//!
//! Byte 0 is the snapshot format version ([`SNAPSHOT_VERSION`]);
//! the remaining bytes are that version's serialised payload. The
//! reader dispatches on byte 0 — the version byte is what makes
//! format evolution safe (RFC 0001 §6.9 *Migration*). The concrete
//! payload codec (here: `serde_json` over [`SnapshotState`]) is an
//! implementation detail *behind* the version byte; a future
//! version may change it without changing the framing.
//!
//! The payload captures the per-tenant state needed to reconstruct
//! the miner: the tree leaves (template token sequence,
//! `template_id`, `template_version`, the `(severity_number,
//! scope_name)` template key, and per-slot `slot_types`), the
//! structured-template-id map (§6.2 step-0 short-circuit), and the
//! WAL high-water mark.
//!
//! # v1 recovery — rebuild from a full replay; do not restore yet
//!
//! [`recover`] **ignores the snapshot payload and rebuilds the tree
//! from a full WAL replay in both branches.** The known-version
//! branch does **not** restore. This is a correctness constraint,
//! not a simplification: the restore-then-replay-the-tail path of
//! RFC 0001 §6.9 step (2) needs the RFC 0008 §6.7 checkpoint /
//! replay-from-offset API (`Wal::checkpoint`), which is an RFC 0008
//! red-gate stub. Restoring a tree from a snapshot and *then*
//! replaying the full WAL — the only replay available without
//! offset support — would **double-apply** every frame the snapshot
//! already captured, corrupting the tree. So until offset-resume
//! lands, recovery discards the snapshot payload and rebuilds from
//! the WAL. What lands now is the snapshot *format* (the leading
//! version byte and the recorded high-water mark) and the
//! version-dispatch + WAL-fallback contract; the restore path is
//! switched on, with no format change, once RFC 0008 §6.7 lands.

use ourios_core::audit::{ParamType, SlotTypes};
use serde::{Deserialize, Serialize};

use crate::tree::OwnedToken;

/// Snapshot format version written as byte 0 of every artefact
/// (RFC 0001 §6.9, §3.5.1). [`load_snapshot`] dispatches on this:
/// a matching byte 0 deserialises the payload; any other value is
/// an [`SnapshotError::UnknownVersion`] that recovery treats as
/// "discard and full-replay" (§3.5.2).
pub const SNAPSHOT_VERSION: u8 = 1;

/// One tenant's full snapshot payload (the bytes after the version
/// byte). Reconstructs the miner's per-tenant state on recovery.
///
/// `leaves` and `structured_templates` are `Vec`s (not maps) so the
/// serialised form is order-deterministic for a given build order;
/// the recovery path rebuilds the in-memory `HashMap`s from them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotState {
    /// Every `Body::String` leaf in the tenant's tree.
    pub leaves: Vec<LeafRecord>,
    /// The §6.2 step-0 structured-template-id map: each
    /// `(severity_number, scope_name)` tuple and the `template_id`
    /// allocated on its first observation. The
    /// `BodyKind::Structured` discriminator is implicit from the
    /// map's identity (RFC 0001 §6.1).
    pub structured_templates: Vec<StructuredTemplateRecord>,
    /// WAL high-water mark this snapshot's tree state reflects, or
    /// `None` if no offset was recorded. Captured so a future
    /// optimisation can resume replay from here (RFC 0008 §6.7)
    /// rather than from the start of the log; v1 does not use it.
    pub wal_high_water: Option<WalHighWater>,
}

/// Serialisable mirror of one tree [`crate::tree::Leaf`]. Carries
/// the §6.1 template-key fields and per-slot type sets — without
/// `(severity_number, scope_name)` two records sharing masked
/// tokens but differing in severity / scope would silently coalesce
/// on restore (H1.4 / H1.5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeafRecord {
    pub template: Vec<TokenRecord>,
    pub template_id: u64,
    pub template_version: u32,
    pub severity_number: u8,
    pub scope_name: Option<String>,
    /// One entry per `TokenRecord::Wildcard` in `template`, in
    /// wildcard-slot ordinal order. Each is the set of observed
    /// `ParamType`s for that slot (RFC 0001 §6.1).
    pub slot_types: Vec<Vec<ParamTypeRecord>>,
}

/// One `(severity_number, scope_name) → template_id` entry of the
/// structured-template-id map.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructuredTemplateRecord {
    pub severity_number: u8,
    pub scope_name: Option<String>,
    pub template_id: u64,
}

/// Serialisable mirror of [`crate::tree::OwnedToken`]. The tree type
/// is kept serde-free (it is on the ingest hot path and its derive
/// surface is the algorithm's, not the codec's); this view exists
/// purely so the snapshot codec lives entirely inside this module.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TokenRecord {
    Fixed(String),
    Wildcard,
}

impl From<&OwnedToken> for TokenRecord {
    fn from(t: &OwnedToken) -> Self {
        match t {
            OwnedToken::Fixed(s) => Self::Fixed(s.clone()),
            OwnedToken::Wildcard => Self::Wildcard,
        }
    }
}

impl From<&TokenRecord> for OwnedToken {
    fn from(t: &TokenRecord) -> Self {
        match t {
            TokenRecord::Fixed(s) => Self::Fixed(s.clone()),
            TokenRecord::Wildcard => Self::Wildcard,
        }
    }
}

/// Serialisable mirror of [`ourios_core::audit::ParamType`]. The
/// core type has no serde derive and a private bit layout; this
/// view keeps the snapshot codec self-contained and stable against
/// the bitset representation. `Unknown(i32)` is carried verbatim so
/// a reader-side catch-all ordinal round-trips.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ParamTypeRecord {
    Ip,
    Uuid,
    Num,
    Hex,
    Ts,
    Path,
    Str,
    Overflow,
    Unknown(i32),
}

impl From<ParamType> for ParamTypeRecord {
    fn from(t: ParamType) -> Self {
        match t {
            ParamType::Ip => Self::Ip,
            ParamType::Uuid => Self::Uuid,
            ParamType::Num => Self::Num,
            ParamType::Hex => Self::Hex,
            ParamType::Ts => Self::Ts,
            ParamType::Path => Self::Path,
            ParamType::Str => Self::Str,
            ParamType::Overflow => Self::Overflow,
            ParamType::Unknown(n) => Self::Unknown(n),
        }
    }
}

impl From<ParamTypeRecord> for ParamType {
    fn from(t: ParamTypeRecord) -> Self {
        match t {
            ParamTypeRecord::Ip => Self::Ip,
            ParamTypeRecord::Uuid => Self::Uuid,
            ParamTypeRecord::Num => Self::Num,
            ParamTypeRecord::Hex => Self::Hex,
            ParamTypeRecord::Ts => Self::Ts,
            ParamTypeRecord::Path => Self::Path,
            ParamTypeRecord::Str => Self::Str,
            ParamTypeRecord::Overflow => Self::Overflow,
            ParamTypeRecord::Unknown(n) => Self::Unknown(n),
        }
    }
}

/// Serialise one [`SlotTypes`] bitset as the ordered set of its
/// members. The core type's byte layout is private (no `bits()`
/// accessor), so the stable view is its `iter()` order — which is
/// also the canonical `ParamType` declaration order, so two equal
/// sets always serialise identically.
fn slot_types_to_record(s: SlotTypes) -> Vec<ParamTypeRecord> {
    s.iter().map(ParamTypeRecord::from).collect()
}

/// Rebuild a [`SlotTypes`] bitset from its serialised member list.
/// `SlotTypes::insert` is idempotent and order-insensitive, so the
/// result is independent of the list order.
fn slot_types_from_record(types: &[ParamTypeRecord]) -> SlotTypes {
    types
        .iter()
        .fold(SlotTypes::new(), |acc, t| acc.insert(ParamType::from(*t)))
}

/// WAL high-water mark, mirroring `ourios_wal::WalOffset`'s
/// `(segment: Uuid, byte)` shape without depending on `ourios-wal`
/// or `uuid` from the miner crate. The segment id is carried as its
/// textual form; the miner only needs to record and round-trip it,
/// not to compare WAL offsets, so a string is sufficient.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalHighWater {
    pub segment: String,
    pub byte: u64,
}

/// Errors from [`load_snapshot`]. Enum-carried states (no panicking
/// accessors on the recovery path): every malformed input is a
/// typed variant the caller dispatches on, and [`recover`] maps all
/// of them to the same "discard, full-replay" outcome.
#[derive(Debug)]
#[non_exhaustive]
pub enum SnapshotError {
    /// Byte 0 is a version this build does not understand. Recovery
    /// rejects the artefact and falls back to full WAL replay
    /// (§3.5.2) rather than misinterpreting the payload bytes.
    UnknownVersion(u8),
    /// Byte 0 matched [`SNAPSHOT_VERSION`] but the payload did not
    /// deserialise. Carries the decoder's message for diagnostics.
    Corrupt(String),
    /// The artefact was empty — no version byte to dispatch on.
    Empty,
}

impl std::fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownVersion(v) => {
                write!(f, "unknown snapshot version byte {v:#04x}")
            }
            Self::Corrupt(detail) => write!(f, "corrupt snapshot payload: {detail}"),
            Self::Empty => f.write_str("empty snapshot artefact (no version byte)"),
        }
    }
}

impl std::error::Error for SnapshotError {}

/// Serialise one tenant's [`SnapshotState`] into the wire artefact:
/// `[SNAPSHOT_VERSION][payload]` (RFC 0001 §6.9, §3.5.1). Byte 0 is
/// always [`SNAPSHOT_VERSION`].
#[must_use]
pub fn snapshot(state: &SnapshotState) -> Vec<u8> {
    // The leaf / param mirror types are infallible to serialise
    // (plain owned data, no maps with non-string keys), so
    // `serde_json::to_vec` cannot fail here; fall back to an empty
    // payload rather than panicking on the unreachable error so the
    // recovery path stays panic-free end to end (a v1 reader treats
    // a too-short / unparseable artefact as "discard, full-replay").
    let mut out = Vec::new();
    out.push(SNAPSHOT_VERSION);
    if let Ok(payload) = serde_json::to_vec(state) {
        out.extend_from_slice(&payload);
    }
    out
}

/// Read the version byte and, when it matches [`SNAPSHOT_VERSION`],
/// deserialise the payload (RFC 0001 §6.9 recovery step). Does
/// **not** decide whether to use the result — [`recover`] owns the
/// v1 "discard and full-replay regardless" policy. This function is
/// the version-dispatch surface §3.5.2 exercises.
///
/// # Errors
///
/// - [`SnapshotError::Empty`] when `bytes` is empty.
/// - [`SnapshotError::UnknownVersion`] when byte 0 is not
///   [`SNAPSHOT_VERSION`].
/// - [`SnapshotError::Corrupt`] when byte 0 matches but the payload
///   does not deserialise.
pub fn load_snapshot(bytes: &[u8]) -> Result<SnapshotState, SnapshotError> {
    match bytes.split_first() {
        None => Err(SnapshotError::Empty),
        Some((&SNAPSHOT_VERSION, payload)) => {
            serde_json::from_slice(payload).map_err(|e| SnapshotError::Corrupt(e.to_string()))
        }
        Some((&other, _)) => Err(SnapshotError::UnknownVersion(other)),
    }
}

/// Outcome of a v1 recovery: which fallback path produced the tree,
/// for snapshot-load telemetry (RFC 0001 §6.9 *Snapshot-load
/// telemetry*). The tree itself always comes from `rebuild` in v1;
/// this only records *why* the snapshot did not short-circuit it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RecoveryOutcome {
    /// No snapshot artefact was supplied (cold start, or the cache
    /// file was absent).
    NoSnapshot,
    /// A snapshot was supplied and its version byte was known. In
    /// v1 the payload is still discarded and the tree is rebuilt
    /// from the WAL (the restore path is gated on RFC 0008 §6.7).
    KnownVersionDiscarded,
    /// A snapshot was supplied but its version byte was unknown or
    /// its payload was corrupt; it was discarded (§3.5.2).
    UnknownOrCorruptDiscarded,
}

/// Recover one tenant's tree state on ingester restart (RFC 0001
/// §6.9 recovery algorithm).
///
/// **v1 behaviour: always rebuild from a full WAL replay; never
/// restore from the snapshot.** `rebuild` is the caller-supplied
/// full-replay closure — in production the ingester's
/// `Wal::replay`-driven path that re-runs `OtlpBatch` frames back
/// through the miner from the start of the log. This function still
/// calls [`load_snapshot`] to exercise and observe the
/// version-dispatch (known → [`RecoveryOutcome::KnownVersionDiscarded`],
/// unknown / corrupt / empty → [`RecoveryOutcome::UnknownOrCorruptDiscarded`]),
/// but the dispatch result is **not** used to skip the replay.
///
/// The miner crate deliberately does not depend on `ourios-wal`:
/// the `OtlpBatch`-decode + tenant-fan-out + miner-ingest pipeline
/// the full replay drives lives in the ingester (`ourios-ingester`),
/// and the snapshot *format* + version-dispatch + WAL-fallback
/// *decision* are what this slice owns. Passing the replay in as a
/// closure keeps both on the correct side of the crate boundary.
///
/// The known-version restore path (deserialise the payload, restore
/// the tree, then replay only the WAL tail from `wal_high_water`)
/// activates — with no format change — once the RFC 0008 §6.7
/// offset-resume API lands. Restoring here and *then* full-replaying
/// would double-apply every captured frame and corrupt the tree, so
/// v1 must discard the payload in both branches.
pub fn recover<T, F>(snapshot_bytes: Option<&[u8]>, rebuild: F) -> (T, RecoveryOutcome)
where
    F: FnOnce() -> T,
{
    let outcome = match snapshot_bytes {
        None => RecoveryOutcome::NoSnapshot,
        Some(bytes) => match load_snapshot(bytes) {
            Ok(_state) => RecoveryOutcome::KnownVersionDiscarded,
            Err(_e) => RecoveryOutcome::UnknownOrCorruptDiscarded,
        },
    };
    (rebuild(), outcome)
}

/// Convert a [`SlotTypes`] vector (the leaf's per-slot type sets)
/// into the serialisable form.
#[must_use]
pub fn slot_types_vec_to_record(slot_types: &[SlotTypes]) -> Vec<Vec<ParamTypeRecord>> {
    slot_types
        .iter()
        .copied()
        .map(slot_types_to_record)
        .collect()
}

/// Inverse of [`slot_types_vec_to_record`].
#[must_use]
pub fn slot_types_vec_from_record(records: &[Vec<ParamTypeRecord>]) -> Vec<SlotTypes> {
    records.iter().map(|r| slot_types_from_record(r)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_state() -> SnapshotState {
        SnapshotState {
            leaves: vec![
                LeafRecord {
                    template: vec![
                        TokenRecord::Fixed("user".to_string()),
                        TokenRecord::Wildcard,
                        TokenRecord::Fixed("logged".to_string()),
                        TokenRecord::Fixed("in".to_string()),
                    ],
                    template_id: 1,
                    template_version: 2,
                    severity_number: 9,
                    scope_name: Some("lib.auth".to_string()),
                    slot_types: vec![vec![ParamTypeRecord::Num, ParamTypeRecord::Str]],
                },
                LeafRecord {
                    template: vec![
                        TokenRecord::Fixed("GET".to_string()),
                        TokenRecord::Fixed("/home".to_string()),
                    ],
                    template_id: 2,
                    template_version: 1,
                    severity_number: 0,
                    scope_name: None,
                    slot_types: vec![],
                },
            ],
            structured_templates: vec![StructuredTemplateRecord {
                severity_number: 17,
                scope_name: Some("lib.payments".to_string()),
                template_id: 3,
            }],
            wal_high_water: Some(WalHighWater {
                segment: "0190b3c8-1a2b-7c3d-9e4f-50607080a0b0".to_string(),
                byte: 4096,
            }),
        }
    }

    #[test]
    fn snapshot_leading_byte_is_version() {
        // Arrange
        let state = sample_state();

        // Act
        let bytes = snapshot(&state);

        // Assert — §3.5.1: byte 0 is the format version.
        assert_eq!(bytes[0], SNAPSHOT_VERSION);
    }

    #[test]
    fn snapshot_round_trips_to_equal_state() {
        // Arrange
        let state = sample_state();

        // Act
        let bytes = snapshot(&state);
        let restored = load_snapshot(&bytes).expect("known version deserialises");

        // Assert — the format deserialises to an equal state.
        assert_eq!(restored, state);
    }

    #[test]
    fn load_snapshot_rejects_unknown_version() {
        // Arrange — a valid snapshot with byte 0 corrupted to an
        // unknown version.
        let mut bytes = snapshot(&sample_state());
        bytes[0] = 0xFF;

        // Act
        let err = load_snapshot(&bytes).expect_err("unknown version must error");

        // Assert
        assert!(matches!(err, SnapshotError::UnknownVersion(0xFF)));
    }

    #[test]
    fn load_snapshot_rejects_empty_input() {
        // Arrange + Act
        let err = load_snapshot(&[]).expect_err("empty input must error");

        // Assert
        assert!(matches!(err, SnapshotError::Empty));
    }

    #[test]
    fn load_snapshot_rejects_corrupt_payload() {
        // Arrange — correct version byte, garbage payload.
        let bytes = [SNAPSHOT_VERSION, 0x7B, 0x21, 0x21];

        // Act
        let err = load_snapshot(&bytes).expect_err("corrupt payload must error");

        // Assert
        assert!(matches!(err, SnapshotError::Corrupt(_)));
    }

    #[test]
    fn recover_with_no_snapshot_rebuilds_and_reports_no_snapshot() {
        // Arrange + Act — v1 always rebuilds via the closure.
        let (tree, outcome) = recover(None, || 42u32);

        // Assert
        assert_eq!(tree, 42);
        assert_eq!(outcome, RecoveryOutcome::NoSnapshot);
    }

    #[test]
    fn recover_with_known_version_still_rebuilds_from_closure() {
        // Arrange — a well-formed snapshot. v1 must NOT restore from
        // it: the rebuild closure's value is what comes back, and the
        // outcome records that the known-version payload was discarded.
        let bytes = snapshot(&sample_state());

        // Act
        let (tree, outcome) = recover(Some(&bytes), || 7u32);

        // Assert — rebuilt value, not anything derived from the snapshot.
        assert_eq!(tree, 7);
        assert_eq!(outcome, RecoveryOutcome::KnownVersionDiscarded);
    }

    #[test]
    fn recover_with_unknown_version_discards_and_rebuilds() {
        // Arrange — unknown version byte.
        let mut bytes = snapshot(&sample_state());
        bytes[0] = 0xFF;

        // Act
        let (tree, outcome) = recover(Some(&bytes), || 9u32);

        // Assert — the stale snapshot is discarded; the tree comes
        // from the rebuild closure (the WAL, in production).
        assert_eq!(tree, 9);
        assert_eq!(outcome, RecoveryOutcome::UnknownOrCorruptDiscarded);
    }

    #[test]
    fn slot_types_record_round_trips() {
        // Arrange — a multi-member slot set.
        let original = SlotTypes::singleton(ParamType::Num).insert(ParamType::Str);

        // Act
        let record = slot_types_to_record(original);
        let restored = slot_types_from_record(&record);

        // Assert
        assert_eq!(restored, original);
    }
}
