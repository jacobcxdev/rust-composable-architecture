//! Measures effect-free commits through one actor with competing producer threads.
//!
//! Actor construction and final shutdown are outside the sample. Each sample
//! includes producer thread creation/join, 1,000 commits per producer and a final
//! ordered read. This is an end-to-end contention workload, not reducer-only cost.

use std::fmt;
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::thread::JoinHandle;
use std::time::Instant;

use rca_core::{
    Effect, MonotonicInstant, ReduceContext, Reducer, Runtime, RuntimeDriver, RuntimeFuture, Store,
};

fn main() {
    divan::main();
}

/// Records every committed action without starting effects.
struct Counter;

impl Reducer for Counter {
    type State = usize;
    type Action = ();

    fn reduce(
        &mut self,
        state: &mut Self::State,
        _action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        *state += 1;
        Effect::none()
    }
}

/// Owns and joins the single actor thread used by this effect-free fixture.
struct BenchRuntime {
    /// Retains the actor handle through its final join.
    shared: Arc<RuntimeShared>,
}

/// Keeps task handles and the driver's monotonic origin alive through shutdown.
struct RuntimeShared {
    /// Owns each accepted thread instead of detaching it.
    running: Mutex<Vec<JoinHandle<()>>>,
    /// Provides one clock epoch shared by driver clones.
    origin: Instant,
}

#[derive(Clone)]
/// Runs each accepted task on a dedicated thread, not a production thread pool.
///
/// These workloads need no timer effects. Elapsed waits complete immediately,
/// and the maximum deadline stays pending. Finite future waits panic at construction.
struct ThreadDriver {
    /// Shares task ownership without creating another worker pool.
    shared: Arc<RuntimeShared>,
}

impl RuntimeDriver for ThreadDriver {
    fn spawn(&self, task: RuntimeFuture) {
        let handle = std::thread::spawn(move || futures_executor::block_on(task));
        self.shared
            .running
            .lock()
            .expect("runtime handle lock must not be poisoned")
            .push(handle);
    }

    fn now(&self) -> MonotonicInstant {
        MonotonicInstant::from_elapsed(self.shared.origin.elapsed())
    }

    fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
        if deadline <= self.now() {
            return Box::pin(std::future::ready(()));
        }
        assert_eq!(
            deadline,
            MonotonicInstant::MAX,
            "store actor runtime does not support finite future deadlines"
        );
        Box::pin(std::future::pending())
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

impl fmt::Debug for ThreadDriver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ThreadDriver").finish_non_exhaustive()
    }
}

impl BenchRuntime {
    /// Starts with no worker; Store construction submits the actor.
    fn new() -> Self {
        Self {
            shared: Arc::new(RuntimeShared {
                running: Mutex::new(Vec::new()),
                origin: Instant::now(),
            }),
        }
    }

    /// Shares this fixture's task ownership and clock origin.
    fn runtime(&self) -> Runtime {
        Runtime::new(ThreadDriver {
            shared: Arc::clone(&self.shared),
        })
    }

    /// Joins the effect-free actor after Store shutdown has completed.
    fn join_all(&self) {
        let handles = std::mem::take(
            &mut *self
                .shared
                .running
                .lock()
                .expect("runtime handle lock must not be poisoned"),
        );
        for handle in handles {
            handle.join().expect("actor task must finish cleanly");
        }
    }
}

impl Drop for BenchRuntime {
    fn drop(&mut self) {
        self.join_all();
    }
}

/// Retains the Store until all scoped producers and its actor have stopped.
struct Fixture {
    /// Serialises every producer's increment into one exact final count.
    store: Store<usize, ()>,
    /// Joins the actor after explicit Store shutdown.
    runtime: BenchRuntime,
    /// Number of fresh producer threads created per timed iteration.
    producers: usize,
}

impl Fixture {
    /// Warms actor startup before Divan starts the measured closure.
    fn new(producers: usize) -> Self {
        let runtime = BenchRuntime::new();
        let store = Store::new(0, Counter, runtime.runtime());
        futures_executor::block_on(store.read(|state| *state)).expect("actor must start");
        Self {
            store,
            runtime,
            producers,
        }
    }

    /// Joins every producer after all its causal action tasks finish.
    fn commit_all(&self, commits_per_producer: usize) {
        std::thread::scope(|scope| {
            for _ in 0..self.producers {
                let store = self.store.clone();
                scope.spawn(move || {
                    futures_executor::block_on(async move {
                        for _ in 0..commits_per_producer {
                            store.send(()).await.expect("commit").finish().await;
                        }
                    });
                });
            }
        });
    }

    /// Reads the committed count after all producers have joined.
    fn final_count(&self) -> usize {
        futures_executor::block_on(self.store.read(|state| *state)).expect("final read")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        futures_executor::block_on(self.store.shutdown()).expect("clean shutdown");
        self.runtime.join_all();
    }
}

#[divan::bench(args = [1_usize, 2, 4, 8], sample_count = 20)]
fn effect_free_commits(bencher: divan::Bencher, producers: usize) {
    let fixture = Fixture::new(producers);
    let mut expected = 0;
    bencher.bench_local(|| {
        fixture.commit_all(1_000);
        expected += producers * 1_000;
        let actual = fixture.final_count();
        assert_eq!(actual, expected, "every admitted action must be reduced");
        divan::black_box(actual)
    });
}

#[cfg(test)]
mod clock_tests {
    #[test]
    fn thread_driver_completes_elapsed_deadlines() {
        use std::sync::Arc;
        use std::task::{Context, Waker};

        use rca_core::{MonotonicInstant, RuntimeDriver};

        use super::{BenchRuntime, ThreadDriver};

        let runtime = BenchRuntime::new();
        let driver = ThreadDriver {
            shared: Arc::clone(&runtime.shared),
        };
        let mut context = Context::from_waker(Waker::noop());

        for deadline in [MonotonicInstant::ZERO, driver.now()] {
            let mut wait = driver.sleep_until(deadline);
            assert!(
                wait.as_mut().poll(&mut context).is_ready(),
                "an elapsed deadline must be ready on its first poll"
            );
        }
    }

    #[test]
    fn thread_driver_max_deadline_stays_pending() {
        use std::sync::Arc;
        use std::task::{Context, Waker};

        use rca_core::{MonotonicInstant, RuntimeDriver};

        use super::{BenchRuntime, ThreadDriver};

        let runtime = BenchRuntime::new();
        let driver = ThreadDriver {
            shared: Arc::clone(&runtime.shared),
        };
        let mut context = Context::from_waker(Waker::noop());
        let mut wait = driver.sleep_until(MonotonicInstant::MAX);

        assert!(
            wait.as_mut().poll(&mut context).is_pending(),
            "an unrepresentable future deadline must remain pending"
        );
        assert!(wait.as_mut().poll(&mut context).is_pending());
    }

    #[test]
    #[should_panic(expected = "store actor runtime does not support finite future deadlines")]
    fn thread_driver_rejects_unsupported_finite_future_deadlines() {
        use std::sync::Arc;
        use std::time::Duration;

        use rca_core::RuntimeDriver;

        use super::{BenchRuntime, ThreadDriver};

        let runtime = BenchRuntime::new();
        let driver = ThreadDriver {
            shared: Arc::clone(&runtime.shared),
        };
        let deadline = driver.now().saturating_add(Duration::from_secs(3_600));

        // This fixture must reject the wait before any future can block a poll.
        drop(driver.sleep_until(deadline));
    }
}
