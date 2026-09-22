//! RFC 0052 §3.2's segment ledger and the retain rule derived from
//! it: which tenants hold frames in which surviving segment, how far
//! each tenant's snapshot horizon has been applied, and which closed
//! segments a capped pass may pop.
//!
//! Eligibility is **per segment and tenant-aware**, never a single
//! global bound: a closed segment is reclaimable when its highest
//! offset is at or below the checkpoint (inclusive) and, for every
//! tenant holding a frame in it, that tenant's last offset there is at
//! or below that tenant's horizon (inclusive). A tenant with no
//! restorable snapshot has no horizon, so the comparison can never
//! succeed for it — the pin is strict — and exactly its own segments
//! are retained while segments holding only other tenants' covered
//! frames are reclaimed whatever their offset.
//!
//! Nothing here reads a directory or a segment header. Everything the
//! old header walk supplied lives in this structure, rebuilt once by
//! [`crate::Wal::rebuild_ledger`] and maintained incrementally by
//! every append and every verified unlink.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use ourios_core::tenant::TenantId;
use uuid::Uuid;

use crate::WalOffset;

/// A tenant's snapshot state as the receiver knows it (RFC 0052 §3.2).
///
/// The two are not interchangeable and an `Option<WalOffset>` cannot
/// hold the difference: only a snapshot that decodes *and restores*
/// may govern reclamation, while a pre-RFC artefact's global mark is
/// decoded for the legacy stale-gap check alone and must never be read
/// as a horizon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TenantHorizon {
    /// A restorable snapshot folded this tenant through `offset`.
    Restorable(WalOffset),
    /// No restorable snapshot, but a pre-RFC (`SNAPSHOT_VERSION` 1)
    /// artefact records this mark. §3.2 decodes it for one purpose —
    /// the legacy stale-gap check — and for no other: it restores
    /// nothing and feeds no miner, so it never governs reclamation.
    RecordedOnly(WalOffset),
}

impl TenantHorizon {
    /// The mark, whatever its provenance. Only [`Self::restorable`]
    /// may bound a reclamation.
    #[must_use]
    pub fn offset(self) -> WalOffset {
        match self {
            Self::Restorable(offset) | Self::RecordedOnly(offset) => offset,
        }
    }

    /// The horizon a pass may reclaim under, `None` for a tenant whose
    /// snapshot does not restore.
    #[must_use]
    pub fn restorable(self) -> Option<WalOffset> {
        match self {
            Self::Restorable(offset) => Some(offset),
            Self::RecordedOnly(_) => None,
        }
    }
}

/// What the caller knows about snapshots when it asks for a pass
/// (RFC 0052 §3.2, §3.7).
///
/// [`Self::NoConsumer`] is a **precondition, not a hint**: it asserts
/// the caller holds no miner state at all. A tenant that merely has no
/// valid snapshot is expressed by the `Pinned` floor, never by this
/// variant — which is why the two are distinct in the API and why the
/// mode a root ran under is recorded durably in `RECLAIM`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotHorizons {
    NoConsumer,
    Known(HashMap<TenantId, TenantHorizon>),
}

impl SnapshotHorizons {
    /// The horizons of a caller that holds miner state, from any
    /// iterable of restorable marks.
    pub fn restorable<I>(marks: I) -> Self
    where
        I: IntoIterator<Item = (TenantId, WalOffset)>,
    {
        Self::Known(
            marks
                .into_iter()
                .map(|(tenant, offset)| (tenant, TenantHorizon::Restorable(offset)))
                .collect(),
        )
    }
}

/// Why a floor is or is not available (RFC 0052 §3.7).
/// `Option<WalOffset>` cannot carry this: `None` means no snapshot
/// consumer exists and the checkpoint alone governs, which is *safe to
/// reclaim*, while a minimum held down by tenants without a valid
/// snapshot is correct but must be visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RetainFloor {
    /// No pass has derived it yet: reclaim nothing, report as such.
    #[default]
    Unknown,
    /// No snapshot consumer exists; the checkpoint alone governs.
    None,
    /// The minimum over every tenant's horizon.
    Min(WalOffset),
    /// Some tenant has no valid snapshot, so the minimum includes its
    /// oldest surviving frame.
    Pinned { offset: WalOffset, tenants: usize },
}

