//! RFC 0052 §3.1's publication barrier: the cut, its ordering, and the
//! checkpoint it stamps.
//!
//! `flush_then_snapshot` already computed the predicate a checkpoint
//! needs — in-flight encodes quiesced, drained publishes settled, both
//! sinks fully drained — but it was reachable only behind an append.
//! An idle node therefore never advanced its checkpoint again, and an
//! idle node is exactly the one whose retained segments have the least
//! reason to exist. So the same barrier runs on its own timer, and the
//! rotation hook becomes capture-only: both hand a **cut** to one owner
//! of barrier I/O rather than doing PUTs inside an ingest turn.
//!
//! A cut is captured under the pipeline's `ingest_bound` exclusion — the
//! quiesce, the mark read, the two drains and the snapshot serialisation
//! — and nothing else runs there. The flush, the snapshot installs and
//! the checkpoint all run outside it, so the barrier's *own* PUTs never
//! stall ingest: a cut costs a drain, not a round trip to the store.
//!
//! One PUT can still land inside the exclusion, and it is not the
//! barrier's: the capture's `quiesce_encodes` waits out an encode worker
//! that may be inside `emit_concurrent`, whose size/ceiling take
//! publishes straight from the worker. A slow store therefore holds the
//! exclusion for that worker's put. Routing those takes through the
//! coordinator — the `detach_concurrent` seam — is issue #834; until
//! then the exclusion's worst case is one in-flight encode's PUT, not a
//! whole cut's.
//!
//! Three rules make the ordering sound, and each is a §5 criterion:
//!
//! - **The slot is one cut deep.** A capture arriving while a cut is
//!   pending coalesces into it; one arriving while nothing is pending
//!   fills it. Coalescing is all-or-nothing and bounded by the sink's
//!   own `ceiling_bytes`: a capture that would take the pending cut past
//!   it parks *every* batch it drained and advances nothing. Parking
//!   only the excess would move the pending cut's mark above frames
//!   sitting in the buffers rather than in its batches, and `run_cut`
//!   would checkpoint through them.
//! - **Cuts are strictly ordered.** A capture holds the exclusion across
//!   the handoff as well as the cut, so the slot never sees a later cut
//!   before an earlier one — and the fold itself is written to be
//!   order-independent, so an inversion could not move the pending cut's
//!   horizon backwards. A pending cut's snapshot bytes
//!   already fold frames whose only durable copy is the running cut's
//!   batches, so it never installs or stamps before the running cut's
//!   outcome — and when that cut *fails*, the pending one is
//!   invalidated: its bytes discarded, its batches merged back into the
//!   buffers, its mark withdrawn.
//! - **The latch is checked twice.** Once before the cut and again
//!   immediately before the install and the stamp, after
//!   `quiesce_publishes` has returned. A publish registered before the
//!   barrier began can panic *while the barrier waits on it*, and the
//!   latch it sets lands after the first check.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use ourios_core::tenant::TenantId;
use ourios_miner::cluster::MinerCluster;
use ourios_miner::snapshot::{SnapshotState, WalHighWater};
use ourios_wal::WalOffset;

use crate::cadence::{BarrierEpochs, Epoch};
use crate::publish::{Drained, PublishCoordinator};
use crate::receiver::CommitCoordinator;
use crate::receiver::pipeline::IngestPipeline;
use crate::snapshot_store;

/// One capture: the frames at or below `mark` taken out of both sinks,
/// plus each tenant's miner state as of that instant.
///
/// No public fields: a caller that could raise `mark` without owning the
/// matching batches would checkpoint over records that are still in the
/// buffers, which is the one thing this type exists to prevent.
pub struct Cut {
    epoch: Epoch,
    mark: Option<WalOffset>,
    drained: Vec<Drained>,
    snapshots: Vec<(TenantId, SnapshotState)>,
    bytes: usize,
}

impl Cut {
    /// The cut's number — the epoch every guard registered before its
    /// capture carries.
    #[must_use]
    pub fn epoch(&self) -> Epoch {
        self.epoch
    }

    /// The high-water mark this cut would checkpoint at, if any.
    #[must_use]
    pub fn mark(&self) -> Option<WalOffset> {
        self.mark
    }

