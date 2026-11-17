//! A tail replica's miner restored at a snapshot (RFC 0065 §6.5
//! *Recovery*), then caught up from the log through what the replica
//! applied while it restored.

use std::sync::Arc;

use tokio::sync::{mpsc, watch};

use super::super::flushing::Source;
use super::super::state_machine::GroupSm;

use super::super::mining::{GroupLogRead, GroupMiner, GroupMinerError, Mined, SharedLog};
use super::super::restoring::{Restored, Restoring};
use super::super::snapshot_record::GroupState;
use super::super::state_machine::{Applied, AppliedEvent, Replay};
use super::super::types::LogId;
use super::{Applying, Follow, Tail, Work};

/// Where `group`'s miner restores from: `source`'s store, over `tail`'s
/// log, reading `sm`'s snapshot.
pub(super) fn restoring(
    group: u32,
    source: Source,
    tail: &Tail,
    sm: &GroupSm,
) -> Restoring<SharedLog> {
    let sm = sm.clone();
    Restoring {
        group,
        source,
        log: Arc::new(tail.log.clone()),
        hold: Arc::clone(&tail.hold),
        installed: Arc::new(move || {
            let snapshot = sm.snapshot()?;
            Some((snapshot.meta.last_log_id?, snapshot.state))
        }),
    }
}

/// How a catch-up ended.
enum Caught {
    Up(GroupMiner),
    /// A later snapshot purged an entry it still had to mine: the snapshot,
    /// and the last entry the replica applied.
    Purged((LogId, GroupState, u64)),
}

impl Follow {
    /// The snapshot `work` installs, when this replica restores at one:
    /// not one `miner` has already mined past, whose log it still holds.
    pub(super) fn installing(&self, work: &Work, miner: &GroupMiner) -> Option<Install> {
        let (
            Work::Event(Applied {
                at,
                event: AppliedEvent::Installed(state),
            }),
            Some(_),
        ) = (work, &self.restoring)
        else {
            return None;
        };
        let passed = miner
            .applied_through()
            .is_some_and(|mined| mined >= at.index);
        Some(if passed {
            Install::Passed
        } else {
            Install::At(*at, *state)
        })
    }

    /// The miner restored at the snapshot `installed` names and caught up
    /// through what `applied` carried meanwhile, the replica's snapshot
    /// builds held until it has. `None` once the host says to stop, or
    /// with the miner stopped when it does not restore.
    pub(super) async fn reinstall(
        &self,
        (at, state): (LogId, GroupState),
        applied: &mut mpsc::Receiver<Applied>,
        stop: &mut watch::Receiver<bool>,
    ) -> Option<GroupMiner> {
        let restoring = self.restoring.as_ref()?;
        let _held = restoring.hold();
        let mut installed = (at, state, at.index);
        loop {
            let restored = match restoring.restore_taking(installed, applied, stop).await? {
                Ok(restored) => restored,
                Err((at, unrestored)) => return self.unrestorable(at, unrestored.to_string()),
            };
            match self.catch_up(restoring, restored, stop).await? {
                Caught::Up(miner) => return Some(miner),
                Caught::Purged(later) => installed = later,
            }
        }
    }

    /// `restored`'s miner, its kept output through the cut dropped and the
    /// flusher its record names assigned, having mined the entries after
    /// the cut through the last the replica applied, read back from the
    /// log; or the later snapshot whose purge took one of them.
    async fn catch_up(
        &self,
        restoring: &Restoring<SharedLog>,
        restored: Restored,
        stop: &mut watch::Receiver<bool>,
    ) -> Option<Caught> {
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
            let Some(entry) = restoring.log.entry(index) else {
                return match (restoring.installed)() {
                    Some((later, state)) if later.index >= index => {
                        Some(Caught::Purged((later, state, through)))
                    }
                    _ => self
                        .unrestorable(at, format!("the log no longer holds entry {index}"))
                        .map(Caught::Up),
                };
            };
            match replay.apply(entry) {
                Ok(Some(event)) => miner = self.follow(miner, event, stop).await?,
                Ok(None) => {}
                Err(e) => return self.unrestorable(at, e.to_string()).map(Caught::Up),
            }
        }
        Some(Caught::Up(miner))
    }

    /// Report the miner stopped at the snapshot through `at`, for `detail`.
    fn unrestorable(&self, at: LogId, detail: String) -> Option<GroupMiner> {
        let error = GroupMinerError::Unrestorable { at, detail };
        self.storage.miner_stopped(self.group, &error);
        None
    }
}

/// What an `Installed` event asks of a replica that restores.
pub(super) enum Install {
    /// Restore at the snapshot through this entry, holding this state.
    At(LogId, GroupState),
    /// The miner has mined past it, and the log still holds what follows.
    Passed,
}

#[cfg(test)]
mod tests;
