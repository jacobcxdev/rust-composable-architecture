//! A simple mutex implementation.
//!
//! This mutex exposes both blocking and async methods for acquiring a lock.
//! It does not promise fair acquisition or poison the data after a panic.

#[cfg(all(test, feature = "std", not(target_family = "wasm")))]
#[path = "../tests/support/mod.rs"]
mod support;

#[cfg(all(feature = "std", not(target_family = "wasm")))]
/// Runs the native example where blocking listener waits are supported.
mod example {
    use std::ops::{Deref, DerefMut};
    use std::sync::{mpsc, Arc};
    use std::thread;

    use event_listener::{listener, Event, Listener};
    use try_lock::{Locked, TryLock};

    /// A simple mutex.
    struct Mutex<T> {
        /// Blocked lock operations.
        lock_ops: Event,

        /// The inner non-blocking mutex.
        data: TryLock<T>,
    }

    impl<T> Mutex<T> {
        /// Creates a mutex.
        fn new(t: T) -> Mutex<T> {
            Mutex {
                lock_ops: Event::new(),
                data: TryLock::new(t),
            }
        }

        /// Attempts to acquire a lock.
        fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
            self.data.try_lock().map(|l| MutexGuard {
                locked: l,
                _notify: NotifyOnDrop(&self.lock_ops),
            })
        }

