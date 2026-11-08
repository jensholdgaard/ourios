//! The group's flusher (RFC 0065 §6.5): one tail replica, named by a
//! flusher assignment the leader appends. The leader names the first
//! healthy learner in tail placement order when the group has none, and
//! replaces the flusher only once it has left the learners or been
//! unhealthy for the patience. A recovered replica does not take the role
//! back, and a new leader keeps the flusher its log names until it has led
//! for the patience.
//!
//! A learner is healthy while its last acknowledgement of the leader's
//! appends or heartbeats is younger than the patience, so it has been
//! unhealthy since that acknowledgement plus the patience, or, with none
//! under this leader, since the leader took over.
//!
//! A flusher that leaves the tail placement while still a learner is
//! §6.1's handoff, to a replica that has mined through the commit point;
//! until replicas mine, it is kept until it is no longer a learner.
//!
//! The assignment is control kind `0x01`. While the log has never held it,
//! the leader appends it only with no membership change in flight and once
//! every member of its latest membership has answered `Identify` reading
//! it (§6.8). [`Steward::pass`] runs the learner reconcile and this step in
//! turn, so a membership change and a new kind are never proposed at once.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use openraft::Instant as _;
use openraft::RaftMetrics;
use openraft::async_runtime::WatchReceiver as _;
use ourios_wal::Control;

use super::host::GroupRaft;
use super::learners::{self, Changes, Group, Tail};
use super::telemetry::{FlusherCounts, Meters};
use super::types::{GroupEntry, NodeId, RaftTypes};

/// How long a flusher may go unhealthy, and a new leader keep its log's
/// flusher, before the leader names another (§6.5). Fixed, not a setting.
pub const PATIENCE: Duration = Duration::from_secs(10);

/// A learner of the committed membership, as the leader sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Learner {
    pub replica: NodeId,
    /// In the group's tail placement; one that left it is being removed.
    pub placed: bool,
    /// The age of its last acknowledgement under this leader.
    pub acked: Option<Duration>,
}

impl Learner {
    fn healthy(&self, patience: Duration) -> bool {
        self.acked.is_some_and(|age| age < patience)
    }

    fn unhealthy_for(&self, led_for: Duration, patience: Duration) -> Duration {
        self.acked
            .map_or(led_for, |age| age.saturating_sub(patience))
    }
}

/// What the leader decides the flusher from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    /// The replica the newest assignment in the leader's log names.
    pub flusher: Option<NodeId>,
    /// The committed learners, placed ones first in placement order.
    pub learners: Vec<Learner>,
    /// How long this node has led the group.
    pub led_for: Duration,
}

/// Why the leader names a flusher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// The group had no flusher.
    Unassigned,
    /// The flusher was no longer a learner of the committed membership.
    NotLearner,
    /// The flusher had been unhealthy for the patience.
    Unhealthy,
}

impl Reason {
    /// The `ourios.wal.replication.flusher.assignment.reason` value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unassigned => "unassigned",
            Self::NotLearner => "not_learner",
            Self::Unhealthy => "unhealthy",
        }
    }
}

/// An assignment the leader should append: the replica it names, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Assignment {
    pub replica: NodeId,
    pub reason: Reason,
}

impl Assignment {
    fn control(self) -> Control {
        Control::FlusherAssignment(*self.replica.uuid().as_bytes())
    }
}

/// The assignment to append, if the leader should append one.
#[must_use]
pub fn plan(view: &View, patience: Duration) -> Option<Assignment> {
    let reason = replacing(view, patience)?;
    view.learners
        .iter()
        .find(|l| l.placed && l.healthy(patience))
        .map(|l| Assignment {
            replica: l.replica,
            reason,
        })
}

/// Why the flusher the log names is to be replaced, if it is.
fn replacing(view: &View, patience: Duration) -> Option<Reason> {
    let current = view
        .flusher
        .map(|f| view.learners.iter().find(|l| l.replica == f));
    match current {
        Some(_) if view.led_for < patience => None,
        None => Some(Reason::Unassigned),
        Some(None) => Some(Reason::NotLearner),
        Some(Some(flusher)) if flusher.unhealthy_for(view.led_for, patience) >= patience => {
            Some(Reason::Unhealthy)
        }
        Some(Some(_)) => None,
    }
}

