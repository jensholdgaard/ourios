//! A flusher proposing its checkpoints (RFC 0065 §6.5, §6.8): once a cut's
//! bundle and `checkpoint.json` are written, it proposes the checkpoint
//! entry to the group's leader. After `not_flusher` it stops proposing to
//! that leader in that term; `stale` and an undelivered proposal are
//! dropped, the next barrier proposing a later cut; `malformed` is a
//! defect, reported, and fails the checkpoint.

use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use openraft::async_runtime::WatchReceiver as _;
use ourios_semconv as semconv;
use ourios_wal::RaftLogId;

use super::codec::raft_id;
use super::host::GroupRaft;
use super::log_store::GroupLogReader;
use super::mining::Captured;
use super::proposer::{Delivered, Proposer};
use super::state_machine::GroupSm;
use super::transport::propose::{ProposedCheckpoint, Report};
use super::types::{LogId, NodeId};

/// How one group's flusher proposes.
#[derive(Clone)]
pub(super) struct Proposing {
    pub(super) group: u32,
    pub(super) proposer: Proposer,
    /// This replica, the flusher.
    pub(super) replica: NodeId,
    /// This replica's raft, once it runs: the leader and term it knows of.
    pub(super) raft: Arc<OnceLock<GroupRaft>>,
    pub(super) log: GroupLogReader,
    pub(super) sm: GroupSm,
    /// The leader that refused it as `not_flusher`, and in which term.
    pub(super) refused: Arc<Mutex<Option<(String, u64)>>>,
}

impl Proposing {
    /// Propose `captured`'s checkpoint, unless the leader in force refused
    /// this flusher in this term; what came of it.
    pub(super) async fn propose(&self, captured: &Captured) -> Option<Delivered> {
        let (leader, term) = self.leader()?;
        let refused = self
            .refused
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if leader.is_some() && refused == leader.clone().map(|l| (l, term)) {
            return None;
        }
        let report = self.report(captured);
        let delivered = self.proposer.send(&report, leader).await;
        match &delivered {
            Delivered::NotFlusher { leader } => {
                *self.refused.lock().unwrap_or_else(PoisonError::into_inner) =
                    Some((leader.clone(), term));
            }
            Delivered::Malformed(why) => tracing::warn!(
                name: semconv::EVENT_OURIOS_WAL_REPLICATION_CHECKPOINT_FAILED,
                {
                    { semconv::OURIOS_WAL_GROUP } = i64::from(self.group),
                    { crate::metrics::ERROR_TYPE } = "_OTHER",
                },
                "the leader of group {} refused the flusher's checkpoint as malformed: {why}",
                self.group,
            ),
            Delivered::Accepted { .. } | Delivered::Stale(_) | Delivered::Undelivered(_) => {}
        }
        Some(delivered)
    }

    /// The checkpoint's proposal: its cut, the log's highest kind there,
    /// and the blocks its allocator had not started.
    fn report(&self, captured: &Captured) -> Report {
        let cut = captured.bundle.cut();
        let cut = raft_id(RaftLogId {
            term: cut.term,
            index: cut.seq,
        });
        let unstarted = u32::try_from(captured.bundle.ids().pending().len()).unwrap_or(u32::MAX);
        Report {
            proposer: self.replica,
            mined: Some(cut),
            unstarted,
            checkpoint: Some(ProposedCheckpoint {
                cut,
                highest_kind: self.highest_kind_at(cut),
            }),
        }
    }

    /// The highest control kind the log holds through `cut`.
    fn highest_kind_at(&self, cut: LogId) -> u8 {
        // The log first, as `Group::controls` reads it.
        let held = self.log.held_controls();
        let snapshot = self.sm.snapshot();
        held.at(snapshot.as_ref(), cut.index).highest_kind
    }

    /// The leader's `node_id` this replica knows of, if any, and its term;
    /// `None` before its raft runs.
    fn leader(&self) -> Option<(Option<String>, u64)> {
        let metrics = self.raft.get()?.metrics().borrow_watched().clone();
        let leader = metrics.current_leader.and_then(|leader| {
            let membership = metrics.membership_config.membership();
            membership.get_node(&leader).cloned()
        });
        Some((leader, metrics.current_term))
    }
}
