//! Per-tenant template-tree snapshot format + recovery dispatch
//! (RFC 0001 §6.9, §3.5.1 / §3.5.2).
//!
//! A snapshot is a **rebuildable recovery-acceleration cache, not
//! durable state** — the WAL is the durable truth
//! (`CLAUDE.md §3.4`). It exists only to shorten cold-start replay; a lost,
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
//! # Recovery — restore active per the §6.9 v2 amendment
//!
//! [`recover`] dispatches on the version byte: a known-version
//! snapshot deserialises and is **returned for restore** (RFC 0001
//! §6.9 step 2, switched on by the 2026-06-12 v2 amendment now that
//! the RFC 0008 §6.7 checkpoint + offset-carrying-sink API has
//! landed); an absent, unknown-version, or corrupt artefact yields
//! `None` and the caller full-replays the WAL (step 3). The caller
//! — the ingester's recovery driver — restores the returned state
//! into the cluster and replays only the WAL tail above the
//! state's `wal_high_water` mark.
//!
//! Historical note: v1 deliberately refused to restore. Without the
//! RFC 0008 §6.7 offset-resume API the only replay available was the
//! full WAL, and restoring a tree and *then* full-replaying would
//! have double-applied every frame the snapshot already captured,
//! corrupting the tree. The v2 amendment resolves the hazard by
//! routing (per-consumer offset horizons), not by refusing.

use ourios_core::audit::{ParamType, Provenance, ProvenanceSet, SlotTypes};
use serde::{Deserialize, Serialize};

use crate::tree::OwnedToken;

/// Snapshot format version written as byte 0 of every artefact
/// (RFC 0001 §6.9, §3.5.1). [`load_snapshot`] dispatches on this:
/// a matching byte 0 deserialises the payload; any other value is
/// a [`SnapshotError::UnknownVersion`] that recovery treats as
/// "discard and full-replay" (§3.5.2).
pub const SNAPSHOT_VERSION: u8 = 1;

/// One tenant's full snapshot payload (the bytes after the version
/// byte) — the per-tenant state a restore would rebuild the miner
/// from.
///
/// `leaves` and `structured_templates` are `Vec`s (not maps) so the
/// serialised form is order-deterministic for a given build order.
/// `MinerCluster::restore_tenant` rebuilds the in-memory tree and
/// maps from these on the known-version recovery path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotState {
    /// Every `Body::String` leaf in the tenant's tree.
    pub leaves: Vec<LeafRecord>,
    /// The §6.2 step-0 structured-template-id map: each
    /// `(severity_number, scope_name, event_name)` tuple (RFC 0037
    /// §3.1) and the `template_id` allocated on its first
    /// observation. The `BodyKind::Structured` discriminator is
    /// implicit from the map's identity (RFC 0001 §6.1).
    pub structured_templates: Vec<StructuredTemplateRecord>,
    /// WAL high-water mark this snapshot's tree state reflects, or
    /// `None` if no offset was recorded. On the known-version
    /// recovery path the driver replays only the WAL tail above
    /// this mark (RFC 0008 §6.7 offset-resume).
    pub wal_high_water: Option<WalHighWater>,
    /// RFC 0050 §3.3 adopted-template map entries.
    /// `#[serde(default)]` — absent in pre-RFC snapshots, which had
    /// no adopted templates. No `SNAPSHOT_VERSION` bump (additive).
    #[serde(default)]
    pub adopted_templates: Vec<AdoptedTemplateRecord>,
}

/// Serialisable mirror of one adopted-template map entry
/// (RFC 0050 §3.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdoptedTemplateRecord {
    /// Canonical template shape — the map key's template half.
    pub canonical: String,
    pub severity_number: u8,
    pub scope_name: Option<String>,
    pub template_id: u64,
    pub template_version: u32,
    /// `true` = adoption-interned (owns its id, no tree leaf,
    /// counted against the RFC 0023 ceiling); `false` = rides a
    /// tree leaf, whose own `LeafRecord` carries the provenance.
    pub owned: bool,
    /// Owned entries only. Written non-empty by this version; an
    /// empty list on an owned entry restores as
    /// `{UpstreamDerived}` — the only origin an owned entry can
    /// have without ever having converged.
    #[serde(default)]
    pub provenance: Vec<ProvenanceRecord>,
    #[serde(default)]
    pub upstream_associations: Vec<String>,
    #[serde(default)]
    pub upstream_association_overflow: u64,
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
    /// RFC 0050 §3.3 provenance origins, in
    /// [`ProvenanceSet::iter`] order. `#[serde(default)]` so
    /// snapshots written before RFC 0050 restore — an empty list
    /// restores as `{Mined}`, the correct migration: every
    /// pre-RFC leaf was minted by the Drain walk. No
    /// `SNAPSHOT_VERSION` bump for this additive field (the
    /// `StructuredTemplateRecord::event_name` precedent).
    #[serde(default)]
    pub provenance: Vec<ProvenanceRecord>,
    /// RFC 0050 §3.2 `observe` associations: the stored upstream
    /// strings (lexicographic) and the overflow count. Absent in
    /// pre-RFC snapshots — defaults to none, which is what those
    /// trees had.
    #[serde(default)]
    pub upstream_associations: Vec<String>,
    #[serde(default)]
    pub upstream_association_overflow: u64,
}

