//! RFC 0052 §3.2's housekeeping pass on the WAL itself: the ledger
//! half under the single-writer position, the record half that must
//! precede the unlinks, and the ledger half again with what the file
//! half did.
//!
//! The values that travel between the halves are [`crate::pass`]'s;
//! this is the [`Wal`] side of them.

use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::atomic::Ordering;

use crate::pass::{
    HousekeepingProgress, PassOutcome, PlannedSegment, ReclaimError, ReclaimOutcome, ReclaimPlan,
    SkipReason,
};
use crate::reclaim_store::{ReclaimSlot, ReclaimStore};
use crate::retain::{SnapshotHorizons, TenantHorizon};
use crate::{
    HousekeepingError, Outstanding, ReclaimGate, Wal, WalOffset, mark_uncertain, pass, reclaim,
    reclaim_store, reconcile, retain, unlink_planned,
};

/// An outcome for a pass nothing is outstanding for (RFC 0052 §3.7).
fn settled_commit(pass: pass::PassId) -> HousekeepingError {
    HousekeepingError::Io {
        op: "housekeeping_commit(plan)",
        source: std::io::Error::new(
            ErrorKind::InvalidInput,
            format!("WAL housekeeping refused: pass {pass} is already settled (RFC 0052 §3.7)"),
        ),
    }
}

/// An outcome whose pass is no longer the outstanding one (§3.7).
fn superseded_commit(pass: pass::PassId, outstanding: pass::PassId) -> HousekeepingError {
    HousekeepingError::Io {
        op: "housekeeping_commit(plan)",
        source: std::io::Error::new(
            ErrorKind::InvalidInput,
            format!(
                "WAL housekeeping refused: outcome from pass {pass} was superseded by pass \
                 {outstanding} (RFC 0052 §3.7)"
            ),
        ),
    }
}

/// Why a plan's record write is refused (RFC 0052 §3.7).
enum PlanRefusal {
    /// A commit has already settled the plan's pass.
    Settled,
    /// The `Wal` that made the plan has been dropped.
    Closed,
    /// A later prepare replaced the plan's pass with this one.
    Superseded(pass::PassId),
}

impl PlanRefusal {
    fn error(self, plan: &ReclaimPlan) -> std::io::Error {
        let why = match self {
            Self::Settled => "is already settled".to_owned(),
            Self::Closed => "outlived its WAL".to_owned(),
            Self::Superseded(outstanding) => format!("was superseded by pass {outstanding}"),
        };
        std::io::Error::new(
            ErrorKind::InvalidInput,
            format!(
                "WAL housekeeping refused: plan from pass {} {why} (RFC 0052 §3.7)",
                plan.pass,
            ),
        )
    }
}

/// What the unlink half verified: the paths it removed, and whether
/// the one parent fsync covering all of them failed — §3.2's uncertain
/// deletion, which leaves every removed path unverified.
struct Unlinked {
    removed: std::collections::HashSet<PathBuf>,
    fsync_failed: bool,
}

impl Wal {
    /// `max_unlinks_per_pass` as a `usize` (RFC 0052 §3.8). The
    /// configured value is validated against
    /// [`crate::MAX_UNLINKS_PER_PASS_CEILING`] at open, so it always fits.
    pub(crate) fn pass_cap(&self) -> usize {
        usize::try_from(self.config.max_unlinks_per_pass).unwrap_or(usize::MAX)
    }

