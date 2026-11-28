//! Direct driver probes separate scheduling, clock and destruction contracts.
//!
//! Event-based completion waits allow slow hosts. Single-poll checks inspect
//! readiness only; they do not claim general executor fairness or latency bounds.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use rca_core::MonotonicInstant;

use super::{Completion, RuntimeFactory};

/// Rejects same-thread inline polling while allowing concurrent worker progress.
pub(super) async fn spawn_is_not_inline(factory: impl RuntimeFactory) {
    let caller = std::thread::current().id();
    let inside = Arc::new(AtomicBool::new(true));
    let observed_inline = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(Completion::default());
    let future_inside = Arc::clone(&inside);
    let future_observed = Arc::clone(&observed_inline);
    let future_finished = Arc::clone(&finished);

    factory.spawn_probe(Box::pin(async move {
        if std::thread::current().id() == caller && future_inside.load(Ordering::Acquire) {
            future_observed.store(true, Ordering::Release);
        }
        future_finished.complete();
    }));
    inside.store(false, Ordering::Release);
    finished.wait_for(1).await;

    assert!(!observed_inline.load(Ordering::Acquire));
}

/// Checks that factory clones observe the same forward-moving clock origin.
pub(super) async fn clone_clock_is_nondecreasing(factory: impl RuntimeFactory) {
    let clone = factory.clone();
    let before = factory.now_probe();
    factory
        .advance(Duration::from_millis(1))
        .await
        .expect("advance shared clock");
    let after = clone.now_probe();

    assert!(after >= before);
    assert!(after.elapsed().saturating_sub(before.elapsed()) >= Duration::from_millis(1));
}

/// Checks a cross-thread wake queued while the first poll is still in progress.
///
/// Joining the waking thread makes this finite on a deterministic executor too.
pub(super) async fn cross_thread_wake_resumes_task(factory: impl RuntimeFactory) {
    let finished = Arc::new(Completion::default());
    let task_finished = Arc::clone(&finished);
    factory.spawn_probe(Box::pin(async move {
        let mut first_poll = true;
        std::future::poll_fn(move |context| {
            if !first_poll {
                return Poll::Ready(());
            }
            first_poll = false;
            let polling_thread = std::thread::current().id();
            let waker = context.waker().clone();
            std::thread::spawn(move || {
                assert_ne!(std::thread::current().id(), polling_thread);
                waker.wake();
            })
            .join()
            .expect("external waking thread must finish");
            // Joining queues the wake before a deterministic driver can report a stall.
            // Still return Pending: completion requires another driver poll.
            Poll::Pending
        })
        .await;
        task_finished.complete();
    }));
    finished.wait_for(1).await;
}

/// Checks pending and elapsed deadlines without imposing an upper latency bound.
pub(super) async fn sleep_is_not_early(factory: impl RuntimeFactory) {
    let deadline = factory
        .now_probe()
        .saturating_add(Duration::from_millis(10));
    let mut sleep = factory.sleep_probe(deadline);
    let pending = poll_once(sleep.as_mut()).is_pending();
    if !pending {
        assert!(factory.now_probe() >= deadline);
    }
    let finished = Arc::new(Completion::default());
    let task_finished = Arc::clone(&finished);
    let task_clock = factory.clone();
    factory.spawn_probe(Box::pin(async move {
        if pending {
            sleep.await;
        }
        assert!(task_clock.now_probe() >= deadline);
        task_finished.complete();
    }));

    factory
        .advance(Duration::from_millis(10))
        .await
        .expect("advance direct sleep");
    finished.wait_for(1).await;

    assert!(factory.now_probe() >= deadline);
    let mut elapsed_sleep = factory.sleep_probe(deadline);
    assert!(poll_once(elapsed_sleep.as_mut()).is_ready());
}

/// Requires an initial pending poll, not a promise that another task will run.
pub(super) async fn yield_offers_scheduling_turn(factory: impl RuntimeFactory) {
    let mut yielded = factory.yield_probe();

    assert!(poll_once(yielded.as_mut()).is_pending());
    yielded.await;
}

