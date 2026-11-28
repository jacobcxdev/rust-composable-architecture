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
    pub(super) shared: Arc<ThreadShared>,
}

/// Shares task ownership without holding the handle lock while tasks execute.
pub(super) struct ThreadShared {
    /// Stores each spawned task until the host takes ownership of its join.
    pub(super) running: Mutex<Vec<JoinHandle<()>>>,
    /// Advances finite waits without blocking a thread inside a future poll.
    clock: ImmediateClock,
}

/// Starts tasks for a host and exposes its shared virtual clock.
#[derive(Clone)]
pub(super) struct ThreadDriver {
    /// Retains the host state while accepted tasks can start descendants.
    pub(super) shared: Arc<ThreadShared>,
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
