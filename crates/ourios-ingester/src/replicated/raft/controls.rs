//! What a group log's control entries say (RFC 0065 §6.5), alone or over
//! the group's snapshot.

use std::collections::BTreeMap;

use ourios_wal::Control;
use uuid::Uuid;

use super::snapshot_record::GroupSnapshot;
use super::types::NodeId;

/// The control entries a log held at one read, and the highest kind it
/// had purged.
#[derive(Debug, Clone, Default)]
pub struct HeldControls {
    controls: BTreeMap<u64, Control>,
    purged_kind: u8,
}

impl HeldControls {
    pub(super) fn new(controls: BTreeMap<u64, Control>, purged_kind: u8) -> Self {
        Self {
            controls,
            purged_kind,
        }
    }

    /// The view of the controls in the log, over `snapshot`'s where it has
    /// one: the snapshot holds every control at or below its last entry,
    /// though the log may have purged them.
    pub(super) fn over(&self, snapshot: Option<&GroupSnapshot>) -> LogControls {
        let committed_kind = snapshot.map_or(self.purged_kind, |s| {
            s.state.highest_kind.max(self.purged_kind)
        });
        let highest_kind = self
            .controls
            .values()
            .map(|c| c.kind())
            .fold(committed_kind, u8::max);
        let introduced = self
            .controls
            .iter()
            .find(|(_, c)| c.kind() == highest_kind && highest_kind > committed_kind)
            .map(|(&index, _)| index);
        let after = snapshot
            .and_then(|s| s.meta.last_log_id)
            .map_or(0, |last| last.index + 1);
        let newest = self
            .controls
            .range(after..)
            .rev()
            .find_map(|(_, c)| match *c {
                Control::FlusherAssignment(replica) => {
                    Some(NodeId::from(Uuid::from_bytes(replica)))
                }
                Control::Checkpoint { .. } | Control::IdBlock { .. } => None,
            });
        LogControls {
            highest_kind,
            introduced,
            flusher: newest.or_else(|| snapshot.and_then(|s| s.state.flusher)),
        }
    }
}

/// What the control entries of a log, committed or not, say (§6.5).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LogControls {
    /// The highest control kind the log holds, `0` with none.
    pub highest_kind: u8,
    /// The index of the first entry of `highest_kind`, the newest entry of
    /// a new kind, unless it was purged (and so committed).
    pub introduced: Option<u64>,
    /// The replica the newest flusher assignment names.
    pub flusher: Option<NodeId>,
}