    /// RFC 0052 §3.2's **ledger half**, under the WAL's single-writer
    /// position: apply `horizons` (capped), derive the retain floor,
    /// pop stale partials and then eligible segments up to
    /// `max_unlinks`, and mark each popped entry reclaiming.
    ///
    /// It does no I/O at all. Everything the old header walk supplied
    /// — a segment's highest offset, each tenant's last offset in it,
    /// its frame bytes — comes from the ledger, so the pass reads no
    /// segment header and lists no directory.
    ///
    /// The cost an append can wait on is **O(cap + tenants)**, not
    /// O(backlog): the horizon walk and the pops are bounded by the
    /// cap, while taking the horizons, deriving the floor and listing
    /// the tenants that are behind are each one sweep of the tenant
    /// set, which `max_tenants` bounds. RFC0052.12's guarantee is
    /// about the backlog — the incident's 1,113 segments — and that is
    /// what the cap holds. There is **no exception**: the backlog
    /// figures and the lag figures are all aggregates the ledger moves
    /// on mutation, so reading them costs nothing here.
    ///
    /// The returned [`ReclaimPlan`] is owned and carries the `RECLAIM`
    /// sidecar it was planned against, so the whole file half —
    /// [`crate::write_plan_record`] and [`unlink_planned`] — needs
    /// neither the guard nor a WAL handle; feed its outcome back
    /// through [`Self::housekeeping_commit`].
    ///
    /// The sidecar's own lock is held for the whole of this half, and
    /// that is the one wait it can add: a pass whose record write is
    /// still in flight finishes it first. Passes do not overlap under
    /// the coordinator's single housekeeping task, so in practice there
    /// is nothing to wait for; the lock is what guarantees that a plan
    /// is never superseded half-way through its record write.
    ///
    /// # Errors
    ///
    /// [`ReclaimError::Housekeeping`] when the pass's mode disagrees
    /// with the mode recorded in the `RECLAIM` header, or when a
    /// tenant's state cannot be told apart from a loss — both refused
    /// before anything is planned or unlinked.
    pub fn housekeeping_prepare(
        &mut self,
        horizons: &SnapshotHorizons,
        max_unlinks: usize,
    ) -> Result<ReclaimPlan, ReclaimError> {
        // A plan that was never committed — the housekeeping task
        // panicked between the halves — strands nothing (§3.7). Its
        // segments are still marked reclaiming and are re-planned
        // below, but its partials left the sweep's list, which is
        // their only record, so they go back on it **before** anything
        // here can return early: a pass refused for a mode or horizon
        // mismatch would otherwise take them with it.
        // Before anything, including the refusals below: an abandoned
        // plan's permit stops authorising the moment this prepare
        // decides to exist. A refusal after this point leaves no
        // outstanding state, so a permit still live across it could
        // unlink past the very decision that failed closed.
        let slot = self.reclaim.clone();
        let held = slot.lock();
        let record = held.as_ref().map(ReclaimStore::record);
        slot.live().store(0, Ordering::Release);
        if let Some(abandoned) = self.outstanding.take() {
            self.withdraw_across_modes(&abandoned, horizons);
            self.requeue_partials(abandoned.partials);
        }
        self.refuse_mode_disagreement(record, horizons)?;
        self.refuse_unexplained_tenants(record, horizons)?;
        // Never above what the `RECLAIM` geometry was built for: the
        // record's `planned` array is sized from the configured cap,
        // so a larger pass would plan entries the slot cannot encode
        // and the whole record write — and with it the pass — would
        // fail.
        let cap = max_unlinks.clamp(1, self.pass_cap());
        let horizons_capped = self.ledger.apply(horizons, cap);
        let take = self.stale_partials.len().min(cap);
        let partials: Vec<PathBuf> = self.stale_partials.drain(..take).collect();
        // The cap is shared, so the partial half can exhaust it on its
        // own. Anything still on the list after the drain is work this
        // pass left for the next tick, and `capped` is the caller's
        // only signal that the budget bound it.
        let partials_capped = !self.stale_partials.is_empty();
        let budget = cap - partials.len();
        let (segments, pops_capped, outcome) = self.pop_segments(record, horizons, budget);
        let (lag_bytes, lag_segments) = self.ledger.lag(self.current_segment_uuid);
        self.passes += 1;
        let pass = pass::PassId::new(slot.instance(), self.passes);
        // Any permit an earlier pass still holds stops authorising
        // anything here: §3.7 lets this prepare supersede that plan,
        // and a horizon that regressed in between has re-pinned the
        // segments it names.
        slot.live().store(self.passes, Ordering::Release);
        let mode = pass::entry_mode(horizons);
        let plan = ReclaimPlan {
            pass,
            segments: segments
                .iter()
                .map(|popped| PlannedSegment {
                    segment: popped.segment,
                    uncertain: popped.uncertain,
                    path: popped.path.clone(),
                    last_offsets: popped.last_offsets.clone(),
                })
                .collect(),
            partials,
            // §3.2's gate is on segment planning *and* the record
            // write: a record written under a version-1 checkpoint
            // witnesses a reclamation that never happened.
            records: outcome == PassOutcome::Planned,
            root: self.config.root.clone(),
            progress: HousekeepingProgress {
                removed_segments: 0,
                removed_partials: 0,
                capped: horizons_capped || pops_capped || partials_capped,
                horizon_remaining: self.ledger.horizon_remaining(),
                unlink_remaining: self.ledger.unlink_remaining(self.current_segment_uuid),
                floor: self.ledger.floor(),
                lag_bytes,
                lag_segments,
                outcome,
            },
            mode,
            slot: slot.clone(),
        };
        self.outstanding = Some(Outstanding {
            pass,
            segments,
            partials: plan.partials.clone(),
            mode,
            progress: plan.progress,
        });
        Ok(plan)
    }