/// One `(severity_number, scope_name, event_name) → template_id`
/// entry of the structured-template-id map (RFC 0037 §3.1).
///
/// `event_name` carries `#[serde(default)]` so snapshots written
/// before RFC 0037 (which keyed only on `(severity_number,
/// scope_name)`) restore with `event_name = None` — the correct
/// migration, since those templates were minted when the event
/// dimension was absent, which is exactly the `event_name = None`
/// class. No `SNAPSHOT_VERSION` bump is needed for this additive
/// field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructuredTemplateRecord {
    pub severity_number: u8,
    pub scope_name: Option<String>,
    #[serde(default)]
    pub event_name: Option<String>,
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

/// Serialisable mirror of [`ourios_core::audit::Provenance`] — the
/// core type keeps its bit layout private and serde-free, same as
/// [`SlotTypes`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProvenanceRecord {
    Mined,
    UpstreamDerived,
    ProducerDeclared,
}

impl From<Provenance> for ProvenanceRecord {
    fn from(p: Provenance) -> Self {
        match p {
            Provenance::Mined => Self::Mined,
            Provenance::UpstreamDerived => Self::UpstreamDerived,
            Provenance::ProducerDeclared => Self::ProducerDeclared,
        }
    }
}

impl From<ProvenanceRecord> for Provenance {
    fn from(p: ProvenanceRecord) -> Self {
        match p {
            ProvenanceRecord::Mined => Self::Mined,
            ProvenanceRecord::UpstreamDerived => Self::UpstreamDerived,
            ProvenanceRecord::ProducerDeclared => Self::ProducerDeclared,
        }
    }
}

/// [`ProvenanceSet`] → wire form, in the set's stable iteration
/// order.
pub(crate) fn provenance_set_to_record(s: ProvenanceSet) -> Vec<ProvenanceRecord> {
    s.iter().map(ProvenanceRecord::from).collect()
}

/// Wire form → [`ProvenanceSet`]. An empty list is a pre-RFC 0050
/// snapshot: restore as `{Mined}` (see [`LeafRecord::provenance`]).
pub(crate) fn record_to_provenance_set(records: &[ProvenanceRecord]) -> ProvenanceSet {
    if records.is_empty() {
        return ProvenanceSet::singleton(Provenance::Mined);
    }
    records.iter().map(|r| Provenance::from(*r)).collect()
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
    /// The payload failed to encode in [`snapshot`]. Not expected for
    /// the plain mirror types, but surfaced rather than silently
    /// writing a truncated artefact every reader would reject.
    Serialize(String),
}

impl std::fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownVersion(v) => {
                write!(f, "unknown snapshot version byte {v:#04x}")
            }
            Self::Corrupt(detail) => write!(f, "corrupt snapshot payload: {detail}"),
            Self::Empty => f.write_str("empty snapshot artefact (no version byte)"),
            Self::Serialize(detail) => write!(f, "snapshot payload failed to encode: {detail}"),
        }
    }
}

impl std::error::Error for SnapshotError {}

/// Serialise one tenant's [`SnapshotState`] into the wire artefact:
/// `[SNAPSHOT_VERSION][payload]` (RFC 0001 §6.9, §3.5.1). Byte 0 is
/// always [`SNAPSHOT_VERSION`].
///
/// # Errors
///
/// Returns [`SnapshotError::Serialize`] if the payload fails to
/// encode. The mirror types are plain owned data so this is not
/// expected in practice, but surfacing it beats writing a truncated
/// `[SNAPSHOT_VERSION]` artefact that every reader would reject as
/// corrupt, forcing perpetual full replays.
pub fn snapshot(state: &SnapshotState) -> Result<Vec<u8>, SnapshotError> {
    let payload = serde_json::to_vec(state).map_err(|e| SnapshotError::Serialize(e.to_string()))?;
    let mut out = Vec::with_capacity(payload.len() + 1);
    out.push(SNAPSHOT_VERSION);
    out.extend_from_slice(&payload);
    Ok(out)
}

/// Read the version byte and, when it matches [`SNAPSHOT_VERSION`],
/// deserialise the payload (RFC 0001 §6.9 recovery step). Does
/// **not** decide what to do with the result — [`recover`] owns the
/// restore-vs-discard dispatch. This function is the
/// version-dispatch surface §3.5.2 exercises.
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

