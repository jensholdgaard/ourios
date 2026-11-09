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
//! it (§6.8). [`Steward::pass`] runs the learner reconcile, this step and
//! the miner configuration's in turn, so a membership change and a new
//! kind are never proposed at once.
//!
//! The leader appends a miner configuration entry, kind `0x04`, when
//! neither its log after the snapshot nor the snapshot holds one, or when
//! its own effective configuration differs from the one in force in a
//! field other than the tree's shape (`miner_config::plan`), under the
//! same kind gate.
//!
//! Last, while the group holds fewer than [`id_blocks::AHEAD`] id blocks,
//! the leader reserves one in the store and appends it, kind `0x03`, under
//! the same kind gate, one block a pass (`id_blocks`).

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use openraft::Instant as _;
use openraft::RaftMetrics;
use openraft::async_runtime::WatchReceiver as _;
use ourios_config::MinerConfig;
use ourios_parquet::Store;
use ourios_wal::{Control, MinerConfigV1};

use super::host::GroupRaft;
use super::id_blocks::{self, Ahead, TopUp};
use super::learners::{self, Changes, Group, Tail};
use super::miner_config;
use super::telemetry::{FlusherCounts, Meters};
use super::types::{GroupEntry, LogId, NodeId, RaftTypes};

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

/// A control the leader plans to append.
struct Planned<F> {
    kind: u8,
    /// The highest kind the log held when the control was planned.
    highest_kind: u8,
    /// Whether the leader still plans the control, after a kind gate
    /// round.
    still: F,
}

/// What one pass changed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Pass {
    pub learners: Changes,
    /// The replica a committed assignment now names.
    pub assigned: Option<NodeId>,
    /// The miner configuration a committed entry now puts in force.
    pub configured: Option<MinerConfigV1>,
    /// What the leader did for a group short of id blocks.
    pub id_block: Option<TopUp>,
}

/// A group's leader-side pass on this node: the learner reconcile, the
/// flusher, the miner configuration, then the id blocks.
#[derive(Debug)]
pub struct Steward {
    patience: Duration,
    /// This node's own effective miner configuration, which it puts in
    /// force while it leads (§6.5).
    miner: MinerConfig,
    /// The store holding RFC 0059's template-id high-water.
    ids: Store,
    /// The term this node leads in, and when this node first saw itself
    /// leading it.
    led: Option<(u64, Instant)>,
    counts: FlusherCounts,
}

impl Steward {
    /// A steward whose node's own effective miner configuration is
    /// `miner`, reserving id blocks in `ids`.
    #[must_use]
    pub fn new(miner: MinerConfig, ids: Store) -> Self {
        Self {
            patience: PATIENCE,
            miner,
            ids,
            led: None,
            counts: Meters::global().flusher(),
        }
    }