    /// An abandoned plan's entries stay marked reclaiming and §3.7
    /// re-plans them **unconditionally**, ahead of anything newly
    /// eligible. That is right under the same mode. It is not right
    /// across a change of mode: a `NoConsumer` pass pops by the
    /// checkpoint alone, with no tenant constraint at all, so carrying
    /// its choices into a `Known` pass would unlink frames of a tenant
    /// that has no snapshot — the loss §3.2's mode guard exists to
    /// prevent, reached through the one window where that guard cannot
    /// see it, since a plan abandoned before its record write leaves
    /// the recorded mode `Unrecorded` and every mode is then admitted.
    ///
    /// The entries are withdrawn rather than restored: the file half
    /// may have had them, and §3.2's ordering puts the record write
    /// first, so the next plan must still treat them as uncertain.
    fn withdraw_across_modes(&mut self, abandoned: &Outstanding, horizons: &SnapshotHorizons) {
        if abandoned.mode == pass::entry_mode(horizons) {
            return;
        }
        for popped in &abandoned.segments {
            self.ledger.withdraw(popped.segment);
        }
    }

    /// The record half of RFC 0052 §3.2's file half: rewrite the
    /// inactive `RECLAIM` slot in place and fsync it. It allocates
    /// nothing and never extends the file, so a pass on a full volume
    /// still commits — which is the case this record exists for.
    ///
    /// Separate from [`unlink_planned`] because §3.2's ordering rule
    /// is exactly that the record is durable **before** the segments
    /// it accounts for are gone.
    ///
    /// The **merge happens here, not in prepare** (§3.2: "off the
    /// writer position it merges those horizons into the record"). The
    /// plan carries each popped segment's per-tenant offsets, not a
    /// finished record: a record snapshotted at prepare would be stale
    /// by the time it is written, and writing it would clobber
    /// whatever landed in between — a `checkpoint_seen` from a
    /// concurrent checkpoint, or a `reclaimed_through` an earlier
    /// commit raised.
    ///
    /// **A plan a later `housekeeping_prepare` superseded is refused
    /// here**, and this is the only place it can be: §3.2 orders the
    /// record write before any unlink, so a caller following that
    /// order cannot reach the file half with a stale plan. It matters
    /// because §3.7's abandoned-plan recovery makes a second prepare
    /// legal, and a horizon that regressed in between withdraws the
    /// segments the first plan named — writing its record would
    /// witness them under the live pass's mode, and the unlinks after
    /// it would remove frames the ledger has since re-pinned.
    ///
    /// It needs nothing of the WAL but its `RECLAIM` sidecar, which
    /// has a lock of its own, so it takes `&self`: a caller whose
    /// journal is behind a mutex uses [`crate::write_plan_record`] on
    /// the plan instead and holds no guard at all. This form writes
    /// through **this** `Wal`'s sidecar, whichever one the plan came
    /// from — a plan from another instance is refused as superseded.
    ///
    /// # Errors
    ///
    /// The plan was superseded, the merge could not assign a slot id,
    /// or the slot write or its fsync failed. Nothing has been
    /// unlinked either way, and [`ReclaimOutcome::RecordFailed`] is
    /// what [`Self::housekeeping_commit`] expects in response.
    pub fn write_plan_record(
        &self,
        plan: &ReclaimPlan,
    ) -> Result<pass::UnlinkPermit, std::io::Error> {
        self.reclaim.write_plan(plan)
    }

