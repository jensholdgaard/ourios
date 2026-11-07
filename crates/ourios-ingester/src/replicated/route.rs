//! Routing an export to its group's leader (RFC 0065 §6.8), from a node
//! that does not lead the group.
//!
//! The router tries the leader the node knows of first, then any leader a
//! refusal names, then the group's voters in placement order, and tries at
//! most as many nodes as the group has voters. It moves on only after an
//! answer that proves nothing was appended: the node does not lead or host
//! the group, or the call was never sent. A rejection the leader gives is
//! returned as it is, and any other outcome ends the routing: the export
//! may have committed, so it is never sent again (§6.6, §9 Q4).

use std::time::Duration;

use super::layout::VotingLayout;
use super::raft::transport::{ForwardRequest, Forwarded, Outcome, Peers, Rejected, VERSION};

/// Where a routed export ended up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Routed {
    /// A leader committed it at `seq`.
    Committed(u64),
    /// Its leader refused it before appending.
    Rejected(Rejected),
    /// No leader committed it: every node tried proved it did not take
    /// it, or the last answer left the outcome uncertain.
    Unavailable(String),
}

/// Forwards this node's exports for groups it does not lead.
#[derive(Clone)]
pub struct Router {
    peers: Peers,
    layout: VotingLayout,
    /// This node's `node_id`, never a target.
    node: String,
    commit_timeout: Duration,
}

impl Router {
    /// Route over `peers` by `layout`, from the node named `node`; each
    /// forward waits for the leader's commit up to `commit_timeout`.
    #[must_use]
    pub fn new(peers: Peers, layout: VotingLayout, node: String, commit_timeout: Duration) -> Self {
        Self {
            peers,
            layout,
            node,
            commit_timeout,
        }
    }

    /// Forward `export` to its group's leader, trying `hint` first.
    pub async fn forward(&self, export: Forwarded, hint: Option<String>) -> Routed {
        let Forwarded {
            group,
            batch,
            credential,
        } = export;
        let voters = self.layout.voters(group);
        let request = ForwardRequest {
            version: VERSION,
            group,
            payload: batch.into(),
            credential,
        };
        let mut tried: Vec<String> = Vec::new();
        let mut named = hint;
        for _ in 0..voters.len() {
            let Some(target) = self.next(named.take(), &voters, &tried) else {
                break;
            };
            let outcome = self
                .peers
                .forward(&target, request.clone(), self.commit_timeout)
                .await;
            tried.push(target);
            match outcome {
                Outcome::Committed(seq) => return Routed::Committed(seq),
                Outcome::Rejected(rejected) => return Routed::Rejected(rejected),
                Outcome::Uncertain(reason) => return Routed::Unavailable(reason),
                Outcome::NotLeader { leader } => named = leader,
                Outcome::NotHosted | Outcome::Unreached(_) => {}
            }
        }
        Routed::Unavailable(format!("no voter of group {group} took the export"))
    }

    /// The next node to try: a named leader not tried yet, else the first
    /// voter in placement order not tried yet; never this node. A named
    /// leader that is not one of the group's voters is passed over, so a
    /// stale or foreign hint cannot use up a try a voter needs.
    fn next(&self, named: Option<String>, voters: &[&str], tried: &[String]) -> Option<String> {
        let fresh =
            |candidate: &str| candidate != self.node && !tried.iter().any(|t| t == candidate);
        named
            .filter(|leader| voters.contains(&leader.as_str()) && fresh(leader))
            .or_else(|| voters.iter().find(|v| fresh(v)).map(|v| (*v).to_owned()))
    }
}

#[cfg(test)]
mod tests;