/// Why the leader held back a kind its log has never held (§6.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hold {
    /// A member of the latest membership had not answered `Identify`
    /// reading the kind.
    MemberUnconfirmed,
    /// A membership change was in flight.
    MembershipChange,
}

impl Hold {
    /// The `ourios.wal.replication.control.hold.reason` value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MemberUnconfirmed => "member_unconfirmed",
            Self::MembershipChange => "membership_change",
        }
    }
}

/// What the kind gate decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gate {
    /// Every member reads the kind.
    Reads,
    Held(Hold),
    /// The term changed during the round, so this node may no longer lead.
    Deposed,
}

/// What one pass changed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Pass {
    pub learners: Changes,
    /// The replica a committed assignment now names.
    pub assigned: Option<NodeId>,
}

/// A group's leader-side pass on this node: the learner reconcile, then
/// the flusher.
#[derive(Debug)]
pub struct Steward {
    patience: Duration,
    /// The term this node leads in, and when this node first saw itself
    /// leading it.
    led: Option<(u64, Instant)>,
    counts: FlusherCounts,
}

impl Default for Steward {
    fn default() -> Self {
        Self::with_patience(PATIENCE)
    }
}

impl Steward {
    /// A steward whose flusher patience is `patience`: [`PATIENCE`] but in
    /// tests.
    #[must_use]
    pub(crate) fn with_patience(patience: Duration) -> Self {
        Self::counting(patience, Meters::global().flusher())
    }

    /// A steward with patience `patience` that counts to `counts`.
    #[must_use]
    pub(crate) fn counting(patience: Duration, counts: FlusherCounts) -> Self {
        Self {
            patience,
            led: None,
            counts,
        }
    }

    /// Reconcile `tail` as the group's learners ([`learners::reconcile`]),
    /// then append a flusher assignment if [`plan`] gives one. Each step
    /// changes nothing on a node that does not lead the group.
    ///
    /// # Errors
    ///
    /// The membership change or the assignment the leader could not
    /// commit; the next pass plans again from the log.
    pub async fn pass(&mut self, group: Group<'_>, tail: Tail<'_>) -> Result<Pass, String> {
        let learners = learners::reconcile(group, tail).await?;
        let assigned = self.assign(group, tail).await?;
        Ok(Pass { learners, assigned })
    }

    /// Append the assignment [`plan`] gives, held back while the log has
    /// never held its kind and not every member reads it.
    async fn assign(&mut self, group: Group<'_>, tail: Tail<'_>) -> Result<Option<NodeId>, String> {
        let Some(chosen) = self.choose(group, tail) else {
            return Ok(None);
        };
        let Some(next) = self.admit(group, tail, chosen).await else {
            return Ok(None);
        };
        group
            .raft
            .client_write(GroupEntry::Control(next.control()))
            .await
            .map_err(|e| e.to_string())?;
        self.counts.assigned(group.log.group(), next.reason);
        Ok(Some(next.replica))
    }

    /// `planned`, unless its kind is above `highest_kind`, the highest the
    /// log holds: then only once every member reads it and the leader still
    /// chooses its replica.
    async fn admit(
        &mut self,
        group: Group<'_>,
        tail: Tail<'_>,
        (planned, highest_kind): (Assignment, u8),
    ) -> Option<Assignment> {
        let kind = planned.control().kind();
        if kind <= highest_kind {
            return Some(planned);
        }
        let metrics = group.raft.metrics().borrow_watched().clone();
        match gate(group.raft, &metrics, tail, kind).await {
            Gate::Reads => {}
            Gate::Held(hold) => {
                self.counts.held(group.log.group(), hold);
                return None;
            }
            Gate::Deposed => return None,
        }
        // The round can outlast the patience, so the replica may no longer
        // be healthy, or no longer the one to choose.
        self.choose(group, tail)
            .map(|(chosen, _)| chosen)
            .filter(|chosen| chosen.replica == planned.replica)
    }