    /// Estimated bytes of acknowledged records the cut holds outside the
    /// sink's own accounting.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Fold `other` into this cut — §3.1's coalesce, past the ceiling
    /// check.
    ///
    /// The batches always join; the epoch, the mark and the per-tenant
    /// snapshots come from whichever of the two captures is the **later**
    /// one, so the fold is independent of the order the two arrive in.
    /// The marks themselves cannot decide it: a `WalOffset` orders on a
    /// segment uuid, which says nothing about which cut was taken first.
    fn absorb(&mut self, other: Cut) {
        let Cut {
            epoch,
            mark,
            drained,
            snapshots,
            bytes,
        } = other;
        self.bytes = self.bytes.saturating_add(bytes);
        self.drained.extend(drained);
        match epoch.cmp(&self.epoch) {
            std::cmp::Ordering::Greater => {
                self.epoch = epoch;
                self.mark = mark.or(self.mark);
                merge_snapshots(&mut self.snapshots, snapshots);
            }
            // No two captures share an epoch, so `Equal` is unreachable;
            // it folds with the older arm, which advances nothing.
            std::cmp::Ordering::Less | std::cmp::Ordering::Equal => {
                self.mark = self.mark.or(mark);
                backfill_snapshots(&mut self.snapshots, snapshots);
            }
        }
    }
}

/// What one capture did to the pending slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureOutcome {
    /// The slot was empty and now holds this cut.
    Filled,
    /// The slot held a cut and this capture folded into it.
    Coalesced,
    /// Coalescing would have taken the pending cut past the sink's
    /// ceiling, so every drained batch was parked and nothing advanced.
    Parked,
}

/// What one cut's run decided.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CutOutcome {
    /// Snapshots installed and the checkpoint advanced — or there was no
    /// mark to stamp, which is the same decision with nothing to record.
    Stamped,
    /// A sink retained something, so neither the snapshots nor the mark
    /// moved. The records are back in the buffers and the next cut
    /// covers them.
    Retained,
    /// The cadence latch held an epoch at or below this cut's.
    Latched,
    /// Nothing was pending.
    Idle,
}

/// What one cut's snapshot install did. Three states, not a `bool`: a
/// cut whose mark is below the installed one did nothing *and* may
/// stamp, which is not the same decision as a write that failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Install {
    /// Every tenant's artefact was written and renamed.
    Written,
    /// Nothing to install — no mark, or an older cut behind a newer
    /// snapshot.
    Superseded,
    /// A write failed, so the horizon on disk is behind this cut's.
    Failed,
}

/// The barrier's owner: the pending slot, the snapshot-install
/// serialisation, and the reach into the journal for the stamp.
pub struct Barrier {
    publish: PublishCoordinator,
    coordinator: Arc<CommitCoordinator>,
    snapshots_root: PathBuf,
    epochs: Arc<BarrierEpochs>,
    /// The single pending slot (§3.1) — metadata *and* the drained
    /// batches, which is why coalescing is bounded by bytes.
    pending: Mutex<Option<Cut>>,
    /// Serialises the whole temp-write / fsync / rename of every
    /// snapshot file across the timer, the rotation hook and shutdown,
    /// and carries the highest mark installed so an older cut cannot
    /// overwrite a newer snapshot.
    install: Mutex<Option<WalOffset>>,
    ceiling_bytes: usize,
}

impl Barrier {
    /// Build the barrier over the sinks it drains, the journal owner it
    /// stamps through, and the snapshots root it installs into.
    ///
    /// `ceiling_bytes` is the sink's own limit, not a new number: §3.1
    /// bounds coalescing by the figure the buffers already answer to,
    /// because RFC 0053 owns the client-facing one.
    #[must_use]
    pub fn new(
        publish: PublishCoordinator,
        coordinator: Arc<CommitCoordinator>,
        snapshots_root: PathBuf,
        ceiling_bytes: usize,
    ) -> Self {
        let epochs = publish.record().epochs();
        Self {
            publish,
            coordinator,
            snapshots_root,
            epochs,
            pending: Mutex::new(None),
            install: Mutex::new(None),
            ceiling_bytes,
        }
    }