impl RetainFloor {
    /// The reported minimum, where there is one.
    #[must_use]
    pub fn offset(self) -> Option<WalOffset> {
        match self {
            Self::Unknown | Self::None => Option::None,
            Self::Min(offset) | Self::Pinned { offset, .. } => Some(offset),
        }
    }

    /// How many tenants are holding the floor down.
    #[must_use]
    pub fn pinned_tenants(self) -> usize {
        match self {
            Self::Pinned { tenants, .. } => tenants,
            Self::Unknown | Self::None | Self::Min(_) => 0,
        }
    }
}

/// A tenant's frames in one segment: RFC 0052 §3.2 needs both ends —
/// the first is what a pin reports as the tenant's oldest surviving
/// frame, the last is what the eligibility rule compares its horizon
/// with. A membership set alone could recover neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Span {
    pub(crate) first: WalOffset,
    pub(crate) last: WalOffset,
}

/// Where a segment stands in the pass (§3.2). A popped entry stays in
/// the ledger and in the byte accounting while it is `Reclaiming`, so
/// nothing a pass has touched becomes undiscoverable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Eligible,
    /// Popped by a pass. `uncertain` is §3.2's uncertain deletion: the
    /// unlink returned `Ok` but the parent fsync did not, so whether
    /// the entry survives a restart is unknown until re-verified.
    Reclaiming {
        uncertain: bool,
    },
}

/// One validated frame, as the recovery walk or a live append sees
/// it. Bundled rather than passed as four arguments so the ledger's
/// one mutation point stays readable.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FrameAt<'a> {
    pub(crate) offset: WalOffset,
    pub(crate) bytes: u64,
    /// `None` for every frame kind that carries no tenant membership:
    /// §3.2 governs those by the checkpoint alone.
    pub(crate) tenant: Option<&'a TenantId>,
    /// The file the frame landed in. Identity is the header uuid, but
    /// the *unlink* needs the real path — a segment an operator
    /// renamed must not be read as already gone, which would raise
    /// `reclaimed_through` over frames still on disk.
    pub(crate) path: &'a Path,
}

/// One surviving segment, as the ledger knows it.
#[derive(Debug)]
struct Segment {
    path: PathBuf,
    /// Highest frame offset in it. For the current append segment this
    /// rises with every append; for a closed one it is fixed.
    highest: WalOffset,
    /// Validated frame bytes, never file size less header.
    bytes: u64,
    members: HashMap<TenantId, Span>,
    /// Tenants whose horizon does not yet cover their last offset
    /// here. Empty means no tenant holds this segment back.
    pending: HashSet<TenantId>,
    state: State,
}

/// A tenant's horizon application state (§3.7). The cursor is the
/// newest segment up to which this tenant's horizon has been applied;
/// horizons are monotone and a tenant's last offsets rise with the
/// segments, so application is a prefix walk oldest-first and the
/// cursor never moves backwards.
#[derive(Debug, Default)]
struct Tenant {
    horizon: Option<WalOffset>,
    cursor: Option<Uuid>,
    segments: BTreeSet<Uuid>,
    /// Segments of this tenant above its cursor — the work the walk
    /// has not reached. Maintained incrementally so reading
    /// `horizon_remaining` is O(1).
    behind: usize,
}

/// The ledger itself. Segments are keyed by their `UUIDv7`, which
/// orders them chronologically and therefore by highest offset, so the
/// "ordered structure keyed by highest offset" §3.7 asks for is this
/// map's own order.
#[derive(Debug, Default)]
pub(crate) struct SegmentLedger {
    segments: BTreeMap<Uuid, Segment>,
    tenants: HashMap<TenantId, Tenant>,
    /// Segments whose `pending` set is empty — the eligible head the
    /// pass pops from under the checkpoint rule.
    unpinned: BTreeSet<Uuid>,
    reclaiming: usize,
    floor: RetainFloor,
}

/// One segment a pass popped, with everything the record write and the
/// unlink need. Ordered oldest-first.
#[derive(Debug, Clone)]
pub(crate) struct Popped {
    pub(crate) path: PathBuf,
    pub(crate) segment: Uuid,
    pub(crate) uncertain: bool,
    pub(crate) last_offsets: Vec<(TenantId, WalOffset)>,
}