/// Drops unpolled waits and checks that the driver can still complete a yield.
pub(super) async fn dropped_sleep_and_yield_are_safe(factory: impl RuntimeFactory) {
    drop(factory.sleep_probe(MonotonicInstant::MAX));
    drop(factory.yield_probe());
    factory.yield_probe().await;
}

/// Checks that accepted ready work is polled and then releases its payload once.
pub(super) async fn accepted_future_drops_once(factory: impl RuntimeFactory) {
    let drops = Arc::new(Completion::default());
    let polls = Arc::new(AtomicUsize::new(0));
    factory.spawn_probe(Box::pin(ReadyDrop {
        drops: Arc::clone(&drops),
        polls: Arc::clone(&polls),
    }));
    drops.wait_for(1).await;

    assert_eq!(
        polls.load(Ordering::Acquire),
        1,
        "accepted ready future must be polled exactly once"
    );
    assert_eq!(drops.count(), 1);
}

/// Delegates host-specific destruction to the adapter's explicit teardown probe.
pub(super) async fn adapter_teardown(factory: impl RuntimeFactory) {
    factory.teardown_probe().await;
}

/// Samples a future without registering a wake that could drive it later.
fn poll_once(future: Pin<&mut (dyn Future<Output = ()> + Send)>) -> Poll<()> {
    let mut context = Context::from_waker(Waker::noop());
    future.poll(&mut context)
}

/// Finishes on its first poll and reports when the executor releases ownership.
struct ReadyDrop {
    /// Remains observable after the future itself has been destroyed.
    drops: Arc<Completion>,
    /// Distinguishes proper completion from unpolled disposal or post-Ready polling.
    polls: Arc<AtomicUsize>,
}

impl std::future::Future for ReadyDrop {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<()> {
        self.polls.fetch_add(1, Ordering::Release);
        Poll::Ready(())
    }
}

impl Drop for ReadyDrop {
    fn drop(&mut self) {
        self.drops.complete();
    }
}