    /// The cadence state this barrier decides against.
    #[must_use]
    pub fn epochs(&self) -> Arc<BarrierEpochs> {
        Arc::clone(&self.epochs)
    }

    /// Capture a cut under the ingest exclusion and hand it to the
    /// pending slot.
    ///
    /// `rotate_when_idle` is the timer's leg: an idle node's last
    /// segment is rotated **first, under the same exclusion**, so no
    /// turn can run between the rotate and the mark read — every frame
    /// at or below the mark is then in the closed segment and every
    /// frame appended after the release is in the new one, above it. The
    /// mark itself is always `last_durable`, the last *acknowledged*
    /// turn's own frame offset, never the rotation boundary.
    pub fn capture(&self, pipeline: &IngestPipeline, rotate_when_idle: bool) -> CaptureOutcome {
        // The offer runs **under the exclusion**, not after it. Released
        // first, the rotation hook could capture a newer cut and fill the
        // slot in the gap, and this — older — cut would then fold over
        // its epoch, its mark and its snapshots. `capture_rotation` runs
        // under the shared side of this same lock, so holding it across
        // the handoff is what orders the two.
        let _bound = pipeline.exclude_ingest();
        pipeline.quiesce_encodes();
        if rotate_when_idle {
            self.rotate_idle();
        }
        let epoch = self.epochs.open_cut();
        let mark = pipeline.acknowledged_durable();
        let (drained, snapshots) =
            pipeline.with_miner(|miner| (self.publish.drain_all(), serialise(miner)));
        self.offer(Cut {
            epoch,
            mark,
            bytes: drained.estimated_bytes(),
            drained: vec![drained],
            snapshots,
        })
    }

    /// Capture a cut from inside an ingest turn that has already
    /// observed a segment change — the rotation hook, now capture-only.
    ///
    /// The caller holds the exclusion and the miner lock, and `mark` is
    /// the rotation point (`prev`), not `last_durable`: the frames at or
    /// below it are exactly the ones the closed segment holds.
    pub fn capture_rotation(&self, miner: &MinerCluster, mark: WalOffset) -> CaptureOutcome {
        let drained = self.publish.drain_all();
        let cut = Cut {
            epoch: self.epochs.open_cut(),
            mark: Some(mark),
            bytes: drained.estimated_bytes(),
            drained: vec![drained],
            snapshots: serialise(miner),
        };
        self.offer(cut)
    }

    /// Run the pending cut, if any — outside the exclusion.
    pub fn run_pending(&self) -> CutOutcome {
        let Some(cut) = self.take_pending() else {
            return CutOutcome::Idle;
        };
        let epoch = cut.epoch;
        let outcome = self.run_cut(cut);
        // §3.1: a pending cut captured behind a cut that did **not**
        // stamp already folds frames whose only durable copy was that
        // cut's batches, so installing or stamping it would suppress
        // records now sitting back in the buffers. Its own batches are
        // unflushed — the task is sequential — so merging them back
        // loses nothing, and the next capture covers everything either
        // cut held. It also releases the publish guards those batches
        // hold: left in the slot they would stall `quiesce_publishes`
        // for the life of the process, shutdown included.
        if outcome != CutOutcome::Stamped {
            self.invalidate_pending();
        }
        self.publish.record().settle_cut(epoch);
        outcome
    }

