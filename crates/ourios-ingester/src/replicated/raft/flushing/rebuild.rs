//! A named flusher's rebuild of what its gap dropped (RFC 0065 §6.7): a
//! bounded replay of the group's log from its first entry, there being no
//! checkpoint to restore from, published a slice at a time. A slice ends
//! once its rows reach the record sink's size trigger, as a flush does,
//! and is made durable, its audit events ahead of its rows, before the
//! next is mined, so the replay holds one slice and its miner's state.

use std::time::{Duration, Instant};

use super::super::mining::SharedLog;
use super::super::publication::Publication;
use super::super::replay::{ReplayError, Replayed};
use crate::record_sink::estimate_bytes;

/// The index of a group log's first entry: `openraft` writes the first
/// membership there, and with no checkpoint applied nothing is purged.
const FIRST: u64 = 0;

/// The most entries one slice mines, so entries that mine no rows, such
/// as batches waiting for a late first configuration, never make a step
/// of the whole replay and hold up the live miner behind it.
const SLICE_ENTRIES: usize = 1024;

/// What one slice of a replay came to.
pub(super) enum Slice {
    /// Every entry through the gap is published, durably.
    Rebuilt,
    /// More is left to replay, or the slice is not yet durable.
    Pending,
    Stopped(ReplayError),
}

pub(super) struct Rebuild {
    replayed: Replayed<SharedLog>,
    started: Instant,
    /// The last slice is published but not yet durable.
    unflushed: bool,
}

impl Rebuild {
    /// A replay of `log` from its first entry through `gap`.
    pub(super) fn start(log: SharedLog, gap: u64) -> Self {
        Self {
            replayed: Replayed::new(log, FIRST, gap),
            started: Instant::now(),
            unflushed: false,
        }
    }

    /// How long since the replay started.
    pub(super) fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Whether the replay has read every entry through `cut`, so a purge
    /// through it removes nothing the replay still needs.
    pub(super) fn passed(&self, cut: u64) -> bool {
        self.replayed.needs_from() > cut
    }

    /// Replay through `gap`, which may have grown, for one slice: until its
    /// rows reach `target_bytes` or it has mined [`SLICE_ENTRIES`] entries.
    /// It is published through `publication` and flushed. A slice the store does not take is flushed again
    /// before the next is mined.
    pub(super) fn slice(
        &mut self,
        gap: u64,
        publication: &Publication,
        target_bytes: usize,
    ) -> Slice {
        self.replayed.reach(gap);
        if self.unflushed && !publication.flush_all() {
            return Slice::Pending;
        }
        self.unflushed = true;
        // Each slice mines at least one entry, so a size trigger of zero
        // still moves the replay on.
        let mut bytes = 0_usize;
        for _ in 0..SLICE_ENTRIES {
            match self.replayed.next() {
                Some(Ok((_, mined))) => {
                    let mined_bytes: usize = mined
                        .rows
                        .iter()
                        .map(|row| estimate_bytes(&row.record))
                        .sum();
                    bytes = bytes.saturating_add(mined_bytes);
                    publication.publish(&mined.audit, &mined.rows);
                    if bytes >= target_bytes.max(1) {
                        break;
                    }
                }
                Some(Err(error)) => return Slice::Stopped(error),
                None => break,
            }
        }
        if !publication.flush_all() {
            return Slice::Pending;
        }
        self.unflushed = false;
        if self.replayed.done() {
            Slice::Rebuilt
        } else {
            Slice::Pending
        }
    }
}
