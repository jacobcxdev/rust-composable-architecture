//! Runs shared black-box contracts against RCA runtime adapters.
//!
//! [`run_all`] checks runtime and Store behaviour through a [`RuntimeFactory`].
//! [`run_test_store`] checks the deterministic assertion tools separately.
//! Each scenario creates and shuts down its own Stores.
//!
//! The caller drives the suite and keeps the runtime host alive. A broken adapter
//! can leave an awaited operation pending. The test runner must impose an
//! independent timeout. A timeout on the tested runtime cannot detect its own stall.
//! Some scenarios catch expected panics, so the suite requires panic unwinding.
//! Unexpected panics fail the suite. The suite does not promise recovery.
//!
//! # Examples
//!
//! ```
//! use rca_test::conformance::{DeterministicFactory, run_all};
//!
//! let factory = DeterministicFactory::new();
//! let scheduler = factory.scheduler();
//! futures_executor::block_on(scheduler.run(run_all(factory))).unwrap();
//! ```

/// Checks action admission, ordering and bounded mailboxes.
mod admission;
/// Checks composed child lifetimes through a separate suite entry point.
mod composition;
/// Checks effect output, cancellation and dependency capture.
mod effects;
/// Checks keyed-child lifetime and action routing.
mod keyed;
/// Checks shutdown, owned state recovery and structural cancellation.
mod lifecycle;
/// Checks observation delivery, cancellation and selector ownership.
mod observation;
/// Supplies isolated process fixtures for terminal panic contracts.
mod panic;
/// Checks direct runtime-driver operations and host teardown.
mod runtime;
/// Checks scoped handle access, invalidation and recovery.
mod scoped_store;
/// Checks deterministic TestStore assertions and drive budgets.
mod test_store;
/// Checks actor timers and the action tasks that own them.
mod time;

use std::error::Error;
use std::fmt;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use event_listener::Event;
use rca_core::{
    Effect, MonotonicInstant, ReduceContext, Reducer, Runtime, RuntimeDriver, RuntimeFuture, Store,
};

use crate::DeterministicRuntime;

#[doc(hidden)]
pub use composition::run_composition;
#[doc(hidden)]
pub use panic::{PanicScenario, assert_panic_output, run_panic_fixture};
#[doc(inline)]
pub use test_store::run_test_store;

/// Boxed future used by runtime conformance factories.
pub type ConformanceFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// Counts cross-task events without relying on scheduling delays.
#[derive(Debug, Default)]
struct Completion {
    /// Published count with release/acquire ordering for preceding writes.
    count: AtomicUsize,
    /// Wakes waiters after the count changes.
    changed: Event,
}

impl Completion {
    /// Publishes one event before waking all current waiters.
    fn complete(&self) {
        self.count.fetch_add(1, Ordering::Release);
        self.changed.notify(usize::MAX);
    }

    /// Reads the count and the writes published before each completed event.
    fn count(&self) -> usize {
        self.count.load(Ordering::Acquire)
    }

    /// Awaits the requested count and rejects an observed excess.
    ///
    /// The second count check closes the listener-registration race. The enclosing
    /// suite drive supplies the budget or timeout if an expected event never arrives.
    async fn wait_for(&self, expected: usize) {
        while self.count() < expected {
            event_listener::listener!(self.changed => listener);
            if self.count() >= expected {
                break;
            }
            listener.await;
        }
        assert_eq!(self.count(), expected);
    }
}

/// Reports adapter-specific conformance setup failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConformanceError(
    /// Adapter-provided failure text retained for the test assertion.
    String,
);

impl ConformanceError {
    /// Creates an error from an adapter diagnostic.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for ConformanceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for ConformanceError {}

/// Supplies one runtime adapter to shared black-box scenarios.
///
/// Clones refer to the same live host and monotonic clock origin. Probe methods
/// call that adapter directly, independently of Store operations. Time can advance
/// between any two calls. A scheduling yield has no maximum elapsed duration.
pub trait RuntimeFactory: Clone + Send + Sync + 'static {
    /// Returns a new erased handle to the same runtime driver.
    fn runtime(&self) -> Runtime;

