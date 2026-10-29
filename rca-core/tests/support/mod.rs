pub(crate) mod immediate_clock;

use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::task::Poll;
use std::thread::JoinHandle;

use immediate_clock::ImmediateClock;
use rca_core::{MonotonicInstant, Runtime, RuntimeDriver, RuntimeFuture};

/// Owns queued tasks and threads without automatically starting descendants.
///
/// Finite future waits advance time when first polled, then yield once. Maximum
/// deadlines stay pending. Tests must stop Stores and independent producers
/// before joining the host. Joining does not start tasks still in the queue.
pub struct ManualRuntime {
    /// Keeps driver state alive while retained handles or tasks need it.
    shared: Arc<Shared>,
}

/// Separates accepted, unpolled tasks from tasks already assigned to threads.
struct Shared {
    /// Owns accepted futures until a test starts, captures or drops them.
    queued: Mutex<Vec<RuntimeFuture>>,
    /// Signals actual acceptance, which can occur after an action receipt.
    queued_changed: Condvar,
    /// Owns join handles until the test joins one batch.
    running: Mutex<Vec<JoinHandle<()>>>,
    /// Counts acceptance calls independently of task polling or completion.
    spawn_count: AtomicUsize,
    /// Shares one finite virtual timeline across all driver handles.
    clock: ImmediateClock,
}

/// Accepts work into the host queue without polling it inline.
#[derive(Clone)]
struct ManualDriver {
    /// Shares task ownership and the clock origin with the host.
    shared: Arc<Shared>,
}

impl fmt::Debug for ManualDriver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ManualDriver")
            .field("spawn_count", &self.shared.spawn_count)
            .finish_non_exhaustive()
    }
}

impl RuntimeDriver for ManualDriver {
    fn spawn(&self, task: RuntimeFuture) {
        self.shared.spawn_count.fetch_add(1, Ordering::Relaxed);
        self.shared
            .queued
            .lock()
            .expect("queued task lock must not be poisoned")
            .push(task);
        self.shared.queued_changed.notify_all();
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

impl ManualRuntime {
    /// Creates an empty host with virtual time at zero.
    pub fn new() -> Self {
        Self {
            shared: Arc::new(Shared {
                queued: Mutex::new(Vec::new()),
                queued_changed: Condvar::new(),
                running: Mutex::new(Vec::new()),
                spawn_count: AtomicUsize::new(0),
                clock: ImmediateClock::new(),
            }),
        }
    }

    /// Returns a driver handle with this host's queue and clock.
    pub fn runtime(&self) -> Runtime {
        Runtime::new(ManualDriver {
            shared: Arc::clone(&self.shared),
        })
    }

    /// Counts accepted spawn calls, including tasks not yet polled.
    pub fn spawn_count(&self) -> usize {
        self.shared.spawn_count.load(Ordering::Relaxed)
    }

    /// Waits until at least one accepted task is available.
    ///
    /// This condition does not establish how many other tasks will be accepted.
    #[allow(
        dead_code,
        reason = "support module is compiled per integration test target"
    )]
    pub fn wait_for_queued_task(&self) {
        let mut queued = self.shared.queued.lock().expect("queued task lock");
        while queued.is_empty() {
            queued = self
                .shared
                .queued_changed
                .wait(queued)
                .expect("queued task lock");
        }
    }

