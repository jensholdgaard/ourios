//! Sending a flusher's proposal to its group's leader (RFC 0065 §6.8).
//!
//! The flusher tries the leader its log names first, then any leader a
//! refusal names, then the group's voters in placement order. It moves on
//! after `not_leader`, `not_hosted` or a call never sent. Unlike a
//! forwarded export, a proposal is sent again after an uncertain outcome:
//! a second copy of a checkpoint is refused as `stale` once the first is
//! in the leader's log, and ignored by every replica if both commit, and a
//! second report appends nothing the first did not. `not_flusher`, `stale`
//! and `malformed` end the sending; each tells the flusher what to do next.

use std::time::Duration;

use super::transport::Peers;
use super::transport::propose::{Outcome, Report};
use super::types::LogId;

/// What sending a proposal came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivered {
    /// The leader counted the report and committed the entry, if any.
    Accepted { appended: Option<LogId> },
    /// The log of `leader`, the node that refused it, names another
    /// flusher: stop proposing to that node in this term.
    NotFlusher { leader: String },
    /// Behind the leader's log: drop it, and report afresh at the next
    /// trigger.
    Stale(String),
    /// A defect in the request.
    Malformed(String),
    /// No leader took it: every node tried proved it did not, or the tries
    /// ran out on uncertain answers.
    Undelivered(String),
}

/// Sends one group's proposals.
#[derive(Clone)]
pub struct Proposer {
    peers: Peers,
    group: u32,
    /// The group's voters in placement order.
    voters: Vec<String>,
    commit_timeout: Duration,
}

impl Proposer {
    #[must_use]
    pub fn new(peers: Peers, group: u32, voters: Vec<String>, commit_timeout: Duration) -> Self {
        Self {
            peers,
            group,
            voters,
            commit_timeout,
        }
    }

    /// Send `report` to the group's leader, trying `leader`, the one its
    /// log names, first. At most twice as many calls as the group has
    /// voters, so uncertain answers cannot keep it sending.
    pub async fn send(&self, report: &Report, leader: Option<String>) -> Delivered {
        let request = report.request(self.group);
        let mut tried: Vec<String> = Vec::new();
        let mut named = leader;
        let mut last = String::from("no voter to send to");
        for _ in 0..2 * self.voters.len() {
            let Some(target) = self.next(named.take(), &tried) else {
                break;
            };
            let outcome = self
                .peers
                .propose(&target, request.clone(), self.commit_timeout)
                .await;
            match outcome {
                Outcome::Accepted { appended } => return Delivered::Accepted { appended },
                Outcome::NotFlusher => return Delivered::NotFlusher { leader: target },
                Outcome::Stale(why) => return Delivered::Stale(why),
                Outcome::Malformed(why) => return Delivered::Malformed(why),
                // The same node again: it may have appended it, and a copy
                // is refused or ignored.
                Outcome::Uncertain(why) => {
                    named = Some(target);
                    last = why;
                }
                Outcome::NotLeader { leader } => {
                    tried.push(target);
                    named = leader;
                    "not the leader".clone_into(&mut last);
                }
                Outcome::NotHosted => {
                    tried.push(target);
                    "not hosted".clone_into(&mut last);
                }
                Outcome::Unreached(why) => {
                    tried.push(target);
                    last = why;
                }
            }
        }
        Delivered::Undelivered(format!(
            "no leader of group {} took the proposal: {last}",
            self.group
        ))
    }

    /// The next node to try: a named node not ruled out yet, if it is one
    /// of the group's voters, else the first voter in placement order not
    /// ruled out.
    fn next(&self, named: Option<String>, tried: &[String]) -> Option<String> {
        let fresh = |candidate: &str| !tried.iter().any(|t| t == candidate);
        named
            .filter(|node| self.voters.contains(node) && fresh(node))
            .or_else(|| self.voters.iter().find(|v| fresh(v)).cloned())
    }
}

#[cfg(test)]
mod tests;
