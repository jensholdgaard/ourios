//! RFC 0052 §3.2's housekeeping pass in its two halves, and the
//! values that travel between them.
//!
//! The ledger half runs under the WAL's single-writer position and
//! does no I/O at all: it applies horizons, derives the floor and pops
//! at most the cap's worth of work. The unlinks and the parent fsync —
//! [`unlink_planned`] — run on the plan's owned paths with no guard
//! and no WAL handle, which is what keeps an append from waiting on an
//! fsync.
//!
//! The `RECLAIM` slot write does **not** yet: `Wal::write_plan_record`
//! takes `&mut Wal` because §3.2 puts the *merge* off the writer
//! position while the record it merges into is the store's live one,
//! which a concurrent checkpoint also writes. A coordinator holding
//! the journal behind a mutex therefore holds it across that write.
//! Closing it means giving the store an ownership of its own, which is
//! §3.7's question, not this module's — see the PR's open question 11.
//!
//! Nothing a pass has touched can become undiscoverable: a popped
//! entry stays in the ledger and in the byte accounting, marked
//! reclaiming, until its deletion is verified.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use ourios_core::tenant::TenantId;
use uuid::Uuid;

use crate::retain::{RetainFloor, SnapshotHorizons};
use crate::{
    CheckpointError, HousekeepingError, WalOffset, ledger, reclaim, segment, sync_parent_dir,
};

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
/// It names the **`Wal` as well as the pass**. A per-WAL sequence
/// alone repeats: a reopen of the same root starts again at one, so a
/// plan left over from the instance before would be accepted by the
/// new one — its segments written into that root's record and its
/// outstanding state settled under a pass it never ran.
///
/// The value is the WAL's own, with no constructor outside this crate:
/// a plan is something a pass hands out, never something a caller
/// builds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PassId {
    wal: u64,
    pass: u64,
}

impl PassId {
    pub(crate) fn new(wal: u64, pass: u64) -> Self {
        Self { wal, pass }
    }

    pub(crate) fn seq(self) -> u64 {
        self.pass
    }
}

impl UnlinkPermit {
    pub(crate) fn new(pass: PassId, live: Arc<AtomicU64>) -> Self {
        Self { pass, live }
    }
}

impl std::fmt::Display for PassId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.wal, self.pass)
    }
}

/// Proof that this pass's `RECLAIM` write ran, which is what
/// [`unlink_planned`] needs before it removes anything.
///
/// §3.2's ordering rule is that the record is durable **before** the
/// segments it accounts for are gone: a segment unlinked with no
/// `planned` entry naming it is an absence nothing at open can
/// explain. Call order alone cannot hold that — the unlink half is
/// public, unlocked and reachable from anywhere — so
/// [`crate::Wal::write_plan_record`] hands this out and the unlink
/// consumes it. It names the pass, so a permit from one plan cannot
/// unlink another's.
///
/// It is also **revocable**, which naming the pass alone is not enough
/// for: §3.7's abandoned-plan recovery lets a later
/// `housekeeping_prepare` supersede a plan whose record was already
/// written, and a horizon that regressed in between re-pins exactly
/// the segments that plan names. The commit would refuse the stale
/// outcome — but only after the files were gone. So the permit holds
/// the WAL's live-pass cell and reads it at the unlink: a pass the WAL
/// has moved past authorises nothing.
#[derive(Debug)]
pub struct UnlinkPermit {
    pass: PassId,
    live: Arc<AtomicU64>,
}

/// Everything the file half needs, owned, so it holds no guard and no
/// WAL handle (RFC 0052 §3.7).
#[derive(Debug, Clone)]
///
/// **Every field is the pass's own**, readable through the accessors
/// below and writable only inside this crate. [`unlink_planned`] is
/// public, holds no guard and removes files, and
/// [`crate::Wal::write_plan_record`] decides from this value whether
/// §3.2's durable witness is owed — so every invariant the ledger half
/// establishes here would otherwise be one field assignment away from
/// being none: the cap RFC0052.12 bounds a pass by, the reserved
/// partial shape, the segment identities the record witnesses, the
/// root the shape is resolved against, and the record-before-unlink
/// ordering itself.
pub struct ReclaimPlan {
    pub(crate) pass: PassId,
    pub(crate) segments: Vec<PlannedSegment>,
    pub(crate) partials: Vec<PathBuf>,
    pub(crate) records: bool,
    pub(crate) root: PathBuf,
    pub(crate) progress: HousekeepingProgress,
}

impl ReclaimPlan {
    /// The pass that produced this plan (§3.7).
    #[must_use]
    pub fn pass(&self) -> PassId {
        self.pass
    }

    /// The segments this pass planned, oldest first.
    #[must_use]
    pub fn segments(&self) -> &[PlannedSegment] {
        &self.segments
    }

    /// The stale partials this pass swept, in the sweep's order.
    #[must_use]
    pub fn partials(&self) -> &[PathBuf] {
        &self.partials
    }

    /// Whether this pass owes a record write at all. §3.2 gates the
    /// record write with the segment planning: a record written under
    /// a version-1 checkpoint witnesses a reclamation that never
    /// happened.
    #[must_use]
    pub fn records(&self) -> bool {
        self.records
    }

    /// The WAL root the unlinks and the parent fsync run against.
    #[must_use]
    pub fn root(&self) -> &std::path::Path {
        &self.root
    }

