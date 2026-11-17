//! Restoring a tail replica's miner at a snapshot (RFC 0065 §6.5
//! *Recovery*): the replica opened from one, or installed one, so the log
//! below its cut is gone and the miner's state there is in the bundle the
//! cut's checkpoint wrote. It restores from that bundle, never a later
//! one, and mines on from the entry after; while the store cannot be read
//! it retries, and a bundle refused or not there fails the group closed.
//!
//! The replica keeps applying meanwhile: it opens even while the store is
//! out, as its log replays the entries past the snapshot. The miner does not
//! hold what the replica applies while it restores, only how far it got,
//! and once restored it mines those entries back from the log, which keeps
//! them: the replica builds no snapshot past the cut, so purges nothing
//! above it, until the miner has caught up.

use std::fmt;
use std::time::Duration;

use std::sync::Arc;

use ourios_semconv as semconv;
use tokio::sync::{mpsc, watch};

use super::bundle::read::{ReadError, read};
use super::bundle::{Cut, CutId};
use super::codec::wal_id;
use super::flushing::{Source, Unread, formed_log};
use super::host::{next, stopped};
use super::mining::{GroupLogRead, GroupMiner};
use super::snapshot_record::GroupState;
use super::state_machine::{Applied, AppliedEvent};
use super::types::LogId;

/// The first wait before another attempt, doubling to [`MOST`].
const FIRST: Duration = Duration::from_millis(250);
const MOST: Duration = Duration::from_secs(30);

/// Why a miner was not restored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unrestored {
    /// The store could not be read; another attempt may restore.
    Unreachable(String),
    /// What the store holds does not restore: the group fails closed.
    Refused(String),
}

impl fmt::Display for Unrestored {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreachable(why) | Self::Refused(why) => f.write_str(why),
        }
    }
}

/// What a tail replica restores `group`'s miner from.
#[derive(Clone)]
pub(super) struct Restoring<L> {
    pub(super) group: u32,
    pub(super) source: Source,
    /// The group's log, which the restored miner reads back from.
    pub(super) log: L,
    /// Set while a restore runs, which holds the replica's snapshot
    /// builds, and so its purges, at the cut.
    pub(super) hold: Arc<watch::Sender<bool>>,
    /// The replica's snapshot now: its last entry and its record's state.
    pub(super) installed: Installed,
}

/// Reads a replica's current snapshot.
pub(super) type Installed = Arc<dyn Fn() -> Option<(LogId, GroupState)> + Send + Sync>;

/// A miner restored at the snapshot through `at`, whose record holds
/// `state`, and the last entry the replica applied while it restored,
/// which it mines back from the log.
pub(super) struct Restored {
    pub(super) miner: GroupMiner,
    pub(super) at: LogId,
    pub(super) state: GroupState,
    pub(super) through: u64,
}

/// Holds the replica's snapshot builds until dropped.
pub(super) struct Held(Arc<watch::Sender<bool>>);

impl Drop for Held {
    fn drop(&mut self) {
        self.0.send_replace(false);
    }
}

