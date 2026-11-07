//! The durability sink the ingest pipeline appends to and fsyncs
//! through, and its RFC 0008 WAL implementation.

use ourios_wal::{
    FrameKind, HousekeepingProgress, PassId, ReclaimError, ReclaimOutcome, ReclaimPlan,
    ReclaimState, RotationKind, RotationState, SnapshotHorizons, Wal, WalOffset,
};

use super::error::ReceiveError;

/// The durability sink the pipeline appends to and fsyncs through.
///
/// Abstracted (rather than a concrete `Wal`) so the WAL-before-ack
/// ordering can be exercised with a spy that records / counts the
/// `append`/`sync` calls and can be made to fail (RFC0003.1/.12,
/// RFC0008.8 per §8). The only production implementation is [`Wal`].
///
/// `Send` so it can live behind the coordinator's `Mutex<Box<dyn Journal>>`
/// as shared state in the async HTTP/gRPC listeners.
pub trait Journal: Send {
    /// Append one `OtlpBatch` frame carrying `payload` (not yet durable),
    /// yielding **this frame's own offset**.
    ///
    /// RFC 0052 §3.1: that offset, not the group sync's reported EOF, is
    /// what the turn stores as `last_durable` and what the barrier reads
    /// as its mark. A sync's EOF can cover a later turn's frame that has
    /// not run yet, and a checkpoint at that EOF would stamp across a
    /// frame nothing had mined.
    ///
    /// # Errors
    ///
    /// [`ReceiveError::WalAppend`] on a persistence failure.
    fn append_batch(&mut self, payload: &[u8]) -> Result<WalOffset, ReceiveError>;

    /// Fsync — appended frames are durable when this returns `Ok`,
    /// yielding the durable high-water offset. The real WAL always has
    /// one (RFC 0008 §6.3); the group-commit coordinator compares it
    /// against waiters and the snapshot writer records it as the
    /// snapshot's WAL high-water mark (RFC 0001 §6.9).
    ///
    /// # Errors
    ///
    /// [`ReceiveError::WalSync`] on an fsync failure.
    fn sync(&mut self) -> Result<WalOffset, ReceiveError>;

    /// The WAL's current unflushed-bytes counter (RFC 0008 §6.8), read
    /// by the coordinator's segment-fill early cut ("until the segment
    /// fills", §3.4). Cheap — an in-memory counter, no syscall.
    fn unflushed_bytes(&self) -> u64;

    /// RFC 0052 §3.7's reclamation surface, reached through the trait
    /// object rather than a downcast so the barrier's tests can drive a
    /// double that observes reclamation.
    ///
    /// The defaults answer [`ReclaimError::NoReclamationSurface`]: the
    /// spy journals the ingest-path tests use persist nothing, and a
    /// silent no-op default would let a missing checkpoint read as a
    /// clean pass. Only [`Wal`] overrides them.
    ///
    /// # Errors
    ///
    /// [`ReclaimError::Checkpoint`] on a sidecar write failure — the
    /// in-memory mark is left where it was either way (§3.1's
    /// fail-closed rule).
    fn checkpoint(&mut self, durable_to: WalOffset) -> Result<(), ReclaimError> {
        let _ = durable_to;
        Err(ReclaimError::NoReclamationSurface)
    }

    /// The journal's persisted checkpoint mark, if any.
    fn last_checkpoint(&self) -> Option<WalOffset> {
        None
    }

    /// The ledger half of one capped housekeeping pass (§3.2), under the
    /// journal's single-writer position. There is no file-half method:
    /// the plan carries the `RECLAIM` sidecar it was planned against, so
    /// [`ourios_wal::write_plan_record`] and [`ourios_wal::unlink_planned`]
    /// run on the plan alone, and no implementation can put that I/O
    /// back under the guard.
    ///
    /// # Errors
    ///
    /// As [`Wal::housekeeping_prepare`].
    fn housekeeping_prepare(
        &mut self,
        horizons: &SnapshotHorizons,
        max_unlinks: usize,
    ) -> Result<ReclaimPlan, ReclaimError> {
        let _ = (horizons, max_unlinks);
        Err(ReclaimError::NoReclamationSurface)
    }

