//! RFC 0052 §3.1's cadence latch: the two shared atomics that decide
//! whether a publication cut may stamp.
//!
//! A panic on any path that holds acknowledged records outside both the
//! sink buffers and Parquet — the age sweep's step, the barrier's own
//! detached flush, an encode worker — leaves those records nowhere a
//! later barrier can see them. A barrier that then observed empty
//! buffers and no in-flight publish would checkpoint straight over
//! them. So every such path carries a guard whose unwinding `Drop`
//! *reports* before it settles the count the barrier waits on, and a
//! cut is refused when a report names an epoch at or below its own.
//!
//! The failure state is **one** packed word rather than two, because a
//! report landing between a reader's two loads can be missed and a
//! clear written against the epoch alone would then erase a failure
//! nothing acted on. Packed, a cut captures the whole state with one
//! acquire load and RFC 0053's clear is a CAS on that exact word.
//!
//! The clear encoding is [`Epoch::RESERVED`] in the high half with the
//! generation zero, and that is load-bearing: a default-zero word would
//! read as "epoch 0 failed", which is at or below every cut's epoch and
//! would refuse every cut on a node that had never panicked.
//! `Epoch::RESERVED` is never assigned to a cut, so the sentinel cannot
//! collide with a real failure.

use std::sync::atomic::{AtomicU64, Ordering};

/// The high half's width, and the mask the low (generation) half keeps.
const EPOCH_SHIFT: u32 = 32;
const GENERATION_MASK: u64 = 0xFFFF_FFFF;

/// A cut's number. Guards carry the epoch of the first cut whose mark
/// could cover their records, which is what keeps the latch honest
/// about scope: a panic in a publish registered *after* cut `E` fails
/// no cut at or below `E`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct Epoch(u32);

impl Epoch {
    /// Never assigned to a cut — the clear sentinel's value.
    const RESERVED: u64 = u32::MAX as u64;

    /// The epoch's ordinal, for logs and tests.
    #[must_use]
    pub fn get(self) -> u32 {
        self.0
    }
}

/// One acquire load of the failure word — the indivisible value a cut
/// decides against.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LatchState(u64);

impl LatchState {
    /// Whether a cut of `epoch` is refused: a cut fails when
    /// `failed_epoch <= epoch`.
    #[must_use]
    pub fn refuses(self, epoch: Epoch) -> bool {
        self.0 >> EPOCH_SHIFT <= u64::from(epoch.0)
    }

    /// The lowest epoch reported failed, or `None` while the latch is
    /// clear.
    #[must_use]
    pub fn failed_epoch(self) -> Option<Epoch> {
        match self.0 >> EPOCH_SHIFT {
            Epoch::RESERVED => None,
            // The high half only ever holds an epoch a cut was given,
            // and those are `u32`s by construction.
            raw => u32::try_from(raw).ok().map(Epoch),
        }
    }

    /// The failure generation. Every report bumps it, so a report that
    /// landed between a cut's capture and RFC 0053's clear makes that
    /// clear's CAS fail and the latch stays set.
    #[must_use]
    pub fn generation(self) -> u64 {
        self.0 & GENERATION_MASK
    }
}

/// The pipeline's shared cadence state, handed at construction to the
/// encode pool, to the sink's publish guards and to the barrier task.
#[derive(Debug)]
pub struct BarrierEpochs {
    /// The number of the **next** cut, taken and incremented at every
    /// capture under the ingest exclusion. A guard registered before
    /// cut `E`'s capture reads `E`; one registered after reads `E + 1`.
    next: AtomicU64,
    /// The packed failure state — see the module docs.
    failed: AtomicU64,
}

impl Default for BarrierEpochs {
    fn default() -> Self {
        Self::new()
    }
}

impl BarrierEpochs {
    /// A node that has never panicked: cut 0 next, the latch clear.
    #[must_use]
    pub fn new() -> Self {
        Self {
            next: AtomicU64::new(0),
            failed: AtomicU64::new(Epoch::RESERVED << EPOCH_SHIFT),
        }
    }

    /// The epoch a guard registered now carries. Read under the ingest
    /// exclusion, so it is ordered against [`Self::open_cut`].
    #[must_use]
    pub fn current(&self) -> Epoch {
        Self::narrow(self.next.load(Ordering::Acquire))
    }

