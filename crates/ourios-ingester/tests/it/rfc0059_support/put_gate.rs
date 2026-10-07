//! A gate that parks the high-water's writes.

use std::time::Duration;

/// Holds every high-water write while armed, so a test can act while a
/// reservation is in flight between its read and its write.
#[derive(Default)]
pub struct PutGate {
    state: std::sync::Mutex<GateState>,
    changed: std::sync::Condvar,
}

#[derive(Default)]
struct GateState {
    armed: bool,
    parked: usize,
}

impl PutGate {
    fn state(&self) -> std::sync::MutexGuard<'_, GateState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn arm(&self) {
        self.state().armed = true;
    }

    /// Wait until a write is parked at the gate.
    pub fn wait_parked(&self) {
        let (state, timeout) = self
            .changed
            .wait_timeout_while(self.state(), Duration::from_secs(30), |s| s.parked == 0)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            !timeout.timed_out() && state.parked > 0,
            "no high-water write reached the gate"
        );
    }

    pub fn release(&self) {
        self.state().armed = false;
        self.changed.notify_all();
    }

    pub(super) fn pass(&self) {
        let mut state = self.state();
        if !state.armed {
            return;
        }
        state.parked += 1;
        self.changed.notify_all();
        drop(
            self.changed
                .wait_while(state, |s| s.armed)
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
    }
}