    /// RFC 0052 §3.2's **ledger half again**, with what the file half
    /// did: removed entries leave the ledger and its byte accounting,
    /// an uncertain deletion keeps the whole removed set reclaiming
    /// for the next pass to re-verify, a failed unlink stays
    /// reclaiming, and a failed record write returns every popped
    /// entry to eligible — nothing a pass touched becomes
    /// undiscoverable.
    ///
    /// `reclaimed_through` is raised here and made durable by the next
    /// record write. The gap is safe because the `planned` list was
    /// durable *before* the unlinks: a crash in it reconciles at open,
    /// where an absent planned segment is treated exactly like a
    /// completed reclamation.
    ///
    /// `pass` is the plan's own [`ReclaimPlan::pass`], and an outcome
    /// that names a **superseded** one is refused with the outstanding
    /// state untouched. Without it, a caller holding a plan a later
    /// prepare replaced could take the refusal from
    /// [`Self::write_plan_record`], report it here as
    /// [`ReclaimOutcome::RecordFailed`] as the protocol says, and tear
    /// down the *live* plan: its entries restored, its partials
    /// requeued, and its own file half then unaccounted for.
    ///
    /// # Errors
    ///
    /// [`ReclaimError::Housekeeping`] when the outcome belongs to a
    /// superseded pass, or when a planned segment names a slot id the
    /// dictionary cannot raise — a record that cannot be told apart
    /// from a loss.
    pub fn housekeeping_commit(
        &mut self,
        pass: pass::PassId,
        outcome: ReclaimOutcome,
    ) -> Result<HousekeepingProgress, ReclaimError> {
        let slot = self.reclaim.clone();
        let mut held = slot.lock();
        let settle = self.outstanding.as_ref().is_some_and(|o| o.pass == pass);
        if settle {
            // The plan is being accounted for, so its permit is spent.
            slot.live().store(0, Ordering::Release);
        }
        let Some(outstanding) = self.outstanding.take() else {
            // Not "nothing to do": a commit has already taken this
            // pass's state, or none was ever prepared. Reporting a
            // clean skip would swallow the outcome's unlink failures
            // and leave the identity unchecked in the one branch that
            // never looks at it.
            return Err(self.housekeeping_failure(settled_commit(pass)));
        };
        if outstanding.pass != pass {
            let refused = superseded_commit(pass, outstanding.pass);
            self.outstanding = Some(outstanding);
            return Err(self.housekeeping_failure(refused));
        }
        let ReclaimOutcome::Unlinked {
            removed,
            failed: _,
            fsync_failed,
        } = outcome
        else {
            // Nothing was unlinked, so every popped entry goes back to
            // eligible and a later pass pops it again.
            for popped in &outstanding.segments {
                self.ledger.restore(popped.segment);
            }
            self.requeue_partials(outstanding.partials);
            return Ok(self.settled(outstanding.progress, 0, 0));
        };
        let unlinked = Unlinked {
            removed: removed.into_iter().collect(),
            fsync_failed,
        };
        let segments = self.settle_segments(&mut held, &outstanding.segments, &unlinked)?;
        let partials = self.settle_partials(outstanding.partials, &unlinked);
        Ok(self.settled(outstanding.progress, segments, partials))
    }

    /// Count the partials whose removal is verified and put the rest
    /// back on the sweep's list. An uncertain deletion is not verified:
    /// one parent fsync covers the whole pass, so its failure requeues
    /// every path the pass removed.
    fn settle_partials(&mut self, popped: Vec<PathBuf>, unlinked: &Unlinked) -> usize {
        let (done, requeued): (Vec<PathBuf>, Vec<PathBuf>) = popped
            .into_iter()
            .partition(|path| unlinked.removed.contains(path) && !unlinked.fsync_failed);
        self.requeue_partials(requeued);
        done.len()
    }

    /// The ledger half's own decisions, carrying what the file half
    /// actually removed and the backlog as it now stands.
    fn settled(
        &self,
        planned: HousekeepingProgress,
        removed_segments: usize,
        removed_partials: usize,
    ) -> HousekeepingProgress {
        HousekeepingProgress {
            removed_segments,
            removed_partials,
            horizon_remaining: self.ledger.horizon_remaining(),
            unlink_remaining: self.ledger.unlink_remaining(self.current_segment_uuid),
            ..planned
        }
    }

    /// One whole pass for a caller that owns the WAL outright: the
    /// ledger half, the record write, the unlinks and the commit.
    ///
    /// The coordinator uses the three-part form instead, because its
    /// journal is behind a mutex and the whole file half has to run
    /// with that guard released (§3.7).
    ///
    /// # Errors
    ///
    /// See [`Self::housekeeping_prepare`] and
    /// [`Self::housekeeping_commit`], plus
    /// [`HousekeepingError::Io`] when an unlink failed: the entries
    /// stay reclaiming and the partials stay on the sweep's list, so
    /// the retry is intact, but §3.1's rule is that such a failure is
    /// logged and the next pass retries it — neither of which a caller
    /// that cannot see it can do.
    pub fn housekeeping_pass(
        &mut self,
        horizons: &SnapshotHorizons,
        max_unlinks: usize,
    ) -> Result<HousekeepingProgress, ReclaimError> {
        let plan = self.housekeeping_prepare(horizons, max_unlinks)?;
        let permit = match self.write_plan_record(&plan) {
            Ok(permit) => permit,
            Err(source) => return self.record_failed(&plan, source),
        };
        self.settle_unlinks(plan.pass, unlink_planned(&plan, permit))
    }

