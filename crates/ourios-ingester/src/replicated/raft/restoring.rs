//! Restoring a tail replica's miner at a snapshot (RFC 0065 §6.5
//! *Recovery*): the replica opened from one, or installed one, so the log
//! below its cut is gone and the miner's state there is in the bundle the
//! cut's checkpoint wrote. It restores from that bundle, never a later
//! one, and mines on from the entry after; while the store cannot be read
//! it retries, and a bundle refused or not there fails the group closed.

use std::fmt;
use std::time::Duration;

use tokio::sync::watch;

use super::bundle::read::{ReadError, read};
use super::bundle::{Cut, CutId};
use super::codec::wal_id;
use super::flushing::{Source, Unread, formed_log};
use super::host::stopped;
use super::mining::{GroupLogRead, GroupMiner};
use super::snapshot_record::GroupState;
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
}

impl<L: GroupLogRead + Clone + Send + 'static> Restoring<L> {
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
        let mut wait = FIRST;
        loop {
            let restoring = self.clone();
            let attempt = tokio::task::spawn_blocking(move || restoring.attempt(at, state));
            let attempted = attempt
                .await
                .unwrap_or_else(|e| Err(Unrestored::Refused(e.to_string())));
            match attempted {
                Err(Unrestored::Unreachable(why)) => {
                    tracing::warn!(
                        group = self.group,
                        error.type = "store_read",
                        "cannot read the bundle at {at} to restore the miner: {why}"
                    );
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
