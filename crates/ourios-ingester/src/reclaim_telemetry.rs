//! RFC 0052 §3.5: the WAL's reclamation state, exported, and the edges
//! of its refusing states, emitted.
//!
//! Two owners, one per kind of state. Rotation edges belong to the
//! commit coordinator, which compares the journal's rotation state on
//! every guard release — a rotation can fail on one append and recover
//! on the next, so a sampler would miss both. Floor and latch edges
//! belong to the housekeeping timer, which compares a categorical
//! projection before and after each pass; the byte, age and lag figures
//! move on every pass and are never part of it.
//!
//! The instruments are observable and read shared cells, never the
//! journal, so a collection never queues behind an fsync. The callbacks
//! hold those cells weakly: the meter provider outlives a receiver that
//! shuts down, and a restarted one in the same process must not see the
//! old one's figures reported beside its own.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::SystemTime;

use opentelemetry::metrics::{Meter, ObservableGauge, ObservableUpDownCounter};
use opentelemetry::{KeyValue, global};
use ourios_semconv as semconv;
use ourios_wal::{
    HousekeepingProgress, ReclaimState, RetainFloor, RotationFault, RotationSite, RotationState,
};

use crate::cadence::BarrierEpochs;

/// The categorical half of [`RotationState`] — what the rotation status
/// metric reports and what an edge is detected on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RotationPhase {
    Healthy,
    Retrying,
    Terminal,
}

impl RotationPhase {
    const ALL: [Self; 3] = [Self::Healthy, Self::Retrying, Self::Terminal];

    #[must_use]
    pub fn of(state: &RotationState) -> Self {
        match state {
            RotationState::Healthy => Self::Healthy,
            RotationState::Retrying(_) => Self::Retrying,
            RotationState::Terminal(_) => Self::Terminal,
        }
    }

    /// The `ourios.wal.rotation.state` value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Retrying => "retrying",
            Self::Terminal => "terminal",
        }
    }

    fn code(self) -> u64 {
        match self {
            Self::Healthy => 0,
            Self::Retrying => 1,
            Self::Terminal => 2,
        }
    }

    fn from_code(code: u64) -> Self {
        match code {
            1 => Self::Retrying,
            2 => Self::Terminal,
            _ => Self::Healthy,
        }
    }
}

/// RFC 0052 §3.5's `IngestState` cell for the rotation state: written
/// by the coordinator under the journal guard, in the order the WAL
/// observed its outcomes, and read lock-free by the exporter. Phase and
/// attempt count share one word so a reader never pairs one call's phase
/// with another's count.
#[derive(Debug)]
pub struct RotationCell(AtomicU64);

const PHASE_SHIFT: u32 = 32;

impl RotationCell {
    #[must_use]
    pub fn new(state: &RotationState) -> Self {
        Self(AtomicU64::new(Self::pack(state)))
    }

    pub fn store(&self, state: &RotationState) {
        self.0.store(Self::pack(state), Ordering::Release);
    }

    /// The phase and the attempts charged against the budget.
    #[must_use]
    pub fn load(&self) -> (RotationPhase, u32) {
        let word = self.0.load(Ordering::Acquire);
        let attempts = u32::try_from(word & u64::from(u32::MAX)).unwrap_or(u32::MAX);
        (RotationPhase::from_code(word >> PHASE_SHIFT), attempts)
    }

    fn pack(state: &RotationState) -> u64 {
        let attempts = match state {
            RotationState::Healthy => 0,
            RotationState::Retrying(fault) | RotationState::Terminal(fault) => fault.attempts(),
        };
        (RotationPhase::of(state).code() << PHASE_SHIFT) | u64::from(attempts)
    }
}

/// The `error.type` a rotation fault is reported under: the step that
/// first failed, as a bounded token rather than its rendered `op`.
pub(crate) fn fault_error_type(fault: &RotationFault) -> &'static str {
    fault.site().map_or("_OTHER", RotationSite::error_type)
}