    /// §3.1's rule for a witness that could not be made durable: the
    /// commit still runs — it is what puts the popped entries back and
    /// requeues the partials — and the failure is the caller's to see.
    /// Swallowing it would report a pass that unlinked nothing as a
    /// clean one, and the next pass would have nothing to retry from.
    fn record_failed(
        &mut self,
        plan: &ReclaimPlan,
        source: std::io::Error,
    ) -> Result<HousekeepingProgress, ReclaimError> {
        let (kind, detail) = (source.kind(), source.to_string());
        let progress = self.housekeeping_commit(plan.pass, ReclaimOutcome::RecordFailed(source))?;
        Err(ReclaimError::Housekeeping {
            progress: Box::new(progress),
            source: HousekeepingError::Io {
                op: "write(RECLAIM slot)",
                source: std::io::Error::new(kind, detail),
            },
        })
    }

    /// Commit what the file half did, then report its unlink failures.
    /// The commit runs first either way: it is what keeps the failed
    /// entries discoverable.
    fn settle_unlinks(
        &mut self,
        pass: pass::PassId,
        outcome: ReclaimOutcome,
    ) -> Result<HousekeepingProgress, ReclaimError> {
        let failure = pass::unlink_failure(&outcome);
        let progress = self.housekeeping_commit(pass, outcome)?;
        match failure {
            Some(source) => Err(ReclaimError::Housekeeping {
                progress: Box::new(progress),
                source,
            }),
            None => Ok(progress),
        }
    }

    /// Pop this pass's segments, or say why it planned none. §3.2's
    /// gate is on segment planning and the record write alone — the
    /// partial sweep runs on every pass, witness or not.
    fn pop_segments(
        &mut self,
        record: Option<&reclaim::ReclaimRecord>,
        horizons: &SnapshotHorizons,
        budget: usize,
    ) -> (Vec<retain::Popped>, bool, PassOutcome) {
        let Some(checkpoint) = self.checkpoint else {
            return (
                Vec::new(),
                false,
                PassOutcome::Skipped(SkipReason::NoCheckpoint),
            );
        };
        if !self.checkpoint_is_settled() {
            return (
                Vec::new(),
                false,
                PassOutcome::Skipped(SkipReason::MigrationWindow),
            );
        }
        let (durable, occupied) = durable_rows(record);
        let bound = retain::PopBound {
            checkpoint,
            current: self.current_segment_uuid,
            tenant_aware: matches!(horizons, SnapshotHorizons::Known(_)),
            rows: retain::PlannedRows::new(&durable, occupied, self.row_capacity(record)),
        };
        let (popped, capped) = self.ledger.pop(bound, budget);
        (popped, capped, PassOutcome::Planned)
    }

    /// How many `planned` rows a record write may carry. The store's
    /// file can be wider than the configured cap, never narrower, and
    /// the open-time reconciliation empties the array, so the
    /// configured cap bounds every row this process writes. A root
    /// with no record writes none, and nothing bounds its pops here.
    fn row_capacity(&self, record: Option<&reclaim::ReclaimRecord>) -> usize {
        match record {
            Some(_) => self.pass_cap(),
            None => usize::MAX,
        }
    }

    /// Fold the unlink results back into the ledger and the record.
    fn settle_segments(
        &mut self,
        store: &mut Option<ReclaimStore>,
        popped: &[retain::Popped],
        unlinked: &Unlinked,
    ) -> Result<usize, ReclaimError> {
        let Some(store) = store.as_mut() else {
            return Ok(0);
        };
        let mut record = store.record().clone();
        let mut done = 0;
        for entry in popped {
            match (
                unlinked.removed.contains(&entry.path),
                unlinked.fsync_failed,
            ) {
                (true, false) => {
                    self.raise_entry(&mut record, entry.segment)?;
                    record.planned.retain(|p| p.segment != entry.segment);
                    self.unreclaimed_bytes = self
                        .unreclaimed_bytes
                        .saturating_sub(self.ledger.remove(entry.segment));
                    done += 1;
                }
                // One parent fsync covers every unlink of the pass, so
                // its failure marks the whole removed set uncertain.
                (true, true) => {
                    self.ledger.hold(entry.segment, true);
                    mark_uncertain(&mut record, entry.segment);
                }
                (false, _) => self.ledger.hold(entry.segment, entry.uncertain),
            }
        }
        store.adopt(record);
        Ok(done)
    }