impl SegmentLedger {
    /// Record a frame, whether from the recovery walk or a live
    /// append.
    pub(crate) fn observe(&mut self, at: FrameAt<'_>) {
        let id = at.offset.segment;
        let entry = self.segments.entry(id).or_insert_with(|| Segment {
            path: at.path.to_path_buf(),
            highest: at.offset,
            bytes: 0,
            members: HashMap::new(),
            pending: HashSet::new(),
            state: State::Eligible,
        });
        // A segment with no tenant frame holds nothing back, so it
        // joins the unpinned head immediately.
        if entry.state == State::Eligible && entry.pending.is_empty() {
            self.unpinned.insert(id);
        }
        entry.bytes += at.bytes;
        entry.highest = entry.highest.max(at.offset);
        let Some(tenant) = at.tenant else {
            return;
        };
        if let Some(span) = entry.members.get_mut(tenant) {
            span.last = span.last.max(at.offset);
            return;
        }
        entry.members.insert(
            tenant.clone(),
            Span {
                first: at.offset,
                last: at.offset,
            },
        );
        // A tenant new to this segment holds it back until its horizon
        // is applied here.
        entry.pending.insert(tenant.clone());
        self.unpinned.remove(&id);
        let state = self.tenants.entry(tenant.clone()).or_default();
        if state.segments.insert(id) && is_above(state.cursor, id) {
            state.behind += 1;
        }
    }

    /// Bytes of validated frames in surviving segments.
    pub(crate) fn bytes(&self) -> u64 {
        self.segments.values().map(|s| s.bytes).sum()
    }

    pub(crate) fn floor(&self) -> RetainFloor {
        self.floor
    }

    /// Segments a received horizon has not yet been applied to, summed
    /// over tenants.
    ///
    /// This is the count of (tenant, segment) pairs above each
    /// tenant's cursor, which is an **upper bound** on §3.7's "between
    /// its cursor and its horizon": the two agree whenever a tenant's
    /// horizon covers its newest segment and this figure is the larger
    /// otherwise. §3.7's own definition cannot be maintained without a
    /// range count proportional to the span a rising horizon newly
    /// covers, which is exactly the unbounded work under the journal
    /// guard that RFC0052.12 forbids.
    pub(crate) fn horizon_remaining(&self) -> usize {
        self.tenants.values().map(|t| t.behind).sum()
    }

    /// Empty-set segments not yet popped, plus entries reclaiming or
    /// uncertain.
    ///
    /// The current append segment is excluded: no pass can ever pop
    /// it, so counting it would make the backlog figure an operator
    /// watches never reach zero on a healthy node.
    pub(crate) fn unlink_remaining(&self, current: Uuid) -> usize {
        self.unpinned.len() - usize::from(self.unpinned.contains(&current)) + self.reclaiming
    }

    /// The oldest surviving frame of `tenant`: its first offset in its
    /// oldest surviving segment.
    pub(crate) fn oldest_frame(&self, tenant: &TenantId) -> Option<WalOffset> {
        let state = self.tenants.get(tenant)?;
        state
            .segments
            .iter()
            .find_map(|id| self.segments.get(id)?.members.get(tenant))
            .map(|span| span.first)
    }

    /// Tenants with surviving frames, in a stable order.
    pub(crate) fn tenants(&self) -> Vec<TenantId> {
        let mut out: Vec<TenantId> = self.tenants.keys().cloned().collect();
        out.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        out
    }

    /// Apply `horizons` to at most `budget` (tenant, segment) pairs,
    /// round-robin across the tenants whose cursor is behind, and
    /// derive the floor. Returns whether the budget bound.
    ///
    /// A tenant with no restorable horizon keeps `None` and its cursor
    /// does not move, so it stays in every one of its segments'
    /// `pending` sets — §3.2's strict pin, costing no work at all on a
    /// pass whose pinned backlog is unchanged.
    pub(crate) fn apply(&mut self, horizons: &SnapshotHorizons, budget: usize) -> bool {
        match horizons {
            SnapshotHorizons::NoConsumer => {
                self.floor = RetainFloor::None;
                false
            }
            SnapshotHorizons::Known(marks) => {
                for (tenant, state) in &mut self.tenants {
                    state.horizon = marks.get(tenant).and_then(|h| h.restorable());
                }
                let capped = self.walk(budget);
                self.floor = self.derive_floor();
                capped
            }
        }
    }