    /// Take the next cut's epoch and advance the counter — the capture
    /// step, under the exclusion.
    ///
    /// The counter stops one below [`Epoch::RESERVED`] rather than
    /// wrapping: a node that somehow reached 2^32 cuts (about 40,000
    /// years at the 300-second default) stops stamping rather than
    /// wrapping into a stale comparison.
    pub fn open_cut(&self) -> Epoch {
        let taken = self
            .next
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |next| {
                (next + 1 < Epoch::RESERVED).then_some(next + 1)
            });
        match taken {
            Ok(previous) | Err(previous) => Self::narrow(previous),
        }
    }

    /// Report `epoch` as failed: lower the failed epoch to it and bump
    /// the generation in the same word, so no report is lost to a
    /// concurrent one.
    ///
    /// Called from a guard's `Drop` **before** it decrements the count
    /// a barrier waits on, so a barrier that observes the count settled
    /// has observed this.
    pub fn report(&self, epoch: Epoch) {
        let mut word = self.failed.load(Ordering::Acquire);
        loop {
            let lowered = (word >> EPOCH_SHIFT).min(u64::from(epoch.0));
            let generation = (word & GENERATION_MASK).wrapping_add(1) & GENERATION_MASK;
            let next = (lowered << EPOCH_SHIFT) | generation;
            match self.failed.compare_exchange_weak(
                word,
                next,
                Ordering::Release,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(seen) => word = seen,
            }
        }
    }

    /// Capture the whole failure state in one acquire load.
    #[must_use]
    pub fn capture(&self) -> LatchState {
        LatchState(self.failed.load(Ordering::Acquire))
    }

    /// The counter is held below [`Epoch::RESERVED`], so every value it
    /// yields is a `u32`.
    fn narrow(raw: u64) -> Epoch {
        Epoch(u32::try_from(raw).unwrap_or(u32::MAX))
    }
}

#[cfg(test)]
mod tests {
    use super::{BarrierEpochs, Epoch};

    #[test]
    fn a_node_that_never_panicked_refuses_no_cut() {
        let epochs = BarrierEpochs::new();
        let cut = epochs.open_cut();
        assert_eq!(cut.get(), 0);
        let state = epochs.capture();
        assert_eq!(state.failed_epoch(), None);
        assert!(
            !state.refuses(cut),
            "the clear sentinel must not read as 'epoch 0 failed'",
        );
    }

    #[test]
    fn a_report_refuses_its_own_cut_and_every_later_one() {
        let epochs = BarrierEpochs::new();
        let first = epochs.open_cut();
        let second = epochs.open_cut();
        epochs.report(second);
        let state = epochs.capture();
        assert_eq!(state.failed_epoch(), Some(second));
        assert!(state.refuses(second), "the failing cut is refused");
        assert!(
            !state.refuses(first),
            "a cut below the reported epoch proceeds — its mark cannot \
             cover records the later guard held",
        );
        assert!(state.refuses(epochs.open_cut()), "and every later cut");
    }

    #[test]
    fn concurrent_reports_keep_the_minimum_and_are_each_counted() {
        let epochs = std::sync::Arc::new(BarrierEpochs::new());
        for _ in 0..8 {
            epochs.open_cut();
        }
        let threads: Vec<_> = (0..8u32)
            .map(|n| {
                let epochs = std::sync::Arc::clone(&epochs);
                std::thread::spawn(move || epochs.report(Epoch(n)))
            })
            .collect();
        for thread in threads {
            thread.join().expect("report thread");
        }
        let state = epochs.capture();
        assert_eq!(
            state.failed_epoch().map(Epoch::get),
            Some(0),
            "the lowest report wins",
        );
        assert_eq!(
            state.generation(),
            8,
            "and none of the eight is lost to a concurrent one",
        );
    }

    #[test]
    fn the_epoch_counter_saturates_rather_than_wrapping() {
        let epochs = BarrierEpochs::new();
        epochs
            .next
            .store(Epoch::RESERVED - 1, std::sync::atomic::Ordering::Release);
        let a = epochs.open_cut();
        let b = epochs.open_cut();
        assert_eq!(a, b, "the ceiling is held rather than wrapped to 0");
        assert_ne!(
            u64::from(a.get()),
            Epoch::RESERVED,
            "and never reaches the reserved sentinel",
        );
    }
}
