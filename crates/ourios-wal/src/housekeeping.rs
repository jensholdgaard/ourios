//! RFC 0052 §3.2's housekeeping pass on the WAL itself: the ledger
//! half under the single-writer position, the record half that must
//! precede the unlinks, and the ledger half again with what the file
//! half did.
//!
//! The values that travel between the halves are [`crate::pass`]'s;
//! this is the [`Wal`] side of them.

use std::io::ErrorKind;
use std::path::PathBuf;

use crate::pass::{
    HousekeepingProgress, PassOutcome, PlannedSegment, ReclaimError, ReclaimOutcome, ReclaimPlan,
    SkipReason,
};
use crate::retain::{SnapshotHorizons, TenantHorizon};
use crate::{
    HousekeepingError, Outstanding, ReclaimGate, Wal, WalOffset, mark_uncertain, pass, reclaim,
    reclaim_store, reconcile, retain, unlink_planned,
};

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

/// A plan whose pass is no longer the outstanding one (RFC 0052 §3.7).
fn superseded(plan: &ReclaimPlan, outstanding: pass::PassId) -> std::io::Error {
    std::io::Error::new(
        ErrorKind::InvalidInput,
        format!(
            "WAL housekeeping refused: plan from pass {} was superseded by pass {outstanding} \
             (RFC 0052 §3.7)",
            plan.pass,
        ),
    )
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
    /// The returned [`ReclaimPlan`] is owned, so the file half needs
    /// neither the guard nor a WAL handle; feed its outcome back
    /// through [`Self::housekeeping_commit`].
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
        if let Some(abandoned) = self.outstanding.take() {
            self.withdraw_across_modes(&abandoned, horizons);
            self.requeue_partials(abandoned.partials);
        }
        self.refuse_mode_disagreement(horizons)?;
        self.refuse_unexplained_tenants(horizons)?;
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
        let (segments, pops_capped, outcome) = self.pop_segments(horizons, budget);
        let (lag_bytes, lag_segments) = self.ledger.lag(self.current_segment_uuid);
        self.passes += 1;
        let pass = pass::PassId::new(self.instance, self.passes);
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
        };
        self.outstanding = Some(Outstanding {
            pass,
            segments,
            partials: plan.partials.clone(),
            mode: pass::entry_mode(horizons),
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
    /// # Errors
    ///
    /// The plan was superseded, the merge could not assign a slot id,
    /// or the slot write or its fsync failed. Nothing has been
    /// unlinked either way, and [`ReclaimOutcome::RecordFailed`] is
    /// what [`Self::housekeeping_commit`] expects in response.
    pub fn write_plan_record(&mut self, plan: &ReclaimPlan) -> Result<(), std::io::Error> {
        let mode = match self.outstanding.as_ref() {
            Some(outstanding) if outstanding.pass == plan.pass => outstanding.mode,
            Some(outstanding) => return Err(superseded(plan, outstanding.pass)),
            None => return Ok(()),
        };
        if !plan.records {
            return Ok(());
        }
        let Some(record) = self.merge_plan(plan, mode)? else {
            return Ok(());
        };
        let Some(store) = self.reclaim.as_mut() else {
            return Ok(());
        };
        store.commit(&record).map_err(|e| match e {
            reclaim_store::StoreError::Io { source, .. } => source,
            reclaim_store::StoreError::Corrupt { detail } => {
                std::io::Error::new(ErrorKind::InvalidData, detail)
            }
        })
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
        let Some(outstanding) = self.outstanding.take() else {
            return Ok(self.progress(0, 0, PassOutcome::Skipped(SkipReason::NoCheckpoint)));
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
        let removed: std::collections::HashSet<PathBuf> = removed.into_iter().collect();
        let segments = self.settle_segments(&outstanding.segments, &removed, fsync_failed)?;
        let partials = self.settle_partials(outstanding.partials, &removed, fsync_failed);
        Ok(self.settled(outstanding.progress, segments, partials))
    }

    /// Count the partials whose removal is verified and put the rest
    /// back on the sweep's list. An uncertain deletion is not verified:
    /// one parent fsync covers the whole pass, so its failure requeues
    /// every path the pass removed.
    fn settle_partials(
        &mut self,
        popped: Vec<PathBuf>,
        removed: &std::collections::HashSet<PathBuf>,
        fsync_failed: bool,
    ) -> usize {
        let (done, requeued): (Vec<PathBuf>, Vec<PathBuf>) = popped
            .into_iter()
            .partition(|path| removed.contains(path) && !fsync_failed);
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
        let Err(source) = self.write_plan_record(&plan) else {
            return self.settle_unlinks(plan.pass, unlink_planned(&plan));
        };
        // The commit is what puts the popped entries back and requeues
        // the partials, so it runs either way — but the failure is the
        // caller's to see. Swallowing it would report a pass that could
        // not make its witness durable as a clean one, and §3.1's
        // fail-closed rule is that such a failure is logged and the
        // next pass retries.
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
        let bound = retain::PopBound {
            checkpoint,
            current: self.current_segment_uuid,
            tenant_aware: matches!(horizons, SnapshotHorizons::Known(_)),
        };
        let (popped, capped) = self.ledger.pop(bound, budget);
        (popped, capped, PassOutcome::Planned)
    }

    /// Merge this pass's popped segments and its mode into the record
    /// the file half will write. A pass that plans nothing and owes no
    /// mode writes no record at all — and a **skipped** pass writes
    /// none whatever it would otherwise owe: §3.2's gate is on segment
    /// planning *and* the record write, because a record written under
    /// a version-1 checkpoint is a witness to a reclamation that never
    /// happened.
    fn merge_plan(
        &self,
        plan: &ReclaimPlan,
        mode: reclaim::EntryMode,
    ) -> Result<Option<reclaim::ReclaimRecord>, std::io::Error> {
        let Some(store) = self.reclaim.as_ref() else {
            return Ok(None);
        };
        let invalid =
            |e: &dyn std::fmt::Display| std::io::Error::new(ErrorKind::InvalidData, e.to_string());
        let geometry = reconcile::configured_geometry(&self.config).map_err(|e| invalid(&e))?;
        // The record as it is **now**, not as prepare saw it.
        let mut record = store.record().clone();
        // §3.2: the first pass adopts its own mode durably before it
        // unlinks anything. A root whose mode is already recorded was
        // checked for disagreement before anything was planned.
        let adopting = record.consumer_mode == reclaim::RecordedMode::Unrecorded;
        if adopting {
            record.consumer_mode = pass::recorded_mode(mode);
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
        // A pass that plans nothing and owes no mode writes no record.
        if adopting || !plan.segments.is_empty() {
            Ok(Some(record))
        } else {
            Ok(None)
        }
    }

    /// Fold the unlink results back into the ledger and the record.
    fn settle_segments(
        &mut self,
        popped: &[retain::Popped],
        removed: &std::collections::HashSet<PathBuf>,
        fsync_failed: bool,
    ) -> Result<usize, ReclaimError> {
        let Some(store) = self.reclaim.as_ref() else {
            return Ok(0);
        };
        let mut record = store.record().clone();
        let mut done = 0;
        for entry in popped {
            match (removed.contains(&entry.path), fsync_failed) {
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
        if let Some(store) = self.reclaim.as_mut() {
            store.adopt(record);
        }
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
    fn refuse_mode_disagreement(&self, horizons: &SnapshotHorizons) -> Result<(), ReclaimError> {
        let Some(store) = self.reclaim.as_ref() else {
            return Ok(());
        };
        let recorded = store.record().consumer_mode;
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
    fn refuse_unexplained_tenants(&self, horizons: &SnapshotHorizons) -> Result<(), ReclaimError> {
        let SnapshotHorizons::Known(marks) = horizons else {
            return Ok(());
        };
        if let Some(store) = self.reclaim.as_ref() {
            for (_, held) in store.record().dictionary.live() {
                let reclaim::SlotState::Live {
                    reclaimed_through: Some(entry),
                } = &held.state
                else {
                    continue;
                };
                if entry.mode != reclaim::EntryMode::Known {
                    continue;
                }
                let restorable = marks.get(&held.key).and_then(|h| h.restorable());
                if restorable.is_none_or(|horizon| horizon < entry.offset) {
                    return Err(self.unrecoverable(&held.key, entry.offset));
                }
            }
        }
        match self.reclaim_gate {
            ReclaimGate::Unwitnessed => self.refuse_legacy_stale_gaps(marks),
            ReclaimGate::FsyncPending | ReclaimGate::Open => Ok(()),
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
        for tenant in self.ledger.tenants() {
            let Some(oldest) = self.ledger.oldest_frame(&tenant) else {
                continue;
            };
            match marks.get(&tenant) {
                Some(TenantHorizon::Restorable(_)) => {}
                Some(TenantHorizon::RecordedOnly(recorded)) if oldest <= *recorded => {}
                Some(TenantHorizon::RecordedOnly(recorded)) => {
                    return Err(self.unrecoverable(&tenant, *recorded));
                }
                None => return Err(self.unrecoverable(&tenant, oldest)),
            }
        }
        Ok(())
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

    fn housekeeping_failure(&self, source: HousekeepingError) -> ReclaimError {
        ReclaimError::Housekeeping {
            progress: Box::new(self.progress(0, 0, PassOutcome::Planned)),
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
