//! Live compaction measurements (issue #925): the bytes each phase tracks
//! against its budget, the budgets themselves, and spill volume. Atomics
//! only, written by the compacting thread and read by whoever exports them,
//! so this crate stays free of any telemetry SDK.

use std::sync::atomic::{AtomicU64, Ordering};

/// The phase of a partition compaction a memory measurement belongs to.
///
/// What each phase tracks, and the budget it is tracked against:
///
/// | Phase | Usage | Limit | Usage may exceed the limit by |
/// |---|---|---|---|
/// | `Read` | encoded bytes of the input object held while it decodes | none | — |
/// | `Sort` | decoded-row buffer of RFC 0036 §3.2 phase 1 | the decoded-row budget | one input's decoded rows |
/// | `Merge` | open runs' decoded batches, each row charged at its run's widest row, plus the output chunk as it fills | the decoded-row budget | one widest row per open run, plus one output chunk |
/// | `Write` | the consolidated writer's in-progress row group (encoded estimate), after each sub-batch | the row-group rotation threshold | one sub-batch |
///
/// The read phase has no budget: an input object is fetched whole.
/// Neither does the encoded output the consolidated writer buffers until
/// its single `put`; only its in-progress row group is bounded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryPhase {
    /// Reading and decoding input files.
    Read,
    /// Forming sorted runs (RFC 0036 §3.2 phase 1).
    Sort,
    /// Merging sorted runs (RFC 0036 §3.2 phase 2).
    Merge,
    /// Encoding and writing the consolidated output.
    Write,
}

impl MemoryPhase {
    /// Every phase.
    pub const ALL: [Self; 4] = [Self::Read, Self::Sort, Self::Merge, Self::Write];

    /// The `ourios.compaction.phase` attribute value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Sort => "sort",
            Self::Merge => "merge",
            Self::Write => "write",
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

/// No budget configured for a phase.
const UNSET: u64 = u64::MAX;

/// Live measurements of the compactions run with them
/// ([`RowHooks::stats`](super::RowHooks::stats)). One per process is the
/// intended use: usage returns to zero when a compaction ends, the limits
/// keep the last compaction's budgets, and the spill totals are cumulative.
#[derive(Debug)]
pub struct CompactionStats {
    usage: [AtomicU64; 4],
    peak: [AtomicU64; 4],
    limit: [AtomicU64; 4],
    spill_runs: AtomicU64,
    spill_bytes: AtomicU64,
}

impl Default for CompactionStats {
    fn default() -> Self {
        Self::new()
    }
}

impl CompactionStats {
    /// No compaction measured yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            usage: [const { AtomicU64::new(0) }; 4],
            peak: [const { AtomicU64::new(0) }; 4],
            limit: [const { AtomicU64::new(UNSET) }; 4],
            spill_runs: AtomicU64::new(0),
            spill_bytes: AtomicU64::new(0),
        }
    }

    /// Bytes `phase` currently tracks against its budget.
    #[must_use]
    pub fn usage(&self, phase: MemoryPhase) -> u64 {
        self.usage[phase.index()].load(Ordering::Relaxed)
    }

    /// The highest [`usage`](Self::usage) `phase` has reached since this was
    /// built: every update is seen, unlike a periodic read.
    #[must_use]
    pub fn peak(&self, phase: MemoryPhase) -> u64 {
        self.peak[phase.index()].load(Ordering::Relaxed)
    }

    /// The budget `phase` was last compacted under, or `None` when the phase
    /// has none or no compaction has run.
    #[must_use]
    pub fn limit(&self, phase: MemoryPhase) -> Option<u64> {
        match self.limit[phase.index()].load(Ordering::Relaxed) {
            UNSET => None,
            limit => Some(limit),
        }
    }

    /// Sorted runs written to local scratch: phase-1 spills and the
    /// intermediate runs of a hierarchical merge.
    #[must_use]
    pub fn spill_runs(&self) -> u64 {
        self.spill_runs.load(Ordering::Relaxed)
    }

    /// Bytes of those runs on local scratch.
    #[must_use]
    pub fn spill_bytes(&self) -> u64 {
        self.spill_bytes.load(Ordering::Relaxed)
    }

    fn set_usage(&self, phase: MemoryPhase, bytes: u64) {
        self.usage[phase.index()].store(bytes, Ordering::Relaxed);
        self.peak[phase.index()].fetch_max(bytes, Ordering::Relaxed);
    }

    fn set_limit(&self, phase: MemoryPhase, bytes: u64) {
        self.limit[phase.index()].store(bytes, Ordering::Relaxed);
    }

    fn add_spill(&self, bytes: u64) {
        self.spill_runs.fetch_add(1, Ordering::Relaxed);
        self.spill_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    fn clear_usage(&self) {
        for usage in &self.usage {
            usage.store(0, Ordering::Relaxed);
        }
    }
}

/// The [`CompactionStats`] a compaction reports to, if any; every update
/// is a no-op without one.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Gauges<'a>(pub(super) Option<&'a CompactionStats>);

impl Gauges<'_> {
    pub(super) fn set(self, phase: MemoryPhase, bytes: u64) {
        if let Some(stats) = self.0 {
            stats.set_usage(phase, bytes);
        }
    }

    pub(super) fn set_limit(self, phase: MemoryPhase, bytes: u64) {
        if let Some(stats) = self.0 {
            stats.set_limit(phase, bytes);
        }
    }

    pub(super) fn spilled(self, bytes: u64) {
        if let Some(stats) = self.0 {
            stats.add_spill(bytes);
        }
    }
}

/// Zeroes every phase's usage when dropped, so a compaction that ends —
/// committed, a no-op, or failed part-way through a phase — leaves none
/// behind.
pub(super) struct UsageReset<'a>(pub(super) Gauges<'a>);

impl Drop for UsageReset<'_> {
    fn drop(&mut self) {
        if let Some(stats) = self.0.0 {
            stats.clear_usage();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_tracks_its_peak_and_resets_while_limits_and_spills_persist() {
        // Arrange
        let stats = CompactionStats::new();
        let gauges = Gauges(Some(&stats));

        // Act
        {
            let _reset = UsageReset(gauges);
            gauges.set_limit(MemoryPhase::Sort, 100);
            gauges.set(MemoryPhase::Sort, 70);
            gauges.set(MemoryPhase::Sort, 30);
            gauges.spilled(5);
            gauges.spilled(7);
        }

        // Assert
        assert_eq!(
            (
                stats.usage(MemoryPhase::Sort),
                stats.peak(MemoryPhase::Sort),
                stats.limit(MemoryPhase::Sort),
                stats.limit(MemoryPhase::Read),
                stats.spill_runs(),
                stats.spill_bytes(),
            ),
            (0, 70, Some(100), None, 2, 12)
        );
    }
}