/// Which recovery path ran, for snapshot-load telemetry (RFC 0001
/// §6.9 *Snapshot-load telemetry*): restore-then-tail-replay versus
/// the two full-replay fallbacks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RecoveryOutcome {
    /// No snapshot artefact was supplied (cold start, or the cache
    /// file was absent); the caller full-replays the WAL.
    NoSnapshot,
    /// A known-version snapshot deserialised; its state is returned
    /// for restore and the caller replays only the WAL tail above
    /// the state's recorded high-water mark (§6.9 step 2).
    Restored,
    /// A snapshot was supplied but its version byte was unknown or
    /// its payload was corrupt; it was discarded (§3.5.2) and the
    /// caller full-replays the WAL.
    UnknownOrCorruptDiscarded,
}

/// Recover one tenant's snapshot state on ingester restart (RFC 0001
/// §6.9 recovery algorithm, restore active per the 2026-06-12 v2
/// amendment).
///
/// Dispatch:
///
/// - `None` → `(None, NoSnapshot)`.
/// - Known version byte → `(Some(state), Restored)`.
/// - Unknown version, corrupt payload, or empty artefact →
///   `(None, UnknownOrCorruptDiscarded)`.
///
/// The caller — the ingester's recovery driver — restores a returned
/// state into the cluster (`MinerCluster::restore_tenant`) and
/// replays only the WAL tail above `state.wal_high_water`; on `None`
/// it full-replays the WAL. The miner crate deliberately does not
/// depend on `ourios-wal`: the replay pipeline lives in the
/// ingester, and the snapshot format + version-dispatch + restore
/// surface are what this crate owns.
#[must_use]
pub fn recover(snapshot_bytes: Option<&[u8]>) -> (Option<SnapshotState>, RecoveryOutcome) {
    match snapshot_bytes {
        None => (None, RecoveryOutcome::NoSnapshot),
        Some(bytes) => match load_snapshot(bytes) {
            Ok(state) => (Some(state), RecoveryOutcome::Restored),
            Err(_e) => (None, RecoveryOutcome::UnknownOrCorruptDiscarded),
        },
    }
}

