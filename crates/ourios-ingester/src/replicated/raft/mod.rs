//! A replicated group's consensus storage over RFC 0008 WALs (RFC 0065
//! §4.3, §6.1).
//!
//! Every fsync runs on a writer thread of its own, never on a Raft
//! runtime worker (§4.7): callers hand records over a channel and await
//! the writer's reply, so one fsync completes every record queued while
//! the previous one ran.

mod codec;
pub mod log_store;
pub mod types;
pub mod vote_log;

#[cfg(test)]
mod tests;

use std::collections::BTreeSet;
use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// How long an open waits for a retiring writer, whose last fsync may be
/// slow (§4.7 saw drive-cache flushes of seconds), before refusing.
const OWNER_WAIT: Duration = Duration::from_secs(30);

/// The lock paths this process owns. An OS lock excludes another open
/// file description on a local filesystem, but over NFS `flock` becomes a
/// per-process POSIX lock that a second open in the same process would
/// pass, so ownership needs both.
static OWNED: Mutex<BTreeSet<PathBuf>> = Mutex::new(BTreeSet::new());

/// Exclusive ownership of a lock path, released on drop.
pub(crate) struct Owner {
    path: PathBuf,
    _file: File,
}

impl Drop for Owner {
    fn drop(&mut self) {
        OWNED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.path);
    }
}

fn try_own(path: &Path, file: &File) -> Result<(), TryLockError> {
    let mut owned = OWNED.lock().unwrap_or_else(PoisonError::into_inner);
    if owned.contains(path) {
        return Err(TryLockError::WouldBlock);
    }
    file.try_lock()?;
    owned.insert(path.to_path_buf());
    Ok(())
}

/// Take the lock at `path`, in this process and in the OS, polling for up
/// to `wait`: a retiring writer releases it within one fsync, a live one
/// never does. The WAL root must be on a local filesystem.
fn take_ownership(path: &Path, wait: Duration, owner: &str) -> io::Result<Owner> {
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)?;
    let path = path.canonicalize()?;
    let deadline = Instant::now() + wait;
    loop {
        match try_own(&path, &file) {
            Ok(()) => return Ok(Owner { path, _file: file }),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(TryLockError::WouldBlock) => {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!("{} is held by another live {owner}", path.display()),
                ));
            }
            Err(TryLockError::Error(e)) => return Err(e),
        }
    }
}

fn writer_gone() -> io::Error {
    io::Error::other("the storage writer thread has stopped")
}

/// How a writer thread's batch ended.
enum Flush {
    /// Written, and fsynced when the batch asked for it.
    Done,
    Failed(io::Error),
    /// The test gate stopped the writer before its fsync.
    Crashed,
}

impl Flush {
    fn reply(&self) -> io::Result<()> {
        match self {
            Self::Done => Ok(()),
            Self::Failed(e) => Err(io::Error::new(e.kind(), e.to_string())),
            Self::Crashed => Err(writer_gone()),
        }
    }
}

/// Run `sync` unless the gate stops the writer first (`None`) or fails
/// the fsync.
fn sync_through<T>(
    gate: &gate::SyncGate,
    sync: impl FnOnce() -> io::Result<T>,
) -> Option<io::Result<T>> {
    match gate.pass() {
        Pass::Sync => Some(sync()),
        Pass::Crash => None,
        Pass::Fail => Some(Err(io::Error::other("injected fsync failure"))),
    }
}

/// What a writer does at the gate before an fsync.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pass {
    Sync,
    /// Stop without the fsync, as a crash would (tests only).
    #[cfg_attr(not(test), allow(dead_code))] // constructed only by the test gate
    Crash,
    /// Report the fsync as failed (tests only).
    #[cfg_attr(not(test), allow(dead_code))] // constructed only by the test gate
    Fail,
}

/// Holds a writer before each fsync, so tests can stall the disk, crash
/// a writer between its write and its fsync, or fail the fsync.
#[cfg(test)]
pub(crate) mod gate {
    use std::sync::{Condvar, Mutex, PoisonError};

    use super::Pass;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum State {
        Open,
        Closed,
        Crashed,
        Failing,
        Panicking,
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
        /// Fsyncs still let through before the gate closes itself.
        budget: Option<u64>,
    }

    impl Inner {
        fn holds(&self) -> bool {
            self.state == State::Closed || (self.state == State::Open && self.budget == Some(0))
        }
    }

    impl Default for SyncGate {
        fn default() -> Self {
            Self {
                inner: Mutex::new(Inner {
                    state: State::Open,
                    waiting: 0,
                    passed: 0,
                    budget: None,
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
            self.lock().budget = None;
            self.set(State::Open);
        }

        /// Let `n` more fsyncs through, then hold every later one.
        pub(crate) fn close_after(&self, n: u64) {
            self.lock().budget = Some(n);
            self.changed.notify_all();
        }

        /// Every writer held here, and every later one, stops without
        /// its fsync.
        pub(crate) fn crash(&self) {
            self.set(State::Crashed);
        }

        /// Every fsync from now on fails.
        pub(crate) fn fail(&self) {
            self.set(State::Failing);
        }

        /// Let `n` more fsyncs through, then fail every later one.
        pub(crate) fn fail_after(&self, n: u64) {
            self.lock().budget = Some(n);
            self.set(State::Failing);
        }

        /// The next writer to reach the gate panics.
        pub(crate) fn panic(&self) {
            self.set(State::Panicking);
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

        pub(crate) fn pass(&self) -> Pass {
            let mut inner = self.lock();
            inner.waiting += 1;
            self.changed.notify_all();
            while inner.holds() {
                inner = self
                    .changed
                    .wait(inner)
                    .unwrap_or_else(PoisonError::into_inner);
            }
            inner.waiting -= 1;
            let spared = inner.budget.is_some_and(|n| n > 0);
            inner.budget = inner.budget.map(|n| n.saturating_sub(1));
            match inner.state {
                State::Panicking => panic!("injected writer panic"),
                State::Open | State::Closed => {
                    inner.passed += 1;
                    Pass::Sync
                }
                State::Failing if spared => {
                    inner.passed += 1;
                    Pass::Sync
                }
                State::Crashed => Pass::Crash,
                State::Failing => Pass::Fail,
            }
        }
    }
}

/// The production gate: always open.
#[cfg(not(test))]
pub(crate) mod gate {
    use super::Pass;

    #[derive(Debug, Default)]
    pub(crate) struct SyncGate;

    impl SyncGate {
        #[allow(clippy::unused_self)] // the test build's gate has state
        pub(crate) fn pass(&self) -> Pass {
            Pass::Sync
        }
    }
}
