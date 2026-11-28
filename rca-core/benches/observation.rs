//! Measures observation publication and consumption through a dedicated actor thread.
//!
//! Persistent fixtures exclude actor construction and final shutdown from samples.
//! Commit loops include admission and causal completion. The shutdown row instead
//! includes construction, registration, shutdown, joining and observer destruction.
//! These workloads include thread scheduling, not only selector or slot operations.

use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::thread::JoinHandle;
use std::time::Instant;

use rca_core::{
    Effect, MonotonicInstant, Observation, ReduceContext, Reducer, Runtime, RuntimeDriver,
    RuntimeFuture, Store,
};

fn main() {
    divan::main();
}

/// Produces one distinct integer per committed action.
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

/// Changes one byte while projections copy the complete 4 KiB state.
struct LargeCounter;

impl Reducer for LargeCounter {
    type State = [u8; 4_096];
    type Action = ();

    fn reduce(
        &mut self,
        state: &mut Self::State,
        _action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        state[0] = state[0].wrapping_add(1);
        Effect::none()
    }
}

/// Retains every worker until the fixture shuts down and joins it.
struct BenchRuntime {
    /// Retains worker handles through Store shutdown.
    shared: Arc<RuntimeShared>,
}

/// Tracks worker ownership and one shared monotonic origin.
struct RuntimeShared {
    /// Includes every accepted worker until the fixture joins it.
    running: Mutex<Vec<JoinHandle<()>>>,
    /// Shared epoch used by cloned drivers.
    origin: Instant,
}

