//! A flusher's reports to its leader (RFC 0065 §6.8): the last entry its
//! miner followed and the id blocks it holds and has not started, which
//! the leader tops the group's blocks up from. It reports when it applies
//! an assignment naming itself, when its allocator starts a block, when it
//! sees a new leader or term, when it applies a checkpoint, and otherwise
//! at least once per flusher patience; one report at a time, and only
//! while it is the flusher.

use std::time::{Duration, Instant};

use tokio::task::JoinHandle;

use super::flusher::PATIENCE;
use super::mining::GroupMiner;
use super::proposing::Proposing;
use super::snapshot_record::IdBlock;
use super::transport::propose::Report;
use super::types::{LogId, NodeId};

/// What a report is sent on a change of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Seen {
    cut: Option<LogId>,
    block: Option<IdBlock>,
    leadership: Option<(Option<NodeId>, u64)>,
}

/// One group's reports from its flusher.
pub(super) struct Reporting {
    proposing: Proposing,
    /// What the last report sent saw, and when; `None` while not the
    /// flusher, so a naming reports at once.
    sent: Option<(Seen, Instant)>,
    running: Option<JoinHandle<()>>,
}

impl Reporting {
    pub(super) fn new(proposing: Proposing) -> Self {
        Self {
            proposing,
            sent: None,
            running: None,
        }
    }

    /// How often it reports at least.
    pub(super) fn every() -> Duration {
        PATIENCE
    }

    /// Send a report from `miner`'s state, if one is due and none is still
    /// in flight.
    pub(super) fn maybe(&mut self, miner: &GroupMiner) {
        if !self.proposing.named() {
            self.sent = None;
            return;
        }
        if self
            .running
            .as_ref()
            .is_some_and(|running| !running.is_finished())
        {
            return;
        }
        let seen = Seen {
            cut: self.proposing.applied_cut(),
            block: miner.blocks().current(),
            leadership: self.proposing.leadership(),
        };
        let due = match self.sent {
            None => true,
            Some((sent, at)) => sent != seen || at.elapsed() >= Self::every(),
        };
        let Some(followed) = miner.followed().filter(|_| due) else {
            return;
        };
        let report = Report {
            proposer: self.proposing.replica(),
            mined: Some(followed),
            unstarted: u32::try_from(miner.blocks().remaining()).unwrap_or(u32::MAX),
            checkpoint: None,
        };
        let proposing = self.proposing.clone();
        self.running = Some(tokio::spawn(async move {
            proposing.report_only(&report).await;
        }));
        self.sent = Some((seen, Instant::now()));
    }
}
