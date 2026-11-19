//! A group's log mined again from its first entry (RFC 0065 §6.7's gap
//! replay when no checkpoint has applied): a [`Replay`] of the state
//! machine feeds a fresh [`GroupMiner`], entry by entry, so each entry
//! yields what the live miner mined from it, the same rows at the same
//! positions and the same audit events, timestamps included. The batches
//! held for a late first configuration come one per item, under the
//! configuration's index, so no item holds more than one entry's output.

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

impl ReplayError {
    /// Its `error.type`: the miner's for a stopped miner.
    #[must_use]
    pub fn error_type(&self) -> &'static str {
        match self {
            Self::Purged { .. } => "purged",
            // The live state machine refused it too, and stopped the replica.
            Self::Refused(_) => "_OTHER",
            Self::Miner(error) => error.error_type(),
        }
    }
}

/// The output of the log's entries from its first through `through`: an
/// item per entry the state machine turns into an event for the miner, and
/// one more per batch a first configuration held, each under the index of
/// the entry that mined it. Past `through` it yields nothing until
/// [`Self::reach`] moves it on, and after its first error nothing at all.
pub struct Replayed<L> {
    log: L,
    replay: Replay,
    miner: GroupMiner,
    next: u64,
    through: u64,
    /// The index of the last entry the miner followed.
    followed: u64,
    /// It stopped at an error, and yields nothing more.
    failed: bool,
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
            followed: first,
            failed: false,
        }
    }
}

impl<L: GroupLogRead> Replayed<L> {
    /// Replay on through `through`, if that is further.
    pub fn reach(&mut self, through: u64) {
        self.through = self.through.max(through);
    }

    /// Every entry through the last one asked for is mined and yielded.
    #[must_use]
    pub fn done(&self) -> bool {
        self.next > self.through && !self.miner.holds()
    }

    /// The lowest index it has still to read: the next entry, or a batch
    /// a first configuration holds below it. Every entry below this is read.
    #[must_use]
    pub fn needs_from(&self) -> u64 {
        self.miner
            .held_from()
            .map_or(self.next, |held| held.min(self.next))
    }

    /// As [`Iterator::next`], reading at most `reads` more of the log,
    /// what it reads taken from `reads`; `None` once they are spent or the
    /// replay is done ([`Self::done`] tells which). A run of entries that
    /// mine nothing, such as blanks and membership changes, then costs a
    /// caller its budget rather than a step of any length (#1310).
    pub fn next_within(&mut self, reads: &mut usize) -> Option<Result<(u64, Mined), ReplayError>> {
        if self.failed {
            return None;
        }
        let item = self.step(reads);
        self.failed = matches!(item, Some(Err(_)));
        item
    }

    /// The next entry the state machine turns into an event, mined,
    /// reading at most `reads` entries of the log.
    fn step(&mut self, reads: &mut usize) -> Option<Result<(u64, Mined), ReplayError>> {
        if let Some(held) = self.miner.next_held_within(reads) {
            let index = self.followed;
            return Some(held.map(|mined| (index, mined)).map_err(ReplayError::Miner));
        }
        // The budget ran out with batches still held: they come before
        // any later entry.
        if self.miner.holds() {
            return None;
        }
        while self.next <= self.through && *reads > 0 {
            *reads -= 1;
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
            self.followed = index;
            let mined = self.miner.step(applied).map_err(ReplayError::Miner);
            return Some(mined.map(|mined| (index, mined)));
        }
        None
    }
}

impl<L: GroupLogRead> Iterator for Replayed<L> {
    type Item = Result<(u64, Mined), ReplayError>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut unbounded = usize::MAX;
        self.next_within(&mut unbounded)
    }
}

#[cfg(test)]
mod tests;