    /// The assignment [`plan`] gives from what the leader sees now, with the
    /// highest kind its log holds.
    fn choose(&mut self, group: Group<'_>, tail: Tail<'_>) -> Option<(Assignment, u8)> {
        let metrics = group.raft.metrics().borrow_watched().clone();
        let led_for = self.led_for(&metrics)?;
        let controls = group.log.controls();
        let view = View {
            flusher: controls.flusher,
            learners: committed_learners(&metrics, tail.placed),
            led_for,
        };
        plan(&view, self.patience).map(|next| (next, controls.highest_kind))
    }

    /// How long this node has led the group, if it leads it.
    fn led_for(&mut self, metrics: &RaftMetrics<RaftTypes>) -> Option<Duration> {
        if metrics.current_leader != Some(metrics.id) {
            self.led = None;
            return None;
        }
        let term = metrics.current_term;
        let since = match self.led {
            Some((led, since)) if led == term => since,
            _ => self.led.insert((term, Instant::now())).1,
        };
        Some(since.elapsed())
    }
}

/// The committed membership's learners, placed ones first in `placed`
/// order, each with the age of its last acknowledgement.
fn committed_learners(metrics: &RaftMetrics<RaftTypes>, placed: &[&str]) -> Vec<Learner> {
    let membership = metrics.committed_membership_config.membership();
    let acked = |replica: &NodeId| {
        let heartbeat = metrics.heartbeat.as_ref()?.get(replica)?.as_ref()?;
        Some(heartbeat.elapsed())
    };
    let mut learners: Vec<_> = membership
        .non_write_voter_ids()
        .filter_map(|replica| {
            let node_id = membership.get_node(&replica)?;
            let position = placed.iter().position(|&p| p == node_id);
            let learner = Learner {
                replica,
                placed: position.is_some(),
                acked: acked(&replica),
            };
            Some((position.unwrap_or(usize::MAX), learner))
        })
        .collect();
    learners.sort_by_key(|&(position, _)| position);
    learners.into_iter().map(|(_, learner)| learner).collect()
}

/// Whether every member of the leader's latest membership reads `kind`:
/// with no membership change in flight, each answered `Identify` under
/// the UUID it is a member as, with a kind at or above it, and the term
/// and membership are unchanged once they have. The leader reads every
/// kind it appends.
async fn gate(
    raft: &GroupRaft,
    metrics: &RaftMetrics<RaftTypes>,
    tail: Tail<'_>,
    kind: u8,
) -> Gate {
    if !membership_settled(metrics) {
        return Gate::Held(Hold::MembershipChange);
    }
    let membership = metrics.membership_config.membership();
    let members: BTreeSet<_> = membership
        .voter_ids()
        .chain(membership.non_write_voter_ids())
        .filter(|&replica| replica != metrics.id)
        .collect();
    for replica in members {
        let Some(node_id) = membership.get_node(&replica) else {
            return Gate::Held(Hold::MemberUnconfirmed);
        };
        let reads = tail
            .peers
            .identify(node_id, tail.patience)
            .await
            .is_ok_and(|answer| answer.replica == replica && answer.highest_control_kind >= kind);
        if !reads {
            return Gate::Held(Hold::MemberUnconfirmed);
        }
    }
    let now = raft.metrics().borrow_watched().clone();
    let unchanged = membership_settled(&now)
        && now.membership_config.log_id() == metrics.membership_config.log_id();
    match (now.current_term == metrics.current_term, unchanged) {
        (false, _) => Gate::Deposed,
        (true, false) => Gate::Held(Hold::MembershipChange),
        (true, true) => Gate::Reads,
    }
}

/// Whether the latest membership has committed.
fn membership_settled(metrics: &RaftMetrics<RaftTypes>) -> bool {
    metrics.membership_config.log_id() == metrics.committed_membership_config.log_id()
}

#[cfg(test)]
mod tests;
