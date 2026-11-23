//! The `CHECKPOINT` half of [`Wal`] (§6.7, RFC 0052 §3.2, RFC 0056 §3.4):
//! advancing the mark, with the held floors, and reading it back.

use crate::checkpoint::{self, HeldFloors};
use crate::{CheckpointError, ReclaimGate, Wal, WalOffset, reconcile};

impl Wal {
    /// Record that records ≤ `durable_to` are on object
    /// storage; segments wholly below this offset may be
    /// reclaimed by [`Self::housekeeping`]. Persists the offset
    /// to the `CHECKPOINT` sidecar per §6.7 (atomic write +
    /// fsync + parent-dir fsync) — durability of the checkpoint
    /// itself is what lets the post-restart recovery driver
    /// suppress already-published records on its Parquet path
    /// (replay is at-least-once and delivers every surviving
    /// frame; the driver's suppression is the only dedup).
    ///
    /// Advance is monotonic: a `durable_to` below the current
    /// checkpoint is rejected. Re-asserting the current value never
    /// moves the mark, but it is a no-op only once the checkpoint is
    /// *settled* — sidecar at version 2, its directory entry fsynced,
    /// and `checkpoint_seen` in the RFC 0052 §3.2 record beside it.
    /// Until then an equal mark rewrites all three, which is how a
    /// version-1 root reaches version 2 and how a durability step a
    /// previous call owed and failed is retried; callers that repeat
    /// an equal mark to finish either get that, at the cost of the
    /// writes.
    ///
    /// # Errors
    ///
    /// See [`CheckpointError`]. When the **sidecar write** fails the
    /// in-memory checkpoint is not advanced — the WAL conservatively
    /// keeps all segments rather than risk a post-crash data-side
    /// dup. When the sidecar write succeeded and RFC 0052 §3.2's
    /// `checkpoint_seen` write then failed, the mark *is* durable and
    /// the in-memory checkpoint tracks it: leaving it behind would let
    /// a later, lower mark pass the monotonicity check and rewrite the
    /// sidecar backwards. The error still surfaces, and the next
    /// [`Self::open`] promotes the witness.
    pub fn checkpoint(&mut self, durable_to: WalOffset) -> Result<(), CheckpointError> {
        self.checkpoint_with_floors(durable_to, &HeldFloors::new())
    }

    /// [`Self::checkpoint`], carrying `floors`, the held tenants' floors
    /// (RFC 0056 §3.4), in place of the previous ones. An equal mark
    /// still writes when the floors changed.
    ///
    /// # Errors
    ///
    /// As [`Self::checkpoint`].
    pub fn checkpoint_with_floors(
        &mut self,
        durable_to: WalOffset,
        floors: &HeldFloors,
    ) -> Result<(), CheckpointError> {
        if let Some(current) = self.checkpoint {
            if durable_to < current {
                return Err(CheckpointError::NonMonotonic {
                    current,
                    attempted: durable_to,
                });
            }
            // RFC 0052 §3.2's upgrade is version-aware, not
            // mark-aware: an idle node offers an unchanged mark on
            // every barrier after the first, so a root whose sidecar
            // is still version 1 would never reach version 2 and
            // housekeeping would stay gated forever. Only an equal
            // mark on an already-version-2 sidecar takes the no-write
            // fast path.
            let settled = self.checkpoint_is_settled();
            let unchanged = durable_to == current && *floors == self.floors;
            if unchanged && checkpoint::skips_rewrite(self.checkpoint_version, settled) {
                return Ok(());
            }
        }
        reconcile::arm(&mut self.reclaim.lock(), &self.config)?;
        // Split at the rename, because the two halves need opposite
        // answers. A failure inside `write` leaves nothing visible
        // under the final name, so nothing advances. A failure of the
        // parent fsync leaves the new mark already visible, so the
        // in-memory mark must follow it: a later call that found a
        // stale mark here would pass the monotonicity check against it
        // and rewrite the sidecar *backwards*, which recovery reads as
        // a lower Parquet suppression horizon and republishes every
        // row above it.
        checkpoint::write(&self.config.root, durable_to, floors)?;
        self.checkpoint = Some(durable_to);
        self.floors.clone_from(floors);
        self.checkpoint_version = Some(checkpoint::SidecarVersion::Current);
        // Advancing the mark is not the same as being allowed to
        // reclaim under it: the rename is visible but its directory
        // entry is not durable until the fsync below returns.
        self.reclaim_gate = ReclaimGate::FsyncPending;
        checkpoint::sync_root(&self.config.root)?;
        self.reclaim_gate = ReclaimGate::Open;
        // The witness is a third durable write and its own failure:
        // reported, never rolled back. `checkpoint_seen` governs only
        // the open-time matrix, and an armed record beside a version-2
        // sidecar is promoted at the next open, so a failure here
        // loses nothing.
        reconcile::witness(&mut self.reclaim.lock())
    }

    /// Every part of the last checkpoint is on disk: the sidecar at
    /// version 2, its directory entry fsynced, and `checkpoint_seen`
    /// in the record beside it. Two callers want exactly this, and for
    /// the same reason — the checkpoint is only as good as the weakest
    /// of the three:
    ///
    /// - the equal-mark no-write path, which must not skip a write a
    ///   previous call owed and failed, or it would skip it for the
    ///   life of the process;
    /// - a housekeeping pass, which must not unlink under a checkpoint
    ///   whose entry a crash could lose, nor under one whose startup
    ///   loss witness is still only `Armed` — RFC 0052 §3.2's matrix
    ///   reads a later missing `CHECKPOINT` as a fresh root without
    ///   `checkpoint_seen`, and by then the frames are gone.
    ///
    /// A legacy root has no witness and is never settled, so a pass
    /// plans nothing until the version-aware upgrade lands.
    pub(crate) fn checkpoint_is_settled(&self) -> bool {
        if self.reclaim_gate != ReclaimGate::Open {
            return false;
        }
        if !checkpoint::reclaims_under(self.checkpoint_version) {
            return false;
        }
        self.reclaim.checkpoint_witnessed()
    }

    /// The `CHECKPOINT` sidecar's offset (`None` =
    /// pre-first-checkpoint). The recovery driver reads it once
    /// at startup as its Parquet-side suppression horizon
    /// (§6.6) — `replay` itself delivers every surviving frame.
    #[must_use]
    pub fn last_checkpoint(&self) -> Option<WalOffset> {
        self.checkpoint
    }

    /// The held floors (RFC 0056 §3.4) beside [`Self::last_checkpoint`]:
    /// empty when no tenant was held at the last write, or the sidecar
    /// predates version 3.
    #[must_use]
    pub fn held_floors(&self) -> &HeldFloors {
        &self.floors
    }
}