    /// Fold the file half's outcome back under the writer position.
    ///
    /// # Errors
    ///
    /// As [`Wal::housekeeping_commit`].
    fn housekeeping_commit(
        &mut self,
        pass: PassId,
        outcome: ReclaimOutcome,
    ) -> Result<HousekeepingProgress, ReclaimError> {
        let _ = (pass, outcome);
        Err(ReclaimError::NoReclamationSurface)
    }

    /// §3.3's append-independent rotation — the barrier task's idle
    /// rotation, and the discharge of a pending rotation-origin
    /// directory fsync.
    ///
    /// The default fails closed like the rest of this surface: a journal
    /// that answers [`Self::segment_age_exceeded`] or
    /// [`Self::owes_rotation_fsync`] but inherits a silent `Ok` here would
    /// have `rotate_if_aged` read an owed rotation or fsync as discharged.
    ///
    /// # Errors
    ///
    /// As [`Wal::rotate`]; the default answers
    /// [`ourios_wal::AppendError::Io`] with an `Unsupported` source.
    fn rotate(&mut self, kind: RotationKind) -> Result<(), ReceiveError> {
        let _ = kind;
        Err(ReceiveError::WalAppend(ourios_wal::AppendError::Io {
            op: "rotate",
            source: std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "this journal exposes no RFC 0052 rotation surface",
            ),
        }))
    }

    /// Whether the current segment has outlived `segment_age_secs` —
    /// the barrier task's idle-rotation predicate (§3.1).
    fn segment_age_exceeded(&self) -> bool {
        false
    }

    /// Whether a rotation-origin directory fsync is still owed (§3.3).
    fn owes_rotation_fsync(&self) -> bool {
        false
    }

    /// §3.5's export surface.
    fn reclaim_state(&self) -> ReclaimState {
        ReclaimState::default()
    }

    /// §3.3's rotation state alone — cheap, because the coordinator
    /// reads it on every guard release to emit the rotation edges.
    fn rotation_state(&self) -> RotationState {
        RotationState::Healthy
    }
}

impl Journal for Wal {
    fn append_batch(&mut self, payload: &[u8]) -> Result<WalOffset, ReceiveError> {
        Wal::append(self, FrameKind::TenantOtlpBatch, payload).map_err(ReceiveError::WalAppend)
    }

    fn sync(&mut self) -> Result<WalOffset, ReceiveError> {
        Wal::sync(self).map_err(ReceiveError::WalSync)
    }

    fn unflushed_bytes(&self) -> u64 {
        self.metrics().unflushed_bytes
    }

    fn checkpoint(&mut self, durable_to: WalOffset) -> Result<(), ReclaimError> {
        Wal::checkpoint(self, durable_to).map_err(ReclaimError::Checkpoint)
    }

    fn last_checkpoint(&self) -> Option<WalOffset> {
        Wal::last_checkpoint(self)
    }

    fn housekeeping_prepare(
        &mut self,
        horizons: &SnapshotHorizons,
        max_unlinks: usize,
    ) -> Result<ReclaimPlan, ReclaimError> {
        Wal::housekeeping_prepare(self, horizons, max_unlinks)
    }

    fn housekeeping_commit(
        &mut self,
        pass: PassId,
        outcome: ReclaimOutcome,
    ) -> Result<HousekeepingProgress, ReclaimError> {
        Wal::housekeeping_commit(self, pass, outcome)
    }

    fn rotate(&mut self, kind: RotationKind) -> Result<(), ReceiveError> {
        Wal::rotate(self, kind).map_err(ReceiveError::WalAppend)
    }

    fn segment_age_exceeded(&self) -> bool {
        Wal::segment_age_exceeded(self)
    }

    fn owes_rotation_fsync(&self) -> bool {
        Wal::owes_rotation_fsync(self)
    }

    fn reclaim_state(&self) -> ReclaimState {
        Wal::reclaim_state(self)
    }

    fn rotation_state(&self) -> RotationState {
        Wal::rotation_state(self).clone()
    }
}