    /// The prefix walk, round-robin so one tenant cannot starve
    /// another's.
    fn walk(&mut self, budget: usize) -> bool {
        let mut spent = 0;
        loop {
            let mut progressed = false;
            let names: Vec<TenantId> = self.tenants.keys().cloned().collect();
            for tenant in names {
                if spent >= budget {
                    return self.any_behind();
                }
                if self.step(&tenant) {
                    spent += 1;
                    progressed = true;
                }
            }
            if !progressed {
                return false;
            }
        }
    }

    /// Advance one tenant's cursor by one segment, if its horizon
    /// covers that segment's last offset for it.
    fn step(&mut self, tenant: &TenantId) -> bool {
        let Some(state) = self.tenants.get(tenant) else {
            return false;
        };
        let Some(horizon) = state.horizon else {
            return false;
        };
        let next = match state.cursor {
            Some(cursor) => state.segments.range(next_after(cursor)).next().copied(),
            None => state.segments.iter().next().copied(),
        };
        let Some(next) = next else {
            return false;
        };
        let covered = self
            .segments
            .get(&next)
            .and_then(|s| s.members.get(tenant))
            .is_some_and(|span| span.last <= horizon);
        if !covered {
            return false;
        }
        if let Some(segment) = self.segments.get_mut(&next) {
            segment.pending.remove(tenant);
            if segment.pending.is_empty() && segment.state == State::Eligible {
                self.unpinned.insert(next);
            }
        }
        if let Some(state) = self.tenants.get_mut(tenant) {
            state.cursor = Some(next);
            state.behind = state.behind.saturating_sub(1);
        }
        true
    }

    fn any_behind(&self) -> bool {
        self.tenants.values().any(|t| t.behind > 0)
    }

    /// §3.2's reported summary: the minimum over every tenant's
    /// horizon and every pin, with the pinning tenants counted.
    fn derive_floor(&self) -> RetainFloor {
        let mut min: Option<WalOffset> = None;
        let mut pinned = 0;
        for (tenant, state) in &self.tenants {
            // A tenant with no restorable snapshot pins the floor at
            // its own oldest surviving frame rather than at a horizon.
            let mark = state.horizon.or_else(|| {
                pinned += 1;
                self.oldest_frame(tenant)
            });
            if let Some(mark) = mark {
                min = Some(min.map_or(mark, |held: WalOffset| held.min(mark)));
            }
        }
        match (min, pinned) {
            // No tenant holds a frame, so nothing pins: the checkpoint
            // alone bounds the pass and the reported minimum is the
            // ledger's own top.
            (None, _) => self
                .segments
                .values()
                .map(|s| s.highest)
                .max()
                .map_or(RetainFloor::Unknown, RetainFloor::Min),
            (Some(offset), 0) => RetainFloor::Min(offset),
            (Some(offset), tenants) => RetainFloor::Pinned { offset, tenants },
        }
    }

    /// Pop at most `budget` segments for this pass: entries already
    /// reclaiming first — §3.7's re-plan of a pass that never
    /// committed — then newly eligible ones, oldest first.
    ///
    /// `checkpoint` bounds every pop inclusively and `current` is
    /// never popped: identity is the header uuid, so a rename cannot
    /// slip the live append target past the guard.
    pub(crate) fn pop(
        &mut self,
        checkpoint: WalOffset,
        current: Uuid,
        tenant_aware: bool,
        budget: usize,
    ) -> (Vec<Popped>, bool) {
        let mut out = Vec::new();
        let replans: Vec<Uuid> = self
            .segments
            .iter()
            .filter(|(_, s)| matches!(s.state, State::Reclaiming { .. }))
            .map(|(id, _)| *id)
            .collect();
        for id in replans {
            if out.len() >= budget {
                return (out, true);
            }
            if let Some(popped) = self.take(id, current) {
                out.push(popped);
            }
        }
        // Under `NoConsumer` no tenant constrains anything, so the
        // candidate order is the whole ledger's; otherwise it is the
        // empty-set head, which is what makes a pinned oldest segment
        // unable to shadow a later eligible one.
        let candidates: Vec<Uuid> = if tenant_aware {
            self.unpinned.iter().copied().collect()
        } else {
            self.segments.keys().copied().collect()
        };
        for id in candidates {
            if out.len() >= budget {
                return (out, true);
            }
            let eligible = self
                .segments
                .get(&id)
                .is_some_and(|s| s.state == State::Eligible && s.highest <= checkpoint);
            if !eligible {
                continue;
            }
            if let Some(popped) = self.take(id, current) {
                out.push(popped);
            }
        }
        (out, false)
    }