/// Emit the registry event for a rotation edge, if `before → after` is
/// one. Entering `Retrying` and leaving it are edges; a further failed
/// attempt inside `Retrying` is not. Entering `Terminal` is, from either
/// side, and nothing leaves it in-process.
pub(crate) fn emit_rotation_edge(before: &RotationState, after: &RotationState) {
    match (RotationPhase::of(before), after) {
        (RotationPhase::Healthy, RotationState::Retrying(fault)) => tracing::warn!(
            name: semconv::EVENT_OURIOS_RECEIVER_WAL_ROTATION_RETRYING,
            { "error.type" = fault_error_type(fault) },
            "WAL rotation failed ({fault}); the next append retries it within the budget"
        ),
        (RotationPhase::Retrying, RotationState::Healthy) => tracing::info!(
            name: semconv::EVENT_OURIOS_RECEIVER_WAL_ROTATION_RECOVERED,
            "a retried WAL rotation succeeded; the rotation state is healthy again"
        ),
        (RotationPhase::Healthy | RotationPhase::Retrying, RotationState::Terminal(fault)) => {
            tracing::error!(
                name: semconv::EVENT_OURIOS_RECEIVER_WAL_ROTATION_TERMINAL,
                { "error.type" = fault_error_type(fault) },
                "WAL rotation retry budget spent ({fault}); appends are refused until a restart"
            );
        }
        _ => {}
    }
}

/// The timer's categorical projection (§3.5): floor kind and latch.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Projection {
    pinned: bool,
    latched: bool,
}

/// What the timer last read, for the instruments to report.
#[derive(Debug, Default)]
struct Observed {
    state: Option<ReclaimState>,
    progress: Option<HousekeepingProgress>,
}

/// The §3.5 export: the WAL state instruments plus the floor and latch
/// edges. Owned by the housekeeping timer, the single observer of the
/// states that change on ticks.
pub struct WalExport {
    observed: Arc<Mutex<Observed>>,
    projection: Mutex<Projection>,
    epochs: Arc<BarrierEpochs>,
    _rotation: Arc<RotationCell>,
    _instruments: Instruments,
}

impl WalExport {
    /// Register the instruments on the `ourios.wal` meter. `rotation` is
    /// the coordinator's cell, read live; everything else is what the
    /// last [`Self::observe`] saw.
    #[must_use]
    pub fn new(rotation: &Arc<RotationCell>, epochs: Arc<BarrierEpochs>) -> Self {
        let observed = Arc::new(Mutex::new(Observed::default()));
        let instruments = Instruments::register(&Arc::downgrade(&observed), rotation);
        Self {
            observed,
            projection: Mutex::new(Projection::default()),
            epochs,
            _rotation: Arc::clone(rotation),
            _instruments: instruments,
        }
    }

    /// Take one pass's reading: emit the floor and latch edges it
    /// crosses, then publish it to the instruments. `progress` is `None`
    /// when the pass produced none (a panic, a journal with no surface);
    /// the last pass's backlog then stays reported.
    pub fn observe(&self, state: ReclaimState, progress: Option<HousekeepingProgress>) {
        let latch = self.epochs.capture().failed_epoch();
        let now = Projection {
            pinned: matches!(state.floor, RetainFloor::Pinned { .. }),
            latched: latch.is_some(),
        };
        let mut projection = self
            .projection
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let before = *projection;
        *projection = now;
        drop(projection);
        match (before.pinned, state.floor) {
            (false, RetainFloor::Pinned { offset, tenants }) => tracing::warn!(
                name: semconv::EVENT_OURIOS_RECEIVER_WAL_RETAIN_FLOOR_PINNED,
                "{tenants} tenant(s) with no valid snapshot pin the WAL retain floor at segment \
                 {} byte {}; reclamation holds their oldest surviving frames",
                offset.segment,
                offset.byte,
            ),
            (true, floor) if !matches!(floor, RetainFloor::Pinned { .. }) => tracing::info!(
                name: semconv::EVENT_OURIOS_RECEIVER_WAL_RETAIN_FLOOR_LIFTED,
                "the WAL retain floor is no longer pinned; every tenant has a snapshot horizon"
            ),
            _ => {}
        }
        if let (false, Some(epoch)) = (before.latched, latch) {
            tracing::error!(
                name: semconv::EVENT_OURIOS_RECEIVER_BARRIER_LATCHED,
                "the cadence_failed latch is set at epoch {}: every cut at or above it is \
                 refused and nothing stamps or reclaims until a restart replays the WAL",
                epoch.get(),
            );
        }
        let mut observed = self.observed.lock().unwrap_or_else(PoisonError::into_inner);
        observed.state = Some(state);
        if progress.is_some() {
            observed.progress = progress;
        }
    }
}