    /// This steward, with `miner` as its node's own effective miner
    /// configuration.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn configured(self, miner: MinerConfig) -> Self {
        Self { miner, ..self }
    }

    /// This steward, reserving id blocks in `ids`.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn reserving(self, ids: Store) -> Self {
        Self { ids, ..self }
    }

    /// A steward whose flusher patience is `patience`, over an empty
    /// in-memory store.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_patience(patience: Duration) -> Self {
        Self::counting(patience, Meters::global().flusher())
    }

    /// A steward with patience `patience` that counts to `counts`, over an
    /// empty in-memory store.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn counting(patience: Duration, counts: FlusherCounts) -> Self {
        Self {
            patience,
            miner: MinerConfig::default(),
            ids: Store::in_memory(),
            led: None,
            counts,
        }
    }

    /// Reconcile `tail` as the group's learners ([`learners::reconcile`]),
    /// append a flusher assignment if [`plan`] gives one, a miner
    /// configuration entry if `miner_config::plan` gives one, then an id
    /// block if the group is short of them. Each step changes nothing on a
    /// node that does not lead the group.
    ///
    /// # Errors
    ///
    /// The membership change or the entry the leader could not commit;
    /// the next pass plans again from the log.
    pub async fn pass(&mut self, group: Group<'_>, tail: Tail<'_>) -> Result<Pass, String> {
        let learners = learners::reconcile(group, tail).await?;
        let assigned = self.assign(group, tail).await?;
        let configured = self.configure(group, tail).await?;
        let id_block = self.top_up(group, tail).await?;
        Ok(Pass {
            learners,
            assigned,
            configured,
            id_block,
        })
    }

    /// Append the assignment [`plan`] gives, held back while the log has
    /// never held its kind and not every member reads it.
    async fn assign(&mut self, group: Group<'_>, tail: Tail<'_>) -> Result<Option<NodeId>, String> {
        let Some((chosen, highest_kind)) = self.choose(group, tail) else {
            return Ok(None);
        };
        let control = chosen.control();
        let planned = Planned {
            kind: control.kind(),
            highest_kind,
            // The round can outlast the patience, so the replica may no
            // longer be healthy, or no longer the one to choose.
            still: |steward: &mut Self| {
                steward
                    .choose(group, tail)
                    .is_some_and(|(again, _)| again.control() == control)
            },
        };
        if !self.admit(group, tail, planned).await {
            return Ok(None);
        }
        propose(group, chosen.control()).await?;
        self.counts.assigned(group.log.group(), chosen.reason);
        Ok(Some(chosen.replica))
    }

    /// Append the miner configuration entry `miner_config::plan` gives,
    /// held back as [`Self::assign`]'s assignment is. An entry appended and
    /// not yet committed is the one in force to the next pass, so a pass
    /// after this one appends none until the configuration changes.
    async fn configure(
        &mut self,
        group: Group<'_>,
        tail: Tail<'_>,
    ) -> Result<Option<MinerConfigV1>, String> {
        let Some((body, highest_kind)) = self.plan_config(group) else {
            return Ok(None);
        };
        let planned = Planned {
            kind: Control::MinerConfig(body).kind(),
            highest_kind,
            // An entry appended during the round may have put another in
            // force.
            still: |steward: &mut Self| {
                steward
                    .plan_config(group)
                    .is_some_and(|(again, _)| again == body)
            },
        };
        if !self.admit(group, tail, planned).await {
            return Ok(None);
        }
        propose(group, Control::MinerConfig(body)).await?;
        Ok(Some(body))
    }

    /// Reserve an id block in the store and append it while the group is
    /// short of them, held back as [`Self::assign`]'s assignment is. The
    /// block is reserved only once the kind gate admits it, and appended
    /// only once its reservation is durable; a failed reservation appends
    /// nothing. A block appended and not yet committed counts to the next
    /// pass, so no pass appends one beside it.
    async fn top_up(&mut self, group: Group<'_>, tail: Tail<'_>) -> Result<Option<TopUp>, String> {
        let Some((floor, highest_kind)) = self.plan_block(group) else {
            return Ok(None);
        };
        let planned = Planned {
            kind: id_blocks::KIND,
            highest_kind,
            still: |steward: &mut Self| steward.plan_block(group).is_some(),
        };
        let admitted = standing(group.raft);
        if !self.admit(group, tail, planned).await {
            return Ok(None);
        }
        let block = match id_blocks::reserve(&self.ids, floor, group.log.group()).await {
            Ok(block) => block,
            Err(refusal) => return Ok(Some(TopUp::Refused(refusal))),
        };
        // A term or membership other than the one admitted under may hold
        // a member the gate never asked, and a group topped up meanwhile
        // needs no block: the block's ids are skipped, never appended later.
        if standing(group.raft) != admitted || self.plan_block(group).is_none() {
            return Ok(None);
        }
        propose(group, id_blocks::control(block)).await?;
        Ok(Some(TopUp::Appended(block)))
    }

    /// Whether the leader may append `planned`'s control: at once when its
    /// kind is at or below the highest the log holds, otherwise once every
    /// member reads the kind and the leader still plans the control.
    async fn admit(
        &mut self,
        group: Group<'_>,
        tail: Tail<'_>,
        planned: Planned<impl FnOnce(&mut Self) -> bool>,
    ) -> bool {
        let Planned {
            kind,
            highest_kind,
            still,
        } = planned;
        if kind <= highest_kind {
            return true;
        }
        let metrics = group.raft.metrics().borrow_watched().clone();
        match gate(group.raft, &metrics, tail, kind).await {
            Gate::Reads => still(self),
            Gate::Held(hold) => {
                self.counts.held(group.log.group(), hold);
                false
            }
            Gate::Deposed => false,
        }
    }

    /// The miner configuration entry `miner_config::plan` gives over the
    /// one in force, with the highest kind the log holds, if this node
    /// leads the group.
    fn plan_config(&mut self, group: Group<'_>) -> Option<(MinerConfigV1, u8)> {
        let metrics = group.raft.metrics().borrow_watched().clone();
        self.led_for(&metrics)?;
        let controls = group.controls();
        miner_config::plan(&self.miner, controls.miner_config.as_ref())
            .map(|body| (body, controls.highest_kind))
    }

    /// The floor of the id block to reserve while the group is short of
    /// them, with the highest kind the log holds, if this node leads the
    /// group.
    fn plan_block(&mut self, group: Group<'_>) -> Option<(u64, u8)> {
        let metrics = group.raft.metrics().borrow_watched().clone();
        self.led_for(&metrics)?;
        let floor = Ahead::of(group).short()?;
        Some((floor, group.controls().highest_kind))
    }

    /// The assignment [`plan`] gives from what the leader sees now, with the
    /// highest kind its log holds.
    fn choose(&mut self, group: Group<'_>, tail: Tail<'_>) -> Option<(Assignment, u8)> {
        let metrics = group.raft.metrics().borrow_watched().clone();
        let led_for = self.led_for(&metrics)?;
        let controls = group.controls();
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

/// The term and the latest membership's log id a kind gate decides under.
fn standing(raft: &GroupRaft) -> (u64, Option<LogId>) {
    let metrics = raft.metrics().borrow_watched().clone();
    (metrics.current_term, *metrics.membership_config.log_id())
}

/// Append `control` to the group's log and wait for it to commit.
async fn propose(group: Group<'_>, control: Control) -> Result<(), String> {
    group
        .raft
        .client_write(GroupEntry::Control(control))
        .await
        .map(drop)
        .map_err(|e| e.to_string())
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
    let confirmed = every_member_confirms(metrics, tail, kind).await;
    let now = raft.metrics().borrow_watched().clone();
    let unchanged = membership_settled(&now)
        && now.membership_config.log_id() == metrics.membership_config.log_id();
    verdict(
        now.current_term == metrics.current_term,
        unchanged,
        confirmed,
    )
}

/// What a gate round found. A lost term or a changed membership outranks
/// an unconfirmed member, which may only have failed because of it.
fn verdict(same_term: bool, membership_unchanged: bool, confirmed: bool) -> Gate {
    match (same_term, membership_unchanged, confirmed) {
        (false, _, _) => Gate::Deposed,
        (true, false, _) => Gate::Held(Hold::MembershipChange),
        (true, true, false) => Gate::Held(Hold::MemberUnconfirmed),
        (true, true, true) => Gate::Reads,
    }
}

/// Whether every member of `metrics`' latest membership but this node
/// answered `Identify` under the UUID it is a member as, reading `kind`.
async fn every_member_confirms(metrics: &RaftMetrics<RaftTypes>, tail: Tail<'_>, kind: u8) -> bool {
    let membership = metrics.membership_config.membership();
    let members: BTreeSet<_> = membership
        .voter_ids()
        .chain(membership.non_write_voter_ids())
        .filter(|&replica| replica != metrics.id)
        .collect();
    for replica in members {
        let Some(node_id) = membership.get_node(&replica) else {
            return false;
        };
        let reads = tail
            .peers
            .identify(node_id, tail.patience)
            .await
            .is_ok_and(|answer| answer.replica == replica && answer.highest_control_kind >= kind);
        if !reads {
            return false;
        }
    }
    true
}

/// Whether the latest membership has committed.
fn membership_settled(metrics: &RaftMetrics<RaftTypes>) -> bool {
    metrics.membership_config.log_id() == metrics.committed_membership_config.log_id()
}

#[cfg(test)]
mod tests;
