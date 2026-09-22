//! RFC 0052 §3.2's housekeeping pass in its two halves, and the
//! values that travel between them.
//!
//! The ledger half runs under the WAL's single-writer position and
//! does no I/O at all: it applies horizons, derives the floor and pops
//! at most the cap's worth of work. The file half — the `RECLAIM` slot
//! write, the unlinks and the parent fsync — runs on the plan's owned
//! paths with no guard and no WAL handle, which is what keeps an
//! append from waiting on an fsync.
//!
//! Nothing a pass has touched can become undiscoverable: a popped
//! entry stays in the ledger and in the byte accounting, marked
//! reclaiming, until its deletion is verified.

use std::collections::BTreeMap;
use std::path::PathBuf;

use ourios_core::tenant::TenantId;
use uuid::Uuid;

use crate::retain::{RetainFloor, SnapshotHorizons};
use crate::{CheckpointError, HousekeepingError, WalOffset, reclaim, sync_parent_dir};

/// What one pass did, and what it still owes (RFC 0052 §3.7).
/// Carried on `Err` too, so partial work, floor and lag stay
/// observable on the failure path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HousekeepingProgress {
    pub removed_segments: usize,
    pub removed_partials: usize,
    /// Either half hit its budget: "more to do" rather than "backlog
    /// drained".
    pub capped: bool,
    /// Segments a received horizon has not yet been applied to,
    /// summed over tenants.
    pub horizon_remaining: usize,
    /// Empty-set segments not yet popped, plus entries reclaiming or
    /// uncertain.
    pub unlink_remaining: usize,
    pub floor: RetainFloor,
    pub lag_bytes: u64,
    pub lag_segments: usize,
    /// Whether this pass planned segments at all, and why not when it
    /// did not. A skipped pass still sweeps partials.
    pub outcome: PassOutcome,
}

/// Whether a pass planned segments (RFC 0052 §3.2). The reason rides
/// this value rather than a separate flag, so "skipped with no reason"
/// is not a state the type can hold; §3.5's cadence counter carries it
/// as `error.type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PassOutcome {
    Planned,
    Skipped(SkipReason),
}

/// Why a pass planned no segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// The root has no checkpoint at all yet.
    NoCheckpoint,
    /// §3.2's migration window: the root still carries a version-1
    /// `CHECKPOINT`, or its witness is not yet durable. Unlinking here
    /// would leave exactly the shape the open-time matrix reads as
    /// "nothing was ever reclaimed".
    MigrationWindow,
}

impl SkipReason {
    /// The `error.type` value §3.5's cadence counter carries.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoCheckpoint => "no_checkpoint",
            Self::MigrationWindow => "migration_window",
        }
    }
}

/// One segment the ledger half popped: the record's view of it and the
/// file the unlink targets.
#[derive(Debug, Clone)]
pub struct PlannedSegment {
    pub unlink: reclaim::PlannedUnlink,
    pub path: PathBuf,
}

/// Everything the file half needs, owned, so it holds no guard and no
/// WAL handle (RFC 0052 §3.7).
#[derive(Debug, Clone)]
pub struct ReclaimPlan {
    pub segments: Vec<PlannedSegment>,
    pub partials: Vec<PathBuf>,
    /// The merged record: this pass's `planned` list, the horizons it
    /// reclaims under and the mode it adopted. `None` when the pass
    /// has nothing to record — a skipped pass writes no record.
    pub record: Option<reclaim::ReclaimRecord>,
    pub root: PathBuf,
    pub progress: HousekeepingProgress,
}

/// What the file half did (RFC 0052 §3.7).
#[derive(Debug)]
pub enum ReclaimOutcome {
    /// The record write or its fsync failed, so nothing was unlinked.
    RecordFailed(std::io::Error),
    Unlinked {
        /// `unlink` returned `Ok` — or found the file already gone,
        /// which completes a previous pass's reclamation.
        removed: Vec<PathBuf>,
        failed: Vec<(PathBuf, std::io::Error)>,
        /// The one parent fsync after the unlinks failed, so **every**
        /// path in `removed` is an uncertain deletion (§3.2).
        fsync_failed: bool,
    },
}