/// The registrations, held so the callbacks live as long as the export.
#[expect(dead_code, reason = "retains the observable-callback registrations")]
struct Instruments {
    updown: Vec<ObservableUpDownCounter<i64>>,
    age: ObservableGauge<f64>,
    gauges: Vec<ObservableGauge<u64>>,
}

type Shared = Weak<Mutex<Observed>>;
type StateRead = fn(&ReclaimState) -> u64;
type BacklogRead = fn(&HousekeepingProgress) -> usize;

const STATE_SUMS: [(&str, &str, StateRead); 7] = [
    (semconv::OURIOS_WAL_SIZE, "By", |s| s.disk_bytes),
    (semconv::OURIOS_WAL_UNFLUSHED_SIZE, "By", |s| {
        s.unflushed_bytes
    }),
    (semconv::OURIOS_WAL_SEGMENT_COUNT, "{segment}", |s| {
        u64::from(s.segment_count)
    }),
    (semconv::OURIOS_WAL_UNRECLAIMED_SIZE, "By", |s| {
        s.unreclaimed_bytes
    }),
    (
        semconv::OURIOS_WAL_RETAIN_FLOOR_PINNED_TENANT_COUNT,
        "{tenant}",
        |s| to_u64(s.floor.pinned_tenants()),
    ),
    (semconv::OURIOS_WAL_RETAIN_FLOOR_LAG_SIZE, "By", |s| {
        s.lag_bytes
    }),
    (
        semconv::OURIOS_WAL_RETAIN_FLOOR_LAG_SEGMENT_COUNT,
        "{segment}",
        |s| to_u64(s.lag_segments),
    ),
];

const BACKLOG: [(&str, BacklogRead); 2] = [
    (semconv::OURIOS_WAL_HOUSEKEEPING_HORIZON_REMAINING, |p| {
        p.horizon_remaining
    }),
    (semconv::OURIOS_WAL_HOUSEKEEPING_UNLINK_REMAINING, |p| {
        p.unlink_remaining
    }),
];

impl Instruments {
    fn register(observed: &Shared, rotation: &Arc<RotationCell>) -> Self {
        let meter = global::meter("ourios.wal");
        let mut updown: Vec<_> = STATE_SUMS
            .into_iter()
            .map(|(name, unit, read)| {
                let observed = Weak::clone(observed);
                meter
                    .i64_observable_up_down_counter(name)
                    .with_unit(unit)
                    .with_callback(move |observer| {
                        if let Some(value) = with(&observed, |o| o.state.as_ref().map(read)) {
                            observer.observe(to_i64(value), &[]);
                        }
                    })
                    .build()
            })
            .collect();
        updown.push(floor_status(&meter, observed));
        updown.push(rotation_status(&meter, rotation));

        let cell = Arc::downgrade(rotation);
        let mut gauges = vec![
            meter
                .u64_observable_gauge(semconv::OURIOS_WAL_ROTATION_CONSECUTIVE_FAILURES)
                .with_unit("{failure}")
                .with_callback(move |observer| {
                    if let Some(cell) = cell.upgrade() {
                        observer.observe(u64::from(cell.load().1), &[]);
                    }
                })
                .build(),
        ];
        gauges.extend(BACKLOG.into_iter().map(|(name, read)| {
            let observed = Weak::clone(observed);
            meter
                .u64_observable_gauge(name)
                .with_unit("{segment}")
                .with_callback(move |observer| {
                    if let Some(value) = with(&observed, |o| o.progress.as_ref().map(read)) {
                        observer.observe(to_u64(value), &[]);
                    }
                })
                .build()
        }));
        Self {
            updown,
            age: unreclaimed_age(&meter, observed),
            gauges,
        }
    }
}

