//! Keeps private metadata locks usable during unwind cleanup.
//!
//! Windows uses the inline standard-library mutex to avoid another native lock
//! dependency. Other platforms retain parking_lot, including its allocation-free
//! first lock on macOS. Callers still release guards before invoking callbacks
//! or destroying user payloads. Recovering a poisoned guard does not repair data.

#[cfg(not(windows))]
pub(crate) use parking_lot::{Mutex, MutexGuard};
#[cfg(windows)]
pub(crate) use std::sync::MutexGuard;

/// Preserves access for cleanup after a panic releases the Windows lock.
#[cfg(windows)]
#[derive(Default)]
pub(crate) struct Mutex<T>(
    /// Owns the lock and value without another allocation.
    std::sync::Mutex<T>,
);

#[cfg(windows)]
impl<T> Mutex<T> {
    /// Stores a value in an unlocked mutex.
    #[inline]
    pub(crate) const fn new(value: T) -> Self {
        Self(std::sync::Mutex::new(value))
    }

    /// Borrows the value even after an earlier guard unwound.
    ///
    /// Callers retain responsibility for their own panic-safe state transitions.
    #[inline]
    pub(crate) fn lock(&self) -> MutexGuard<'_, T> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(all(test, windows))]
ordinary_test_items! {
    impl<T> Mutex<T> {
        /// Lets callback probes detect a held guard without blocking.
        pub(crate) fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
            match self.0.try_lock() {
                Ok(guard) => Some(guard),
                Err(std::sync::TryLockError::Poisoned(error)) => Some(error.into_inner()),
                Err(std::sync::TryLockError::WouldBlock) => None,
            }
        }
    }

    /// Keeps the original panic and lets later cleanup access the stored state.
    #[test]
    fn lock_after_unwind_preserves_state_and_original_panic() {
        use std::panic::{AssertUnwindSafe, catch_unwind};

        let value = Mutex::<u32>::default();
        *value.lock() = 7;
        let panic = catch_unwind(AssertUnwindSafe(|| {
            let mut stored = value.lock();
            assert!(value.try_lock().is_none(), "the control must hold the lock");
            *stored = 11;
            panic!("metadata guard unwind");
        }))
        .expect_err("the original guarded operation must unwind");
        assert_eq!(panic.downcast_ref::<&str>(), Some(&"metadata guard unwind"));
        assert!(value.0.is_poisoned(), "the control must reach poison recovery");

        assert_eq!(
            *value
                .try_lock()
                .expect("poison does not hide an unlocked value"),
            11
        );
        {
            let mut stored = value.lock();
            assert_eq!(*stored, 11, "recovery must not reset partial state");
            *stored = 13;
        }
        assert_eq!(*value.lock(), 13, "later cleanup can reacquire the lock");
    }
}
