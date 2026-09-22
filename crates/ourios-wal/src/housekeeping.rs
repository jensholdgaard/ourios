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
    /// segment header and lists no directory, and an append taken
    /// concurrently waits for O(cap) work whatever the backlog.
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
        let cap = max_unlinks.max(1);
        self.refuse_mode_disagreement(horizons)?;
        self.refuse_unexplained_tenants(horizons)?;
        // A plan that was never committed — the housekeeping task
        // panicked between the halves — strands nothing (§3.7). Its
        // segments are still marked reclaiming and are re-planned
        // below, but its partials left the sweep's list, which is
        // their only record: they go back on it first.
        if let Some(abandoned) = self.outstanding.take() {
            self.requeue_partials(abandoned.partials);
        }
        let horizons_capped = self.ledger.apply(horizons, cap);
        let take = self.stale_partials.len().min(cap);
        let partials: Vec<PathBuf> = self.stale_partials.drain(..take).collect();
        let budget = cap - partials.len();
        let (segments, pops_capped, outcome) = self.pop_segments(horizons, budget);
        let mut plan = ReclaimPlan {
            segments: Vec::new(),
            partials,
            record: None,
            root: self.config.root.clone(),
            progress: HousekeepingProgress {
                removed_segments: 0,
                removed_partials: 0,
                capped: horizons_capped || pops_capped,
                horizon_remaining: self.ledger.horizon_remaining(),
                unlink_remaining: self.ledger.unlink_remaining(self.current_segment_uuid),
                floor: self.ledger.floor(),
                lag_bytes: 0,
                lag_segments: 0,
                outcome,
            },
        };
        let (lag_bytes, lag_segments) = self.ledger.lag(self.floor_bound(horizons));
        plan.progress.lag_bytes = lag_bytes;
        plan.progress.lag_segments = lag_segments;
        self.merge_plan(&mut plan, &segments, pass::entry_mode(horizons))?;
        self.outstanding = Some(Outstanding {
            segments,
            partials: plan.partials.clone(),
            progress: plan.progress,
        });
        Ok(plan)
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
    /// # Errors
    ///
    /// The slot write or its fsync failed; nothing has been unlinked,
    /// and [`ReclaimOutcome::RecordFailed`] is what
    /// [`Self::housekeeping_commit`] expects in response.
    pub fn write_plan_record(&mut self, plan: &ReclaimPlan) -> Result<(), std::io::Error> {
        match (plan.record.as_ref(), self.reclaim.as_mut()) {
            (Some(record), Some(store)) => store.commit(record).map_err(|e| match e {
                reclaim_store::StoreError::Io { source, .. } => source,
                reclaim_store::StoreError::Corrupt { detail } => {
                    std::io::Error::new(ErrorKind::InvalidData, detail)
                }
            }),
            // A plan carrying a record is only ever produced from a
            // root that has one; a plan without one has nothing to
            // write.
            (Some(_), None) | (None, _) => Ok(()),
        }
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
    /// # Errors
    ///
    /// [`ReclaimError::Housekeeping`] when a planned segment names a
    /// slot id the dictionary cannot raise — a record that cannot be
    /// told apart from a loss.
    pub fn housekeeping_commit(
        &mut self,
        outcome: ReclaimOutcome,
    ) -> Result<HousekeepingProgress, ReclaimError> {
        let Some(outstanding) = self.outstanding.take() else {
            return Ok(self.progress(0, 0, PassOutcome::Skipped(SkipReason::NoCheckpoint)));
        };
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
    /// [`Self::housekeeping_commit`].
    pub fn housekeeping_pass(
        &mut self,
        horizons: &SnapshotHorizons,
        max_unlinks: usize,
    ) -> Result<HousekeepingProgress, ReclaimError> {
        let plan = self.housekeeping_prepare(horizons, max_unlinks)?;
        match self.write_plan_record(&plan) {
            Ok(()) => self.housekeeping_commit(unlink_planned(&plan)),
            Err(source) => self.housekeeping_commit(ReclaimOutcome::RecordFailed(source)),
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
        &mut self,
        plan: &mut ReclaimPlan,
        popped: &[retain::Popped],
        mode: reclaim::EntryMode,
    ) -> Result<(), ReclaimError> {
        if plan.progress.outcome != PassOutcome::Planned {
            return Ok(());
        }
        let Some(store) = self.reclaim.as_ref() else {
            return Ok(());
        };
        let geometry = reconcile::configured_geometry(&self.config).map_err(|e| {
            self.housekeeping_failure(HousekeepingError::Io {
                op: "sizing(RECLAIM)",
                source: std::io::Error::new(ErrorKind::InvalidData, e.to_string()),
            })
        })?;
        let mut record = store.record().clone();
        // §3.2: the first pass adopts its own mode durably before it
        // unlinks anything. A root whose mode is already recorded was
        // checked for disagreement before anything was planned.
        let adopting = record.consumer_mode == reclaim::RecordedMode::Unrecorded;
        if adopting {
            record.consumer_mode = pass::recorded_mode(mode);
        }
        for entry in popped {
            record.planned.retain(|p| p.segment != entry.segment);
            let planned = pass::plan_entry(
                &mut record.dictionary,
                geometry,
                entry.segment,
                entry.uncertain,
                &entry.last_offsets,
            )
            .map_err(|e| {
                self.housekeeping_failure(HousekeepingError::Io {
                    op: "assign(RECLAIM slot id)",
                    source: std::io::Error::new(ErrorKind::InvalidData, e.to_string()),
                })
            })?;
            plan.segments.push(PlannedSegment {
                unlink: planned.clone(),
                path: entry.path.clone(),
            });
            record.planned.push(planned);
        }
        if adopting || !popped.is_empty() {
            plan.record = Some(record);
        }
        Ok(())
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

    /// The bound the lag figures are taken against: the floor when one
    /// governs, the checkpoint when it alone does.
    fn floor_bound(&self, horizons: &SnapshotHorizons) -> Option<WalOffset> {
        match horizons {
            SnapshotHorizons::NoConsumer => self.checkpoint,
            SnapshotHorizons::Known(_) => match (self.ledger.floor().offset(), self.checkpoint) {
                (Some(floor), Some(checkpoint)) => Some(floor.min(checkpoint)),
                (floor, checkpoint) => floor.or(checkpoint),
            },
        }
    }

    fn progress(
        &self,
        removed_segments: usize,
        removed_partials: usize,
        outcome: PassOutcome,
    ) -> HousekeepingProgress {
        HousekeepingProgress {
            removed_segments,
            removed_partials,
            capped: false,
            horizon_remaining: self.ledger.horizon_remaining(),
            unlink_remaining: self.ledger.unlink_remaining(self.current_segment_uuid),
            floor: self.ledger.floor(),
            lag_bytes: 0,
            lag_segments: 0,
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