    /// Raise every tenant named by a completed segment's `planned`
    /// entry, exactly as the reconciliation at open would.
    fn raise_entry(
        &self,
        record: &mut reclaim::ReclaimRecord,
        segment: uuid::Uuid,
    ) -> Result<(), ReclaimError> {
        let mode = match record.consumer_mode {
            reclaim::RecordedMode::Known => reclaim::EntryMode::Known,
            reclaim::RecordedMode::NoConsumer => reclaim::EntryMode::NoConsumer,
            reclaim::RecordedMode::Unrecorded => return Ok(()),
        };
        let Some(planned) = record
            .planned
            .iter()
            .find(|p| p.segment == segment)
            .cloned()
        else {
            return Ok(());
        };
        for (&id, &offset) in &planned.last_offsets {
            record
                .dictionary
                .raise_reclaimed(id, reclaim::Entry { mode, offset })
                .map_err(|e| {
                    self.housekeeping_failure(HousekeepingError::Io {
                        op: "raise(RECLAIM reclaimed_through)",
                        source: std::io::Error::new(
                            ErrorKind::InvalidData,
                            format!("planned segment {segment} names {e}"),
                        ),
                    })
                })?;
        }
        Ok(())
    }

    /// §3.2's precondition, checked from durable state: every pass on
    /// a root runs under the mode the record carries, and a
    /// disagreement is refused before anything is planned. A root that
    /// has checkpointed but never reclaimed has no entry to infer
    /// from, which is why the header and not an entry is the witness.
    fn refuse_mode_disagreement(
        &self,
        record: Option<&reclaim::ReclaimRecord>,
        horizons: &SnapshotHorizons,
    ) -> Result<(), ReclaimError> {
        let Some(record) = record else {
            return Ok(());
        };
        let recorded = record.consumer_mode;
        let attempted = pass::entry_mode(horizons);
        match recorded {
            reclaim::RecordedMode::Unrecorded => Ok(()),
            _ if recorded.admits(attempted) => Ok(()),
            _ => Err(
                self.housekeeping_failure(HousekeepingError::ModeDisagreement {
                    recorded: pass::mode_name(recorded),
                    attempted: pass::mode_name(pass::recorded_mode(attempted)),
                }),
            ),
        }
    }

    /// §3.2's halt-or-pin, and the legacy root's stale-gap belt.
    ///
    /// A `Known` entry whose tenant's restorable horizon is below it —
    /// including a tenant with an entry and no restorable snapshot at
    /// all — is unrecoverable state: the frames below that horizon are
    /// gone and a pin cannot rebuild what they held. A tenant with no
    /// entry has lost nothing and pins at its oldest surviving frame.
    /// An entry whose tenant has no surviving frame is satisfied by
    /// absence: there is no log left to rebuild from, and a tenant that
    /// writes again is back in the ledger and checked as before.
    fn refuse_unexplained_tenants(
        &self,
        record: Option<&reclaim::ReclaimRecord>,
        horizons: &SnapshotHorizons,
    ) -> Result<(), ReclaimError> {
        let SnapshotHorizons::Known(marks) = horizons else {
            return Ok(());
        };
        if let Some(record) = record {
            for (_, held) in record.dictionary.live() {
                let reclaim::SlotState::Live {
                    reclaimed_through: Some(entry),
                } = &held.state
                else {
                    continue;
                };
                if entry.mode != reclaim::EntryMode::Known
                    || !self.ledger.may_hold_frames(&held.key)
                {
                    continue;
                }
                let restorable = marks.get(&held.key).and_then(|h| h.restorable());
                if restorable.is_none_or(|horizon| horizon < entry.offset) {
                    return Err(self.unrecoverable(&held.key, entry.offset));
                }
            }
        }
        // A post-RFC root is unwitnessed too until its first checkpoint,
        // and its tenants have frames and no snapshot as a matter of
        // course: that pass is §3.2's skip, not a refusal (issue #889).
        match self.reclaim_gate {
            ReclaimGate::Unwitnessed if self.on_legacy_branch() => {
                self.refuse_legacy_stale_gaps(marks)
            }
            ReclaimGate::Unwitnessed | ReclaimGate::FsyncPending | ReclaimGate::Open => Ok(()),
        }
    }

    /// The legacy branch rests on "#793 means no served root ever
    /// reclaimed", and §3.2 belts rather than trusts it: a tenant
    /// whose snapshot does not restore and whose oldest surviving
    /// frame sits above its last recorded horizon is the shape
    /// reclamation under a version-1 checkpoint leaves behind. A
    /// tenant with no recorded horizon at all has nothing to compare,
    /// which is the one direction that could replay past reclaimed
    /// data, so it fails closed too.
    fn refuse_legacy_stale_gaps(
        &self,
        marks: &std::collections::HashMap<ourios_core::tenant::TenantId, TenantHorizon>,
    ) -> Result<(), ReclaimError> {
        match self.first_legacy_stale_gap(marks) {
            Some((tenant, horizon)) => Err(self.unrecoverable(&tenant, horizon)),
            None => Ok(()),
        }
    }