/// Runs each task on its own thread for timer-free benchmark workloads.
///
/// Elapsed waits complete and `MAX` stays pending. Finite future waits panic
/// at construction instead of blocking a worker during polling.
#[derive(Clone)]
struct ThreadDriver {
    /// Shares the original fixture's clock and task ownership.
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
            "observation runtime does not support finite future deadlines"
        );
        Box::pin(std::future::pending())
    }

    fn yield_now(&self) -> RuntimeFuture {
        let mut yielded = false;
        Box::pin(futures_util::future::poll_fn(move |cx| {
            if yielded {
                Poll::Ready(())
            } else {
                yielded = true;
                cx.waker().wake_by_ref();
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
    /// Creates a worker owner before actor construction.
    fn new() -> Self {
        Self {
            shared: Arc::new(RuntimeShared {
                running: Mutex::new(Vec::new()),
                origin: Instant::now(),
            }),
        }
    }

    /// Shares the fixture's worker owner with the Store.
    fn runtime(&self) -> Runtime {
        Runtime::new(ThreadDriver {
            shared: Arc::clone(&self.shared),
        })
    }

    /// Joins all recorded workers after Store shutdown.
    fn join_all(&self) {
        loop {
            let handles = std::mem::take(
                &mut *self
                    .shared
                    .running
                    .lock()
                    .expect("runtime handle lock must not be poisoned"),
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

impl Drop for BenchRuntime {
    fn drop(&mut self) {
        self.join_all();
    }
}

/// Keeps the actor alive across samples and shuts it down before joining workers.
struct Fixture<State, R>
where
    State: Send + 'static,
    R: Reducer<State = State, Action = ()>,
{
    /// Owns selector registrations and the mutable projected state.
    store: Store<State, ()>,
    /// Joins actor work after shutdown.
    runtime: BenchRuntime,
    /// Keeps the reducer type in this fixture without storing another reducer.
    _reducer: std::marker::PhantomData<R>,
}

impl<State, R> Fixture<State, R>
where
    State: Send + 'static,
    R: Reducer<State = State, Action = ()> + Send + 'static,
{
    /// Starts and warms the actor outside the persistent sample loop.
    fn new(state: State, reducer: R) -> Self {
        let runtime = BenchRuntime::new();
        let store = Store::new(state, reducer, runtime.runtime());
        futures_executor::block_on(store.read(|_| ())).expect("actor start");
        Self {
            store,
            runtime,
            _reducer: std::marker::PhantomData,
        }
    }

    /// Awaits publication and causal completion for each action.
    fn commit(&self, count: usize) {
        futures_executor::block_on(async {
            for _ in 0..count {
                self.store.send(()).await.expect("commit").finish().await;
            }
        });
    }
}

impl<State, R> Drop for Fixture<State, R>
where
    State: Send + 'static,
    R: Reducer<State = State, Action = ()>,
{
    fn drop(&mut self) {
        futures_executor::block_on(self.store.shutdown()).expect("clean shutdown");
        self.runtime.join_all();
    }
}

#[derive(Default)]
/// Counts selector work and concurrently retained projection values.
struct RetentionStats {
    /// Counts actual selector invocations, including initial selection.
    calls: AtomicUsize,
    /// Includes consumer-owned initial values and actor-owned pending updates.
    live: AtomicUsize,
    /// Includes transient overlap while a new value replaces the previous slot.
    maximum: AtomicUsize,
}

/// Releases one live-value count when its slot or observer drops it.
struct TrackedValue {
    /// Releases exactly one live-value count when this payload is destroyed.
    stats: Arc<RetentionStats>,
}

impl TrackedValue {
    /// Records a real projection without copying the counter into its payload.
    fn new(value: usize, stats: &Arc<RetentionStats>) -> Self {
        stats.calls.fetch_add(1, Ordering::Relaxed);
        let live = stats.live.fetch_add(1, Ordering::AcqRel) + 1;
        stats.maximum.fetch_max(live, Ordering::AcqRel);
        divan::black_box(value);
        Self {
            stats: Arc::clone(stats),
        }
    }
}

impl Drop for TrackedValue {
    fn drop(&mut self) {
        self.stats.live.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Retains every registered observer so publication cannot be optimised away.
fn register_observers(
    store: &Store<usize, ()>,
    count: usize,
    calls: &Arc<AtomicUsize>,
) -> Vec<Observation<usize>> {
    futures_executor::block_on(async {
        let mut observations = Vec::with_capacity(count);
        for _ in 0..count {
            let calls = Arc::clone(calls);
            observations.push(
                store
                    .observe(move |state| {
                        calls.fetch_add(1, Ordering::Relaxed);
                        *state
                    })
                    .await
                    .expect("registration"),
            );
        }
        observations
    })
}

#[divan::bench(args = [0_usize, 1, 8, 64], sample_count = 20)]
fn active_observer_publication(bencher: divan::Bencher, count: usize) {
    let fixture = Fixture::new(0, Counter);
    let calls = Arc::new(AtomicUsize::new(0));
    let observations = register_observers(&fixture.store, count, &calls);
    assert_eq!(calls.load(Ordering::Acquire), count);
    let mut commits = 0;
    bencher.bench_local(|| {
        let before = calls.load(Ordering::Acquire);
        fixture.commit(100);
        assert_eq!(calls.load(Ordering::Acquire) - before, count * 100);
        commits += 100;
        divan::black_box(&observations);
    });
    assert_eq!(
        futures_executor::block_on(fixture.store.read(|state| *state)).unwrap(),
        commits
    );
}

#[divan::bench(sample_count = 20)]
fn large_projection_publication(bencher: divan::Bencher) {
    let fixture = Fixture::new([0; 4_096], LargeCounter);
    let mut observation = futures_executor::block_on(
        fixture
            .store
            .observe(|state| Box::<[u8; 4_096]>::new(*state)),
    )
    .expect("registration");
    let mut expected = 0_u8;
    let mut published = false;
    bencher.bench_local(|| {
        fixture.commit(100);
        expected = expected.wrapping_add(100);
        published = true;
        divan::black_box(&observation);
    });
    assert_eq!(
        *futures_executor::block_on(observation.next()).expect("initial projection"),
        [0; 4_096]
    );
    if published {
        // Divan permits zero samples or zero max-time, which leave no update.
        let latest = futures_executor::block_on(observation.next()).expect("latest projection");
        assert_eq!(latest[0], expected);
        assert!(latest[1..].iter().all(|&byte| byte == 0));
    }
}

#[divan::bench(sample_count = 20)]
fn slow_consumer_slot_replacement(bencher: divan::Bencher) {
    let fixture = Fixture::new(0, Counter);
    let stats = Arc::new(RetentionStats::default());
    let selector_stats = Arc::clone(&stats);
    let observation = futures_executor::block_on(
        fixture
            .store
            .observe(move |state| TrackedValue::new(*state, &selector_stats)),
    )
    .expect("registration");
    bencher.bench_local(|| {
        let before = stats.calls.load(Ordering::Acquire);
        fixture.commit(100);
        assert_eq!(stats.calls.load(Ordering::Acquire) - before, 100);
        // This consumer keeps its initial value as well as the actor's latest slot.
        assert_eq!(stats.live.load(Ordering::Acquire), 2);
        assert_eq!(stats.maximum.load(Ordering::Acquire), 3);
        divan::black_box(&observation);
    });
    drop(observation);
    fixture.commit(1);
    assert_eq!(stats.live.load(Ordering::Acquire), 0);
}

#[divan::bench(sample_count = 20)]
fn observation_handoff(bencher: divan::Bencher) {
    let fixture = Fixture::new(0, Counter);
    let mut observation =
        futures_executor::block_on(fixture.store.observe(|state| *state)).expect("registration");
    assert_eq!(futures_executor::block_on(observation.next()), Some(0));
    let mut expected = 0;
    bencher.bench_local(|| {
        fixture.commit(1);
        expected += 1;
        let value = futures_executor::block_on(observation.next());
        assert_eq!(value, Some(expected));
        divan::black_box(value)
    });
}

#[divan::bench(sample_count = 20)]
fn cancelled_next_resume(bencher: divan::Bencher) {
    let fixture = Fixture::new(0, Counter);
    let mut observation =
        futures_executor::block_on(fixture.store.observe(|state| *state)).expect("registration");
    assert_eq!(futures_executor::block_on(observation.next()), Some(0));
    let mut expected = 0;
    bencher.bench_local(|| {
        let mut pending = Box::pin(observation.next());
        let waker = futures_util::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(matches!(pending.as_mut().poll(&mut cx), Poll::Pending));
        drop(pending);
        fixture.commit(1);
        expected += 1;
        let value = futures_executor::block_on(observation.next());
        assert_eq!(value, Some(expected));
        divan::black_box(value)
    });
}

#[divan::bench(sample_count = 20)]
fn observation_drop_cleanup(bencher: divan::Bencher) {
    let fixture = Fixture::new(0, Counter);
    bencher.bench_local(|| {
        let observation = futures_executor::block_on(fixture.store.observe(|state| *state))
            .expect("registration");
        drop(observation);
        fixture.commit(1);
    });
}

#[divan::bench(args = [1_usize, 8, 64], sample_count = 10)]
fn shutdown_with_observers(bencher: divan::Bencher, count: usize) {
    bencher.bench_local(|| {
        let runtime = BenchRuntime::new();
        let store = Store::new(0, Counter, runtime.runtime());
        let calls = Arc::new(AtomicUsize::new(0));
        let observations = register_observers(&store, count, &calls);
        assert_eq!(calls.load(Ordering::Acquire), count);
        futures_executor::block_on(store.shutdown()).expect("clean shutdown");
        runtime.join_all();
        assert_eq!(calls.load(Ordering::Acquire), count);
        divan::black_box(observations);
    });
}

#[cfg(test)]
mod runtime_contracts {
    //! Checks timer boundaries without starting workers or measuring a workload.

    /// An elapsed deadline completes on its first poll.
    #[test]
    fn elapsed_deadline_is_ready_on_first_poll() {
        use std::sync::Arc;
        use std::task::{Context, Poll, Waker};

        use rca_core::{MonotonicInstant, RuntimeDriver};

        use super::{BenchRuntime, ThreadDriver};

        let runtime = BenchRuntime::new();
        let driver = ThreadDriver {
            shared: Arc::clone(&runtime.shared),
        };
        let mut context = Context::from_waker(Waker::noop());
        let mut elapsed = driver.sleep_until(MonotonicInstant::ZERO);

        assert_eq!(elapsed.as_mut().poll(&mut context), Poll::Ready(()));
    }

    /// The maximum deadline remains pending instead of completing after overflow.
    #[test]
    fn maximum_deadline_stays_pending() {
        use std::sync::Arc;
        use std::task::{Context, Poll, Waker};

        use rca_core::{MonotonicInstant, RuntimeDriver};

        use super::{BenchRuntime, ThreadDriver};

        let runtime = BenchRuntime::new();
        let driver = ThreadDriver {
            shared: Arc::clone(&runtime.shared),
        };
        let mut context = Context::from_waker(Waker::noop());
        let mut distant = driver.sleep_until(MonotonicInstant::MAX);

        assert_eq!(distant.as_mut().poll(&mut context), Poll::Pending);
        assert_eq!(distant.as_mut().poll(&mut context), Poll::Pending);
    }

    /// Finite future waits fail at construction because these workloads need no timers.
    #[test]
    #[should_panic(expected = "observation runtime does not support finite future deadlines")]
    fn finite_future_deadline_is_rejected_before_polling() {
        use std::sync::Arc;
        use std::time::Duration;

        use rca_core::RuntimeDriver;

        use super::{BenchRuntime, ThreadDriver};

        let runtime = BenchRuntime::new();
        let driver = ThreadDriver {
            shared: Arc::clone(&runtime.shared),
        };
        let deadline = driver.now().saturating_add(Duration::from_secs(3_600));

        // Do not poll: the former implementation would block this thread.
        drop(driver.sleep_until(deadline));
    }
}
