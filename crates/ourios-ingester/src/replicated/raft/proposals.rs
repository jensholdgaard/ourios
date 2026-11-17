//! The leader's side of `Propose` (RFC 0065 §6.8): it takes a flusher's
//! report and checkpoint only while it hosts and leads the group, from the
//! flusher its log names, and only when nothing in them is behind its log.
//! Every replica checks the proposer and the cut again when it applies the
//! entry (§6.5), so these checks only keep out of the log entries every
//! replica would ignore.

use std::collections::BTreeMap;
use std::sync::Arc;

use openraft::async_runtime::WatchReceiver as _;
use openraft::errors::{ClientWriteError, RaftError};
use ourios_wal::Control;

use super::codec::{raft_id, wal_id};
use super::controls::HeldControls;
use super::host::{GroupHost, GroupRaft};
use super::id_blocks::{Reported, Reports};
use super::learners::Group;
use super::log_store::GroupLogReader;
use super::mining::GroupLogRead;
use super::snapshot_record::GroupSnapshot;
use super::state_machine::GroupSm;
use super::transport::propose::{ProposeTarget, Proposed, Refusal};
use super::transport::propose::{ProposedCheckpoint, Report};
use super::types::{GroupEntry, LogId};

/// A node's groups, as `Propose` reaches them.
pub struct Proposals {
    groups: Arc<BTreeMap<u32, Hosted>>,
}

/// One group this node hosts.
#[derive(Clone)]
struct Hosted {
    raft: GroupRaft,
    log: GroupLogReader,
    sm: GroupSm,
    /// One proposal at a time: check 5 and the append see one log.
    one: Arc<tokio::sync::Mutex<()>>,
    /// The latest report it accepted.
    reports: Reports,
}

impl Proposals {
    /// The groups `host` runs.
    #[must_use]
    pub fn new(host: &GroupHost) -> Self {
        let groups = host
            .groups()
            .filter_map(|group| {
                let hosted = Hosted {
                    raft: host.raft(group)?.clone(),
                    log: host.log(group)?.clone(),
                    sm: host.state_machine(group)?.clone(),
                    one: Arc::default(),
                    reports: Reports::default(),
                };
                Some((group, hosted))
            })
            .collect();
        Self {
            groups: Arc::new(groups),
        }
    }
}

#[cfg(test)]
impl Proposals {
    /// Hold `group`'s proposal lock, so a proposal queues behind it.
    pub(super) async fn held(&self, group: u32) -> tokio::sync::OwnedMutexGuard<()> {
        let hosted = self.groups.get(&group).expect("hosted");
        Arc::clone(&hosted.one).lock_owned().await
    }
}

impl Proposals {
    /// The reports `group`'s leader accepted, which its steward tops the
    /// group's id blocks up from.
    #[must_use]
    pub fn reports(&self, group: u32) -> Option<Reports> {
        self.groups.get(&group).map(|hosted| hosted.reports.clone())
    }
}

impl ProposeTarget for Proposals {
    fn propose(
        &self,
        proposed: Proposed,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Option<LogId>, Refusal>> + Send>>
    {
        let hosted = self.groups.get(&proposed.group).cloned();
        Box::pin(async move {
            let Some(hosted) = hosted else {
                return Err(Refusal::NotHosted);
            };
            hosted.propose(proposed.report).await
        })
    }
}

impl Hosted {
    /// Checks 2 to 5 in order, then the checkpoint's append, all under the
    /// group's lock, so a proposal that waited for it is checked against
    /// the leadership and the flusher as they are once it runs.
    async fn propose(&self, report: Result<Report, String>) -> Result<Option<LogId>, Refusal> {
        let _one = self.one.lock().await;
        self.leads()?;
        let report = report.map_err(Refusal::Malformed)?;
        let group = Group {
            raft: &self.raft,
            log: &self.log,
            sm: &self.sm,
        };
        if group.controls().flusher != Some(report.proposer) {
            return Err(Refusal::NotFlusher);
        }
        self.fresh(&report).map_err(Refusal::Stale)?;
        let appended = self.append(&report).await?;
        // Only an accepted request counts toward the top-up.
        if let Some(mined) = report.mined {
            self.reports.record(Reported {
                proposer: report.proposer,
                mined,
                unstarted: report.unstarted,
            });
        }
        Ok(appended)
    }

