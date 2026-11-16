//! A group's log mined again from its first entry (RFC 0065 §6.7's gap
//! replay when no checkpoint has applied): a [`Replay`] of the state
//! machine feeds a fresh [`GroupMiner`], entry by entry, so each entry
//! yields what the live miner mined from it, the same rows at the same
//! positions and the same audit events, timestamps included.

use std::fmt;
use std::io;

use super::mining::{GroupLogRead, GroupMiner, GroupMinerError, Mined};
use super::state_machine::Replay;

/// Why a replay stopped.
#[derive(Debug)]
pub enum ReplayError {
    /// The log no longer holds the entry at `index`.
    Purged { index: u64 },
    /// The state machine refused the entry, as the live one did.
    Refused(io::Error),
    /// The miner stopped, as the live one did.
    Miner(GroupMinerError),
}

impl fmt::Display for ReplayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Purged { index } => write!(f, "the log no longer holds entry {index}"),
            Self::Refused(error) => write!(f, "the state machine refused an entry: {error}"),
            Self::Miner(error) => write!(f, "the miner stopped: {error}"),
        }
    }
}

impl std::error::Error for ReplayError {}

/// The output of the log's entries from its first through `through`, one
/// item per entry the state machine turns into an event for the miner; it
/// ends after `through` or at its first error.
pub struct Replayed<L> {
    log: L,
    replay: Replay,
    miner: GroupMiner,
    next: u64,
    through: u64,
    stopped: bool,
}

impl<L: GroupLogRead + Clone + 'static> Replayed<L> {
    /// Replay `log` from `first`, the index of its first entry, through
    /// `through`.
    #[must_use]
    pub fn new(log: L, first: u64, through: u64) -> Self {
        Self {
            miner: GroupMiner::new(log.clone()),
            log,
            replay: Replay::from_start(),
            next: first,
            through,
            stopped: false,
        }
    }
}

impl<L: GroupLogRead> Replayed<L> {
    /// The next entry the state machine turns into an event, mined.
    fn step(&mut self) -> Option<Result<(u64, Mined), ReplayError>> {
        while self.next <= self.through {
            let index = self.next;
            self.next += 1;
            let Some(entry) = self.log.entry(index) else {
                return Some(Err(ReplayError::Purged { index }));
            };
            let applied = match self.replay.apply(entry) {
                Ok(Some(applied)) => applied,
                Ok(None) => continue,
                Err(error) => return Some(Err(ReplayError::Refused(error))),
            };
            let mined = self.miner.apply(applied).map_err(ReplayError::Miner);
            return Some(mined.map(|mined| (index, mined)));
        }
        None
    }
}

impl<L: GroupLogRead> Iterator for Replayed<L> {
    type Item = Result<(u64, Mined), ReplayError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.stopped {
            return None;
        }
        let item = self.step();
        self.stopped = !matches!(item, Some(Ok(_)));
        item
    }
}

#[cfg(test)]
mod tests;
