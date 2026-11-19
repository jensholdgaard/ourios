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
    /// The index of the assignment naming this replica, so one that names
    /// it again after another replica (A, B, A) is a change, though it is
    /// named at every check (#1333).
    assigned: Option<u64>,
}

/// What [`Reporting::maybe`] observes of the replica as it decides.
#[derive(Debug, Clone, Copy)]
struct Observed {
    seen: Seen,
    proposer: NodeId,
    /// The last entry its miner followed.
    followed: Option<LogId>,
    /// The id blocks it holds and has not started.
    unstarted: u32,
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
        let observed = Observed {
            seen: Seen {
                cut: visible_cut(self.proposing.applied_cut(), followed, reported),
                block: miner.blocks().current(),
                leadership: self.proposing.leadership(),
                assigned: self.proposing.assigned_at(),
            },
            proposer: self.proposing.replica(),
            followed,
            unstarted: u32::try_from(miner.blocks().remaining()).unwrap_or(u32::MAX),
        };
        let Some(report) = to_send(self.sent, observed, forced) else {
            return;
        };
        let proposing = self.proposing.clone();
        self.running = Some(tokio::spawn(async move {
            proposing.report_only(&report).await;
        }));
        self.sent = Some(observed.seen);
    }
}

/// The report `observed` calls for, given what the last one `sent` saw:
/// none until the miner has followed an entry, and otherwise one when
/// [`due`] says so.
fn to_send(sent: Option<Seen>, observed: Observed, forced: bool) -> Option<Report> {
    let followed = observed
        .followed
        .filter(|_| due(sent, observed.seen, forced))?;
    Some(Report {
        proposer: observed.proposer,
        mined: Some(followed),
        unstarted: observed.unstarted,
        checkpoint: None,
    })
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
            assigned: Some(1),
        }
    }

    fn observed(seen: Seen) -> Observed {
        Observed {
            seen,
            proposer: NodeId::from(3),
            followed: Some(id(20)),
            unstarted: 2,
        }
    }

    fn report(unstarted: u32) -> Report {
        Report {
            proposer: NodeId::from(3),
            mined: Some(id(20)),
            unstarted,
            checkpoint: None,
        }
    }

    /// RFC 0065 §6.8, #1333: a new allocator block, a new leader or term,
    /// and an assignment naming this replica again after another each send
    /// a report at once, carrying what it has mined and the blocks it holds
    /// unstarted; with nothing changed, none goes until the tick.
    #[test]
    fn each_trigger_sends_a_report_before_the_tick() {
        let before = seen(5);
        let block = Seen {
            block: IdBlock::new(100, 199),
            ..before
        };
        let leader = Seen {
            leadership: Some((Some(NodeId::from(2)), 7)),
            ..before
        };
        let again = Seen {
            assigned: Some(9),
            ..before
        };
        for (trigger, now) in [
            ("a new block", block),
            ("a new leader", leader),
            ("named again", again),
        ] {
            assert_eq!(
                to_send(Some(before), observed(now), false),
                Some(report(2)),
                "{trigger}"
            );
        }
        assert_eq!(
            to_send(Some(before), observed(before), false),
            None,
            "nothing changed"
        );
        assert_eq!(
            to_send(Some(before), observed(before), true),
            Some(report(2)),
            "the tick"
        );
        let unmined = Observed {
            followed: None,
            ..observed(block)
        };
        assert_eq!(
            to_send(Some(before), unmined, false),
            None,
            "nothing mined yet"
        );
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
