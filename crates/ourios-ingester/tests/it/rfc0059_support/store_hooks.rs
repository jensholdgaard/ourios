//! A store wrapper that injects failures and races at the high-water.

use std::sync::Arc;

use ourios_parquet::Store;

use super::PutGate;
use super::hooked_store::HookedStore;

/// A store that can be switched off, and can let another writer win the
/// high-water's create.
#[derive(Clone, Default)]
pub struct Hooks {
    pub down: Arc<std::sync::atomic::AtomicBool>,
    pub race_the_create: Arc<std::sync::atomic::AtomicBool>,
    /// Let another writer create the high-water right after the next read
    /// that finds it absent: the window between a start's trust decision
    /// and its seat.
    pub create_after_absent_read: Arc<std::sync::atomic::AtomicBool>,
    /// Refuse every call as an S3 `403` would.
    pub denied: Arc<std::sync::atomic::AtomicBool>,
    /// Reads of the high-water attempted, whether or not they succeed.
    pub high_water_reads: Arc<std::sync::atomic::AtomicUsize>,
    /// When set to `n`, the high-water's `n`th write from now fails, and
    /// every later one with it; 0 (the default) never fails one.
    pub high_water_puts_until_failure: Arc<std::sync::atomic::AtomicUsize>,
    /// Parks the high-water's writes while armed.
    pub high_water_put_gate: Arc<PutGate>,
    /// Delete the high-water just before its next write, which then fails
    /// not-found, as a backend answering a compare-and-swap on a missing
    /// key with `404` does: the window between a reservation's read and
    /// its write.
    pub delete_before_next_put: Arc<std::sync::atomic::AtomicBool>,
    /// Raised by every read of a data file, as a shutdown signal arriving
    /// mid-scan would raise it.
    pub raise_on_data_read: Arc<std::sync::OnceLock<Arc<std::sync::atomic::AtomicBool>>>,
}

impl Hooks {
    pub fn wrap(&self, store: Store) -> Store {
        let hooks = self.clone();
        store.wrap_backend(move |inner| Arc::new(HookedStore { inner, hooks }))
    }

    pub fn set_down(&self, down: bool) {
        self.down.store(down, std::sync::atomic::Ordering::Release);
    }

    /// Fail the high-water write `high_water_puts_until_failure` counts
    /// down to.
    pub(super) fn count_down_put(&self) -> object_store::Result<()> {
        let left = &self.high_water_puts_until_failure;
        match left.load(std::sync::atomic::Ordering::Acquire) {
            0 => Ok(()),
            1 => Err(object_store::Error::Generic {
                store: "hooked",
                source: "the high-water write fails".into(),
            }),
            n => {
                left.store(n - 1, std::sync::atomic::Ordering::Release);
                Ok(())
            }
        }
    }

    pub(super) fn enter(&self) -> object_store::Result<()> {
        if self.denied.load(std::sync::atomic::Ordering::Acquire) {
            return Err(object_store::Error::PermissionDenied {
                path: "hooked".to_owned(),
                source: "AccessDenied".into(),
            });
        }
        if self.down.load(std::sync::atomic::Ordering::Acquire) {
            return Err(object_store::Error::Generic {
                store: "hooked",
                source: "the store is down".into(),
            });
        }
        Ok(())
    }
}
