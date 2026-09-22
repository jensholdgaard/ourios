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
use crate::{CheckpointError, HousekeepingError, WalOffset, reclaim, segment, sync_parent_dir};

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

/// One segment the ledger half popped: its identity, the horizon each
/// tenant is to be reclaimed under, and the file the unlink targets.
///
/// Identity and path are both here because they answer different
/// questions. The uuid is what the record names and what the unlink
/// **verifies** before removing anything; the path is where the file
/// was when the ledger last listed it, which an operator can change
/// underneath a pass.
#[derive(Debug, Clone)]
pub struct PlannedSegment {
    pub segment: Uuid,
    /// §3.2's uncertain deletion, carried from a previous pass whose
    /// parent fsync failed: for these, and only these, an `unlink`
    /// that finds the file gone completes the reclamation.
    pub uncertain: bool,
    pub last_offsets: Vec<(TenantId, WalOffset)>,
    pub path: PathBuf,
}

/// Which `housekeeping_prepare` produced a plan. The WAL mints one per
/// pass and keeps it beside the outstanding state, so a plan a later
/// prepare superseded can be told from the live one — §3.7's
/// abandoned-plan recovery re-plans under a new id, and a horizon that
/// regressed in between can have withdrawn exactly the segments the
/// old plan names.
///
/// The value is the WAL's own, with no constructor outside this crate:
/// a plan is something a pass hands out, never something a caller
/// builds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PassId(u64);

impl PassId {
    pub(crate) fn new(pass: u64) -> Self {
        Self(pass)
    }
}

impl std::fmt::Display for PassId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Everything the file half needs, owned, so it holds no guard and no
/// WAL handle (RFC 0052 §3.7).
#[derive(Debug, Clone)]
pub struct ReclaimPlan {
    /// The pass that produced this plan (§3.7).
    pub pass: PassId,
    pub segments: Vec<PlannedSegment>,
    pub partials: Vec<PathBuf>,
    /// Whether this pass owes a record write at all. §3.2 gates the
    /// record write with the segment planning: a record written under
    /// a version-1 checkpoint witnesses a reclamation that never
    /// happened.
    pub records: bool,
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
    // Debris first, then segments — §3.2's order for a shared cap.
    // Debris has no identity to check: a `.wal.partial` is a file no
    // reader ever depended on, so "already gone" simply completes one.
    let partials = plan
        .partials
        .iter()
        .map(|path| (path, unlink_partial(path)));
    let segments = plan
        .segments
        .iter()
        .map(|segment| (&segment.path, unlink_segment(segment)));
    let mut removed = Vec::new();
    let mut failed = Vec::new();
    for (path, outcome) in partials.chain(segments) {
        match outcome {
            Ok(()) => removed.push(path.clone()),
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

/// Remove one stale `<uuid>.wal.partial`. An unlink that finds it gone
/// completes a previous pass's removal, which is what makes §3.2's
/// uncertain deletion verifiable.
fn unlink_partial(path: &std::path::Path) -> Result<(), std::io::Error> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Remove one planned segment, **by identity rather than by path**.
///
/// The ledger records where a segment was when it was last listed, and
/// an operator can move it between then and here. Two shapes follow:
///
/// - the path now holds a *different* segment — unlinking it would
///   destroy frames nothing planned;
/// - the path holds nothing at all, while the segment survives under
///   its new name — counting that as removed would raise
///   `reclaimed_through` over frames still on disk, which is the one
///   thing the record must never do.
///
/// So the header is read first and a mismatch is a failure to retry.
/// `NotFound` completes only a deletion a previous pass already made
/// and could not verify (§3.2's uncertain case); on a first attempt it
/// is the renamed-survivor shape and is retried, which the next open
/// resolves by uuid when it rebuilds the ledger from the directory.
fn unlink_segment(segment: &PlannedSegment) -> Result<(), std::io::Error> {
    match segment_identity(&segment.path) {
        Ok(Some(found)) if found == segment.segment => std::fs::remove_file(&segment.path),
        Ok(Some(found)) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "{} now holds segment {found}, not the planned {}",
                segment.path.display(),
                segment.segment,
            ),
        )),
        Ok(None) if segment.uncertain => Ok(()),
        Ok(None) => Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "{} is gone but this pass never unlinked it; segment {} may survive elsewhere",
                segment.path.display(),
                segment.segment,
            ),
        )),
        Err(source) => Err(source),
    }
}

/// The uuid in a segment file's header, or `None` when the file is not
/// there at all.
fn segment_identity(path: &std::path::Path) -> Result<Option<Uuid>, std::io::Error> {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    segment::read_header(&mut file)
        .map(|header| Some(header.segment_uuid))
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))
}

/// The unlink failures of one pass, as the error its caller sees.
///
/// One error stands for the set: every failed path stays queued and is
/// retried together on the next tick, so the count beside the first is
/// what tells a single stuck file from a failing volume. A pass whose
/// only trouble was the parent fsync is **not** one of these: §3.2
/// reads that as the uncertain deletion it defines, re-verified by the
/// next pass rather than retried as a failure.
pub(crate) fn unlink_failure(outcome: &ReclaimOutcome) -> Option<HousekeepingError> {
    let ReclaimOutcome::Unlinked { failed, .. } = outcome else {
        return None;
    };
    let (path, source) = failed.first()?;
    Some(HousekeepingError::Io {
        op: "unlink(planned path)",
        source: std::io::Error::new(
            source.kind(),
            format!(
                "{} of this pass's unlinks failed, the first at {}: {source}",
                failed.len(),
                path.display(),
            ),
        ),
    })
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