    /// Append `report`'s checkpoint, if it carries one; its committed log
    /// id.
    async fn append(&self, report: &Report) -> Result<Option<LogId>, Refusal> {
        let Some(ProposedCheckpoint { cut, highest_kind }) = report.checkpoint else {
            return Ok(None);
        };
        let entry = GroupEntry::Control(Control::Checkpoint {
            proposer: *report.proposer.uuid().as_bytes(),
            cut: wal_id(&cut),
            highest_kind,
        });
        match self.raft.client_write(entry).await {
            Ok(written) => Ok(Some(written.log_id)),
            Err(RaftError::APIError(ClientWriteError::ForwardToLeader(forward))) => {
                Err(Refusal::NotLeader {
                    leader: forward.leader_node,
                })
            }
            Err(e) => Err(Refusal::Failed(e.to_string())),
        }
    }

    /// Check 5 against the log as it stands.
    fn fresh(&self, report: &Report) -> Result<(), String> {
        let held = self.log.held_controls();
        let snapshot = self.sm.snapshot();
        let applied = *self.sm.checkpoints().borrow();
        let view = View {
            snapshot: snapshot.as_ref(),
            applied,
            held: &held,
            committed: self.raft.metrics().borrow_watched().local_committed,
            log: &self.log,
        };
        stale(report, &view)
    }

    /// Check 2: this node leads the group, else the leader it knows of.
    fn leads(&self) -> Result<(), Refusal> {
        let metrics = self.raft.metrics().borrow_watched().clone();
        match metrics.current_leader {
            Some(leader) if leader == metrics.id => Ok(()),
            leader => Err(Refusal::NotLeader {
                leader: leader.and_then(|leader| {
                    let membership = metrics.membership_config.membership();
                    membership.get_node(&leader).cloned()
                }),
            }),
        }
    }
}

/// The leader's log as check 5 reads it.
pub(super) struct View<'a> {
    /// The snapshot at the last checkpoint it applied.
    pub(super) snapshot: Option<&'a GroupSnapshot>,
    /// The cut of the last checkpoint it applied, which the snapshot can
    /// lag while it is being built.
    pub(super) applied: Option<LogId>,
    /// The control entries its log holds, committed or not.
    pub(super) held: &'a HeldControls,
    /// Its commit index.
    pub(super) committed: Option<LogId>,
    pub(super) log: &'a dyn GroupLogRead,
}

/// Check 5: `report`'s `mined` is at or above the last applied cut, and
/// its checkpoint, if any, cuts at an entry of the log at or below the
/// commit index, above the last applied cut and every cut the log
/// proposes after it, with the log's highest kind there and a miner
/// configuration in force there. Why it is stale, otherwise.
pub(super) fn stale(report: &Report, view: &View<'_>) -> Result<(), String> {
    let last_cut = view
        .applied
        .max(view.snapshot.and_then(|s| s.meta.last_log_id));
    match report.mined {
        None => Err("the report names no entry its miner followed".to_owned()),
        Some(mined) if Some(mined) < last_cut => {
            Err(format!("its miner followed {mined}, below the last cut"))
        }
        Some(_) => report
            .checkpoint
            .map_or(Ok(()), |checkpoint| stale_cut(checkpoint, last_cut, view)),
    }
}

/// Check 5's checkpoint arm, `last_cut` being the last applied one.
fn stale_cut(
    ProposedCheckpoint { cut, highest_kind }: ProposedCheckpoint,
    last_cut: Option<LogId>,
    view: &View<'_>,
) -> Result<(), String> {
    let held = view.log.entry(cut.index).map(|entry| entry.log_id);
    if held != Some(cut) || view.committed.is_none_or(|c| cut.index > c.index) {
        return Err(format!("the cut {cut} is no committed entry of the log"));
    }
    let proposed = view.held.proposed_after(view.snapshot).map(raft_id);
    if Some(cut) <= last_cut.max(proposed) {
        return Err(format!("the cut {cut} is not above the last one"));
    }
    let at = view.held.at(view.snapshot, cut.index);
    if at.highest_kind != highest_kind {
        return Err(format!(
            "the log's highest kind at {cut} is {}, not {highest_kind}",
            at.highest_kind
        ));
    }
    if !at.configured {
        return Err(format!("no miner configuration is in force at {cut}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
