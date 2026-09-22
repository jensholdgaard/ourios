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

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
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
    /// A pass has handed this segment to a file half that no commit
    /// has reported back on, so its unlink may already have happened.
    ///
    /// It lives on the segment rather than on [`State::Reclaiming`]
    /// because it has to **survive a withdrawal**: a horizon that
    /// regresses puts a popped entry back to `Eligible` (§3.7), and if
    /// the file was already gone, re-planning it later as a first
    /// attempt would fail on the missing path for the life of the
    /// process. Cleared by the commit that says what the file half
    /// did — which is either that nothing was unlinked at all, or
    /// [`State::Reclaiming`]'s own `uncertain`.
    attempted: bool,
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
    /// The sum of every tenant's `behind`, maintained beside it so
    /// reading it is O(1) rather than a per-tenant sum on the pass.
    behind_total: usize,
    /// Entries a pass popped and no commit has settled, ordered so a
    /// re-plan walks them oldest-first and costs O(cap) — a pass pops
    /// at most the cap, so this set never grows past it.
    reclaiming: BTreeSet<Uuid>,
    /// Frame bytes and segment count at least one tenant still holds
    /// back — §3.5's lag figures, moved on every mutation that changes
    /// a `pending` set or a held segment's bytes, so reading them
    /// costs nothing under the writer position.
    held_bytes: u64,
    held_segments: usize,
    /// Whether the last pass received horizons at all. `NoConsumer`
    /// applies none and can apply none, so the membership the ledger
    /// still tracks is not a backlog any pass will work off.
    consumer: bool,
    floor: RetainFloor,
}

/// One walk of the ledger inside a pass.
#[derive(Debug, Clone, Copy)]
struct Drain {
    bound: PopBound,
    budget: usize,
    admit: Admit,
}

/// Which candidates a walk admits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Admit {
    /// Entries a pass left marked reclaiming, re-planned ahead of
    /// anything newly eligible (§3.7).
    Reclaiming,
    /// Empty-set segments at or below the checkpoint.
    Eligible,
}

