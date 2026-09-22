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
//! the checkpoint all run outside it, so a slow object store stalls
//! ingest for the length of a drain and never for a PUT.
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
//! - **Cuts are strictly ordered.** A pending cut's snapshot bytes
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
        let cut = {
            let _bound = pipeline.exclude_ingest();
            pipeline.quiesce_encodes();
            if rotate_when_idle {
                self.rotate_idle();
            }
            let epoch = self.epochs.open_cut();
            let mark = pipeline.last_durable();
            let (drained, snapshots) =
                pipeline.with_miner(|miner| (self.publish.drain_all(), serialise(miner)));
            Cut {
                epoch,
                mark,
                bytes: drained.estimated_bytes(),
                drained: vec![drained],
                snapshots,
            }
        };
        self.offer(cut)
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
        // §3.1: a pending cut captured behind a *failed* one already
        // folds frames whose only durable copy was that cut's batches,
        // so installing or stamping it would suppress records now sitting
        // requeued in the buffers. Its own batches are unflushed — the
        // task is sequential — so merging them back loses nothing, and
        // the next capture covers everything either cut held.
        if outcome == CutOutcome::Retained {
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
        existing.bytes = existing.bytes.saturating_add(cut.bytes);
        existing.drained.extend(cut.drained);
        existing.mark = cut.mark.or(existing.mark);
        existing.epoch = cut.epoch;
        merge_snapshots(&mut existing.snapshots, cut.snapshots);
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
        // Publishes registered *before* this cut settle here. The
        // outcome defends the data — a failure in any of them means no
        // stamp even though the cut's own flush succeeded.
        if !self.publish.record().quiesce_publishes().all_ok(epoch) {
            return CutOutcome::Retained;
        }
        // The recheck defends the ordering: a publish registered before
        // the barrier began can panic while it waits above.
        if self.epochs.capture().refuses(epoch) {
            return CutOutcome::Latched;
        }
        self.install(&snapshots, mark);
        self.stamp(mark);
        CutOutcome::Stamped
    }

    /// Install this cut's snapshot bytes, serialised against every other
    /// installer and monotone in the mark.
    ///
    /// A write failure is deliberately not a stamp blocker: the data is
    /// in the store and the snapshot only governs replay depth.
    fn install(&self, snapshots: &[(TenantId, SnapshotState)], mark: Option<WalOffset>) {
        let mut installed = self.install.lock().unwrap_or_else(PoisonError::into_inner);
        if let (Some(previous), Some(mark)) = (*installed, mark)
            && mark < previous
        {
            return;
        }
        for (tenant, state) in snapshots {
            let mut state = state.clone();
            state.wal_high_water = mark.map(|offset| WalHighWater {
                segment: offset.segment.to_string(),
                byte: offset.byte,
            });
            if let Err(e) = snapshot_store::write(&self.snapshots_root, tenant, &state, mark) {
                tracing::warn!(
                    error = %e,
                    "barrier: snapshot write failed (the next start may replay more from the WAL)"
                );
            }
        }
        if mark.is_some() {
            *installed = mark;
        }
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
