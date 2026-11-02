//! Manually polls Shared tasks without advancing time or scheduling background work.
//!
//! The clock stays at zero. Elapsed waits complete. Future waits never wake.
//! Each yield requests another poll before it completes.
//!
//! Tests must shut down each Store, or drop it and call `turn`, before returning.
//! Clones share the task queue; dropping a clone does not cancel another owner's tasks.
//! An assertion panic outside `turn` can bypass that cleanup and leave queued tasks.

use rca_core::{RuntimeDriver, RuntimeFuture};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

#[derive(Clone, Default)]
/// Shares pending tasks between a test and the runtimes it constructs.
pub(super) struct Driver(Arc<Mutex<Vec<RuntimeFuture>>>);
impl std::fmt::Debug for Driver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FoldDriver")
    }
}
impl RuntimeDriver for Driver {
    fn spawn(&self, task: RuntimeFuture) {
        self.0.lock().unwrap().push(task);
    }
    fn now(&self) -> rca_core::MonotonicInstant {
        rca_core::MonotonicInstant::from_elapsed(std::time::Duration::ZERO)
    }
    fn sleep_until(&self, deadline: rca_core::MonotonicInstant) -> RuntimeFuture {
        if deadline <= self.now() {
            Box::pin(std::future::ready(()))
        } else {
            Box::pin(std::future::pending())
        }
    }
    fn yield_now(&self) -> RuntimeFuture {
        let mut yielded = false;
        Box::pin(std::future::poll_fn(move |context| {
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
impl Driver {
    /// Polls each task present at entry once, outside the queue lock.
    ///
    /// Tasks spawned during polling wait until the next turn. A task panic unwinds
    /// and drops the remaining drained tasks; it does not resume their execution.
    pub(super) fn turn(&self) {
        let tasks = std::mem::take(&mut *self.0.lock().unwrap());
        for mut task in tasks {
            if task
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
            {
                self.0.lock().unwrap().push(task);
            }
        }
    }

    /// Polls a future before each of at most 32 actor turns.
    ///
    /// The bound is a fixture progress check, not a time limit.
    /// Future clock deadlines never become due.
    /// The allocation assays include this helper's boxed future in their totals.
    pub(super) fn finish<F: Future>(&self, future: F) -> F::Output {
        let mut future = Box::pin(future);
        for _ in 0..32 {
            if let Poll::Ready(value) = poll(future.as_mut()) {
                return value;
            }
            self.turn();
        }
        panic!("bounded actor progress");
    }
}

/// Polls once without advancing actors or scheduling a later wake.
pub(super) fn poll<F: Future>(future: std::pin::Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}