/// A state metric, per the `OpenTelemetry` status-metric convention:
/// every member of the state enum is reported on each observation, 1
/// for the current one and 0 for the rest, so a transition never leaves
/// a stale series at 1.
fn floor_status(meter: &Meter, observed: &Shared) -> ObservableUpDownCounter<i64> {
    let observed = Weak::clone(observed);
    meter
        .i64_observable_up_down_counter(semconv::OURIOS_WAL_RETAIN_FLOOR_STATUS)
        .with_unit("1")
        .with_callback(move |observer| {
            let Some(current) = with(&observed, |o| {
                o.state.as_ref().map(|s| floor_state(s.floor))
            }) else {
                return;
            };
            for kind in FLOOR_STATES {
                observer.observe(
                    i64::from(kind == current),
                    &[KeyValue::new(semconv::OURIOS_WAL_RETAIN_FLOOR_STATE, kind)],
                );
            }
        })
        .build()
}

/// The rotation state metric, read live from the coordinator's cell —
/// the same every-member shape as [`floor_status`].
fn rotation_status(meter: &Meter, rotation: &Arc<RotationCell>) -> ObservableUpDownCounter<i64> {
    let cell = Arc::downgrade(rotation);
    meter
        .i64_observable_up_down_counter(semconv::OURIOS_WAL_ROTATION_STATUS)
        .with_unit("1")
        .with_callback(move |observer| {
            let Some((current, _)) = cell.upgrade().map(|cell| cell.load()) else {
                return;
            };
            for phase in RotationPhase::ALL {
                observer.observe(
                    i64::from(phase == current),
                    &[KeyValue::new(
                        semconv::OURIOS_WAL_ROTATION_STATE,
                        phase.as_str(),
                    )],
                );
            }
        })
        .build()
}

/// The age is taken at collect time from the stored mint time, so it
/// keeps rising between passes during exactly the stall it exists to
/// show.
fn unreclaimed_age(meter: &Meter, observed: &Shared) -> ObservableGauge<f64> {
    let observed = Weak::clone(observed);
    meter
        .f64_observable_gauge(semconv::OURIOS_WAL_UNRECLAIMED_AGE)
        .with_unit("s")
        .with_callback(move |observer| {
            if let Some(oldest) = with(&observed, |o| o.state.as_ref()?.oldest_unreclaimed) {
                let age = SystemTime::now()
                    .duration_since(oldest)
                    .unwrap_or_default()
                    .as_secs_f64();
                observer.observe(age, &[]);
            }
        })
        .build()
}

const FLOOR_STATES: [&str; 4] = ["unknown", "none", "min", "pinned"];

/// The `ourios.wal.retain_floor.state` value.
fn floor_state(floor: RetainFloor) -> &'static str {
    match floor {
        RetainFloor::Unknown => "unknown",
        RetainFloor::None => "none",
        RetainFloor::Min(_) => "min",
        RetainFloor::Pinned { .. } => "pinned",
    }
}

/// Read what the timer last observed, if its export is still alive.
fn with<T>(observed: &Shared, read: impl FnOnce(&Observed) -> Option<T>) -> Option<T> {
    let observed = observed.upgrade()?;
    let guard = observed.lock().unwrap_or_else(PoisonError::into_inner);
    read(&guard)
}

fn to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use ourios_wal::{RotationFault, RotationSite, RotationState};

    use super::{RotationCell, RotationPhase};

    fn fault(attempts: u32) -> ourios_wal::RotationFault {
        RotationFault::new(
            RotationSite::Create,
            &std::io::Error::other("disk full"),
            attempts,
            3,
        )
    }

    #[test]
    fn the_rotation_cell_round_trips_phase_and_attempts() {
        let cell = RotationCell::new(&RotationState::Healthy);
        assert_eq!(cell.load(), (RotationPhase::Healthy, 0));
        cell.store(&RotationState::Retrying(fault(2)));
        assert_eq!(cell.load(), (RotationPhase::Retrying, 2));
        cell.store(&RotationState::Terminal(fault(3)));
        assert_eq!(cell.load(), (RotationPhase::Terminal, 3));
    }
}