    /// One barrier tick: capture, then run. A panic anywhere inside is
    /// caught and lowers the latch to the tick's own epoch, so the
    /// barrier task takes the next tick instead of dying with the
    /// process's only stamping path.
    pub fn tick(&self, pipeline: &IngestPipeline, rotate_when_idle: bool) -> CutOutcome {
        let epoch = self.epochs.current();
        // §3.1 checks the latch *before* the cut, and this is where that
        // check belongs: `run_cut`'s is the authoritative one, but it
        // runs after the capture has already quiesced the pool, rotated
        // an idle segment and emptied both sinks. A latched node would
        // keep doing all three on every tick — mutating WAL and sink
        // state for a cut that cannot stamp — instead of standing still
        // until a restart.
        if self.epochs.capture().refuses(epoch) {
            // The rotation hook does not consult the latch — it runs on
            // the request path, where a cut it cannot take is still a
            // drain it must not lose. So a latched node keeps acquiring
            // pending cuts, and this early return is the only place left
            // that can settle them: left in the slot their publish
            // guards never drop, and `quiesce_publishes` waits on them
            // for the life of the process, shutdown included.
            self.invalidate_pending();
            // Each of those parks dates a settlement, and on a latched
            // node nothing ever retires them: `run_pending`'s
            // `settle_cut` is on the path this return skips. A
            // settlement dated at or below the epoch current now refuses
            // no cut a later tick could take — `refuses` needs
            // `epoch < at` — so settling against it here is what keeps
            // the list from growing for the life of the process.
            self.publish.record().settle_cut(self.epochs.current());
            return CutOutcome::Latched;
        }
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.capture(pipeline, rotate_when_idle);
            self.run_pending()
        }));
        if let Ok(outcome) = outcome {
            return outcome;
        }
        // The tick panicked outside any batch guard, so nothing reported
        // for it. Lower the latch to this tick's epoch and drop whatever
        // it left in the slot: the frames stay in the WAL and a restart
        // replays them.
        self.epochs.report(epoch);
        self.invalidate_pending();
        CutOutcome::Latched
    }

    /// The pending cut's mark, for tests and the status surface.
    #[must_use]
    pub fn pending_mark(&self) -> Option<WalOffset> {
        self.lock_pending().as_ref().and_then(Cut::mark)
    }

    /// The pending cut's epoch, for tests and the status surface. A
    /// coalesced capture adopts its epoch; a parked one leaves it, which
    /// is the difference RFC0052.14's ceiling leg is about.
    #[must_use]
    pub fn pending_epoch(&self) -> Option<Epoch> {
        self.lock_pending().as_ref().map(Cut::epoch)
    }

    /// Fill, coalesce into, or park against the pending slot (§3.1).
    fn offer(&self, cut: Cut) -> CaptureOutcome {
        let mut pending = self.lock_pending();
        let Some(existing) = pending.as_mut() else {
            *pending = Some(cut);
            return CaptureOutcome::Filled;
        };
        if existing.bytes.saturating_add(cut.bytes) > self.ceiling_bytes {
            drop(pending);
            self.park(cut);
            return CaptureOutcome::Parked;
        }
        existing.absorb(cut);
        CaptureOutcome::Coalesced
    }

    /// All-or-nothing: put every batch this capture drained back into
    /// the buffers as `ready`, dated with the capture's own epoch, and
    /// advance nothing.
    fn park(&self, cut: Cut) {
        for batch in cut.drained {
            self.publish.park(batch);
        }
    }

    /// Discard a pending cut: its snapshot bytes go, its batches go back
    /// into the buffers, its mark is withdrawn and its epoch is spent.
    fn invalidate_pending(&self) {
        let Some(cut) = self.take_pending() else {
            return;
        };
        self.park(cut);
    }

    fn take_pending(&self) -> Option<Cut> {
        self.lock_pending().take()
    }

    fn lock_pending(&self) -> std::sync::MutexGuard<'_, Option<Cut>> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// §3.3's idle rotation, under the exclusion and before the cut. A
    /// failure is logged rather than raised: the rotation is
    /// discretionary, and refusing to cut over it would stop reclamation
    /// on exactly the node whose segments have least reason to exist.
    fn rotate_idle(&self) {
        if let Err(e) = self.coordinator.rotate_if_aged() {
            tracing::warn!(
                error = %e,
                "barrier: the idle rotation failed; the cut proceeds against the open segment"
            );
        }
    }

    /// The store-I/O half, outside the exclusion (§3.1's pseudocode from
    /// `cut_ok` on).
    fn run_cut(&self, cut: Cut) -> CutOutcome {
        let Cut {
            epoch,
            mark,
            drained,
            snapshots,
            ..
        } = cut;
        if self.epochs.capture().refuses(epoch) {
            // Nothing in the store yet, so the batches go back where a
            // later cut can find them.
            for batch in drained {
                self.publish.park(batch);
            }
            return CutOutcome::Latched;
        }
        let mut published = true;
        for batch in drained {
            published &= self.publish.write_ordered(batch, "barrier");
        }
        if !published {
            return CutOutcome::Retained;
        }
        // Publishes registered *before* this cut settle here — the wait
        // §3.1 has always made, now also reporting.
        let outcomes = self.publish.record().quiesce_publishes();
        // Two independent refusals, and the order is only about which
        // one is *named*. The recheck defends the ordering: a publish
        // registered before the barrier began can panic while it waits
        // above, and the latch it sets lands after the first check. The
        // outcome defends the data: a failure in any of those publishes
        // means no stamp even though the cut's own flush succeeded.
        if self.epochs.capture().refuses(epoch) {
            return CutOutcome::Latched;
        }
        if !outcomes.all_ok(epoch) {
            return CutOutcome::Retained;
        }
        // A failed install must not be followed by a stamp. §3.1 says a
        // snapshot write failure is not a checkpoint blocker, and adds
        // the condition that makes that safe: recovery must gate the
        // *Parquet* side on `max(X, S)`. `recovery::DriverSink` does not
        // yet — that gate is RFC0052.10's — so advancing `X` over a
        // snapshot still at `S` would republish every row in `(S, X]`
        // on the next start. Until the gate lands, a failed install
        // costs one cadence rather than duplicate rows.
        match self.install(&snapshots, mark) {
            Install::Failed => CutOutcome::Retained,
            Install::Written | Install::Superseded => {
                self.stamp(mark);
                CutOutcome::Stamped
            }
        }
    }

    /// Install this cut's snapshot bytes, serialised against every other
    /// installer and monotone in the mark.
    ///
    /// A cut with no mark installs nothing: an artefact without a
    /// concrete horizon is discarded at the next start, so writing one
    /// over a tenant's only valid snapshot would trade a full replay for
    /// a cut that had nothing to stamp anyway.
    fn install(&self, snapshots: &[(TenantId, SnapshotState)], mark: Option<WalOffset>) -> Install {
        let Some(mark) = mark else {
            return Install::Superseded;
        };
        let mut installed = self.install.lock().unwrap_or_else(PoisonError::into_inner);
        if installed.is_some_and(|previous| mark < previous) {
            return Install::Superseded;
        }
        let high_water = WalHighWater {
            segment: mark.segment.to_string(),
            byte: mark.byte,
        };
        for (tenant, state) in snapshots {
            let mut state = state.clone();
            state.wal_high_water = Some(high_water.clone());
            if let Err(e) = snapshot_store::write(&self.snapshots_root, tenant, &state) {
                tracing::warn!(
                    name: ourios_semconv::EVENT_OURIOS_RECEIVER_SNAPSHOT_ERROR,
                    error = %e,
                    "barrier: snapshot write failed, so this cut does not stamp; the next \
                     one retries (no acknowledged data is lost — the WAL is durable)"
                );
                return Install::Failed;
            }
        }
        *installed = Some(mark);
        Install::Written
    }

    /// §6.7's monotone, idempotent stamp. A failure logs and leaves the
    /// in-memory mark where it was, so nothing past the *previous* mark
    /// becomes reclaimable — segments already eligible under it still
    /// are.
    fn stamp(&self, mark: Option<WalOffset>) {
        let Some(mark) = mark else {
            return;
        };
        if let Err(e) = self.coordinator.checkpoint(mark) {
            tracing::warn!(
                error = %e,
                "barrier: the checkpoint write failed; nothing past the previous mark is reclaimed"
            );
        }
    }
}

