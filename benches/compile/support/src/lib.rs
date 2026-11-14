//! Runs effect-free compile fixtures through a real Store and executor.
//!
//! These fixtures measure consumer compilation and binary shape, not runtime
//! latency. The shared executor supports task scheduling and yielding, not timers.
//! Timer calls fail immediately so an expanded fixture cannot hang silently.

use std::fmt;
use std::task::Poll;
use std::time::Instant;

use futures_executor::{ThreadPool, ThreadPoolBuilder};
use rca_core::{MonotonicInstant, Reducer, Runtime, RuntimeDriver, RuntimeFuture, Store};

/// Single-worker executor used by the effect-free compile fixtures.
#[derive(Clone)]
struct FixtureDriver {
    /// Shared task queue and its one worker thread.
    pool: ThreadPool,
    /// One monotonic origin shared by driver clones.
    origin: Instant,
}

impl fmt::Debug for FixtureDriver {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FixtureDriver")
            .finish_non_exhaustive()
    }
}

impl RuntimeDriver for FixtureDriver {
    fn spawn(&self, task: RuntimeFuture) {
        self.pool.spawn_ok(task);
    }

    fn now(&self) -> MonotonicInstant {
        MonotonicInstant::from_elapsed(self.origin.elapsed())
    }

    fn sleep_until(&self, _deadline: MonotonicInstant) -> RuntimeFuture {
        panic!("effect-free compile fixtures do not support timers");
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

/// Creates the limited executor shared by the compile fixtures.
///
/// # Panics
///
/// Panics if the worker cannot start. The driver also panics when asked to create
/// a timer; these fixtures must remain effect-free.
#[must_use]
pub fn runtime() -> Runtime {
    let pool = ThreadPoolBuilder::new()
        .pool_size(1)
        .name_prefix("rca-compile-fixture-")
        .create()
        .expect("compile fixture thread pool must start");
    Runtime::new(FixtureDriver {
        pool,
        origin: Instant::now(),
    })
}

/// Commits one action, checks its final state and shuts down the `Store`.
///
/// The state check makes the fixture's intended result observable during smoke
/// execution. This checks semantics, not a fixed optimiser or code-generation result.
///
/// # Panics
///
/// Panics if executor creation, admission, state verification or shutdown fails.
pub fn exercise<State, Action, R>(
    state: State,
    reducer: R,
    action: Action,
    verify: impl FnOnce(&State) + Send + 'static,
) where
    State: Send + 'static,
    Action: Send + 'static,
    R: Reducer<State = State, Action = Action> + Send + 'static,
{
    let store = Store::new(state, reducer, runtime());
    let task = futures_executor::block_on(store.send(action)).expect("action must commit");
    futures_executor::block_on(task.finish());
    futures_executor::block_on(store.read(verify)).expect("committed state must match");
    futures_executor::block_on(store.shutdown()).expect("fixture Store shuts down");
}

#[test]
#[should_panic(expected = "effect-free compile fixtures do not support timers")]
fn timer_requests_fail_immediately() {
    let _wait = runtime().clock().sleep_until(MonotonicInstant::ZERO);
}