    /// Mark one segment reclaiming and describe it for the record.
    fn take(&mut self, id: Uuid, current: Uuid) -> Option<Popped> {
        if id == current {
            return None;
        }
        let segment = self.segments.get_mut(&id)?;
        let uncertain = matches!(segment.state, State::Reclaiming { uncertain: true });
        if segment.state == State::Eligible {
            self.reclaiming += 1;
        }
        segment.state = State::Reclaiming { uncertain };
        self.unpinned.remove(&id);
        let mut last_offsets: Vec<(TenantId, WalOffset)> = segment
            .members
            .iter()
            .map(|(tenant, span)| (tenant.clone(), span.last))
            .collect();
        last_offsets.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
        Some(Popped {
            path: segment.path.clone(),
            segment: id,
            uncertain,
            last_offsets,
        })
    }

    /// A popped segment the file half never unlinked: it goes back to
    /// eligible and the next pass pops it again.
    pub(crate) fn restore(&mut self, id: Uuid) {
        let Some(segment) = self.segments.get_mut(&id) else {
            return;
        };
        if matches!(segment.state, State::Reclaiming { .. }) {
            self.reclaiming -= 1;
        }
        segment.state = State::Eligible;
        if segment.pending.is_empty() {
            self.unpinned.insert(id);
        }
    }

    /// A popped segment whose unlink failed, or whose deletion is
    /// uncertain: it stays reclaiming and counted, re-verified by the
    /// next pass.
    pub(crate) fn hold(&mut self, id: Uuid, uncertain: bool) {
        if let Some(segment) = self.segments.get_mut(&id) {
            segment.state = State::Reclaiming { uncertain };
        }
    }

    /// A segment whose deletion is verified: it and its bytes leave
    /// the ledger, and a tenant whose last surviving segment this was
    /// leaves with it, so tenant churn cannot leave a permanent pin.
    pub(crate) fn remove(&mut self, id: Uuid) -> u64 {
        let Some(segment) = self.segments.remove(&id) else {
            return 0;
        };
        if matches!(segment.state, State::Reclaiming { .. }) {
            self.reclaiming -= 1;
        }
        self.unpinned.remove(&id);
        for tenant in segment.members.keys() {
            let Some(state) = self.tenants.get_mut(tenant) else {
                continue;
            };
            state.segments.remove(&id);
            if is_above(state.cursor, id) {
                state.behind = state.behind.saturating_sub(1);
            }
            // The cursor is an ordering bound, not a reference: it
            // keeps the uuid of the segment it reached even once that
            // segment is gone, so the next pass resumes above it
            // rather than re-walking a prefix it has already applied.
            if state.segments.is_empty() {
                self.tenants.remove(tenant);
            }
        }
        segment.bytes
    }

    /// Bytes and segments below the floor — §3.5's lag figures, taken
    /// from the per-segment accounting rather than an inspection the
    /// cap would bound.
    pub(crate) fn lag(&self, floor: Option<WalOffset>) -> (u64, usize) {
        let Some(floor) = floor else {
            return (0, 0);
        };
        self.segments
            .values()
            .filter(|s| s.highest <= floor)
            .fold((0, 0), |(bytes, count), s| (bytes + s.bytes, count + 1))
    }
}

/// A tenant's segments strictly after `cursor`.
fn next_after(cursor: Uuid) -> (std::ops::Bound<Uuid>, std::ops::Bound<Uuid>) {
    (
        std::ops::Bound::Excluded(cursor),
        std::ops::Bound::Unbounded,
    )
}

/// Whether `id` sits above a cursor that may not exist yet.
fn is_above(cursor: Option<Uuid>, id: Uuid) -> bool {
    cursor.is_none_or(|at| id > at)
}