    /// What the ledger half decided, as of this plan.
    #[must_use]
    pub fn progress(&self) -> &HousekeepingProgress {
        &self.progress
    }
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
///
/// `permit` is [`crate::Wal::write_plan_record`]'s receipt, consumed
/// here: §3.2's record-before-unlink ordering is the one invariant a
/// caller could otherwise break by call order alone, and a plan whose
/// permit belongs to a different pass unlinks nothing.
#[must_use]
// By value because that is what spends it: a reference would let one
// record write authorise any number of unlinks, which is the ordering
// this permit exists to enforce.
#[allow(clippy::needless_pass_by_value)]
pub fn unlink_planned(plan: &ReclaimPlan, permit: UnlinkPermit) -> ReclaimOutcome {
    let UnlinkPermit { pass, live } = permit;
    if pass != plan.pass || live.load(Ordering::Acquire) != pass.seq() {
        return ReclaimOutcome::RecordFailed(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "WAL housekeeping refused: pass {}'s permit does not authorise pass {} \
                 (RFC 0052 §3.2)",
                pass, plan.pass,
            ),
        ));
    }
    // Debris first, then segments — §3.2's order for a shared cap.
    // Debris has no identity to check: a `.wal.partial` is a file no
    // reader ever depended on, so "already gone" simply completes one.
    let partials = plan
        .partials
        .iter()
        .map(|path| (path, unlink_partial(&plan.root, path)));
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
///
/// The name is re-checked against §3.3's reserved shape under this
/// plan's root first. A segment is protected by its header uuid, which
/// a partial has nowhere to carry, and this function is public, takes
/// no guard and is handed a plan whose `partials` a caller can reach —
/// so without the check an `unlink` here could name any path at all. A
/// path that fails it is a failure the pass reports, not a silent skip.
fn unlink_partial(root: &std::path::Path, path: &std::path::Path) -> Result<(), std::io::Error> {
    if !ledger::is_reserved_partial(root, path) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "{} is not a stale partial of the WAL root at {}",
                path.display(),
                root.display(),
            ),
        ));
    }
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
/// [`crate::Wal::housekeeping_pass`] returns this after its commit. A
/// caller driving §3.7's three-part protocol owns the
/// [`ReclaimOutcome`] itself and can read `failed` directly; this is
/// here so both paths report the same thing.
///
/// One error stands for the set: every failed path stays queued and is
/// retried together on the next tick, so the count beside the first is
/// what tells a single stuck file from a failing volume. A pass whose
/// only trouble was the parent fsync is **not** one of these: §3.2
/// reads that as the uncertain deletion it defines, re-verified by the
/// next pass rather than retried as a failure.
#[must_use]
pub fn unlink_failure(outcome: &ReclaimOutcome) -> Option<HousekeepingError> {
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{
        Arc, AtomicU64, HousekeepingProgress, PassId, PassOutcome, ReclaimOutcome, ReclaimPlan,
        RetainFloor, UnlinkPermit, Uuid, unlink_planned,
    };

    /// The plan's paths are the crate's own, but [`unlink_planned`] is
    /// public, holds no guard and removes files — so the reserved
    /// shape is re-checked at the point of use rather than trusted
    /// from the sweep's seeding. A planned *segment* is protected by
    /// the header uuid the unlink verifies; a partial has nowhere to
    /// carry one, so this is the only check it has.
    #[test]
    fn a_partial_outside_the_reserved_shape_is_not_unlinked() {
        let tmp = tempfile::TempDir::new().expect("temp");
        let root = tmp.path();
        // The wrong name in the right directory, the right name in the
        // wrong one, and a reserved partial that is really the pass's.
        let misnamed = root.join("keep-me");
        let elsewhere = tempfile::TempDir::new().expect("temp");
        let outside = elsewhere
            .path()
            .join(format!("{}.wal.partial", Uuid::now_v7()));
        let reserved = root.join(format!("{}.wal.partial", Uuid::now_v7()));
        for path in [&misnamed, &outside, &reserved] {
            std::fs::write(path, b"debris").expect("write");
        }

        let built = plan(
            root,
            vec![misnamed.clone(), outside.clone(), reserved.clone()],
        );
        let live = Arc::new(AtomicU64::new(built.pass.seq()));
        let outcome = unlink_planned(&built, UnlinkPermit::new(built.pass, live));
        let ReclaimOutcome::Unlinked {
            removed, failed, ..
        } = outcome
        else {
            panic!("the unlink half ran");
        };

        assert_eq!(removed, vec![reserved.clone()], "only the pass's own");
        assert!(!reserved.exists());
        assert!(
            misnamed.exists() && outside.exists(),
            "neither a foreign name nor a foreign root is this pass's to remove",
        );
        assert_eq!(
            failed
                .iter()
                .map(|(path, _)| path.clone())
                .collect::<Vec<_>>(),
            vec![misnamed, outside],
            "and each is reported rather than silently skipped",
        );
    }

    fn plan(root: &std::path::Path, partials: Vec<PathBuf>) -> ReclaimPlan {
        ReclaimPlan {
            pass: PassId::new(1, 1),
            segments: Vec::new(),
            partials,
            records: false,
            root: root.to_path_buf(),
            progress: HousekeepingProgress {
                removed_segments: 0,
                removed_partials: 0,
                capped: false,
                horizon_remaining: 0,
                unlink_remaining: 0,
                floor: RetainFloor::Unknown,
                lag_bytes: 0,
                lag_segments: 0,
                outcome: PassOutcome::Planned,
            },
        }
    }
}