/// Convert a [`SlotTypes`] vector (the leaf's per-slot type sets)
/// into the serialisable form.
#[must_use]
pub(crate) fn slot_types_vec_to_record(slot_types: &[SlotTypes]) -> Vec<Vec<ParamTypeRecord>> {
    slot_types
        .iter()
        .copied()
        .map(slot_types_to_record)
        .collect()
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
                    provenance: vec![ProvenanceRecord::Mined],
                    upstream_associations: vec!["user <*> logged in".to_string()],
                    upstream_association_overflow: 2,
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
                    provenance: vec![],
                    upstream_associations: vec![],
                    upstream_association_overflow: 0,
                },
            ],
            structured_templates: vec![StructuredTemplateRecord {
                severity_number: 17,
                scope_name: Some("lib.payments".to_string()),
                event_name: Some("gen_ai.client.inference.operation.details".to_string()),
                template_id: 3,
            }],
            wal_high_water: Some(WalHighWater {
                segment: "0190b3c8-1a2b-7c3d-9e4f-50607080a0b0".to_string(),
                byte: 4096,
            }),
            adopted_templates: vec![AdoptedTemplateRecord {
                canonical: "copy <*> done".to_string(),
                severity_number: 9,
                scope_name: None,
                template_id: 4,
                template_version: 1,
                owned: true,
                provenance: vec![ProvenanceRecord::UpstreamDerived],
                upstream_associations: vec!["copy <file> done".to_string()],
                upstream_association_overflow: 1,
            }],
        }
    }

    #[test]
    fn structured_template_record_without_event_name_restores_as_none() {
        // RFC 0037 §3.1 migration: a structured-template record written
        // before `event_name` was added has no such field. `#[serde(default)]`
        // must restore it as `None` — the correct reading, since those
        // templates were minted when the event dimension was absent. This
        // pins the backward-compat guarantee across future codec refactors.
        let pre_rfc0037 = r#"{"severity_number":17,"scope_name":"lib.payments","template_id":3}"#;
        let record: StructuredTemplateRecord =
            serde_json::from_str(pre_rfc0037).expect("pre-RFC0037 record must deserialize");
        assert_eq!(record.event_name, None);
        assert_eq!(record.severity_number, 17);
        assert_eq!(record.scope_name.as_deref(), Some("lib.payments"));
        assert_eq!(record.template_id, 3);
    }

    #[test]
    fn leaf_record_without_rfc0050_fields_restores_with_defaults() {
        // RFC 0050 §3.3 migration: a leaf record written before the
        // provenance / association fields existed must deserialize
        // with empty defaults — and the empty provenance list reads
        // back as `{Mined}` (every pre-RFC leaf was minted by the
        // Drain walk). Same shape as the RFC 0037 `event_name`
        // migration above; no `SNAPSHOT_VERSION` bump.
        let pre_rfc0050 = r#"{
            "template": [{"Fixed":"disk"},{"Fixed":"full"}],
            "template_id": 7,
            "template_version": 1,
            "severity_number": 0,
            "scope_name": null,
            "slot_types": []
        }"#;
        let record: LeafRecord =
            serde_json::from_str(pre_rfc0050).expect("pre-RFC0050 record must deserialize");
        assert!(record.provenance.is_empty());
        assert!(record.upstream_associations.is_empty());
        assert_eq!(record.upstream_association_overflow, 0);
        assert_eq!(
            record_to_provenance_set(&record.provenance),
            ProvenanceSet::singleton(Provenance::Mined),
        );
    }

    #[test]
    fn provenance_records_round_trip_the_set() {
        let set = ProvenanceSet::singleton(Provenance::Mined).insert(Provenance::UpstreamDerived);
        let records = provenance_set_to_record(set);
        assert_eq!(
            records,
            vec![ProvenanceRecord::Mined, ProvenanceRecord::UpstreamDerived],
        );
        assert_eq!(record_to_provenance_set(&records), set);
    }

    #[test]
    fn snapshot_leading_byte_is_version() {
        // Arrange
        let state = sample_state();

        // Act
        let bytes = snapshot(&state).expect("snapshot encodes");

        // Assert — §3.5.1: byte 0 is the format version.
        assert_eq!(bytes[0], SNAPSHOT_VERSION);
    }

    #[test]
    fn snapshot_round_trips_to_equal_state() {
        // Arrange
        let state = sample_state();

        // Act
        let bytes = snapshot(&state).expect("snapshot encodes");
        let restored = load_snapshot(&bytes).expect("known version deserialises");

        // Assert — the format deserialises to an equal state.
        assert_eq!(restored, state);
    }

    #[test]
    fn load_snapshot_rejects_unknown_version() {
        // Arrange — a valid snapshot with byte 0 corrupted to an
        // unknown version.
        let mut bytes = snapshot(&sample_state()).expect("snapshot encodes");
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
    fn recover_with_no_snapshot_reports_no_snapshot() {
        // Arrange + Act — no artefact: the caller full-replays.
        let (state, outcome) = recover(None);

        // Assert
        assert_eq!(state, None);
        assert_eq!(outcome, RecoveryOutcome::NoSnapshot);
    }

    /// Replaces `recover_with_known_version_still_rebuilds_from_
    /// closure`, which asserted the v1 discard contract (known
    /// version → payload discarded, full rebuild). That contract was
    /// retired by the RFC 0001 §6.9 v2 amendment (2026-06-12): with
    /// RFC 0008 §6.7 offset-resume landed, a known-version snapshot
    /// is returned for restore and the driver replays only the WAL
    /// tail above its high-water mark.
    #[test]
    fn recover_with_known_version_returns_the_state_for_restore() {
        // Arrange — a well-formed snapshot.
        let bytes = snapshot(&sample_state()).expect("snapshot encodes");

        // Act
        let (state, outcome) = recover(Some(&bytes));

        // Assert — the deserialised state comes back for restore.
        assert_eq!(state, Some(sample_state()));
        assert_eq!(outcome, RecoveryOutcome::Restored);
    }

    #[test]
    fn recover_with_unknown_version_discards() {
        // Arrange — unknown version byte.
        let mut bytes = snapshot(&sample_state()).expect("snapshot encodes");
        bytes[0] = 0xFF;

        // Act
        let (state, outcome) = recover(Some(&bytes));

        // Assert — the stale snapshot is discarded; the caller
        // rebuilds from the WAL (full replay, §3.5.2).
        assert_eq!(state, None);
        assert_eq!(outcome, RecoveryOutcome::UnknownOrCorruptDiscarded);
    }

    #[test]
    fn slot_types_to_record_captures_every_member() {
        // The serialisable form lists each member of the set (in
        // `SlotTypes::iter` / canonical `ParamType` order). The
        // restore-side inverse lives in `MinerCluster::restore_tenant`;
        // here we pin the forward encoding `snapshot` relies on.
        let record =
            slot_types_to_record(SlotTypes::singleton(ParamType::Num).insert(ParamType::Str));

        // Exact order, not just membership — the byte-determinism of the
        // snapshot rests on `SlotTypes::iter`'s canonical order.
        assert_eq!(record, vec![ParamTypeRecord::Num, ParamTypeRecord::Str]);
    }
}