/// Each tenant's miner state as of the cut, serialised under the miner
/// lock so it never runs past the mark.
fn serialise(miner: &MinerCluster) -> Vec<(TenantId, SnapshotState)> {
    miner
        .tenant_ids()
        .into_iter()
        .map(|tenant| {
            let state = miner.snapshot_state(&tenant);
            (tenant, state)
        })
        .collect()
}

/// Latest wins per tenant; a tenant absent from the newer capture keeps
/// the older bytes, which are still cut-consistent at its own horizon.
fn merge_snapshots(
    into: &mut Vec<(TenantId, SnapshotState)>,
    newer: Vec<(TenantId, SnapshotState)>,
) {
    for (tenant, state) in newer {
        match into.iter_mut().find(|(existing, _)| *existing == tenant) {
            Some(slot) => slot.1 = state,
            None => into.push((tenant, state)),
        }
    }
}

/// The same rule from the other side: `into` is already the later
/// capture, so `older` only fills the tenants it has no bytes for.
fn backfill_snapshots(
    into: &mut Vec<(TenantId, SnapshotState)>,
    older: Vec<(TenantId, SnapshotState)>,
) {
    for (tenant, state) in older {
        if !into.iter().any(|(existing, _)| *existing == tenant) {
            into.push((tenant, state));
        }
    }
}