    /// Schedules a direct driver probe.
    fn spawn_probe(&self, future: RuntimeFuture);

    /// Returns direct driver monotonic time.
    fn now_probe(&self) -> MonotonicInstant;

    /// Creates a direct driver sleep.
    fn sleep_probe(&self, deadline: MonotonicInstant) -> RuntimeFuture;

    /// Creates a direct driver yield.
    fn yield_probe(&self) -> RuntimeFuture;

    /// Advances enough adapter time for `duration` to elapse.
    ///
    /// A virtual host advances its clock. A real-time host waits. Completion does
    /// not imply that every task or resulting Store action has finished.
    ///
    /// # Errors
    ///
    /// Returns the adapter's diagnostic if the requested drive cannot complete.
    fn advance(&self, duration: Duration) -> ConformanceFuture<Result<(), ConformanceError>>;

    /// Verifies adapter-owned teardown independently from the suite runtime.
    ///
    /// Creates a separate host and checks that dropping it releases pending work.
    /// It must not shut down the host that drives the enclosing suite.
    fn teardown_probe(&self) -> ConformanceFuture<()>;
}

/// Deterministic runtime factory used by the shared suite.
#[derive(Clone, Debug)]
pub struct DeterministicFactory {
    /// Shared task queue and virtual clock for this factory's suite.
    scheduler: DeterministicRuntime,
}

impl DeterministicFactory {
    /// Creates an isolated deterministic factory.
    #[must_use]
    pub fn new() -> Self {
        Self {
            scheduler: DeterministicRuntime::new(),
        }
    }

    /// Returns scheduler used to drive the suite future.
    #[must_use]
    pub fn scheduler(&self) -> DeterministicRuntime {
        self.scheduler.clone()
    }
}

impl Default for DeterministicFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl RuntimeFactory for DeterministicFactory {
    fn runtime(&self) -> Runtime {
        self.scheduler.runtime()
    }

    fn spawn_probe(&self, future: RuntimeFuture) {
        self.scheduler.spawn_task(future);
    }

    fn now_probe(&self) -> MonotonicInstant {
        self.scheduler.now()
    }

    fn sleep_probe(&self, deadline: MonotonicInstant) -> RuntimeFuture {
        self.scheduler.sleep_until(deadline)
    }

    fn yield_probe(&self) -> RuntimeFuture {
        RuntimeDriver::yield_now(&self.scheduler.driver)
    }

    fn advance(&self, duration: Duration) -> ConformanceFuture<Result<(), ConformanceError>> {
        let scheduler = self.scheduler.clone();
        Box::pin(async move {
            scheduler
                .advance(duration)
                .map(|_| ())
                .map_err(|error| ConformanceError::new(error.to_string()))
        })
    }

    fn teardown_probe(&self) -> ConformanceFuture<()> {
        Box::pin(async move {
            let scheduler = DeterministicRuntime::new();
            let drops = Arc::new(AtomicUsize::new(0));
            let store = Store::new((), TeardownReducer, scheduler.runtime());
            let mut closed = Box::pin(store.closed());
            let receipt = store
                .try_send(())
                .expect("live teardown Store accepts work");
            let task = scheduler
                .run(async { receipt.await })
                .await
                .expect("teardown action drive")
                .expect("teardown action commits");
            scheduler
                .run(task.finish())
                .await
                .expect("teardown action completes");
            assert!(task.is_finished());
            assert!(!store.is_closed());
            assert!(poll_once(closed.as_mut()).is_pending());
            scheduler.spawn_task(Box::pin(CyclicPendingDrop {
                _runtime: scheduler.runtime(),
                drops: Arc::clone(&drops),
            }));
            scheduler
                .run_until_stalled()
                .expect("teardown probe must stall");
            drop(scheduler);
            assert_eq!(drops.load(Ordering::Acquire), 1);
            assert!(store.is_closed());
            assert!(poll_once(closed.as_mut()).is_ready());
        })
    }
}

