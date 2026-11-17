//! A flusher's reports to its leader (RFC 0065 §6.8): the last entry its
//! miner followed and the id blocks it holds and has not started, which
//! the leader tops the group's blocks up from. It reports when it applies
//! an assignment naming itself, when its allocator starts a block, when it
//! sees a new leader or term, when it applies a checkpoint, and otherwise
//! at least once per flusher patience; one report at a time, and only
//! while it is the flusher.

use std::time::Duration;

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
    /// What the last report sent saw; `None` while not the flusher, so a
    /// naming reports at once.
    sent: Option<Seen>,
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

    /// Send a report from `miner`'s state, if one is due, or `forced` by the
    /// patience tick, and none is still in flight.
    pub(super) fn maybe(&mut self, miner: &GroupMiner, forced: bool) {
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
        let followed = miner.followed();
        let reported = self.sent.and_then(|sent| sent.cut);
        let seen = Seen {
            cut: visible_cut(self.proposing.applied_cut(), followed, reported),
            block: miner.blocks().current(),
            leadership: self.proposing.leadership(),
        };
        let Some(followed) = followed.filter(|_| due(self.sent, seen, forced)) else {
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
        self.sent = Some(seen);
    }
}

/// The applied cut a report can stand on: `applied` once the miner has
/// followed through it, else `reported`, the one already reported. A
/// report from below an applied cut is refused as stale, so the cut is not
/// seen until the miner reaches it.
fn visible_cut(
    applied: Option<LogId>,
    followed: Option<LogId>,
    reported: Option<LogId>,
) -> Option<LogId> {
    match (applied, followed) {
        (Some(cut), Some(followed)) if followed.index >= cut.index => Some(cut),
        _ => reported,
    }
}

/// Whether a report is due: none sent since this replica became the
/// flusher, something it reports on changed since the last, or the
/// patience tick forces one.
fn due(sent: Option<Seen>, seen: Seen, forced: bool) -> bool {
    forced || sent != Some(seen)
}

#[cfg(test)]
mod tests {
    use openraft::vote::leader_id_std::CommittedLeaderId;

    use super::*;

    fn id(index: u64) -> LogId {
        LogId {
            leader_id: CommittedLeaderId::new(2),
            index,
        }
    }

    fn seen(cut: u64) -> Seen {
        Seen {
            cut: Some(id(cut)),
            block: None,
            leadership: None,
        }
    }

    /// RFC 0065 §6.8: a checkpoint the state machine applied ahead of the
    /// miner is not seen until the miner follows through it, so the report
    /// it triggers comes once its `mined` can pass check 5.
    #[test]
    fn an_applied_cut_is_seen_once_the_miner_follows_through_it() {
        assert_eq!(
            visible_cut(Some(id(12)), Some(id(10)), Some(id(5))),
            Some(id(5))
        );
        assert_eq!(
            visible_cut(Some(id(12)), Some(id(12)), Some(id(5))),
            Some(id(12))
        );
        assert_eq!(visible_cut(Some(id(12)), Some(id(13)), None), Some(id(12)));
        assert_eq!(visible_cut(None, Some(id(13)), None), None);
    }

    /// The patience tick forces a report, so one goes out at least every
    /// patience whatever was sent between ticks; otherwise only a change
    /// calls for one.
    #[test]
    fn the_patience_tick_forces_a_report_and_a_change_calls_for_one() {
        assert!(due(None, seen(5), false), "first as flusher");
        assert!(!due(Some(seen(5)), seen(5), false), "nothing changed");
        assert!(due(Some(seen(5)), seen(12), false), "a new cut");
        assert!(due(Some(seen(5)), seen(5), true), "the patience tick");
    }
}
