//! Runs test tasks on owned threads with an immediate virtual clock.
//!
//! Finite future waits advance shared virtual time on their first poll, then yield.
//! Elapsed waits complete on their first poll. Maximum deadlines stay pending
//! without advancing time. Constructing or dropping an unpolled wait has no effect.
//! Tests must stop Stores and independent producers before joining this host.

#[path = "immediate_clock.rs"]
mod immediate_clock;

use immediate_clock::ImmediateClock;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::thread::JoinHandle;

use rca_core::{MonotonicInstant, Runtime, RuntimeDriver, RuntimeFuture};

/// Owns threads for tests that control progress through explicit signals.
pub struct ThreadRuntime {
    /// Keeps task handles and shared virtual time alive for all driver handles.
    shared: Arc<ThreadShared>,
}

/// Shares task ownership without holding the handle lock while tasks execute.
struct ThreadShared {
    /// Stores each spawned task until the host takes ownership of its join.
    running: Mutex<Vec<JoinHandle<()>>>,
    /// Advances finite waits without blocking a thread inside a future poll.
    clock: ImmediateClock,
}

/// Starts tasks for a host and exposes its shared virtual clock.
#[derive(Clone)]
struct ThreadDriver {
    /// Retains the host state while accepted tasks can start descendants.
    shared: Arc<ThreadShared>,
}

impl fmt::Debug for ThreadDriver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ThreadDriver").finish_non_exhaustive()
    }
}

impl RuntimeDriver for ThreadDriver {
    fn spawn(&self, task: RuntimeFuture) {
        let handle = std::thread::spawn(move || futures_executor::block_on(task));
        self.shared
            .running
            .lock()
            .expect("thread handle lock must not be poisoned")
            .push(handle);
    }

    fn now(&self) -> MonotonicInstant {
        self.shared.clock.now()
    }

    fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
        self.shared.clock.sleep_until(deadline)
    }

    fn yield_now(&self) -> RuntimeFuture {
        let mut yielded = false;
        Box::pin(futures_util::future::poll_fn(move |context| {
            if yielded {
                Poll::Ready(())
            } else {
                yielded = true;
                context.waker().wake_by_ref();
                Poll::Pending
            }
        }))
    }
}

impl ThreadRuntime {
    /// Creates a host with no tasks and virtual time at zero.
    pub fn new() -> Self {
        Self {
            shared: Arc::new(ThreadShared {
                running: Mutex::new(Vec::new()),
                clock: ImmediateClock::new(),
            }),
        }
    }

    /// Returns a driver handle that shares this host's task ownership.
    pub fn runtime(&self) -> Runtime {
        Runtime::new(ThreadDriver {
            shared: Arc::clone(&self.shared),
        })
    }

    /// Joins registered tasks and the descendants they register before completion.
    ///
    /// Independent producers must stop before this call. The final empty batch
    /// does not prevent a retained driver handle from starting a later task.
    ///
    /// # Panics
    ///
    /// Panics if the handle lock is poisoned or a joined task panics. A task
    /// panic stops the join immediately, so remaining tasks are not all joined.
    pub fn join_all(&self) {
        loop {
            let handles = std::mem::take(
                &mut *self
                    .shared
                    .running
                    .lock()
                    .expect("thread handle lock must not be poisoned"),
            );
            if handles.is_empty() {
                return;
            }
            for handle in handles {
                handle.join().expect("runtime task must finish cleanly");
            }
        }
    }
}

impl Drop for ThreadRuntime {
    fn drop(&mut self) {
        self.join_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::task::{Context, Waker};
    use std::time::{Duration, Instant};

    #[test]
    fn maximum_deadline_does_not_complete_early() {
        let runtime = ThreadRuntime::new();
        let driver = ThreadDriver {
            shared: Arc::clone(&runtime.shared),
        };
        let mut wait = driver.sleep_until(MonotonicInstant::MAX);
        let mut context = Context::from_waker(Waker::noop());

        assert!(wait.as_mut().poll(&mut context).is_pending());
        assert!(wait.as_mut().poll(&mut context).is_pending());
        assert_eq!(driver.now(), MonotonicInstant::ZERO);
    }

    #[test]
    fn elapsed_deadline_completes_on_first_poll() {
        let runtime = ThreadRuntime::new();
        let driver = ThreadDriver {
            shared: Arc::clone(&runtime.shared),
        };
        let mut wait = driver.sleep_until(MonotonicInstant::ZERO);
        let mut context = Context::from_waker(Waker::noop());

        assert!(wait.as_mut().poll(&mut context).is_ready());
    }

    #[test]
    fn finite_wait_advances_shared_time_then_yields() {
        let runtime = ThreadRuntime::new();
        let driver = ThreadDriver {
            shared: Arc::clone(&runtime.shared),
        };
        let cloned = driver.clone();
        let deadline = MonotonicInstant::from_elapsed(Duration::from_secs(10));
        let mut wait = driver.sleep_until(deadline);
        let mut context = Context::from_waker(Waker::noop());

        assert_eq!(driver.now(), MonotonicInstant::ZERO);
        assert!(wait.as_mut().poll(&mut context).is_pending());
        assert_eq!(driver.now(), deadline);
        assert_eq!(cloned.now(), deadline);
        assert!(wait.as_mut().poll(&mut context).is_ready());
    }

    #[test]
    fn cancelling_an_unpolled_wait_does_not_advance_time() {
        let runtime = ThreadRuntime::new();
        let driver = ThreadDriver {
            shared: Arc::clone(&runtime.shared),
        };
        let deadline = MonotonicInstant::from_elapsed(Duration::from_secs(10));

        drop(driver.sleep_until(deadline));
        assert_eq!(driver.now(), MonotonicInstant::ZERO);
    }

    #[test]
    fn join_all_joins_tasks_spawned_while_joining_a_parent() {
        let runtime = ThreadRuntime::new();
        let parent_driver = ThreadDriver {
            shared: Arc::clone(&runtime.shared),
        };
        let child_driver = parent_driver.clone();
        let (release_parent, parent_gate) = mpsc::channel();

        parent_driver.spawn(Box::pin(async move {
            parent_gate.recv().unwrap();
            child_driver.spawn(Box::pin(async {}));
        }));

        let (drained_parent, remaining) = std::thread::scope(|scope| {
            let joined = scope.spawn(|| {
                runtime.join_all();
                runtime.shared.running.lock().unwrap().len()
            });

            // The empty list proves that join_all owns the blocked parent handle.
            // This deadline only bounds a broken test, not the task ordering.
            let deadline = Instant::now() + Duration::from_secs(5);
            let drained_parent = loop {
                if runtime.shared.running.lock().unwrap().is_empty() {
                    break true;
                }
                if Instant::now() >= deadline {
                    break false;
                }
                std::thread::yield_now();
            };

            release_parent.send(()).unwrap();
            (drained_parent, joined.join())
        });

        // Join the child before reporting the failure from the original implementation.
        runtime.join_all();
        assert!(drained_parent, "join_all did not take the parent handle");
        assert_eq!(remaining.unwrap(), 0, "join_all left a child handle behind");
    }
}