/// The snapshots root is fsynced — and its parent with it — before any
/// listed artefact is read as a horizon (RFC0052.13).
///
/// A failure **fails startup** rather than discarding the snapshots:
/// reclamation may already have removed the frames they cover, so a
/// horizon whose directory entry may not be durable must never govern
/// reclamation, and no state only a snapshot could rebuild may be thrown
/// away on the strength of a directory read that might not survive.
///
/// # Errors
///
/// [`snapshot_store::SnapshotStoreError::Io`] when either fsync fails.
pub fn fsync_snapshots_root(root: &Path) -> Result<(), snapshot_store::SnapshotStoreError> {
    snapshot_store::fsync_root(root)
}

#[cfg(test)]
mod tests {
    use ourios_core::tenant::TenantId;
    use ourios_miner::snapshot::{SnapshotState, WalHighWater};
    use ourios_wal::WalOffset;

    use super::Cut;
    use crate::cadence::{BarrierEpochs, Epoch};

    fn state(byte: u64) -> SnapshotState {
        SnapshotState {
            leaves: Vec::new(),
            structured_templates: Vec::new(),
            wal_high_water: Some(WalHighWater {
                segment: "segment".to_owned(),
                byte,
            }),
            adopted_templates: Vec::new(),
        }
    }

    fn offset(byte: u64) -> WalOffset {
        WalOffset {
            segment: uuid::Uuid::from_u128(7),
            byte,
        }
    }

    fn cut(epoch: Epoch, byte: u64, snapshots: &[(&str, u64)]) -> Cut {
        Cut {
            epoch,
            mark: Some(offset(byte)),
            drained: Vec::new(),
            snapshots: snapshots
                .iter()
                .map(|(tenant, at)| (TenantId::new(*tenant), state(*at)))
                .collect(),
            bytes: 0,
        }
    }

    fn horizon(cut: &Cut, tenant: &str) -> Option<u64> {
        cut.snapshots
            .iter()
            .find(|(id, _)| id.as_str() == tenant)
            .and_then(|(_, state)| state.wal_high_water.as_ref())
            .map(|high_water| high_water.byte)
    }

    /// §3.1's ordering rule, at the one place the two captures meet: the
    /// newer cut's epoch, mark and per-tenant snapshots win whichever way
    /// round the two arrive.
    ///
    /// The handoff is serialised under the ingest exclusion, so the
    /// inverted order should be unreachable — but an inversion that ever
    /// *did* occur would move the pending cut's horizon backwards while
    /// leaving the newer capture's snapshot bytes in the slot: an
    /// artefact covering frames above the mark it is stamped at, which
    /// the next start would replay over.
    #[test]
    fn folding_two_cuts_is_independent_of_the_order_they_arrive_in() {
        let epochs = BarrierEpochs::new();
        let (first, second) = (epochs.open_cut(), epochs.open_cut());
        assert!(second > first, "the later capture takes the later epoch");

        let mut in_order = cut(first, 100, &[("checkout", 100), ("search", 100)]);
        in_order.absorb(cut(second, 200, &[("checkout", 200)]));

        let mut inverted = cut(second, 200, &[("checkout", 200)]);
        inverted.absorb(cut(first, 100, &[("checkout", 100), ("search", 100)]));

        for folded in [&in_order, &inverted] {
            assert_eq!(folded.epoch(), second, "the newer capture's epoch");
            assert_eq!(folded.mark(), Some(offset(200)), "and its mark");
            assert_eq!(
                horizon(folded, "checkout"),
                Some(200),
                "and its bytes for a tenant both captures hold",
            );
            assert_eq!(
                horizon(folded, "search"),
                Some(100),
                "while a tenant only the older capture saw keeps its own \
                 cut-consistent bytes",
            );
        }
    }
}
