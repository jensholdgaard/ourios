//! A group's checkpoint (RFC 0065 §6.5): which checkpoint entries every
//! replica applies, and the group as it stood at the cut of one.
//!
//! Since the group's checkpoint, the state machine keeps only the
//! positions at which the term, the membership, the flusher, the highest
//! kind, the applied id block and the miner configuration changed. That gives the term of every
//! entry since the checkpoint, and the group at any of them, without
//! holding the entries.

use openraft::type_config::alias::StoredMembershipOf;
use ourios_wal::RaftLogId;
use uuid::Uuid;

use super::codec::raft_id;
use super::snapshot_record::{GroupMinerConfig, GroupSnapshot, GroupState, IdBlock, SnapshotMeta};
use super::types::{LogId, NodeId, RaftTypes};

type StoredMembership = StoredMembershipOf<RaftTypes>;

/// What changed at an applied entry.
#[derive(Debug, Clone)]
pub(super) enum Change {
    Term(u64),
    Membership(StoredMembership),
    Flusher(NodeId),
    HighestKind(u8),
    IdBlock(IdBlock),
    MinerConfig(GroupMinerConfig),
}

/// A checkpoint entry's body.
#[derive(Debug, Clone, Copy)]
pub(super) struct Proposal {
    pub(super) proposer: NodeId,
    pub(super) cut: LogId,
    pub(super) highest_kind: u8,
}

impl Proposal {
    pub(super) fn new(proposer: [u8; 16], cut: RaftLogId, highest_kind: u8) -> Self {
        Self {
            proposer: NodeId::from(Uuid::from_bytes(proposer)),
            cut: raft_id(cut),
            highest_kind,
        }
    }
}

/// The group's checkpoint and the changes applied since.
#[derive(Debug, Clone, Default)]
pub(super) struct History {
    /// The snapshot at the last applied checkpoint's cut, or the one the
    /// group opened from or installed.
    checkpoint: Option<GroupSnapshot>,
    /// In index order, every index above the checkpoint's cut.
    changes: Vec<(u64, Change)>,
    /// The term of the last entry applied.
    term: Option<u64>,
}

impl History {
    pub(super) fn new(checkpoint: Option<GroupSnapshot>) -> Self {
        let term = checkpoint
            .as_ref()
            .and_then(|c| c.meta.last_log_id)
            .map(|cut| cut.leader_id.term);
        Self {
            checkpoint,
            changes: Vec::new(),
            term,
        }
    }

    pub(super) fn checkpoint(&self) -> Option<&GroupSnapshot> {
        self.checkpoint.as_ref()
    }

    pub(super) fn cut(&self) -> Option<LogId> {
        self.checkpoint.as_ref().and_then(|c| c.meta.last_log_id)
    }

    /// Note the entry at `log_id` applied, before what it changes.
    pub(super) fn entered(&mut self, log_id: LogId) {
        let term = log_id.leader_id.term;
        if self.term.replace(term) != Some(term) {
            self.changes.push((log_id.index, Change::Term(term)));
        }
    }

    pub(super) fn changed(&mut self, index: u64, change: Change) {
        self.changes.push((index, change));
    }

    /// The term of the entry at `index`, if it is at or above the cut.
    fn term_at(&self, index: u64) -> Option<u64> {
        let cut = self.cut();
        if cut.is_some_and(|cut| index < cut.index) {
            return None;
        }
        self.changes
            .iter()
            .rev()
            .filter(|(at, _)| *at <= index)
            .find_map(|(_, change)| match change {
                Change::Term(term) => Some(*term),
                _ => None,
            })
            .or(cut.map(|cut| cut.leader_id.term))
    }

    /// The group as it stood at `cut`, an index at or above the
    /// checkpoint's.
    fn at(&self, cut: LogId) -> GroupSnapshot {
        let mut snapshot = self.checkpoint.clone().unwrap_or_else(|| GroupSnapshot {
            meta: SnapshotMeta::default(),
            state: GroupState::default(),
        });
        snapshot.meta.last_log_id = Some(cut);
        let through_cut = self.changes.iter().take_while(|(at, _)| *at <= cut.index);
        for (_, change) in through_cut {
            match change {
                Change::Term(_) => {}
                Change::Membership(m) => snapshot.meta.last_membership = m.clone(),
                Change::Flusher(flusher) => snapshot.state.flusher = Some(*flusher),
                Change::HighestKind(kind) => snapshot.state.highest_kind = *kind,
                Change::IdBlock(block) => snapshot.state.id_block = Some(*block),
                Change::MinerConfig(config) => snapshot.state.miner_config = Some(*config),
            }
        }
        snapshot
    }