    /// The same belt, at startup and before anything replaces the
    /// snapshots it reads (RFC 0052 §3.2): on a pre-RFC root — the
    /// legacy branch with no `RECLAIM` record yet — a version-1
    /// artefact's mark is decoded for this check alone and offered as
    /// [`TenantHorizon::RecordedOnly`]. Checked here, the evidence is
    /// still on disk; once the post-recovery write has replaced those
    /// artefacts at version 2, every tenant reads as restorable and the
    /// pass-time check can no longer see a gap. Any other root is `Ok`:
    /// a post-RFC root without its first checkpoint has tenants with
    /// frames and no snapshot as a matter of course.
    ///
    /// # Errors
    ///
    /// [`HousekeepingError::Unrecoverable`] naming the first tenant
    /// whose oldest surviving frame no recorded horizon explains.
    pub fn refuse_legacy_stale_gaps_at_open(
        &self,
        marks: &std::collections::HashMap<ourios_core::tenant::TenantId, TenantHorizon>,
    ) -> Result<(), HousekeepingError> {
        match self.first_legacy_stale_gap(marks) {
            Some((tenant, horizon)) if self.on_legacy_branch() => {
                Err(HousekeepingError::Unrecoverable {
                    tenant: tenant.as_str().to_owned(),
                    horizon,
                })
            }
            Some(_) | None => Ok(()),
        }
    }

    /// Whether this is a pre-RFC root (RFC 0052 §3.2): still on the
    /// legacy branch, with no `RECLAIM` record yet. Only such a root is
    /// held to the startup legacy stale-gap check.
    #[must_use]
    pub fn on_legacy_branch(&self) -> bool {
        self.reclaim_gate == ReclaimGate::Unwitnessed && !self.reclaim.has_record()
    }

    /// The first tenant whose oldest surviving frame is not explained
    /// by its horizon, with the horizon to name. A tenant with no
    /// recorded horizon at all has nothing to compare, which is the one
    /// direction that could replay past reclaimed data, so it counts.
    fn first_legacy_stale_gap(
        &self,
        marks: &std::collections::HashMap<ourios_core::tenant::TenantId, TenantHorizon>,
    ) -> Option<(ourios_core::tenant::TenantId, WalOffset)> {
        self.ledger.tenants().into_iter().find_map(|tenant| {
            let oldest = self.ledger.oldest_frame(&tenant)?;
            match marks.get(&tenant) {
                Some(TenantHorizon::Restorable(_)) => None,
                Some(TenantHorizon::RecordedOnly(recorded)) if oldest <= *recorded => None,
                Some(TenantHorizon::RecordedOnly(recorded)) => Some((tenant, *recorded)),
                None => Some((tenant, oldest)),
            }
        })
    }

    fn unrecoverable(
        &self,
        tenant: &ourios_core::tenant::TenantId,
        horizon: WalOffset,
    ) -> ReclaimError {
        self.housekeeping_failure(HousekeepingError::Unrecoverable {
            tenant: tenant.as_str().to_owned(),
            horizon,
        })
    }

    /// A refusal, with the state as it stands. Every caller of this is
    /// a check that runs **before** `pop_segments`, so the pass planned
    /// nothing and the progress beside the error must not say it did.
    fn housekeeping_failure(&self, source: HousekeepingError) -> ReclaimError {
        ReclaimError::Housekeeping {
            progress: Box::new(self.progress(0, 0, PassOutcome::Skipped(SkipReason::Refused))),
            source,
        }
    }

    fn progress(
        &self,
        removed_segments: usize,
        removed_partials: usize,
        outcome: PassOutcome,
    ) -> HousekeepingProgress {
        let (lag_bytes, lag_segments) = self.ledger.lag(self.current_segment_uuid);
        HousekeepingProgress {
            removed_segments,
            removed_partials,
            capped: false,
            horizon_remaining: self.ledger.horizon_remaining(),
            unlink_remaining: self.ledger.unlink_remaining(self.current_segment_uuid),
            floor: self.ledger.floor(),
            lag_bytes,
            lag_segments,
            outcome,
        }
    }

    /// Paths a pass did not verify go back at the **head** of the
    /// list, in order: it is the sweep's only record, so anything
    /// dropped here becomes invisible to every later pass, and a path
    /// pushed to the tail would be retried only after every candidate
    /// behind it.
    fn requeue_partials(&mut self, paths: Vec<PathBuf>) {
        self.stale_partials.splice(..0, paths);
    }
}

