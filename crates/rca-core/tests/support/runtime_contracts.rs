//! Checks the clocks, task disposal and thread joins of the two integration test hosts.

/// The manual host keeps queued tasks idle until the caller starts or drops them.
mod manual {
    use crate::support::{ManualDriver, ManualRuntime, Shared};
    use rca_core::{MonotonicInstant, RuntimeDriver};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Records whether a future's destructor can acquire the queue lock.
    struct QueueDropProbe {
        /// Keeps the queue alive until the disposal check completes.
        shared: Arc<Shared>,
        /// Reports whether destruction occurred outside the queue lock.
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
        use std::task::Context;

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
        use std::task::Context;

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

/// The threaded host must join child tasks that a parent starts during shutdown.
mod threaded {
    use crate::thread_runtime::{ThreadDriver, ThreadRuntime};
    use rca_core::{MonotonicInstant, RuntimeDriver};
    use std::sync::{Arc, mpsc};
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