        /// Blocks until a lock is acquired.
        fn lock(&self) -> MutexGuard<'_, T> {
            loop {
                // Attempt grabbing a lock.
                if let Some(guard) = self.try_lock() {
                    return guard;
                }

                // Register before retrying so an intervening unlock cannot be lost.
                listener!(self.lock_ops => listener);

                // The first failure can precede registration and its notification.
                if let Some(guard) = self.try_lock() {
                    return guard;
                }

                // Wait for a notification.
                listener.wait();
            }
        }

        /// Acquires a lock asynchronously.
        async fn lock_async(&self) -> MutexGuard<'_, T> {
            loop {
                // Attempt grabbing a lock.
                if let Some(guard) = self.try_lock() {
                    return guard;
                }

                // Register before retrying so an intervening unlock cannot be lost.
                listener!(self.lock_ops => listener);

                // The first failure can precede registration and its notification.
                if let Some(guard) = self.try_lock() {
                    return guard;
                }

                // Wait until a notification is received.
                listener.await;
            }
        }
    }

    /// Holds exclusive access and retains the inner guard's non-Send, non-Sync bounds.
    struct MutexGuard<'a, T> {
        /// Unlocks first because fields drop in declaration order.
        locked: Locked<'a, T>,
        /// Notifies only after the protected value is available again.
        _notify: NotifyOnDrop<'a>,
    }

    /// Notifies a waiter after the preceding guard field unlocks.
    struct NotifyOnDrop<'a>(
        /// The mutex's acquisition event, borrowed for the guard's lifetime.
        &'a Event,
    );

    impl<T> Deref for MutexGuard<'_, T> {
        type Target = T;

        fn deref(&self) -> &T {
            &self.locked
        }
    }

    impl<T> DerefMut for MutexGuard<'_, T> {
        fn deref_mut(&mut self) -> &mut T {
            &mut self.locked
        }
    }

    impl Drop for NotifyOnDrop<'_> {
        fn drop(&mut self) {
            self.0.notify(1);
        }
    }

    /// Demonstrates shared updates followed by asynchronous acquisition.
    pub(super) fn entry() {
        /// Uses several workers so the demonstration can encounter contention.
        const N: usize = 10;

        // A shared counter.
        let counter = Arc::new(Mutex::new(0));

        // A channel that signals when all threads are done.
        let (tx, rx) = mpsc::channel();

        // Spawn a bunch of threads incrementing the counter.
        for _ in 0..N {
            let counter = counter.clone();
            let tx = tx.clone();

            thread::spawn(move || {
                let mut counter = counter.lock();
                *counter += 1;

                // If this is the last increment, signal that we're done.
                if *counter == N {
                    tx.send(()).unwrap();
                }
            });
        }

        // Wait until the last thread increments the counter.
        rx.recv().unwrap();

        // The same mutex also supports async acquisition.
        futures_lite::future::block_on(async {
            assert_eq!(*counter.lock_async().await, N);
        });

        println!("Done!");
    }

    #[cfg(test)]
    mod tests {
        use super::Mutex;
        use std::cell::Cell;
        use std::future::Future;
        use std::panic::{catch_unwind, AssertUnwindSafe};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use std::task::{Context, Poll, Waker};

        #[test]
        fn blocking_wait_retries_notifications_until_data_is_unlocked() {
            if crate::support::run_in_subprocess(
                "example::tests::blocking_wait_retries_notifications_until_data_is_unlocked",
            ) {
                return;
            }

            let mutex = Arc::new(Mutex::new(41));
            let mut held = mutex.lock();
            let worker_mutex = Arc::clone(&mutex);
            let worker = std::thread::spawn(move || {
                let mut acquired = worker_mutex.lock();
                *acquired += 1;
                *acquired
            });
            while mutex.lock_ops.total_listeners() == 0 {
                std::thread::yield_now();
            }
            assert_eq!(mutex.lock_ops.notify(1), 1);
            // A second notification succeeds only after the worker consumes the
            // first wait, retries the held lock and registers another listener.
            while mutex.lock_ops.notify(1) == 0 {
                std::thread::yield_now();
            }
            *held = 42;
            drop(held);
            assert_eq!(worker.join().expect("blocking worker completes"), 43);
            assert_eq!(mutex.lock_ops.total_listeners(), 0);
            assert_eq!(*mutex.lock(), 43);
        }

        #[test]
        fn pending_async_lock_preserves_state_and_releases_its_listener() {
            fn assert_send_sync<T: Send + Sync>() {}
            assert_send_sync::<Mutex<Cell<usize>>>();

            let mutex = Mutex::new(Cell::new(41));
            let held = mutex.lock();
            let mut waiting = Box::pin(mutex.lock_async());
            let mut context = Context::from_waker(Waker::noop());
            assert!(waiting.as_mut().poll(&mut context).is_pending());
            assert_eq!(mutex.lock_ops.total_listeners(), 1);
            held.set(42);
            drop(held);

            let Poll::Ready(acquired) = waiting.as_mut().poll(&mut context) else {
                panic!("the released lock must be acquired without another wake");
            };
            assert_eq!(acquired.get(), 42);
            acquired.set(43);
            drop(acquired);
            drop(waiting);
            assert_eq!(mutex.lock_ops.total_listeners(), 0);
            assert_eq!(mutex.lock().get(), 43);
        }

        #[test]
        fn cancelling_the_notified_waiter_releases_the_next_waiter() {
            let mutex = Mutex::new(3);
            let held = mutex.lock();
            let mut first = Box::pin(mutex.lock_async());
            let mut second = Box::pin(mutex.lock_async());
            let second_wakes = Arc::new(AtomicUsize::new(0));
            let wakes = Arc::clone(&second_wakes);
            let waker = waker_fn::waker_fn(move || {
                wakes.fetch_add(1, Ordering::SeqCst);
            });
            let mut first_context = Context::from_waker(Waker::noop());
            let mut second_context = Context::from_waker(&waker);
            assert!(first.as_mut().poll(&mut first_context).is_pending());
            assert!(second.as_mut().poll(&mut second_context).is_pending());
            assert_eq!(mutex.lock_ops.total_listeners(), 2);

            drop(held);
            assert_eq!(second_wakes.load(Ordering::SeqCst), 0);
            drop(first);
            assert_eq!(second_wakes.load(Ordering::SeqCst), 1);
            let Poll::Ready(mut acquired) = second.as_mut().poll(&mut second_context) else {
                panic!("cancellation must hand the notification to the remaining waiter");
            };
            *acquired += 4;
            drop(acquired);
            drop(second);
            assert_eq!(mutex.lock_ops.total_listeners(), 0);
            assert_eq!(*mutex.lock(), 7);
        }

        #[test]
        fn notification_observes_unlocked_data_even_when_its_waker_panics() {
            for panic_after_reentry in [false, true] {
                let mutex = Arc::new(Mutex::new(10));
                let held = mutex.lock();
                let mut waiting = Box::pin(mutex.lock_async());
                let observed = Arc::new(AtomicUsize::new(0));
                let callback_mutex = Arc::clone(&mutex);
                let callback_observed = Arc::clone(&observed);
                let waker = waker_fn::waker_fn(move || {
                    let mut acquired = callback_mutex
                        .try_lock()
                        .expect("notification must run after unlocking");
                    *acquired += 1;
                    callback_observed.store(*acquired, Ordering::SeqCst);
                    drop(acquired);
                    if panic_after_reentry {
                        panic!("wake callback failed");
                    }
                });
                let mut context = Context::from_waker(&waker);
                assert!(waiting.as_mut().poll(&mut context).is_pending());

                let result = catch_unwind(AssertUnwindSafe(|| drop(held)));
                if panic_after_reentry {
                    assert_eq!(
                        result.unwrap_err().downcast_ref::<&str>(),
                        Some(&"wake callback failed")
                    );
                } else {
                    result.expect("the ordinary wake must not panic");
                }
                assert_eq!(observed.load(Ordering::SeqCst), 11);
                let Poll::Ready(acquired) = waiting.as_mut().poll(&mut context) else {
                    panic!("the mutex must remain usable after the callback");
                };
                assert_eq!(*acquired, 11);
                drop(acquired);
                drop(waiting);
                assert_eq!(mutex.lock_ops.total_listeners(), 0);
                assert_eq!(*mutex.lock(), 11);
            }
        }
    }
}

#[cfg(any(target_family = "wasm", not(feature = "std")))]
/// Keeps the example buildable when blocking listener waits are unavailable.
mod example {
    /// Explains why this feature and platform combination does not run the demo.
    pub(super) fn entry() {
        println!("This example requires std and a non-wasm target.");
    }
}

/// Runs the implementation selected by the crate features and target.
fn main() {
    example::entry();
}