/// Runs runtime and Store contracts for one adapter.
///
/// Composition and `TestStore` scenarios have separate entry points. The caller
/// drives this future and supplies an independent timeout, as described in the
/// [module documentation](self).
///
/// # Panics
///
/// Panics when an adapter or Store violates an asserted contract, or setup fails.
pub async fn run_all(factory: impl RuntimeFactory) {
    admission::sequential_fifo(factory.clone()).await;
    admission::concurrent_acceptance_order(factory.clone()).await;
    admission::non_interleaving_reducer(factory.clone()).await;
    admission::bounded_backpressure(factory.clone()).await;
    admission::full_mailbox_try_send(factory.clone()).await;
    admission::receipt_cancellation(factory.clone()).await;
    admission::cancellation_before_and_after_admission(factory.clone()).await;
    admission::linearizable_reads(factory.clone()).await;
    admission::cancelled_reads(factory.clone()).await;
    admission::inference_and_public_bounds(factory.clone()).await;

    effects::parallel_effect_return(factory.clone()).await;
    effects::typed_cancellation(factory.clone()).await;
    effects::in_flight_replacement(factory.clone()).await;
    effects::stale_output_rejection(factory.clone()).await;
    effects::unrelated_lineages(factory.clone()).await;
    effects::source_order_and_dependency_capture(factory.clone()).await;
    effects::parent_and_descendant_cancellation(factory.clone()).await;
    effects::delayed_replacement_dependency_capture(factory.clone()).await;

    time::equal_deadline_order(factory.clone()).await;
    time::interval_progress(factory.clone()).await;
    time::timer_removal(factory.clone()).await;
    time::action_task_completion(factory.clone()).await;
    time::zero_period_panics();

    observation::initial_and_ordered(factory.clone()).await;
    observation::slow_consumer_coalescing(factory.clone()).await;
    observation::cancelled_take_resumes(factory.clone()).await;
    observation::dropped_observation_closes(factory.clone()).await;
    observation::clone_free_notification_take_race(factory.clone()).await;
    observation::cancelled_observation_construction(factory.clone()).await;

    lifecycle::graceful_shutdown(factory.clone()).await;
    lifecycle::pending_effect_shutdown(factory.clone()).await;
    lifecycle::owned_state_extraction(factory.clone()).await;
    lifecycle::final_owner_drop(factory.clone()).await;
    lifecycle::store_isolation(factory.clone()).await;
    lifecycle::non_owning_resources_close(factory.clone()).await;
    lifecycle::optional_scope_total_orders(factory.clone()).await;

    scoped_store::run(factory.clone()).await;

    keyed::run(factory.clone()).await;

    runtime::spawn_is_not_inline(factory.clone()).await;
    runtime::cross_thread_wake_resumes_task(factory.clone()).await;
    runtime::clone_clock_is_nondecreasing(factory.clone()).await;
    runtime::sleep_is_not_early(factory.clone()).await;
    runtime::yield_offers_scheduling_turn(factory.clone()).await;
    runtime::dropped_sleep_and_yield_are_safe(factory.clone()).await;
    runtime::accepted_future_drops_once(factory.clone()).await;
    runtime::adapter_teardown(factory).await;
}

/// Retains a runtime handle inside work that only host teardown can finish.
struct CyclicPendingDrop {
    /// Keeps the driver reachable without keeping its owning host alive.
    _runtime: Runtime,
    /// Records that the host discarded this pending future exactly once.
    drops: Arc<AtomicUsize>,
}

impl std::future::Future for CyclicPendingDrop {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _context: &mut std::task::Context<'_>) -> std::task::Poll<()> {
        std::task::Poll::Pending
    }
}

impl Drop for CyclicPendingDrop {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::AcqRel);
    }
}

/// Accepts an action without effects before the isolated host teardown.
struct TeardownReducer;

impl Reducer for TeardownReducer {
    type State = ();
    type Action = ();

    fn reduce(
        &mut self,
        _state: &mut Self::State,
        _action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        Effect::none()
    }
}

/// Inspects readiness once without scheduling further polls.
fn poll_once<F: Future>(mut future: Pin<&mut F>) -> Poll<F::Output> {
    let mut context = Context::from_waker(Waker::noop());
    future.as_mut().poll(&mut context)
}
