//! A group's tail replicas as `openraft` learners (RFC 0065 §6.1). The
//! group's leader keeps its learner set equal to the tail placement: it
//! identifies each placed peer (§6.8), adds those it does not hold under
//! the UUID they answer with, and removes learners that left the placement
//! or whose `node_id` now answers under another UUID. Every change is a
//! committed, learner-only membership change; the voters, the write quorum
//! and voter placement never move.
//!
//! No learner is held back from removal for being the flusher yet: §6.1
//! removes it last once it mines. Once it has gone, the leader's next
//! [`super::flusher`] pass names another.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use openraft::async_runtime::WatchReceiver as _;
use openraft::{ChangeMembers, Precondition};

use super::host::GroupRaft;
use super::log_store::{GroupLogReader, LogControls};
use super::state_machine::GroupSm;
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

/// A group this node hosts: its `Raft`, a view of its log, and its state
/// machine, whose snapshot holds what the log purged.
#[derive(Clone, Copy)]
pub struct Group<'a> {
    pub raft: &'a GroupRaft,
    pub log: &'a GroupLogReader,
    pub sm: &'a GroupSm,
}

impl Group<'_> {
    /// The controls the log holds over those of the group's snapshot.
    #[must_use]
    pub fn controls(&self) -> LogControls {
        self.log.controls_over(self.sm.snapshot().as_ref())
    }
}

/// A group's tail placement and how a pass reaches it.
#[derive(Clone, Copy)]
pub struct Tail<'a> {
    /// The tail replicas' `node_id`s, in placement order.
    pub placed: &'a [&'a str],
    pub peers: &'a Peers,
    /// How long each `Identify` call may take.
    pub patience: Duration,
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
/// `tail` peers and commit the changes [`plan`] gives, adding before
/// removing. It plans from the committed membership only, and changes
/// nothing while a membership change is in flight, while the membership
/// is joint, while the newest entry of a new control kind in its log is
/// uncommitted (§6.5), or on a node that does not lead the group; a later
/// pass picks up from there.
///
/// # Errors
///
/// The membership change the leader could not commit; the next pass
/// starts again from the committed membership.
pub async fn reconcile(group: Group<'_>, tail: Tail<'_>) -> Result<Changes, String> {
    let Some((members, planned_at)) = led_members(group) else {
        return Ok(Changes::default());
    };
    let mut answers = BTreeMap::new();
    for &node_id in tail.placed {
        if let Ok(identity) = tail.peers.identify(node_id, tail.patience).await {
            answers.insert(node_id.to_owned(), identity);
        }
    }
    // A new learner reads every kind in the log, committed or not (§6.8).
    let log_kind = group.controls().highest_kind;
    let changes = plan(&members, tail.placed, &answers, log_kind);
    // An entry of a new kind appended during the `Identify` calls holds
    // the change back until it commits.
    if !kinds_settled(group) {
        return Ok(Changes::default());
    }
    apply(group.raft, &changes, planned_at).await?;
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

/// The group's committed membership, if this node leads it, no change to
/// it is in flight, and `log`'s newest entry of a new kind has committed:
/// a membership still uncommitted would be planned against as if it had
/// been, and §6.5 never has a new kind and a membership change in flight
/// together.
fn led_members(group: Group<'_>) -> Option<(Members, Option<LogId>)> {
    let metrics = group.raft.metrics().borrow_watched().clone();
    if metrics.current_leader != Some(metrics.id) {
        return None;
    }
    let committed = &metrics.committed_membership_config;
    if committed.log_id() != metrics.membership_config.log_id() {
        return None;
    }
    if !kinds_settled(group) {
        return None;
    }
    stable(committed.membership()).map(|members| (members, *committed.log_id()))
}

/// Whether `log`'s newest entry of a new control kind, if any, has
/// committed.
fn kinds_settled(group: Group<'_>) -> bool {
    let committed = group.raft.metrics().borrow_watched().local_committed;
    group
        .controls()
        .introduced
        .is_none_or(|i| committed.is_some_and(|c| c.index >= i))
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
