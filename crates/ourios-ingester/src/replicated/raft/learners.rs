//! A group's tail replicas as `openraft` learners (RFC 0065 §6.1). The
//! group's leader keeps its learner set equal to the tail placement: it
//! identifies each placed peer (§6.8), adds those it does not hold under
//! the UUID they answer with, and removes learners that left the placement
//! or whose `node_id` now answers under another UUID. Every change is a
//! committed, learner-only membership change; the voters, the write quorum
//! and voter placement never move.
//!
//! The flusher (§6.5) is not named yet, so no learner is held back from
//! removal for being it.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use openraft::async_runtime::WatchReceiver as _;
use openraft::{ChangeMembers, Precondition};

use super::host::GroupRaft;
use super::transport::{Identity, Peers};
use super::types::{LogId, Membership, NodeId};
use crate::replicated::placement::place;

/// The tail pool: its members' `node_id`s and the learners per group.
#[derive(Debug, Clone)]
pub struct TailPool {
    peers: Vec<String>,
    replicas: usize,
}

impl TailPool {
    #[must_use]
    pub fn new(peers: Vec<String>, replicas: usize) -> Self {
        Self { peers, replicas }
    }

    /// `group`'s tail replicas, in placement order.
    #[must_use]
    pub fn placed(&self, group: u32) -> Vec<&str> {
        place(group, self.peers.iter().map(String::as_str), self.replicas)
    }
}

/// The learner changes one pass makes.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Changes {
    /// New learners, each UUID with its `node_id`.
    pub add: BTreeMap<NodeId, String>,
    /// Learners to remove.
    pub remove: BTreeSet<NodeId>,
}

/// A group's membership, as the leader sees it.
pub struct Members {
    pub voters: BTreeSet<NodeId>,
    /// Each learner's UUID with its `node_id`.
    pub learners: BTreeMap<NodeId, String>,
}

/// The changes that bring `members`' learners to `placed`, given the
/// identities the placed peers answered with; a placed peer that did not
/// answer keeps the learner it has. `log_kind` is the highest control kind
/// the group's log holds, which a new learner must read.
#[must_use]
pub fn plan(
    members: &Members,
    placed: &[&str],
    answers: &BTreeMap<String, Identity>,
    log_kind: u8,
) -> Changes {
    Changes {
        add: additions(members, placed, answers, log_kind),
        remove: removals(members, placed, answers),
    }
}

/// The placed peers that answered under a UUID the membership does not
/// hold and read every kind the log holds.
fn additions(
    members: &Members,
    placed: &[&str],
    answers: &BTreeMap<String, Identity>,
    log_kind: u8,
) -> BTreeMap<NodeId, String> {
    let held = |replica: &NodeId| {
        members.learners.contains_key(replica) || members.voters.contains(replica)
    };
    placed
        .iter()
        .filter_map(|&node_id| answers.get(node_id).map(|answer| (node_id, answer)))
        .filter(|(_, answer)| !held(&answer.replica) && answer.highest_control_kind >= log_kind)
        .map(|(node_id, answer)| (answer.replica, node_id.to_owned()))
        .collect()
}

/// The learners that left the placement, or whose `node_id` answered
/// under another UUID.
fn removals(
    members: &Members,
    placed: &[&str],
    answers: &BTreeMap<String, Identity>,
) -> BTreeSet<NodeId> {
    let stale = |replica: &NodeId, node_id: &String| {
        let left = !placed.contains(&node_id.as_str());
        let replaced = answers
            .get(node_id)
            .is_some_and(|answer| answer.replica != *replica);
        left || replaced
    };
    members
        .learners
        .iter()
        .filter(|(replica, node_id)| stale(replica, node_id))
        .map(|(replica, _)| *replica)
        .collect()
}

/// One pass for `group` on this node: if it leads the group, identify the
/// placed tail peers and commit the changes [`plan`] gives, adding before
/// removing. It plans from the committed membership only, and changes
/// nothing while a membership change is in flight, while the membership
/// is joint, or on a node that does not lead the group; a later pass
/// picks up from there.
///
/// # Errors
///
/// The membership change the leader could not commit; the next pass
/// starts again from the committed membership.
pub async fn reconcile(
    raft: &GroupRaft,
    placed: &[&str],
    peers: &Peers,
    patience: Duration,
) -> Result<Changes, String> {
    let Some((members, planned_at)) = led_members(raft) else {
        return Ok(Changes::default());
    };
    let mut answers = BTreeMap::new();
    for &node_id in placed {
        if let Ok(identity) = peers.identify(node_id, patience).await {
            answers.insert(node_id.to_owned(), identity);
        }
    }
    // No control kind exists yet (§6.5), so every learner reads the log.
    let changes = plan(&members, placed, &answers, 0);
    apply(raft, &changes, planned_at).await?;
    Ok(changes)
}

/// Commit `changes`, each edit only while the effective membership is
/// still the one stored at `planned_at`, the membership they were planned
/// from, or the one the previous edit of this pass committed. A change
/// committed meanwhile, a joint one above all, fails the edit rather than
/// being flattened by it.
///
/// # Errors
///
/// The edit the leader refused or could not commit; nothing after it is
/// tried.
pub(super) async fn apply(
    raft: &GroupRaft,
    changes: &Changes,
    planned_at: Option<LogId>,
) -> Result<(), String> {
    let edits = [
        (!changes.add.is_empty()).then(|| ChangeMembers::AddNodes(changes.add.clone())),
        (!changes.remove.is_empty()).then(|| ChangeMembers::RemoveNodes(changes.remove.clone())),
    ];
    let mut at = planned_at;
    for edit in edits.into_iter().flatten() {
        let guard = Precondition::LastMembershipLogId {
            last_membership_log_id: at,
        };
        let committed = raft
            .change_membership_if(edit, false, [guard])
            .await
            .map_err(|e| e.to_string())?;
        at = Some(committed.log_id);
    }
    Ok(())
}

/// The group's committed membership, if this node leads it and no change
/// to it is in flight: one still uncommitted would be planned against as
/// if it had been.
fn led_members(raft: &GroupRaft) -> Option<(Members, Option<LogId>)> {
    let metrics = raft.metrics().borrow_watched().clone();
    if metrics.current_leader != Some(metrics.id) {
        return None;
    }
    let committed = &metrics.committed_membership_config;
    if committed.log_id() != metrics.membership_config.log_id() {
        return None;
    }
    stable(committed.membership()).map(|members| (members, *committed.log_id()))
}

/// The members of `membership`, unless it is joint: a learner edit there
/// would collapse the voters to one configuration and move the write
/// quorum, so the pass waits for the joint change to finish.
fn stable(membership: &Membership) -> Option<Members> {
    if membership.get_joint_config().len() != 1 {
        return None;
    }
    let voters: BTreeSet<_> = membership.voter_ids().collect();
    let learners = membership
        .non_write_voter_ids()
        .filter_map(|id| membership.get_node(&id).map(|node| (id, node.clone())))
        .collect();
    Some(Members { voters, learners })
}

#[cfg(test)]
mod tests;