    /// Takes the only queued task so a test can control each poll.
    ///
    /// # Panics
    ///
    /// Panics if the queue is poisoned or does not contain exactly one task.
    #[allow(
        dead_code,
        reason = "support module is compiled per integration test target"
    )]
    pub fn take_only_queued_task(&self) -> RuntimeFuture {
        let mut queued = self.shared.queued.lock().expect("queued task lock");
        assert_eq!(queued.len(), 1, "expected exactly one captured actor");
        queued.pop().expect("one queued task")
    }

    /// Takes ownership of the current task batch without starting its descendants.
    ///
    /// The queue lock is released before the caller can poll or drop the tasks.
    /// Tasks accepted later belong to the next batch.
    ///
    /// # Panics
    ///
    /// Panics if the queue lock is poisoned.
    pub fn take_queued_tasks(&self) -> Vec<RuntimeFuture> {
        std::mem::take(&mut *self.shared.queued.lock().expect("queued task lock"))
    }

    /// Starts one queued batch on owned threads, without draining descendants.
    ///
    /// The test owner must not join this host concurrently with this call.
    /// Thread creation runs outside the handle lock, including its error path.
    pub fn start_all(&self) {
        let tasks = self.take_queued_tasks();
        for task in tasks {
            let handle = std::thread::spawn(move || futures_executor::block_on(task));
            self.shared
                .running
                .lock()
                .expect("running task lock must not be poisoned")
                .push(handle);
        }
    }

    /// Drops one queued batch after releasing the queue lock.
    ///
    /// Destructors can re-enter the driver and enqueue new work safely. That
    /// new work belongs to the next batch, not this operation.
    #[allow(
        dead_code,
        reason = "support module is compiled per integration test target"
    )]
    pub fn drop_queued(&self) {
        let tasks = self.take_queued_tasks();
        drop(tasks);
    }

    /// Joins one registered thread batch and returns every panic result.
    ///
    /// The caller must first stop live Stores and independent producers. No
    /// lock is held while a task finishes or its panic result is collected.
    pub fn join_all(&self) -> Vec<std::thread::Result<()>> {
        let handles = std::mem::take(
            &mut *self
                .shared
                .running
                .lock()
                .expect("running task lock must not be poisoned"),
        );
        handles.into_iter().map(JoinHandle::join).collect()
    }
}

impl fmt::Debug for ManualRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ManualRuntime")
            .field("spawn_count", &self.spawn_count())
            .finish_non_exhaustive()
    }
}

impl Drop for ManualRuntime {
    fn drop(&mut self) {
        self.drop_queued();
        drop(self.join_all());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::task::Context;

    /// Records whether a future's destructor can acquire the queue lock.
    struct QueueDropProbe {
        shared: Arc<Shared>,
        unlocked: Arc<AtomicBool>,
    }

    impl Drop for QueueDropProbe {
        fn drop(&mut self) {
            self.unlocked
                .store(self.shared.queued.try_lock().is_ok(), Ordering::Relaxed);
        }
    }

    #[test]
    fn maximum_clock_wait_remains_pending() {
        let host = ManualRuntime::new();
        let driver = ManualDriver {
            shared: Arc::clone(&host.shared),
        };
        let mut wait = driver.sleep_until(MonotonicInstant::MAX);
        let mut context = Context::from_waker(futures_util::task::noop_waker_ref());

        assert!(wait.as_mut().poll(&mut context).is_pending());
        assert!(wait.as_mut().poll(&mut context).is_pending());
        assert_eq!(driver.now(), MonotonicInstant::ZERO);
    }

    #[test]
    fn finite_wait_advances_shared_time_without_blocking() {
        let host = ManualRuntime::new();
        let driver = ManualDriver {
            shared: Arc::clone(&host.shared),
        };
        let clone = driver.clone();
        let deadline = MonotonicInstant::from_elapsed(std::time::Duration::from_secs(60));
        let mut context = Context::from_waker(futures_util::task::noop_waker_ref());

        drop(driver.sleep_until(deadline));
        assert_eq!(clone.now(), MonotonicInstant::ZERO);

        let mut wait = driver.sleep_until(deadline);
        assert!(wait.as_mut().poll(&mut context).is_pending());
        assert_eq!(clone.now(), deadline);
        assert!(wait.as_mut().poll(&mut context).is_ready());
        assert!(
            clone
                .sleep_until(deadline)
                .as_mut()
                .poll(&mut context)
                .is_ready()
        );
    }

    #[test]
    fn queued_future_drop_releases_the_queue_lock() {
        for drop_host in [false, true] {
            let host = ManualRuntime::new();
            let unlocked = Arc::new(AtomicBool::new(false));
            let probe = QueueDropProbe {
                shared: Arc::clone(&host.shared),
                unlocked: Arc::clone(&unlocked),
            };
            ManualDriver {
                shared: Arc::clone(&host.shared),
            }
            .spawn(Box::pin(async move { drop(probe) }));

            if drop_host {
                drop(host);
            } else {
                host.drop_queued();
            }

            assert!(unlocked.load(Ordering::Relaxed), "drop_host={drop_host}");
        }
    }
}
