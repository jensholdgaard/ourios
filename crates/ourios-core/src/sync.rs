//! Lock helpers shared across crates.

use std::sync::{Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Lock `mutex`, taking the guard even if another thread panicked while
/// holding it.
///
/// Use it only where every critical section leaves the protected value
/// consistent at each step, so a panic part-way cannot expose a torn state.
pub fn lock_recover<T: ?Sized>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// [`lock_recover`] for the shared side of an [`RwLock`].
pub fn read_recover<T: ?Sized>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(PoisonError::into_inner)
}

/// [`lock_recover`] for the exclusive side of an [`RwLock`].
pub fn write_recover<T: ?Sized>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_recover_returns_the_value_behind_a_poisoned_mutex() {
        let mutex = Mutex::new(7);
        let _ = std::panic::catch_unwind(|| {
            let _guard = mutex.lock().unwrap();
            panic!("poison the mutex");
        });
        assert!(mutex.is_poisoned());
        assert_eq!(*lock_recover(&mutex), 7);
    }

    #[test]
    fn read_and_write_recover_return_the_value_behind_a_poisoned_rwlock() {
        let lock = RwLock::new(7);
        let _ = std::panic::catch_unwind(|| {
            let _guard = lock.write().unwrap();
            panic!("poison the rwlock");
        });
        assert!(lock.is_poisoned());
        *write_recover(&lock) += 1;
        assert_eq!(*read_recover(&lock), 8);
    }
}
