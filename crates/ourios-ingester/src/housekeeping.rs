//! RFC 0052 §3.2's housekeeping pass, one tick at a time.
//!
//! The receiver runs [`Housekeeper::tick`] every `housekeeping_secs` on a
//! task of its own, separate from the barrier and the age sweep, so a
//! stopped sweep (#795) or a latched barrier never stops reclamation.
//! What a pass may unlink is decided inside the WAL against the
//! checkpoint and the barrier's snapshot ledger; this layer only owns
//! the cadence's failure policy.

use std::sync::Arc;

use ourios_wal::{HousekeepingProgress, ReclaimError};

use crate::barrier::Barrier;
use crate::metrics::{CADENCE_PANIC, ERROR_TYPE};
use crate::publish::PublishCoordinator;
use crate::receiver::CommitCoordinator;
use crate::reclaim_telemetry::WalExport;

/// What one housekeeping tick came to.
#[derive(Debug)]
pub enum HousekeepingTick {
    /// The pass ran; a skipped pass is one of these too, with its reason
    /// in the progress.
    Completed(HousekeepingProgress),
    /// The pass returned an error. Nothing was unlinked past the bound,
    /// and the next tick retries.
    Failed(ReclaimError),
    /// The pass unwound. Counted as `error.type = cadence_panic`; the
    /// checkpoint is untouched and the next tick's prepare re-plans
    /// whatever plan this one left uncommitted (§3.7).
    Panicked,
}

/// The owner of the housekeeping cadence's inputs: the journal owner it
/// reclaims through, the barrier whose installs are the horizons, and
/// the counter a panic is recorded on. It is also RFC 0052 §3.5's
/// single observer of the states that change on ticks, so it owns the
/// WAL export.
pub struct Housekeeper {
    coordinator: Arc<CommitCoordinator>,
    barrier: Arc<Barrier>,
    cadence: PublishCoordinator,
    max_unlinks: usize,
    export: WalExport,
}

impl Housekeeper {
    /// `max_unlinks` is `WalConfig::max_unlinks_per_pass`.
    #[must_use]
    pub fn new(
        coordinator: Arc<CommitCoordinator>,
        barrier: Arc<Barrier>,
        cadence: PublishCoordinator,
        max_unlinks: usize,
    ) -> Self {
        let export = WalExport::new(&coordinator.rotation_cell(), barrier.epochs());
        Self {
            coordinator,
            barrier,
            cadence,
            max_unlinks,
            export,
        }
    }

    /// One capped pass under `catch_unwind`, so a panic costs one tick
    /// rather than the task: reclamation is the only thing standing
    /// between a healthy node and #793's unbounded WAL. Every tick, the
    /// panicked one included, ends by handing the WAL's state to the
    /// export, which is how a run whose checkpoint never advances still
    /// reports its unreclaimed bytes growing.
    pub fn tick(&self) -> HousekeepingTick {
        let pass = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.coordinator
                .maintain(&self.barrier.snapshot_horizons(), self.max_unlinks)
        }));
        let tick = self.settle(pass);
        let progress = match &tick {
            HousekeepingTick::Completed(progress) => Some(*progress),
            HousekeepingTick::Failed(ReclaimError::Housekeeping { progress, .. }) => {
                Some(**progress)
            }
            HousekeepingTick::Failed(_) | HousekeepingTick::Panicked => None,
        };
        self.export
            .observe(self.coordinator.reclaim_state(), progress);
        tick
    }

    fn settle(
        &self,
        pass: std::thread::Result<Result<HousekeepingProgress, ReclaimError>>,
    ) -> HousekeepingTick {
        match pass {
            Ok(Ok(progress)) => HousekeepingTick::Completed(progress),
            Ok(Err(e)) => {
                tracing::warn!(
                    name: ourios_semconv::EVENT_OURIOS_RECEIVER_WAL_HOUSEKEEPING_ERROR,
                    { { ERROR_TYPE } = e.error_type() },
                    "housekeeping: the pass failed; nothing past its bound was unlinked and \
                     the next pass retries: {e}"
                );
                HousekeepingTick::Failed(e)
            }
            Err(_) => {
                self.cadence.record_cadence_panic();
                tracing::error!(
                    name: ourios_semconv::EVENT_OURIOS_RECEIVER_WAL_HOUSEKEEPING_ERROR,
                    { { ERROR_TYPE } = CADENCE_PANIC },
                    "housekeeping: the pass panicked; the checkpoint is untouched and the next \
                     pass re-plans what this one left uncommitted"
                );
                HousekeepingTick::Panicked
            }
        }
    }
}