/// What bounds one pass's pops (RFC 0052 §3.2).
#[derive(Debug, Clone, Copy)]
pub(crate) struct PopBound {
    /// Every pop is at or below this, inclusively.
    pub(crate) checkpoint: WalOffset,
    /// The append target, which is never popped: identity is the
    /// header uuid, so a rename cannot slip it past the guard.
    pub(crate) current: Uuid,
    /// Whether tenant horizons constrain the candidates — false only
    /// under `SnapshotHorizons::NoConsumer`.
    pub(crate) tenant_aware: bool,
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
        self.record_frame(at);
        let Some(tenant) = at.tenant else {
            return;
        };
        if self.extend_span(tenant, at.offset) {
            // A frame appended into a segment whose horizon was
            // already applied here puts that tenant behind again. The
            // append path is the only place that can happen — the
            // current segment's last offset rises after the pass that
            // cleared it — and leaving it cleared would let the
            // segment be reclaimed after rotation over frames the
            // tenant has not snapshotted.
            self.repin(tenant, at.offset.segment);
            return;
        }
        self.join(tenant, at.offset);
    }

    /// The segment half of an observation: create the entry if this is
    /// its first frame, then take the frame's bytes and its offset.
    fn record_frame(&mut self, at: FrameAt<'_>) {
        let id = at.offset.segment;
        let entry = self.segments.entry(id).or_insert_with(|| Segment {
            path: at.path.to_path_buf(),
            highest: at.offset,
            bytes: 0,
            members: HashMap::new(),
            pending: HashSet::new(),
            state: State::Eligible,
            attempted: false,
        });
        // A segment with no tenant frame holds nothing back, so it
        // joins the unpinned head immediately.
        if entry.state == State::Eligible && entry.pending.is_empty() {
            self.unpinned.insert(id);
        }
        entry.bytes += at.bytes;
        entry.highest = entry.highest.max(at.offset);
        // A held segment's bytes are part of §3.5's lag figures.
        if !entry.pending.is_empty() {
            self.held_bytes += at.bytes;
        }
    }

    /// Raise `tenant`'s last offset in this segment, if it holds one
    /// there already. Returns whether it did.
    fn extend_span(&mut self, tenant: &TenantId, offset: WalOffset) -> bool {
        let Some(entry) = self.segments.get_mut(&offset.segment) else {
            return false;
        };
        let Some(span) = entry.members.get_mut(tenant) else {
            return false;
        };
        span.last = span.last.max(offset);
        true
    }

    /// `tenant`'s first frame in this segment: it joins the membership,
    /// holds the segment back until its horizon is applied here, and
    /// the segment counts against its own backlog.
    fn join(&mut self, tenant: &TenantId, offset: WalOffset) {
        let id = offset.segment;
        if let Some(entry) = self.segments.get_mut(&id) {
            entry.members.insert(
                tenant.clone(),
                Span {
                    first: offset,
                    last: offset,
                },
            );
        }
        self.pin(tenant, id);
        let state = self.tenants.entry(tenant.clone()).or_default();
        if state.segments.insert(id) && is_above(state.cursor, id) {
            state.behind += 1;
            self.behind_total += 1;
        }
    }

    /// `tenant` starts holding `id` back: the segment leaves the
    /// eligible head and joins §3.5's held figures. Returns whether
    /// this tenant was not already holding it.
    fn pin(&mut self, tenant: &TenantId, id: Uuid) -> bool {
        let Some(segment) = self.segments.get_mut(&id) else {
            return false;
        };
        let held = !segment.pending.is_empty();
        if !segment.pending.insert(tenant.clone()) {
            return false;
        }
        if !held {
            self.held_bytes += segment.bytes;
            self.held_segments += 1;
        }
        self.unpinned.remove(&id);
        true
    }

    /// `tenant` stops holding `id` back. A segment nothing holds any
    /// more leaves §3.5's held figures and, unless a pass has popped
    /// it, rejoins the eligible head.
    fn unpin(&mut self, tenant: &TenantId, id: Uuid) {
        let Some(segment) = self.segments.get_mut(&id) else {
            return;
        };
        if !segment.pending.remove(tenant) || !segment.pending.is_empty() {
            return;
        }
        self.held_bytes = self.held_bytes.saturating_sub(segment.bytes);
        self.held_segments = self.held_segments.saturating_sub(1);
        if segment.state == State::Eligible {
            self.unpinned.insert(id);
        }
    }

    /// What one segment contributes to the held figures right now.
    fn held_of(&self, id: Uuid) -> (u64, usize) {
        match self.segments.get(&id) {
            Some(segment) if !segment.pending.is_empty() => (segment.bytes, 1),
            _ => (0, 0),
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
    /// Zero until a pass receives horizons, and zero again under
    /// `NoConsumer`: the ledger keeps its tenant membership either way
    /// — a later `Known` pass resumes from it — but reporting it as
    /// remaining work would export a backlog no pass will ever reduce.
    pub(crate) fn horizon_remaining(&self) -> usize {
        if self.consumer { self.behind_total } else { 0 }
    }

    /// Empty-set segments not yet popped, plus entries reclaiming or
    /// uncertain.
    ///
    /// The current append segment is excluded: no pass can ever pop
    /// it, so counting it would make the backlog figure an operator
    /// watches never reach zero on a healthy node.
    pub(crate) fn unlink_remaining(&self, current: Uuid) -> usize {
        self.unpinned.len() - usize::from(self.unpinned.contains(&current)) + self.reclaiming.len()
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
                self.consumer = false;
                self.floor = RetainFloor::None;
                false
            }
            SnapshotHorizons::Known(marks) => {
                self.consumer = true;
                self.receive(marks);
                let capped = self.walk(budget);
                self.floor = self.derive_floor();
                capped
            }
        }
    }

    /// Take this pass's horizons.
    ///
    /// A horizon that **regresses or disappears** — a snapshot that
    /// stopped restoring, a tenant that fell out of the ledger the
    /// receiver keeps — must put the tenant back where it was before
    /// anything was applied: `derive_floor` would otherwise report the
    /// pin while the segments a higher horizon had already cleared sat
    /// in the eligible head, and the pass would reclaim exactly the
    /// frames the pin exists to keep. Rewinding is O(that tenant's
    /// segments) and happens only when its horizon goes backwards,
    /// which the steady state never does.
    fn receive(&mut self, marks: &HashMap<TenantId, TenantHorizon>) {
        let rewound: Vec<TenantId> = self
            .tenants
            .iter()
            .filter(|(tenant, state)| {
                let next = marks.get(*tenant).and_then(|h| h.restorable());
                state
                    .horizon
                    .is_some_and(|held| next.is_none_or(|n| n < held))
            })
            .map(|(tenant, _)| tenant.clone())
            .collect();
        for tenant in rewound {
            self.rewind(&tenant);
        }
        for (tenant, state) in &mut self.tenants {
            state.horizon = marks.get(tenant).and_then(|h| h.restorable());
        }
    }

    /// Put one tenant back to its unapplied state: it holds every one
    /// of its segments again and its cursor starts from the oldest.
    ///
    /// A segment a pass had already **popped** is withdrawn too. §3.7
    /// re-plans a reclaiming entry unconditionally, which is right
    /// while horizons are monotone — §3.7 states that as a property of
    /// the input — but a plan whose commit never ran, followed by a
    /// horizon that regressed, would re-plan and unlink frames the
    /// tenant now needs again. Withdrawing costs nothing either way:
    /// if the file half had already unlinked it, the next pass's
    /// unlink finds it gone and completes the reclamation.
    fn rewind(&mut self, tenant: &TenantId) {
        let Some(state) = self.tenants.get_mut(tenant) else {
            return;
        };
        state.cursor = None;
        self.behind_total = self.behind_total - state.behind + state.segments.len();
        state.behind = state.segments.len();
        let segments: Vec<Uuid> = state.segments.iter().copied().collect();
        for id in segments {
            self.pin(tenant, id);
            if let Some(entry) = self.segments.get_mut(&id)
                && matches!(entry.state, State::Reclaiming { .. })
            {
                entry.state = State::Eligible;
                self.reclaiming.remove(&id);
            }
            self.unpinned.remove(&id);
        }
    }

    /// The prefix walk, round-robin so one tenant cannot starve
    /// another's. Returns whether the budget bound it.
    ///
    /// The queue holds the tenants that can still progress, seeded
    /// once from those actually behind — a pinned backlog seeds none.
    /// A tenant that takes a step goes to the back; one that cannot is
    /// **dropped**, because nothing another tenant's step does can
    /// cover a segment for it: `next_covered` reads only that tenant's
    /// own cursor, horizon and last offsets. So each turn either
    /// spends budget or retires a tenant, and the walk is
    /// O(tenants + budget) rather than the O(budget × tenants) a
    /// rescan of every tenant per round would cost under the writer
    /// position.
    fn walk(&mut self, budget: usize) -> bool {
        let mut active: VecDeque<TenantId> = self
            .tenants
            .iter()
            .filter(|(_, state)| state.behind > 0)
            .map(|(tenant, _)| tenant.clone())
            .collect();
        let mut spent = 0;
        while spent < budget {
            let Some(tenant) = active.pop_front() else {
                return false;
            };
            if self.step(&tenant) {
                spent += 1;
                active.push_back(tenant);
            }
        }
        self.any_behind()
    }

    /// Advance one tenant's cursor by one segment, if its horizon
    /// covers that segment's last offset for it.
    fn step(&mut self, tenant: &TenantId) -> bool {
        let Some(next) = self.next_covered(tenant) else {
            return false;
        };
        self.apply_to(tenant, next);
        true
    }

    /// The oldest segment above this tenant's cursor whose last offset
    /// for it the tenant's horizon covers. `None` ends the prefix
    /// walk: last offsets rise with the segments, so nothing above an
    /// uncovered one is covered either.
    fn next_covered(&self, tenant: &TenantId) -> Option<Uuid> {
        let state = self.tenants.get(tenant)?;
        let horizon = state.horizon?;
        let next = match state.cursor {
            Some(cursor) => state.segments.range(next_after(cursor)).next().copied(),
            None => state.segments.iter().next().copied(),
        }?;
        let span = self.segments.get(&next)?.members.get(tenant)?;
        (span.last <= horizon).then_some(next)
    }

    /// Put `tenant` back behind `segment`: it holds the segment again
    /// and its cursor drops below it, so the next pass re-applies.
    ///
    /// The cursor is only ever moved backwards here, and only because
    /// the premise that makes it monotone — a tenant's last offsets
    /// rise *with* the segments — is what an append into an already
    /// applied segment breaks. The re-walk is bounded by one segment:
    /// the cursor lands on this segment's predecessor in the tenant's
    /// own set.
    fn repin(&mut self, tenant: &TenantId, segment: Uuid) {
        let covered = self
            .tenants
            .get(tenant)
            .and_then(|state| state.horizon)
            .zip(
                self.segments
                    .get(&segment)
                    .and_then(|entry| entry.members.get(tenant)),
            )
            .is_some_and(|(horizon, span)| span.last <= horizon);
        if covered {
            return;
        }
        if !self.pin(tenant, segment) {
            return;
        }
        if let Some(state) = self.tenants.get_mut(tenant)
            && !is_above(state.cursor, segment)
        {
            state.cursor = state.segments.range(..segment).next_back().copied();
            state.behind += 1;
            self.behind_total += 1;
        }
    }

    /// Record that `tenant`'s horizon now covers `segment`: it stops
    /// holding the segment back, and the cursor moves past it.
    fn apply_to(&mut self, tenant: &TenantId, segment: Uuid) {
        self.unpin(tenant, segment);
        if let Some(state) = self.tenants.get_mut(tenant) {
            state.cursor = Some(segment);
            state.behind = state.behind.saturating_sub(1);
            self.behind_total = self.behind_total.saturating_sub(1);
        }
    }

    fn any_behind(&self) -> bool {
        self.behind_total > 0
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
    /// `bound` carries the checkpoint every pop is bounded by
    /// (inclusive), the segment that may never be popped, and whether
    /// tenants constrain the candidates at all.
    pub(crate) fn pop(&mut self, bound: PopBound, budget: usize) -> (Vec<Popped>, bool) {
        let mut out = Vec::new();
        let replan = Drain {
            bound,
            budget,
            admit: Admit::Reclaiming,
        };
        if self.drain(&mut out, replan) {
            return (out, true);
        }
        let capped = self.drain(
            &mut out,
            Drain {
                admit: Admit::Eligible,
                ..replan
            },
        );
        (out, capped)
    }

    /// Pop the candidates `drain` admits, oldest first, while the
    /// budget holds. Returns whether the budget bound.
    fn drain(&mut self, out: &mut Vec<Popped>, drain: Drain) -> bool {
        for id in self.candidates(drain) {
            if out.len() >= drain.budget {
                return true;
            }
            if !self.admits(id, drain) {
                continue;
            }
            if let Some(popped) = self.take(id, drain.bound.current) {
                out.push(popped);
            }
        }
        false
    }

    /// The ids one drain walks, **bounded by the budget**. `Reclaiming`
    /// is §3.7's re-plan of a pass that never committed, taken ahead of
    /// anything newly eligible. `Eligible` under `NoConsumer` walks the
    /// whole ledger, since no tenant constrains anything; otherwise it
    /// walks the empty-set head, which is what keeps a pinned oldest
    /// segment from shadowing a later eligible one.
    ///
    /// Every branch takes at most **one more than** the budget and
    /// stops at the first segment above the checkpoint. That is what
    /// makes the walk O(cap) and not O(backlog): both structures are
    /// ordered by `UUIDv7`, which is the order of the segments' highest
    /// offsets, so nothing after the first uncovered one is covered
    /// either, and `take` is lazy so nothing past it is even visited.
    /// The `NoConsumer` branch may additionally skip entries already
    /// reclaiming, of which a pass leaves at most the cap.
    ///
    /// The one extra id is what lets [`Self::drain`] tell "the budget
    /// bound this pass" from "the backlog drained": truncating at
    /// exactly the budget would make every full pass report `capped`
    /// as false and the caller read a backlog that is still there as
    /// finished.
    fn candidates(&self, drain: Drain) -> Vec<Uuid> {
        let take = drain.budget.saturating_add(1);
        let covered = |id: &Uuid| {
            self.segments
                .get(id)
                .is_some_and(|s| s.highest <= drain.bound.checkpoint)
        };
        match (drain.admit, drain.bound.tenant_aware) {
            (Admit::Reclaiming, _) => self.reclaiming.iter().take(take).copied().collect(),
            (Admit::Eligible, true) => self
                .unpinned
                .iter()
                .take_while(|id| covered(id))
                .take(take)
                .copied()
                .collect(),
            (Admit::Eligible, false) => self
                .segments
                .keys()
                .take_while(|id| covered(id))
                .filter(|id| !self.reclaiming.contains(id))
                .take(take)
                .copied()
                .collect(),
        }
    }

    fn admits(&self, id: Uuid, drain: Drain) -> bool {
        self.segments
            .get(&id)
            .is_some_and(|segment| match drain.admit {
                Admit::Reclaiming => true,
                Admit::Eligible => {
                    segment.state == State::Eligible && segment.highest <= drain.bound.checkpoint
                }
            })
    }

    /// Mark one segment reclaiming and describe it for the record.
    fn take(&mut self, id: Uuid, current: Uuid) -> Option<Popped> {
        if id == current {
            return None;
        }
        let segment = self.segments.get_mut(&id)?;
        // §3.2's uncertain deletion, plus the case §3.7's abandoned
        // plan creates: a pass handed this to a file half that may or
        // may not have unlinked it, and no commit ever said.
        // Re-planning it as certain would make an `unlink` that finds
        // it gone a failure to retry forever.
        let uncertain =
            segment.attempted || matches!(segment.state, State::Reclaiming { uncertain: true });
        if segment.state == State::Eligible {
            self.reclaiming.insert(id);
        }
        segment.state = State::Reclaiming { uncertain };
        segment.attempted = true;
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
    ///
    /// This is the record-failed arm alone, where **nothing** was
    /// unlinked, so the attempt is withdrawn with the entry and the
    /// next plan is a genuine first one.
    pub(crate) fn restore(&mut self, id: Uuid) {
        let Some(segment) = self.segments.get_mut(&id) else {
            return;
        };
        if matches!(segment.state, State::Reclaiming { .. }) {
            self.reclaiming.remove(&id);
        }
        segment.state = State::Eligible;
        segment.attempted = false;
        if segment.pending.is_empty() {
            self.unpinned.insert(id);
        }
    }

    /// A popped segment whose unlink failed, or whose deletion is
    /// uncertain: it stays reclaiming and counted, re-verified by the
    /// next pass. The commit has spoken, so the attempt is no longer
    /// unresolved — `uncertain` is what survives of it.
    pub(crate) fn hold(&mut self, id: Uuid, uncertain: bool) {
        if let Some(segment) = self.segments.get_mut(&id) {
            segment.state = State::Reclaiming { uncertain };
            segment.attempted = false;
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
            self.reclaiming.remove(&id);
        }
        if !segment.pending.is_empty() {
            self.held_bytes = self.held_bytes.saturating_sub(segment.bytes);
            self.held_segments = self.held_segments.saturating_sub(1);
        }
        self.unpinned.remove(&id);
        for tenant in segment.members.keys() {
            let Some(state) = self.tenants.get_mut(tenant) else {
                continue;
            };
            state.segments.remove(&id);
            if is_above(state.cursor, id) {
                state.behind = state.behind.saturating_sub(1);
                self.behind_total = self.behind_total.saturating_sub(1);
            }
            // The cursor is an ordering bound, not a reference: it
            // keeps the uuid of the segment it reached even once that
            // segment is gone, so the next pass resumes above it
            // rather than re-walking a prefix it has already applied.
            if state.segments.is_empty() {
                self.behind_total = self.behind_total.saturating_sub(state.behind);
                self.tenants.remove(tenant);
            }
        }
        segment.bytes
    }

    /// §3.5's lag figures: the frame bytes and the segment count at
    /// least one tenant still holds back. Moved on every mutation of a
    /// `pending` set rather than walked, so the pass pays nothing for
    /// them under the writer position whatever the backlog.
    ///
    /// The **current append segment is excluded**, exactly as it is
    /// from [`Self::unlink_remaining`] and for the same reason: no
    /// pass can ever pop it, so counting it would hold a gauge an
    /// operator watches above zero on a healthy node.
    ///
    /// A pass with no floor holds nothing back and lags by nothing
    /// however far the checkpoint reaches: `NoConsumer` applies no
    /// horizon and can apply none, and before the first pass nothing
    /// has been derived. The ledger keeps its tenant membership either
    /// way, so a later `Known` pass reports from it.
    ///
    /// This is §3.5's figure **without its "below the checkpoint"
    /// clause**, which cannot be maintained beside it. The checkpoint
    /// moves, so counting only the segments under it costs a range
    /// count on every advance — the eager promotion RFC0052.12
    /// forbids, and the same arithmetic §3.7 already spends to define
    /// `unlink_remaining` without the bound. The figure is therefore
    /// an upper bound on §3.5's: equal to it whenever the checkpoint
    /// covers every held segment, and larger by the post-checkpoint
    /// tail otherwise. It never under-reports retained bytes, which is
    /// the safe direction for this gauge.
    pub(crate) fn lag(&self, current: Uuid) -> (u64, usize) {
        let (bytes, segments) = match self.floor {
            RetainFloor::Unknown | RetainFloor::None => return (0, 0),
            RetainFloor::Min(_) | RetainFloor::Pinned { .. } => self.held_of(current),
        };
        (
            self.held_bytes.saturating_sub(bytes),
            self.held_segments.saturating_sub(segments),
        )
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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use std::collections::BTreeSet;

    use super::{FrameAt, PopBound, SegmentLedger, SnapshotHorizons, State, is_above};
    use crate::WalOffset;
    use ourios_core::tenant::TenantId;

    const ALPHA: &str = "alpha";
    const BETA: &str = "beta";
    const GAMMA: &str = "gamma";

    /// `horizon_remaining`, `unlink_remaining` and the lag figures are
    /// all O(1) reads, which is only honest if the aggregates behind
    /// them track the per-tenant and per-segment state they stand for.
    /// Every mutation moves them, so a drift is a figure the pass
    /// reports and nobody can notice — this drives a sequence through
    /// every state transition and re-derives each one the long way,
    /// from a fresh walk of the segments and tenants, after each step.
    #[test]
    fn the_o1_aggregates_equal_the_sums_they_stand_for() {
        let (mut ledger, offsets, current) = seeded();
        check(&ledger, current, "the rebuild");

        let caught_up = horizons(&[(ALPHA, offsets[6]), (BETA, offsets[7])]);
        ledger.apply(&caught_up, 2);
        check(&ledger, current, "a capped application");
        ledger.apply(&caught_up, 8);
        check(&ledger, current, "the rest of the application");

        let bound = PopBound {
            checkpoint: offsets[5],
            current,
            tenant_aware: true,
        };
        let (popped, _) = ledger.pop(bound, 8);
        check(&ledger, current, "a pop");
        for entry in &popped {
            ledger.hold(entry.segment, true);
        }
        check(&ledger, current, "an uncertain hold");
        for entry in &popped {
            ledger.restore(entry.segment);
        }
        check(&ledger, current, "a restore");

        let (popped, _) = ledger.pop(bound, 8);
        for entry in &popped {
            ledger.remove(entry.segment);
        }
        check(&ledger, current, "a removal");

        // A no-consumer pass applies no horizon and reports no
        // backlog, while the membership behind both stays put.
        ledger.apply(&SnapshotHorizons::NoConsumer, 8);
        check(&ledger, current, "a no-consumer pass");

        // A horizon that disappears rewinds both tenants...
        ledger.apply(&horizons(&[]), 8);
        check(&ledger, current, "a rewind");

        // ...and an append above the applied horizon re-pins one.
        ledger.apply(&caught_up, 8);
        append(&mut ledger, current, 4096, ALPHA);
        check(&ledger, current, "a re-pinning append");

        // A no-consumer pass pops by the checkpoint alone, so it can
        // take a segment tenants are still holding — the one path that
        // removes a *held* segment from the figures.
        let rotated = uuid::Uuid::now_v7();
        let newest = append(&mut ledger, rotated, 64, ALPHA);
        check(&ledger, rotated, "a rotation");
        ledger.apply(&SnapshotHorizons::NoConsumer, 8);
        let (popped, _) = ledger.pop(
            PopBound {
                checkpoint: newest,
                current: rotated,
                tenant_aware: false,
            },
            8,
        );
        assert!(!popped.is_empty(), "the pass pops what the tenants hold");
        for entry in &popped {
            ledger.remove(entry.segment);
        }
        check(&ledger, rotated, "a held segment removed under no consumer");
    }

    /// The walk is round-robin, and a tenant that cannot take a step
    /// neither holds budget from one that can nor stops the walk from
    /// draining. Both follow from retiring a blocked tenant instead of
    /// rescanning it, and neither is visible in the loop's shape.
    #[test]
    fn the_horizon_walk_is_round_robin_and_retires_a_blocked_tenant() {
        let mut ledger = SegmentLedger::default();
        let mut alpha = Vec::new();
        let mut beta = Vec::new();
        let mut blocked = None;
        for _ in 0..3 {
            let segment = uuid::Uuid::now_v7();
            alpha.push(append(&mut ledger, segment, 64, ALPHA));
            beta.push(append(&mut ledger, segment, 128, BETA));
            let gamma = append(&mut ledger, segment, 192, GAMMA);
            // Below gamma's own first frame, so its horizon covers no
            // segment at all and it can never take a step.
            blocked.get_or_insert(WalOffset {
                segment: gamma.segment,
                byte: 0,
            });
        }
        let marks = horizons(&[
            (ALPHA, alpha[2]),
            (BETA, beta[2]),
            (GAMMA, blocked.expect("three segments")),
        ]);

        assert!(ledger.apply(&marks, 2), "the budget bound this pass");
        assert_eq!(
            (behind(&ledger, ALPHA), behind(&ledger, BETA)),
            (2, 2),
            "the two rounds went one each, not both to the first tenant",
        );
        assert_eq!(behind(&ledger, GAMMA), 3, "and none to the blocked one");

        assert!(
            !ledger.apply(&marks, 64),
            "a walk the budget does not bind reports drained, even with a \
             tenant still behind that no budget could advance",
        );
        assert_eq!((behind(&ledger, ALPHA), behind(&ledger, BETA)), (0, 0));
        assert_eq!(
            ledger.horizon_remaining(),
            3,
            "gamma's three, and only those"
        );
    }

    fn behind(ledger: &SegmentLedger, tenant: &str) -> usize {
        ledger.tenants[&TenantId::new(tenant)].behind
    }

    /// Four segments, two tenants in each, and the offsets in the
    /// order they were appended. The newest segment is the current
    /// one, as it is after any open.
    fn seeded() -> (SegmentLedger, Vec<WalOffset>, uuid::Uuid) {
        let mut ledger = SegmentLedger::default();
        let segments: Vec<uuid::Uuid> = (0..4).map(|_| uuid::Uuid::now_v7()).collect();
        let mut offsets = Vec::new();
        for (index, segment) in segments.iter().enumerate() {
            let byte = 64 + index as u64;
            offsets.push(append(&mut ledger, *segment, byte, ALPHA));
            offsets.push(append(&mut ledger, *segment, byte + 64, BETA));
        }
        let current = *segments.last().expect("four segments");
        (ledger, offsets, current)
    }

    fn append(
        ledger: &mut SegmentLedger,
        segment: uuid::Uuid,
        byte: u64,
        tenant: &str,
    ) -> WalOffset {
        let offset = WalOffset { segment, byte };
        ledger.observe(FrameAt {
            offset,
            bytes: 32,
            tenant: Some(&TenantId::new(tenant)),
            path: Path::new("/wal/x.wal"),
        });
        offset
    }

    fn horizons(marks: &[(&str, WalOffset)]) -> SnapshotHorizons {
        SnapshotHorizons::restorable(
            marks
                .iter()
                .map(|(tenant, offset)| (TenantId::new(*tenant), *offset)),
        )
    }

    /// Every O(1) figure against a fresh walk of the state it stands
    /// for, plus the two indexes the figures are read off.
    fn check(ledger: &SegmentLedger, current: uuid::Uuid, what: &str) {
        check_backlog(ledger, what);
        check_indexes(ledger, current, what);
        check_lag(ledger, current, what);
    }

    /// Each tenant's own `behind`, recounted from its cursor, and the
    /// sum the pass reports as `horizon_remaining`.
    fn check_backlog(ledger: &SegmentLedger, what: &str) {
        let mut behind = 0;
        for (tenant, state) in &ledger.tenants {
            let walked = state
                .segments
                .iter()
                .filter(|id| is_above(state.cursor, **id))
                .count();
            assert_eq!(state.behind, walked, "{tenant:?} is behind after {what}");
            behind += walked;
        }
        assert_eq!(ledger.behind_total, behind, "behind_total after {what}");
    }

    /// The eligible head and the pop index against the segment states
    /// they mirror, and `unlink_remaining` against both.
    fn check_indexes(ledger: &SegmentLedger, current: uuid::Uuid, what: &str) {
        let reclaiming: BTreeSet<uuid::Uuid> = ledger
            .segments
            .iter()
            .filter(|(_, s)| matches!(s.state, State::Reclaiming { .. }))
            .map(|(id, _)| *id)
            .collect();
        assert_eq!(ledger.reclaiming, reclaiming, "the pop index after {what}");
        let unpinned: BTreeSet<uuid::Uuid> = ledger
            .segments
            .iter()
            .filter(|(_, s)| s.pending.is_empty() && s.state == State::Eligible)
            .map(|(id, _)| *id)
            .collect();
        assert_eq!(ledger.unpinned, unpinned, "the eligible head after {what}");
        assert_eq!(
            ledger.unlink_remaining(current),
            unpinned.len() - usize::from(unpinned.contains(&current)) + reclaiming.len(),
            "unlink_remaining after {what}",
        );
    }

    /// The held aggregates, and the lag they are reported as.
    fn check_lag(ledger: &SegmentLedger, current: uuid::Uuid, what: &str) {
        let held = || ledger.segments.values().filter(|s| !s.pending.is_empty());
        assert_eq!(ledger.held_segments, held().count(), "held_segments {what}");
        assert_eq!(
            ledger.held_bytes,
            held().map(|s| s.bytes).sum::<u64>(),
            "held_bytes after {what}",
        );
        let lagging = || {
            ledger
                .segments
                .iter()
                .filter(|(id, s)| **id != current && !s.pending.is_empty())
        };
        let expected = match ledger.floor.offset() {
            None => (0, 0),
            Some(_) => (lagging().map(|(_, s)| s.bytes).sum(), lagging().count()),
        };
        assert_eq!(ledger.lag(current), expected, "the lag after {what}");
    }
}