/// Challenges the conformance oracles and their shared completion primitive.
#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::conformance::DeterministicFactory;

    #[cfg(all(target_os = "macos", target_arch = "aarch64", not(loom)))]
    #[test]
    fn completion_observes_progress_before_cold_listener_registration() {
        use crate::cold_listener_alloc;

        let completion = Arc::new(Completion::default());
        let producer = Arc::clone(&completion);
        let mut context = Context::from_waker(Waker::noop());
        cold_listener_alloc::prepare();
        let worker = std::thread::spawn(move || {
            if !cold_listener_alloc::await_interception() {
                return false;
            }
            producer.complete();
            cold_listener_alloc::produced();
            true
        });
        let first_poll = {
            let mut waiting = std::pin::pin!(completion.wait_for(1));
            cold_listener_alloc::arm();
            let result = waiting.as_mut().poll(&mut context);
            cold_listener_alloc::disarm();
            result
        };
        let worker_completed = worker.join().expect("producer joined");
        cold_listener_alloc::assert_completed();
        assert!(worker_completed);
        assert_eq!(first_poll, Poll::Ready(()));
        assert_eq!(completion.count(), 1);
        assert_eq!(Arc::strong_count(&completion), 1);
    }

    /// Polls on the caller's stack and forwards only work that remains pending.
    #[derive(Clone, Debug)]
    struct InlinePolling(DeterministicFactory);

    impl rca_core::RuntimeDriver for InlinePolling {
        fn spawn(&self, mut task: rca_core::RuntimeFuture) {
            // Deliberately violate only the no-inline-polling spawn contract.
            if poll_once(task.as_mut()).is_pending() {
                self.0.spawn_probe(task);
            }
        }

        fn now(&self) -> MonotonicInstant {
            self.0.now_probe()
        }

        fn sleep_until(&self, deadline: MonotonicInstant) -> rca_core::RuntimeFuture {
            self.0.sleep_probe(deadline)
        }

        fn yield_now(&self) -> rca_core::RuntimeFuture {
            self.0.yield_probe()
        }
    }

    impl RuntimeFactory for InlinePolling {
        fn runtime(&self) -> rca_core::Runtime {
            rca_core::Runtime::new(self.clone())
        }

        fn spawn_probe(&self, future: rca_core::RuntimeFuture) {
            rca_core::RuntimeDriver::spawn(self, future);
        }

        fn now_probe(&self) -> MonotonicInstant {
            rca_core::RuntimeDriver::now(self)
        }

        fn sleep_probe(&self, deadline: MonotonicInstant) -> rca_core::RuntimeFuture {
            rca_core::RuntimeDriver::sleep_until(self, deadline)
        }

        fn yield_probe(&self) -> rca_core::RuntimeFuture {
            rca_core::RuntimeDriver::yield_now(self)
        }

        fn advance(
            &self,
            duration: Duration,
        ) -> crate::conformance::ConformanceFuture<Result<(), crate::conformance::ConformanceError>>
        {
            self.0.advance(duration)
        }

        fn teardown_probe(&self) -> crate::conformance::ConformanceFuture<()> {
            self.0.teardown_probe()
        }
    }

    /// Selects one accepted-future lifecycle violation for a negative oracle check.
    #[derive(Clone, Copy, Debug)]
    enum SpawnFault {
        /// Drops accepted work without a first poll while the host remains alive.
        Discard,
        /// Polls a completed future again on the backing executor, not inline.
        RepollReady,
    }

    /// Deliberately violates one lifecycle rule while retaining the other probes.
    #[derive(Clone, Debug)]
    struct InvalidSpawn {
        /// Keeps clock, yield and host lifetime independent of the spawn violation.
        factory: DeterministicFactory,
        /// Fixes the single invalid behaviour exercised by this instance.
        fault: SpawnFault,
    }

    impl rca_core::RuntimeDriver for InvalidSpawn {
        fn spawn(&self, mut task: rca_core::RuntimeFuture) {
            match self.fault {
                SpawnFault::Discard => drop(task),
                SpawnFault::RepollReady => {
                    self.factory
                        .spawn_probe(Box::pin(std::future::poll_fn(move |context| {
                            if task.as_mut().poll(context).is_pending() {
                                return Poll::Pending;
                            }
                            // ReadyDrop tolerates this forbidden poll so the detector
                            // must reject it, rather than relying on a future's panic.
                            let _ = task.as_mut().poll(context);
                            Poll::Ready(())
                        })));
                }
            }
        }

        fn now(&self) -> MonotonicInstant {
            self.factory.now_probe()
        }

        fn sleep_until(&self, deadline: MonotonicInstant) -> rca_core::RuntimeFuture {
            self.factory.sleep_probe(deadline)
        }

        fn yield_now(&self) -> rca_core::RuntimeFuture {
            self.factory.yield_probe()
        }
    }

    impl RuntimeFactory for InvalidSpawn {
        fn runtime(&self) -> rca_core::Runtime {
            rca_core::Runtime::new(self.clone())
        }

        fn spawn_probe(&self, future: rca_core::RuntimeFuture) {
            rca_core::RuntimeDriver::spawn(self, future);
        }

        fn now_probe(&self) -> MonotonicInstant {
            rca_core::RuntimeDriver::now(self)
        }

        fn sleep_probe(&self, deadline: MonotonicInstant) -> rca_core::RuntimeFuture {
            rca_core::RuntimeDriver::sleep_until(self, deadline)
        }

        fn yield_probe(&self) -> rca_core::RuntimeFuture {
            rca_core::RuntimeDriver::yield_now(self)
        }

        fn advance(
            &self,
            duration: Duration,
        ) -> crate::conformance::ConformanceFuture<Result<(), crate::conformance::ConformanceError>>
        {
            self.factory.advance(duration)
        }

        fn teardown_probe(&self) -> crate::conformance::ConformanceFuture<()> {
            self.factory.teardown_probe()
        }
    }

    #[test]
    fn ready_task_detector_rejects_discarded_unpolled_work() {
        use std::panic::{AssertUnwindSafe, catch_unwind};

        let factory = InvalidSpawn {
            factory: DeterministicFactory::new(),
            fault: SpawnFault::Discard,
        };
        let mut check = Box::pin(accepted_future_drops_once(factory));
        let panic = catch_unwind(AssertUnwindSafe(|| poll_once(check.as_mut())))
            .expect_err("discarding accepted work must fail the conformance detector");
        let message = panic
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| panic.downcast_ref::<String>().map(String::as_str));
        assert!(message.is_some_and(|message| message.contains("polled exactly once")));
    }

    #[test]
    fn ready_task_detector_rejects_polling_completed_work() {
        use std::panic::{AssertUnwindSafe, catch_unwind};

        let factory = InvalidSpawn {
            factory: DeterministicFactory::new(),
            fault: SpawnFault::RepollReady,
        };
        let scheduler = factory.factory.scheduler();
        let mut check = Box::pin(accepted_future_drops_once(factory));
        assert_eq!(poll_once(check.as_mut()), Poll::Pending);
        scheduler
            .run_until_stalled()
            .expect("invalid driver finishes and drops its ready task");
        let panic = catch_unwind(AssertUnwindSafe(|| poll_once(check.as_mut())))
            .expect_err("polling completed work must fail the conformance detector");
        let message = panic
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| panic.downcast_ref::<String>().map(String::as_str));
        assert!(message.is_some_and(|message| message.contains("polled exactly once")));
    }

    #[test]
    fn inline_spawn_detector_rejects_synchronous_driver_polling() {
        use std::panic::{AssertUnwindSafe, catch_unwind};

        let factory = InlinePolling(DeterministicFactory::new());
        let scheduler = factory.0.scheduler();
        let capture = Arc::new(());
        let retained = Arc::clone(&capture);
        let yield_once = factory.yield_probe();
        factory.spawn_probe(Box::pin(async move {
            yield_once.await;
            drop(retained);
        }));
        assert_eq!(Arc::strong_count(&capture), 2, "pending task stays owned");
        scheduler
            .run_until_stalled()
            .expect("forwarded pending task completes");
        assert_eq!(
            Arc::strong_count(&capture),
            1,
            "completed task releases capture"
        );

        let mut check = Box::pin(spawn_is_not_inline(factory));
        let panic = catch_unwind(AssertUnwindSafe(|| poll_once(check.as_mut())))
            .expect_err("inline polling must fail the conformance detector");
        let message = panic
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| panic.downcast_ref::<String>().map(String::as_str));
        assert_eq!(
            message,
            Some("assertion failed: !observed_inline.load(Ordering::Acquire)")
        );
        // The panicked checker is dropped, never polled again.
        drop(check);
        assert_eq!(
            scheduler
                .run_until_stalled()
                .expect("completed inline probe leaves no task")
                .polls(),
            0
        );
    }

    #[test]
    fn inline_spawn_detector_accepts_deferred_driver_polling() {
        let factory = DeterministicFactory::new();
        let scheduler = factory.scheduler();
        let mut check = Box::pin(spawn_is_not_inline(factory));

        assert_eq!(poll_once(check.as_mut()), Poll::Pending);
        scheduler
            .run_until_stalled()
            .expect("deferred spawn probe completes");
        assert_eq!(poll_once(check.as_mut()), Poll::Ready(()));
    }

    #[test]
    fn completion_wait_rearms_and_cancellation_preserves_progress() {
        use std::sync::atomic::AtomicUsize;
        use std::task::Wake;

        /// Counts wakes without scheduling a task, exposing rearming and cancellation.
        #[derive(Default)]
        struct WakeCount(AtomicUsize);

        impl Wake for WakeCount {
            fn wake(self: Arc<Self>) {
                self.wake_by_ref();
            }

            fn wake_by_ref(self: &Arc<Self>) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let completion = Completion::default();
        let notifications = Arc::new(WakeCount::default());
        let waker = Waker::from(Arc::clone(&notifications));
        let mut context = Context::from_waker(&waker);
        {
            let mut ready = std::pin::pin!(completion.wait_for(0));
            assert_eq!(ready.as_mut().poll(&mut context), Poll::Ready(()));
        };
        {
            let mut cancelled = std::pin::pin!(completion.wait_for(2));
            assert!(cancelled.as_mut().poll(&mut context).is_pending());
            assert_eq!(Arc::strong_count(&notifications), 3);
        };
        assert_eq!(
            Arc::strong_count(&notifications),
            2,
            "cancellation released the waker"
        );
        assert_eq!(completion.count(), 0);

        let waiting = completion.wait_for(2);
        let frame_bytes = std::mem::size_of_val(&waiting);
        let mut waiting = std::pin::pin!(waiting);
        assert!(waiting.as_mut().poll(&mut context).is_pending());
        {
            let mut cancelled_after_wake = std::pin::pin!(completion.wait_for(2));
            assert!(
                cancelled_after_wake
                    .as_mut()
                    .poll(&mut context)
                    .is_pending()
            );
            completion.complete();
            assert_eq!(completion.count(), 1);
            assert_eq!(notifications.0.load(Ordering::SeqCst), 2);
        };
        // Drop the notified companion before re-arming the surviving waiter.
        assert!(waiting.as_mut().poll(&mut context).is_pending());
        assert_eq!(
            Arc::strong_count(&notifications),
            3,
            "only the live waiter remains"
        );
        completion.complete();
        assert_eq!(completion.count(), 2);
        assert_eq!(
            notifications.0.load(Ordering::SeqCst),
            3,
            "rearmed waiter was notified"
        );
        assert_eq!(waiting.as_mut().poll(&mut context), Poll::Ready(()));
        assert_eq!(
            Arc::strong_count(&notifications),
            2,
            "completion released the waker"
        );
        {
            let mut ready = std::pin::pin!(completion.wait_for(2));
            assert_eq!(ready.as_mut().poll(&mut context), Poll::Ready(()));
        };
        assert_eq!(completion.count(), 2, "waits never consume completions");
        assert_eq!(notifications.0.load(Ordering::SeqCst), 3);
        eprintln!("completion wait: frame_bytes={frame_bytes}");
    }

    #[test]
    fn external_wake_completes_before_deterministic_stall() {
        let factory = DeterministicFactory::new();
        let scheduler = factory.scheduler();
        let completed = Arc::new(AtomicBool::new(false));
        let task_completed = Arc::clone(&completed);
        scheduler.spawn_task(Box::pin(async move {
            cross_thread_wake_resumes_task(factory).await;
            task_completed.store(true, Ordering::Release);
        }));
        scheduler.run_until_stalled().expect("external wake drive");
        assert!(
            completed.load(Ordering::Acquire),
            "external wake must resume its task"
        );
    }

    #[test]
    fn external_wake_resumes_an_already_stalled_task() {
        let factory = DeterministicFactory::new();
        let scheduler = factory.scheduler();
        let ready = Arc::new(AtomicBool::new(false));
        let completed = Arc::new(AtomicBool::new(false));
        let saved_waker = Arc::new(Mutex::new(None));
        let task_ready = Arc::clone(&ready);
        let task_completed = Arc::clone(&completed);
        let task_waker = Arc::clone(&saved_waker);
        factory.spawn_probe(Box::pin(async move {
            std::future::poll_fn(move |context| {
                if task_ready.load(Ordering::Acquire) {
                    return Poll::Ready(());
                }
                *task_waker.lock().expect("save task waker") = Some(context.waker().clone());
                Poll::Pending
            })
            .await;
            task_completed.store(true, Ordering::Release);
        }));
        scheduler.run_until_stalled().expect("initial task stall");
        assert!(!completed.load(Ordering::Acquire));
        let polling_thread = std::thread::current().id();
        std::thread::spawn(move || {
            assert_ne!(std::thread::current().id(), polling_thread);
            ready.store(true, Ordering::Release);
            let waker = saved_waker
                .lock()
                .expect("take task waker")
                .take()
                .expect("task was polled");
            waker.wake();
        })
        .join()
        .expect("external waking thread must finish");
        scheduler
            .run_until_stalled()
            .expect("externally woken task drive");
        assert!(
            completed.load(Ordering::Acquire),
            "external wake must requeue a stalled task"
        );
    }
}