impl ReclaimSlot {
    /// [`Wal::write_plan_record`] and [`crate::write_plan_record`]
    /// both land here, holding this slot's lock and nothing else.
    ///
    /// The plan is checked against the live-pass cell under the same
    /// lock the write holds, and every prepare and commit moves that
    /// cell only with the lock held, so "this plan is still the live
    /// one" stays true until the slot is written. The same lock is
    /// what keeps two record writes — two passes, or a pass and a
    /// checkpoint — from ever sharing the two-slot alternation.
    pub(crate) fn write_plan(
        &self,
        plan: &ReclaimPlan,
    ) -> Result<pass::UnlinkPermit, std::io::Error> {
        let mut held = self.lock();
        if held.closed() {
            return Err(PlanRefusal::Closed.error(plan));
        }
        match self.live().load(Ordering::Acquire) {
            // Not superseded but **settled**: a commit has already
            // taken this pass's outstanding state. Answering `Ok` here
            // would say the witness was written when none was, and the
            // caller's next `unlink_planned` would remove segments the
            // commit had returned to eligible with nothing on disk
            // accounting for them.
            0 => return Err(PlanRefusal::Settled.error(plan)),
            live => {
                let live = pass::PassId::new(self.instance(), live);
                if live != plan.pass {
                    return Err(PlanRefusal::Superseded(live).error(plan));
                }
            }
        }
        let permit = pass::UnlinkPermit::new(plan.pass, std::sync::Arc::clone(self.live()));
        if !plan.records {
            return Ok(permit);
        }
        let hook = held.hook();
        let Some(store) = held.as_mut() else {
            return Ok(permit);
        };
        let Some(record) = merge_plan(store.record(), plan, self.max_unlinks_per_pass())? else {
            return Ok(permit);
        };
        hook.run();
        store.commit(&record).map(|()| permit).map_err(|e| match e {
            reclaim_store::StoreError::Io { source, .. } => source,
            reclaim_store::StoreError::Corrupt { detail } => {
                std::io::Error::new(ErrorKind::InvalidData, detail)
            }
        })
    }
}

/// The segments the live record already holds a `planned` row for,
/// and how many rows it holds. The merge in [`ReclaimSlot::write_plan`]
/// reads the same record, and nothing between the two halves adds a
/// row: a later prepare supersedes this plan rather than writing
/// beside it, and a superseded plan's write is refused.
fn durable_rows(
    record: Option<&reclaim::ReclaimRecord>,
) -> (std::collections::BTreeSet<uuid::Uuid>, usize) {
    record.map_or_else(Default::default, |record| {
        let planned = &record.planned;
        (planned.iter().map(|p| p.segment).collect(), planned.len())
    })
}

/// Merge this pass's popped segments and its mode into `current`, the
/// record as it stands **now** rather than as prepare saw it. A pass
/// that plans nothing and owes no mode writes no record at all — and
/// a **skipped** pass writes none whatever it would otherwise owe:
/// §3.2's gate is on segment planning *and* the record write, because
/// a record written under a version-1 checkpoint is a witness to a
/// reclamation that never happened.
fn merge_plan(
    current: &reclaim::ReclaimRecord,
    plan: &ReclaimPlan,
    max_unlinks_per_pass: u32,
) -> Result<Option<reclaim::ReclaimRecord>, std::io::Error> {
    let invalid =
        |e: &dyn std::fmt::Display| std::io::Error::new(ErrorKind::InvalidData, e.to_string());
    let geometry = reconcile::geometry(max_unlinks_per_pass).map_err(|e| invalid(&e))?;
    let mut record = current.clone();
    // §3.2: the first pass adopts its own mode durably before it
    // unlinks anything. A root whose mode is already recorded was
    // checked for disagreement before anything was planned.
    let adopting = record.consumer_mode == reclaim::RecordedMode::Unrecorded;
    if adopting {
        record.consumer_mode = pass::recorded_mode(plan.mode);
    }
    for entry in &plan.segments {
        record.planned.retain(|p| p.segment != entry.segment);
        let planned = pass::plan_entry(
            &mut record.dictionary,
            geometry,
            entry.segment,
            entry.uncertain,
            &entry.last_offsets,
        )
        .map_err(|e| invalid(&e))?;
        record.planned.push(planned);
    }
    if adopting || !plan.segments.is_empty() {
        Ok(Some(record))
    } else {
        Ok(None)
    }
}
