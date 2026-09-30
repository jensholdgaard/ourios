//! RFC 0052 §3.5's export surface: the WAL state the receiver reads to
//! publish the reclamation instruments and the rotation edges.

use crate::{ReclaimState, RotationState, Wal};

impl Wal {
    /// The WAL state RFC 0052 §3.5 exports.
    #[must_use]
    pub fn reclaim_state(&self) -> ReclaimState {
        let metrics = self.metrics();
        let (lag_bytes, lag_segments) = self.ledger.lag(self.current_segment_uuid);
        ReclaimState {
            unflushed_bytes: metrics.unflushed_bytes,
            disk_bytes: metrics.disk_bytes,
            segment_count: metrics.segment_count,
            unreclaimed_bytes: self.unreclaimed_bytes,
            checkpoint: self.checkpoint,
            stale_partials: self.stale_partials.len(),
            reclaimable: self.checkpoint_is_settled(),
            floor: self.ledger.floor(),
            rotation: self.rotation.clone(),
            oldest_unreclaimed: self.oldest_unreclaimed(),
            lag_bytes,
            lag_segments,
        }
    }

    /// RFC 0052 §3.3's rotation state alone — what the commit
    /// coordinator compares across every call under the journal guard to
    /// emit the rotation edges, without [`Self::reclaim_state`]'s
    /// directory walk.
    #[must_use]
    pub fn rotation_state(&self) -> &RotationState {
        &self.rotation
    }

    fn oldest_unreclaimed(&self) -> Option<std::time::SystemTime> {
        if self.unreclaimed_bytes == 0 {
            return None;
        }
        let (secs, nanos) = self.ledger.oldest_with_frames()?.get_timestamp()?.to_unix();
        std::time::UNIX_EPOCH.checked_add(std::time::Duration::new(secs, nanos))
    }
}
