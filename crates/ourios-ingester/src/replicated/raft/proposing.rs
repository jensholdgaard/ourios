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
use super::types::NodeId;

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
    /// Propose `captured`'s checkpoint; what came of it, `None` before its
    /// raft runs or once a later checkpoint has passed the cut.
    pub(super) async fn propose(&self, captured: &Captured) -> Option<Delivered> {
        let leader = self.leader()?;
        let report = self.report(captured)?;
        Some(self.send(&report, leader).await)
    }

    /// Send the flusher's report, `report`, with no entry (§6.8); what came
    /// of it, `None` before its raft runs.
    pub(super) async fn report_only(&self, report: &Report) -> Option<Delivered> {
        let leader = self.leader()?;
        Some(self.send(report, leader).await)
    }

    /// Send `report` to `leader`, in `term`, never to the node that refused
    /// this flusher as `not_flusher` in it.
    async fn send(&self, report: &Report, (leader, term): (Option<String>, u64)) -> Delivered {
        let refused = self
            .refused
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let refused = refused
            .filter(|(_, refused_in)| *refused_in == term)
            .map(|(node, _)| node);
        let delivered = self.proposer.send(report, refused.as_deref(), leader).await;
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
                "the leader of group {} refused the flusher's proposal as malformed: {why}",
                self.group,
            ),
            Delivered::Accepted { .. } | Delivered::Stale(_) | Delivered::Undelivered(_) => {}
        }
        delivered
    }

    /// This replica.
    pub(super) fn replica(&self) -> NodeId {
        self.replica
    }

    /// Whether this replica's state machine names it the flusher.
    pub(super) fn named(&self) -> bool {
        self.sm.flusher() == Some(self.replica)
    }

    /// The cut of the last checkpoint this replica applied.
    pub(super) fn applied_cut(&self) -> Option<super::types::LogId> {
        *self.sm.checkpoints().borrow()
    }

    /// The leader this replica knows of and the term, as ids, for noticing
    /// a change.
    pub(super) fn leadership(&self) -> Option<(Option<NodeId>, u64)> {
        let metrics = self.raft.get()?.metrics().borrow_watched().clone();
        Some((metrics.current_leader, metrics.current_term))
    }

    /// The checkpoint's proposal: its cut, the log's highest kind there,
    /// and the blocks its allocator had not started; `None` once a later
    /// checkpoint, another flusher's, has passed the cut, which every
    /// replica would ignore.
    fn report(&self, captured: &Captured) -> Option<Report> {
        let cut = captured.bundle.cut();
        let cut = raft_id(RaftLogId {
            term: cut.term,
            index: cut.seq,
        });
        // The log first, as `Group::controls` reads it.
        let held = self.log.held_controls();
        let snapshot = self.sm.snapshot();
        if snapshot.as_ref().and_then(|s| s.meta.last_log_id) >= Some(cut) {
            return None;
        }
        let highest_kind = held.at(snapshot.as_ref(), cut.index).highest_kind;
        let unstarted = u32::try_from(captured.bundle.ids().pending().len()).unwrap_or(u32::MAX);
        Some(Report {
            proposer: self.replica,
            mined: Some(cut),
            unstarted,
            checkpoint: Some(ProposedCheckpoint { cut, highest_kind }),
        })
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
