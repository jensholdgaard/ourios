//! A replicated group's consensus storage over RFC 0008 WALs (RFC 0065
//! §4.3, §6.1).
//!
//! Every fsync runs on a writer thread of its own, never on a Raft
//! runtime worker (§4.7): callers hand records over a channel and await
//! the writer's reply, so one fsync completes every record queued while
//! the previous one ran.

pub mod types;
pub mod vote_log;

use std::io;

fn writer_gone() -> io::Error {
    io::Error::other("the storage writer thread has stopped")
}

/// Holds a writer before each fsync, so tests can stall the disk or
/// crash a writer between its write and its fsync.
#[cfg(test)]
pub(crate) mod gate {
    use std::sync::{Condvar, Mutex, PoisonError};

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum State {
        Open,
        Closed,
        Crashed,
    }

    #[derive(Debug)]
    pub(crate) struct SyncGate {
        inner: Mutex<Inner>,
        changed: Condvar,
    }

    #[derive(Debug)]
    struct Inner {
        state: State,
        waiting: usize,
        passed: u64,
    }

    impl Default for SyncGate {
        fn default() -> Self {
            Self {
                inner: Mutex::new(Inner {
                    state: State::Open,
                    waiting: 0,
                    passed: 0,
                }),
                changed: Condvar::new(),
            }
        }
    }

    impl SyncGate {
        fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
            self.inner.lock().unwrap_or_else(PoisonError::into_inner)
        }

        fn set(&self, state: State) {
            self.lock().state = state;
            self.changed.notify_all();
        }

        pub(crate) fn close(&self) {
            self.set(State::Closed);
        }

        pub(crate) fn open(&self) {
            self.set(State::Open);
        }

        /// Every writer held here, and every later one, stops without
        /// its fsync.
        pub(crate) fn crash(&self) {
            self.set(State::Crashed);
        }

        /// Fsyncs that went through.
        pub(crate) fn passed(&self) -> u64 {
            self.lock().passed
        }

        /// Blocks until a writer is held at the gate.
        pub(crate) fn wait_for_writer(&self) {
            let mut inner = self.lock();
            while inner.waiting == 0 {
                inner = self
                    .changed
                    .wait(inner)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        }

        /// Whether the writer may fsync; `false` once crashed.
        pub(crate) fn pass(&self) -> bool {
            let mut inner = self.lock();
            inner.waiting += 1;
            self.changed.notify_all();
            while inner.state == State::Closed {
                inner = self
                    .changed
                    .wait(inner)
                    .unwrap_or_else(PoisonError::into_inner);
            }
            inner.waiting -= 1;
            let open = inner.state == State::Open;
            if open {
                inner.passed += 1;
            }
            open
        }
    }
}

/// The production gate: always open.
#[cfg(not(test))]
pub(crate) mod gate {
    #[derive(Debug, Default)]
    pub(crate) struct SyncGate;

    impl SyncGate {
        #[allow(clippy::unused_self)] // the test build's gate has state
        pub(crate) fn pass(&self) -> bool {
            true
        }
    }
}