/// The single object-safe error the reclamation surface answers with
/// (RFC 0052 §3.7). A `Box<dyn Journal>` cannot infer an associated
/// error type, which is why one enum rather than the concrete WAL's
/// two.
#[derive(Debug)]
pub enum ReclaimError {
    Checkpoint(CheckpointError),
    Housekeeping {
        /// Boxed because the progress is the larger half of the value
        /// and every success path returns it by itself.
        progress: Box<HousekeepingProgress>,
        source: HousekeepingError,
    },
}

impl std::fmt::Display for ReclaimError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Checkpoint(source) => write!(f, "{source}"),
            Self::Housekeeping { source, .. } => write!(f, "{source}"),
        }
    }
}

impl std::error::Error for ReclaimError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Checkpoint(source) => Some(source),
            Self::Housekeeping { source, .. } => Some(source),
        }
    }
}

impl From<CheckpointError> for ReclaimError {
    fn from(e: CheckpointError) -> Self {
        Self::Checkpoint(e)
    }
}

/// RFC 0052 §3.2's unlink half, on the plan's **owned paths**: it
/// takes no guard and no WAL handle, which is the whole point. The
/// record write precedes it (`Wal::write_plan_record`), and
/// `Wal::housekeeping_commit` folds this result back under the writer
/// position afterwards.
///
/// Partials go first and segments after, so a pass sharing one cap
/// between them spends it in §3.2's order. One parent fsync covers
/// every unlink of the pass, which is why its failure marks **every**
/// removed path uncertain rather than the last one.
#[must_use]
pub fn unlink_planned(plan: &ReclaimPlan) -> ReclaimOutcome {
    let mut removed = Vec::new();
    let mut failed = Vec::new();
    for path in plan
        .partials
        .iter()
        .chain(plan.segments.iter().map(|s| &s.path))
    {
        match std::fs::remove_file(path) {
            // Already gone is not the same as durably gone, but it is
            // the re-verification of a previous pass's uncertain
            // deletion: the fsync below is what completes it.
            Ok(()) => removed.push(path.clone()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => removed.push(path.clone()),
            Err(source) => failed.push((path.clone(), source)),
        }
    }
    let fsync_failed = !removed.is_empty() && sync_parent_dir(&plan.root).is_err();
    ReclaimOutcome::Unlinked {
        removed,
        failed,
        fsync_failed,
    }
}

/// The mode a pass runs under, from what the caller knows.
pub(crate) fn entry_mode(horizons: &SnapshotHorizons) -> reclaim::EntryMode {
    match horizons {
        SnapshotHorizons::NoConsumer => reclaim::EntryMode::NoConsumer,
        SnapshotHorizons::Known(_) => reclaim::EntryMode::Known,
    }
}

pub(crate) fn recorded_mode(mode: reclaim::EntryMode) -> reclaim::RecordedMode {
    match mode {
        reclaim::EntryMode::Known => reclaim::RecordedMode::Known,
        reclaim::EntryMode::NoConsumer => reclaim::RecordedMode::NoConsumer,
    }
}

pub(crate) fn mode_name(mode: reclaim::RecordedMode) -> &'static str {
    match mode {
        reclaim::RecordedMode::Unrecorded => "unrecorded",
        reclaim::RecordedMode::Known => "Known",
        reclaim::RecordedMode::NoConsumer => "NoConsumer",
    }
}

/// The `planned` entry for one popped segment, assigning a slot id to
/// every tenant it names. Ids are never renumbered and never reused
/// while the tenant is recorded, so `assign` is idempotent per tenant.
pub(crate) fn plan_entry(
    dictionary: &mut reclaim::Dictionary,
    geometry: reclaim::Geometry,
    segment: Uuid,
    uncertain: bool,
    members: &[(TenantId, WalOffset)],
) -> Result<reclaim::PlannedUnlink, reclaim::DictionaryFull> {
    let mut last_offsets = BTreeMap::new();
    for (tenant, offset) in members {
        let id = match dictionary.id_of(tenant) {
            Some(id) => id,
            None => dictionary.assign(tenant, geometry)?,
        };
        last_offsets.insert(id, *offset);
    }
    Ok(reclaim::PlannedUnlink {
        segment,
        uncertain,
        last_offsets,
    })
}
