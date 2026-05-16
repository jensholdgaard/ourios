//! Wall-clock abstraction for time-dependent producers.
//!
//! Anything that stamps a [`SystemTime`] onto a record (audit
//! events today; future snapshots, dedup windows, WAL framing)
//! goes through [`Clock`] rather than calling [`SystemTime::now`]
//! directly. The motivation is two-fold:
//!
//! - **Determinism in tests.** Wall-clock assertions are flaky
//!   under NTP step / leap-second / VM-pause. Tests that need to
//!   observe a timestamp inject a [`TestClock`] with a fixed
//!   value and assert exact equality.
//! - **Future-proofing the producer surface.** The mining and
//!   WAL paths will eventually need a monotonic vs. wall-clock
//!   split, time-skew detection on multi-ingester deployments
//!   (`hazards.md` H8), and a single seam to plug those concerns
//!   in.
//!
//! The trait is `Send` so a `Box<dyn Clock>` can move across
//! threads with the cluster that owns it, matching the
//! [`crate::audit::AuditSink`] shape.

use std::time::SystemTime;

/// Source of "now" for time-dependent producers.
///
/// The contract is intentionally narrow: return *some*
/// [`SystemTime`] suitable for stamping a record. Implementations
/// are free to use the host clock, a fixed value, a recorded
/// trace, or a monotonic increment — the consumer should not
/// assume which.
pub trait Clock: Send {
    /// Returns the clock's current value. Idempotent under
    /// repeat calls only for clocks that explicitly say so
    /// (e.g. [`TestClock`]); [`SystemClock`] advances with the
    /// host wall clock between calls.
    fn now(&self) -> SystemTime;
}

/// Wall-clock implementation backed by [`SystemTime::now`].
///
/// Production default — the host clock with all of its real-world
/// quirks (NTP correction, leap seconds, VM pauses). Producers
/// that need deterministic time substitute a different impl at
/// construction.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl SystemClock {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// Fixed-time clock for tests.
///
/// Returns the same [`SystemTime`] from every [`Clock::now`]
/// call, regardless of host wall-clock progression. Sufficient
/// for tests that assert a single event's timestamp; tests that
/// need multiple distinct times can construct a clock per
/// observation or wrap the value in their own interior-mutable
/// container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TestClock(SystemTime);

impl TestClock {
    /// Build a clock that always returns `t`.
    #[must_use]
    pub const fn new(t: SystemTime) -> Self {
        Self(t)
    }

    /// Convenience: a clock pinned to the Unix epoch. Useful when
    /// the exact time value is uninteresting and the test just
    /// wants a stable timestamp for equality assertions.
    #[must_use]
    pub const fn epoch() -> Self {
        Self(SystemTime::UNIX_EPOCH)
    }
}

impl Clock for TestClock {
    fn now(&self) -> SystemTime {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_clock_returns_its_constructed_value() {
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let c = TestClock::new(t);
        assert_eq!(c.now(), t);
        // Idempotent — same value on repeat calls.
        assert_eq!(c.now(), t);
    }

    #[test]
    fn test_clock_epoch_returns_unix_epoch() {
        let c = TestClock::epoch();
        assert_eq!(c.now(), SystemTime::UNIX_EPOCH);
    }

    #[test]
    fn system_clock_returns_a_real_wall_clock_value() {
        // The clock itself is not under test — just that the
        // trait impl exists and produces a sane non-zero value.
        let c = SystemClock::new();
        let t = c.now();
        assert!(t > SystemTime::UNIX_EPOCH);
    }
}
