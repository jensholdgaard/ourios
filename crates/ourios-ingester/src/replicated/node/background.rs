//! A replicated node's background work (RFC 0065 §6.1, §6.5, §6.8): each
//! voter group's formation and every group's steward, each reporting why it
//! waits or fails once per reason rather than once per attempt.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use ourios_config::MinerConfig;
use ourios_parquet::Store;
use ourios_semconv as semconv;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use super::super::raft::flusher::{self, Steward, StewardError};
use super::super::raft::formation::{self, Patience, Reach, Voter};
use super::super::raft::learners::{Group, Tail};
use super::super::raft::telemetry::Meters;
use super::Node;

/// How long a forming voter waits on each `Identify` call, and between
/// passes. Tuned from `ourios.wal.replication.start.waiting` in real starts.
pub const FORMATION: Patience = Patience {
    identify: Duration::from_secs(5),
    retry: Duration::from_secs(1),
};

/// How often a group's steward passes. Tuned from the flusher-assignment and
/// control-hold counts and `ourios.wal.replication.steward.failed`.
pub const STEWARD_INTERVAL: Duration = Duration::from_secs(1);

/// What a node's background work needs besides the node.
pub struct Background {
    /// The store holding the formation records and the template ids.
    pub store: Store,
    /// The node's effective miner configuration, which a leader proposes.
    pub miner: MinerConfig,
    /// Each group's tail replicas, in placement order.
    pub placed: BTreeMap<u32, Vec<String>>,
}

/// A reason reported once, until it changes or clears.
#[derive(Debug, Default)]
pub(super) struct Once(Option<&'static str>);

impl Once {
    /// `reason`, if it is new: neither the last one reported nor `None`.
    /// `None` clears it, so the same reason is reported again later.
    pub(super) fn changed(&mut self, reason: Option<&'static str>) -> Option<&'static str> {
        let new = reason.filter(|now| self.0 != Some(*now));
        self.0 = reason;
        new
    }
}

impl Node {
    /// Run the node's background work until `stop` says to stop: each voter
    /// group's formation and every group's steward.
    #[must_use]
    pub fn run(
        &self,
        background: &Background,
        stop: &watch::Receiver<bool>,
    ) -> Vec<JoinHandle<()>> {
        let mut tasks = Vec::new();
        for group in self.host.groups() {
            if self.voter {
                tasks.extend(self.forming(group, &background.store, stop.clone()));
            }
            tasks.extend(self.stewarding(group, background, stop.clone()));
        }
        tasks
    }

    /// Form `group` from its record, reporting each new reason it waits.
    fn forming(
        &self,
        group: u32,
        store: &Store,
        mut stop: watch::Receiver<bool>,
    ) -> Option<JoinHandle<()>> {
        let raft = self.host.raft(group)?.clone();
        let voters: Vec<String> = self
            .layout
            .voters(group)
            .into_iter()
            .map(str::to_owned)
            .collect();
        let (name, peers, store) = (self.name.clone(), self.peers.clone(), store.clone());
        Some(tokio::spawn(async move {
            let voters: Vec<&str> = voters.iter().map(String::as_str).collect();
            let voter = Voter {
                group,
                voters: &voters,
                node_id: &name,
            };
            let reach = Reach {
                store: &store,
                peers: &peers,
                patience: FORMATION,
            };
            let mut once = Once::default();
            let mut waiting = |why: &formation::Waiting| {
                if let Some(reason) = once.changed(Some(why.as_str())) {
                    report_waiting(Some(group), reason, why);
                }
            };
            tokio::select! {
                formed = formation::form(voter, &raft, &reach, &mut waiting) => {
                    if let Err(error) = formed {
                        Meters::global().storage().formation_stopped(group, &error);
                    }
                }
                () = stopped(&mut stop) => {}
            }
        }))
    }

    /// Pass `group`'s steward every [`STEWARD_INTERVAL`], reporting each new
    /// reason a pass fails.
    fn stewarding(
        &self,
        group: u32,
        background: &Background,
        mut stop: watch::Receiver<bool>,
    ) -> Option<JoinHandle<()>> {
        let raft = self.host.raft(group)?.clone();
        let log = self.host.log(group)?.clone();
        let sm = self.host.state_machine(group)?.clone();
        let mut steward = Steward::new(background.miner, background.store.clone());
        if let Some(reports) = self.proposals.reports(group) {
            steward = steward.with_reports(reports);
        }
        let placed = background.placed.get(&group).cloned().unwrap_or_default();
        let peers = self.peers.clone();
        Some(tokio::spawn(async move {
            let placed: Vec<&str> = placed.iter().map(String::as_str).collect();
            let mut ticks = tokio::time::interval(STEWARD_INTERVAL);
            ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
            let mut once = Once::default();
            loop {
                let group_of = Group {
                    raft: &raft,
                    log: &log,
                    sm: &sm,
                };
                let tail = Tail {
                    placed: &placed,
                    peers: &peers,
                    patience: flusher::PATIENCE,
                };
                let passed = tokio::select! {
                    _ = ticks.tick() => steward.pass(group_of, tail).await,
                    () = stopped(&mut stop) => return,
                };
                let reason = passed.as_ref().err().map(StewardError::error_type);
                if let (Some(reason), Err(error)) = (once.changed(reason), &passed) {
                    report_failed(group, reason, error);
                }
            }
        }))
    }
}

/// Report a start waiting: on the bootstrap lock, or for `group` to form.
pub(super) fn report_waiting(group: Option<u32>, reason: &'static str, why: &dyn fmt::Display) {
    tracing::warn!(
        name: semconv::EVENT_OURIOS_WAL_REPLICATION_START_WAITING,
        {
            { crate::metrics::ERROR_TYPE } = reason,
            { semconv::OURIOS_WAL_GROUP } = group.map(i64::from),
        },
        "the replicated start waits: {why}",
    );
}

/// Report `group`'s steward pass failing.
fn report_failed(group: u32, reason: &'static str, error: &StewardError) {
    tracing::warn!(
        name: semconv::EVENT_OURIOS_WAL_REPLICATION_STEWARD_FAILED,
        {
            { semconv::OURIOS_WAL_GROUP } = i64::from(group),
            { crate::metrics::ERROR_TYPE } = reason,
        },
        "the steward of group {group} failed a pass: {error}",
    );
}

/// Once `stop` says to stop, or its sender is gone.
async fn stopped(stop: &mut watch::Receiver<bool>) {
    let _ = stop.wait_for(|stop| *stop).await;
}

#[cfg(test)]
mod tests;
