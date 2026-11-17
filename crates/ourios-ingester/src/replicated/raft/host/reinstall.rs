//! A tail replica's miner restored at a snapshot (RFC 0065 §6.5
//! *Recovery*), then caught up from the log through what the replica
//! applied while it restored.

use tokio::sync::{mpsc, watch};

use super::super::log_store::GroupLogReader;
use super::super::mining::{GroupLogRead, GroupMiner, GroupMinerError, Mined};
use super::super::restoring::{Restored, Restoring};
use super::super::snapshot_record::GroupState;
use super::super::state_machine::{Applied, AppliedEvent, Replay};
use super::super::types::LogId;
use super::{Applying, Follow, Work};

impl Follow {
    /// The snapshot `work` installs, when this replica restores at one.
    pub(super) fn installing(&self, work: &Work) -> Option<(LogId, GroupState)> {
        match (work, &self.restoring) {
            (
                Work::Event(Applied {
                    at,
                    event: AppliedEvent::Installed(state),
                }),
                Some(_),
            ) => Some((*at, *state)),
            _ => None,
        }
    }

    /// The miner restored at the snapshot `installed` names and caught up
    /// through what `applied` carried meanwhile, the replica's snapshot
    /// builds held until it has. `None` once the host says to stop, or
    /// with the miner stopped when it does not restore.
    pub(super) async fn reinstall(
        &self,
        installed: (LogId, GroupState),
        applied: &mut mpsc::Receiver<Applied>,
        stop: &mut watch::Receiver<bool>,
    ) -> Option<GroupMiner> {
        let restoring = self.restoring.as_ref()?;
        let _held = restoring.hold();
        match restoring.restore_taking(installed, applied, stop).await? {
            Ok(restored) => self.catch_up(restoring, restored, stop).await,
            Err((at, unrestored)) => self.unrestorable(at, unrestored.to_string()),
        }
    }

    /// `restored`'s miner, its kept output through the cut dropped and the
    /// flusher its record names assigned, having mined the entries after
    /// the cut through the last the replica applied, read back from the
    /// log.
    async fn catch_up(
        &self,
        restoring: &Restoring<GroupLogReader>,
        restored: Restored,
        stop: &mut watch::Receiver<bool>,
    ) -> Option<GroupMiner> {
        let Restored {
            mut miner,
            at,
            state,
            through,
        } = restored;
        let keeping = self.keeping.clone();
        let applying = Applying {
            cut: Some(at.index),
            assigned: state.flusher,
        };
        let keep = move || keeping.keep(at.index, &Mined::default(), applying);
        tokio::task::spawn_blocking(keep).await.ok()?;
        let mut replay = Replay::from_snapshot(at, state);
        for index in at.index + 1..=through {
            let entry = restoring.log.entry(index);
            let replayed = match entry {
                Some(entry) => replay.apply(entry).map_err(|e| e.to_string()),
                None => Err(format!("the log no longer holds entry {index}")),
            };
            match replayed {
                Ok(Some(event)) => miner = self.follow(miner, event, stop).await?,
                Ok(None) => {}
                Err(detail) => return self.unrestorable(at, detail),
            }
        }
        Some(miner)
    }

    /// Report the miner stopped at the snapshot through `at`, for `detail`.
    fn unrestorable(&self, at: LogId, detail: String) -> Option<GroupMiner> {
        let error = GroupMinerError::Unrestorable { at, detail };
        self.storage.miner_stopped(self.group, &error);
        None
    }
}