impl<L: GroupLogRead + Clone + Send + 'static> Restoring<L> {
    /// Hold the replica's snapshot builds at the cut until the guard drops.
    pub(super) fn hold(&self) -> Held {
        self.hold.send_replace(true);
        Held(Arc::clone(&self.hold))
    }

    /// The miner restored at the snapshot through `at`, taking what
    /// `applied` carries meanwhile without holding it, from past `through`,
    /// the last entry the replica applied: a later snapshot starts the
    /// restore again at its cut. `None` once `stop` says so or
    /// the group ends.
    ///
    /// # Errors
    ///
    /// The cut and why, when the store holds no bundle there that restores.
    pub(super) async fn restore_taking(
        &self,
        (mut at, mut state, mut through): (LogId, GroupState, u64),
        applied: &mut mpsc::Receiver<Applied>,
        stop: &mut watch::Receiver<bool>,
    ) -> Option<Result<Restored, (LogId, Unrestored)>> {
        through = through.max(at.index);
        loop {
            let mut stopping = stop.clone();
            let restoring = self.restore(at, state, &mut stopping);
            tokio::pin!(restoring);
            loop {
                tokio::select! {
                    restored = &mut restoring => {
                        return restored.map(|restored| {
                            restored
                                .map(|miner| Restored { miner, at, state, through })
                                .map_err(|why| (at, why))
                        });
                    }
                    event = next(applied, stop) => match event? {
                        Applied { at: later, event: AppliedEvent::Installed(installed) } => {
                            (at, state) = (later, installed);
                            through = through.max(later.index);
                            break;
                        }
                        Applied { at: taken, .. } => through = through.max(taken.index),
                    },
                }
            }
        }
    }

    /// The miner at the snapshot through `at`, whose record holds `state`,
    /// once the bundle at its cut restores; `None` once `stop` says so.
    ///
    /// # Errors
    ///
    /// [`Unrestored::Refused`] when the store holds no bundle at the cut
    /// that restores.
    pub(super) async fn restore(
        &self,
        at: LogId,
        state: GroupState,
        stop: &mut watch::Receiver<bool>,
    ) -> Option<Result<GroupMiner, Unrestored>> {
        let (mut wait, mut held) = (FIRST, false);
        loop {
            let restoring = self.clone();
            let attempt = tokio::task::spawn_blocking(move || restoring.attempt(at, state));
            // A read can hang until the store's client gives up; the stop
            // does not wait for it.
            let attempted = tokio::select! {
                attempted = attempt => attempted
                    .unwrap_or_else(|e| Err(Unrestored::Refused(e.to_string()))),
                () = stopped(stop) => return None,
            };
            match attempted {
                Err(Unrestored::Unreachable(why)) => {
                    if !held {
                        held = true;
                        self.report_held(at, &why);
                    }
                }
                done => return Some(done),
            }
            tokio::select! {
                () = tokio::time::sleep(wait) => {}
                () = stopped(stop) => return None,
            }
            wait = (wait * 2).min(MOST);
        }
    }

    /// Report the restore at `at` held: the store could not be read.
    fn report_held(&self, at: LogId, why: &str) {
        tracing::warn!(
            name: semconv::EVENT_OURIOS_WAL_REPLICATION_RESTORE_HELD,
            {
                { semconv::OURIOS_WAL_GROUP } = i64::from(self.group),
                { crate::metrics::ERROR_TYPE } = "store_read",
            },
            "group {} cannot restore its miner at {at} until the store is read: {why}",
            self.group,
        );
    }

    /// One attempt, blocking: the group's log id, then the bundle at the
    /// cut the record names.
    fn attempt(&self, at: LogId, state: GroupState) -> Result<GroupMiner, Unrestored> {
        let log = formed_log(&self.source, self.group).map_err(|unread| match unread {
            Unread::Absent => Unrestored::Refused(format!(
                "group {} has a snapshot through {at} but no formation",
                self.group
            )),
            Unread::Failed {
                error_type: "store_read",
                error,
            } => Unrestored::Unreachable(error),
            Unread::Failed { error, .. } => Unrestored::Refused(error),
        })?;
        let config = state.miner_config.ok_or_else(|| {
            Unrestored::Refused(format!("the snapshot through {at} holds no configuration"))
        })?;
        let id = wal_id(&at);
        let cut = Cut {
            id: CutId {
                log,
                seq: id.index,
                term: id.term,
            },
            latest_block: state.id_block,
            config: Some(config),
        };
        let (bundle, states) = read(&self.source.store, &cut).map_err(|e| match e {
            ReadError::Store { .. } => Unrestored::Unreachable(e.to_string()),
            refused => Unrestored::Refused(refused.to_string()),
        })?;
        GroupMiner::restored(self.log.clone(), (&bundle, &states), (at, config))
            .map_err(|e| Unrestored::Refused(e.to_string()))
    }
}

#[cfg(test)]
mod tests;