    /// Apply the checkpoint `proposal` at `at`, `flusher` being the one in
    /// force there, if every replica would (§6.5): proposed by that
    /// flusher, its cut above the checkpoint's, below `at` and the log id
    /// of an entry applied, with the highest kind there the body's.
    /// Returns the new cut; otherwise nothing changes.
    pub(super) fn apply(
        &mut self,
        at: LogId,
        flusher: Option<NodeId>,
        proposal: Proposal,
    ) -> Option<LogId> {
        let Proposal {
            proposer,
            cut,
            highest_kind,
        } = proposal;
        let applies = flusher == Some(proposer)
            && Some(cut) > self.cut()
            && cut.index < at.index
            && self.term_at(cut.index) == Some(cut.leader_id.term);
        let snapshot = applies.then(|| self.at(cut))?;
        if snapshot.state.highest_kind != highest_kind {
            return None;
        }
        self.changes.retain(|(index, _)| *index > cut.index);
        self.checkpoint = Some(snapshot);
        Some(cut)
    }
}

#[cfg(test)]
mod tests {
    use openraft::vote::leader_id_std::CommittedLeaderId;

    use super::*;

    fn id(term: u64, index: u64) -> LogId {
        LogId {
            leader_id: CommittedLeaderId::new(term),
            index,
        }
    }

    /// Entries `1..=8`, terms 2 then 3 from index 5, the flusher 7 from
    /// index 2 and kind 1 with it.
    fn history() -> History {
        let mut history = History::default();
        for index in 1..=8 {
            history.entered(id(if index < 5 { 2 } else { 3 }, index));
            if index == 2 {
                history.changed(2, Change::Flusher(NodeId::from(7)));
                history.changed(2, Change::HighestKind(1));
            }
        }
        history
    }

    fn proposal(proposer: u64, cut: LogId, highest_kind: u8) -> Proposal {
        Proposal {
            proposer: NodeId::from(proposer),
            cut,
            highest_kind,
        }
    }

    #[test]
    fn the_term_of_every_entry_since_the_checkpoint_is_known() {
        let history = history();
        let terms: Vec<_> = (1..=8).map(|i| history.term_at(i)).collect();
        let expected = [2, 2, 2, 2, 3, 3, 3, 3].map(Some);
        assert_eq!(terms, expected);
        assert_eq!(
            history.changes.len(),
            4,
            "a term change, not an entry, each"
        );
    }

    #[test]
    fn entries_in_the_same_term_record_nothing() {
        let mut same_term = History::new(None);
        same_term.entered(id(4, 1));
        for index in 2..1_000 {
            same_term.entered(id(4, index));
        }
        assert_eq!(same_term.changes.len(), 1, "one term change");

        let mut history = history();
        let held = history.changes.len();
        let flusher = Some(NodeId::from(7));
        let cut = history.apply(id(3, 9), flusher, proposal(7, id(3, 6), 1));
        assert_eq!(cut, Some(id(3, 6)));
        history.entered(id(3, 9));
        history.entered(id(3, 10));
        assert!(
            history.changes.len() < held,
            "trimmed, nothing new in term 3"
        );
        assert_eq!(history.term_at(10), Some(3));
    }

    #[test]
    fn a_checkpoint_every_replica_would_refuse_changes_nothing() {
        let flusher = Some(NodeId::from(7));
        let refused = [
            (proposal(9, id(2, 3), 1), "not the flusher"),
            (proposal(7, id(3, 9), 1), "a cut not below the entry"),
            (
                proposal(7, id(3, 3), 1),
                "a cut whose term the log does not hold",
            ),
            (proposal(7, id(2, 3), 0), "a highest kind not the log's"),
        ];
        for (proposal, why) in refused {
            let mut history = history();
            assert_eq!(history.apply(id(3, 9), flusher, proposal), None, "{why}");
            assert_eq!(history.cut(), None, "{why}");
            assert_eq!(history.changes.len(), 4, "{why}");
        }
    }

