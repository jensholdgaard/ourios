//! RFC 0052 §3.5's export surface: the WAL state the receiver reads to
//! publish the reclamation instruments and the rotation edges.

use std::collections::HashMap;

use ourios_core::tenant::TenantId;

use crate::reclaim::SlotState;
use crate::{ReclaimState, RotationState, Wal, WalOffset};

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

    /// Each tenant's reclaimed-through offset as the `RECLAIM` record
    /// holds it (RFC 0052 §3.2): the last frame of that tenant a pass
    /// unlinked. A tenant with nothing reclaimed is absent, as is every
    /// tenant of a root with no record. This is the witness that
    /// explains a snapshot horizon whose segment is gone.
    #[must_use]
    pub fn reclaimed_through(&self) -> HashMap<TenantId, WalOffset> {
        let held = self.reclaim.lock();
        let Some(store) = held.as_ref() else {
            return HashMap::new();
        };
        store
            .record()
            .dictionary
            .live()
            .filter_map(|(_, slot)| match &slot.state {
                SlotState::Live {
                    reclaimed_through: Some(entry),
                } => Some((slot.key.clone(), entry.offset)),
                SlotState::Live {
                    reclaimed_through: None,
                }
                | SlotState::Tombstoned => None,
            })
            .collect()
    }

    fn oldest_unreclaimed(&self) -> Option<std::time::SystemTime> {
        if self.unreclaimed_bytes == 0 {
            return None;
        }
        let (secs, nanos) = self.ledger.oldest_with_frames()?.get_timestamp()?.to_unix();
        std::time::UNIX_EPOCH.checked_add(std::time::Duration::new(secs, nanos))
    }
}