    #[test]
    fn an_applied_checkpoint_holds_the_group_at_its_cut_and_trims_the_changes() {
        let mut history = history();
        history.changed(6, Change::Flusher(NodeId::from(9)));
        let flusher = Some(NodeId::from(9));
        let cut = history.apply(id(3, 9), flusher, proposal(9, id(2, 3), 1));
        assert_eq!(cut, Some(id(2, 3)));
        let checkpoint = history.checkpoint().expect("applied");
        assert_eq!(checkpoint.meta.last_log_id, Some(id(2, 3)));
        assert_eq!(
            checkpoint.state.flusher,
            Some(NodeId::from(7)),
            "as of the cut"
        );
        assert_eq!(checkpoint.state.highest_kind, 1);
        let kept: Vec<_> = history.changes.iter().map(|(i, _)| *i).collect();
        assert_eq!(kept, [5, 6], "only the changes above the cut");

        let again = proposal(9, id(2, 3), 1);
        assert_eq!(history.apply(id(3, 10), flusher, again), None, "not above");
        let below = proposal(9, id(2, 2), 1);
        assert_eq!(history.apply(id(3, 10), flusher, below), None, "below");
        let later_term_same_index = proposal(9, id(3, 3), 1);
        assert_eq!(
            history.apply(id(3, 10), flusher, later_term_same_index),
            None,
            "above by term but not an entry in the log"
        );
        let next = history.apply(id(3, 10), flusher, proposal(9, id(3, 6), 1));
        assert_eq!(next, Some(id(3, 6)));
        let checkpoint = history.checkpoint().expect("applied");
        assert_eq!(checkpoint.state.flusher, flusher);
        assert_eq!(history.term_at(7), Some(3), "from the cut");
    }

    fn default_config() -> GroupMinerConfig {
        let body = super::super::miner_config::body_of(&ourios_config::MinerConfig::default());
        GroupMinerConfig::new(body).expect("valid")
    }

    /// [`history`] with the first miner configuration at index 5.
    fn configured_at_5() -> History {
        let mut history = history();
        history.changed(5, Change::HighestKind(4));
        history.changed(5, Change::MinerConfig(default_config()));
        history
    }

    /// A cut before the group's first miner configuration is ignored, as
    /// the batches below it were never mined.
    #[test]
    fn a_cut_before_the_first_miner_configuration_is_ignored() {
        let flusher = Some(NodeId::from(7));
        let mut history = configured_at_5();
        let held = history.changes.len();
        let before = history.apply(id(3, 9), flusher, proposal(7, id(2, 3), 1));
        assert_eq!(before, None, "no configuration in force at the cut");
        assert_eq!(history.cut(), None);
        assert_eq!(history.changes.len(), held, "nothing trimmed");
    }

    /// A cut at or above the first miner configuration applies, and the
    /// configuration stays in force through the checkpoint.
    #[test]
    fn a_cut_at_or_above_the_first_miner_configuration_applies() {
        let flusher = Some(NodeId::from(7));
        let mut history = configured_at_5();
        let at = history.apply(id(3, 9), flusher, proposal(7, id(3, 5), 4));
        assert_eq!(at, Some(id(3, 5)), "the configuration's own entry");
        let checkpoint = history.checkpoint().expect("applied");
        assert_eq!(checkpoint.state.miner_config, Some(default_config()));
        let above = history.apply(id(3, 10), flusher, proposal(7, id(3, 7), 4));
        assert_eq!(above, Some(id(3, 7)), "in force from the checkpoint");
    }

    /// A checkpoint holds the id block in force at its cut, not one
    /// applied after it, and the next keeps it over the trim.
    #[test]
    fn an_applied_checkpoint_holds_the_id_block_in_force_at_its_cut() {
        let block = |after, through| IdBlock::new(after, through).expect("valid");
        let flusher = Some(NodeId::from(7));
        let mut history = History::default();
        for index in 1..=8 {
            history.entered(id(if index < 5 { 2 } else { 3 }, index));
            match index {
                1 => history.changed(1, Change::Flusher(NodeId::from(7))),
                2 => {
                    history.changed(2, Change::HighestKind(3));
                    history.changed(2, Change::IdBlock(block(0, 100)));
                }
                6 => history.changed(6, Change::IdBlock(block(100, 200))),
                _ => {}
            }
        }
        let id_block_at = |history: &mut History, at, cut| {
            let applied = history.apply(id(3, at), flusher, proposal(7, cut, 3));
            assert_eq!(applied, Some(cut));
            history.checkpoint().expect("applied").state.id_block
        };
        let at_cut = id_block_at(&mut history, 9, id(2, 4));
        assert_eq!(at_cut, Some(block(0, 100)), "as of the cut");
        let trimmed = id_block_at(&mut history, 10, id(3, 5));
        assert_eq!(trimmed, Some(block(0, 100)), "kept over the trim");
        let later = id_block_at(&mut history, 11, id(3, 7));
        assert_eq!(later, Some(block(100, 200)));
    }
}
