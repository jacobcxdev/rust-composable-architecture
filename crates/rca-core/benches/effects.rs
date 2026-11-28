//! End-to-end effect workloads on a thread-per-task test driver.
//!
//! Store startup and final shutdown are outside timing. Admission, effect work,
//! completion and OS-thread scheduling are inside each sample. Finished thread
//! handles stay owned until fixture cleanup. These are not isolated reducer costs.
//! Virtual time moves only when a row explicitly advances it; zero-deadline waits
//! use the elapsed-clock fast path rather than registering with this driver.

use std::collections::HashMap;
use std::fmt;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::thread::JoinHandle;
use std::time::Duration;

use rca_core::{
    Effect, MonotonicInstant, ReduceContext, Reducer, Runtime, RuntimeDriver, RuntimeFuture, Store,
};

fn main() {
    divan::main();
}

/// Owns the task threads and manually advanced clock for one Store fixture.
struct BenchRuntime {
    /// Keeps workers and clock registrations alive until explicit joining.
    shared: Arc<RuntimeShared>,
}

/// Shared task ownership and the clock's registered waits.
struct RuntimeShared {
    /// Includes completed handles until the fixture's final join.
    running: Mutex<Vec<JoinHandle<()>>>,
    /// Virtual nanoseconds; these finite workloads stay below overflow.
    now_nanos: AtomicU64,
    /// Assigns unique registration keys within this fixture.
    next_sleep: AtomicU64,
    /// Serialises registration with the advancement scan.
    sleepers: Mutex<HashMap<u64, Sleeper>>,
}

/// One pending deadline and its most recently supplied task waker.
struct Sleeper {
    /// First virtual instant at which this wait may complete.
    deadline: MonotonicInstant,
    /// Wakes the task that most recently polled this wait.
    waker: Waker,
}

/// Shares the fixture clock and launches each task on a fresh OS thread.
#[derive(Clone)]
struct BenchDriver {
    /// Does not own a separate clock domain when cloned.
    shared: Arc<RuntimeShared>,
}

/// A cancellable registration in the fixture's virtual clock.
struct BenchSleep {
    /// Retains the registration map until this wait is dropped.
    shared: Arc<RuntimeShared>,
    /// Selects only this wait's entry during removal.
    id: u64,
    /// Absolute virtual deadline, independent of polling frequency.
    deadline: MonotonicInstant,
    /// Avoids a removal lock when no entry was ever installed.
    registered: bool,
}

impl RuntimeDriver for BenchDriver {
    fn spawn(&self, task: RuntimeFuture) {
        self.shared
            .running
            .lock()
            .expect("runtime handle lock")
            .push(std::thread::spawn(move || futures_executor::block_on(task)));
    }

    fn now(&self) -> MonotonicInstant {
        MonotonicInstant::from_elapsed(Duration::from_nanos(
            self.shared.now_nanos.load(Ordering::Acquire),
        ))
    }

    fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
        let id = self.shared.next_sleep.fetch_add(1, Ordering::Relaxed);
        Box::pin(BenchSleep {
            shared: Arc::clone(&self.shared),
            id,
            deadline,
            registered: false,
        })
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

impl fmt::Debug for BenchDriver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BenchDriver").finish_non_exhaustive()
    }
}

impl Future for BenchSleep {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.poll_inner(
            cx,
            #[cfg(test)]
            || {},
        )
    }
}

impl BenchSleep {
    /// Rechecks time while holding the registration lock used by advancement.
    ///
    /// The test hook forces the otherwise racy interval before registration.
    fn poll_inner(
        &mut self,
        cx: &mut Context<'_>,
        #[cfg(test)] after_clock_read: impl FnOnce(),
    ) -> Poll<()> {
        let now = MonotonicInstant::from_elapsed(Duration::from_nanos(
            self.shared.now_nanos.load(Ordering::Acquire),
        ));
        if now >= self.deadline {
            self.unregister();
            return Poll::Ready(());
        }

        #[cfg(test)]
        after_clock_read();

        let waker = cx.waker().clone();
        let mut sleepers = self.shared.sleepers.lock().expect("sleeper lock");
        let now = MonotonicInstant::from_elapsed(Duration::from_nanos(
            self.shared.now_nanos.load(Ordering::Acquire),
        ));
        if now >= self.deadline {
            let previous = sleepers.remove(&self.id);
            drop(sleepers);
            self.registered = false;
            drop(previous);
            return Poll::Ready(());
        }

        let previous = sleepers.insert(
            self.id,
            Sleeper {
                deadline: self.deadline,
                waker,
            },
        );
        drop(sleepers);
        self.registered = true;
        drop(previous);
        Poll::Pending
    }

    /// Removes registration before dropping its waker outside the clock lock.
    fn unregister(&mut self) {
        if self.registered {
            let previous = self
                .shared
                .sleepers
                .lock()
                .expect("sleeper lock")
                .remove(&self.id);
            self.registered = false;
            drop(previous);
        }
    }
}

impl Drop for BenchSleep {
    fn drop(&mut self) {
        self.unregister();
    }
}

impl BenchRuntime {
    /// Starts an empty virtual clock before the Store actor is constructed.
    fn new() -> Self {
        Self {
            shared: Arc::new(RuntimeShared {
                running: Mutex::new(Vec::new()),
                now_nanos: AtomicU64::new(0),
                next_sleep: AtomicU64::new(0),
                sleepers: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Creates the runtime facade without creating another clock or worker owner.
    fn runtime(&self) -> Runtime {
        Runtime::new(BenchDriver {
            shared: Arc::clone(&self.shared),
        })
    }

    /// Advances this bounded fixture and wakes registered due tasks outside the lock.
    fn advance_by(&self, nanos: u64) {
        let now = self
            .shared
            .now_nanos
            .fetch_add(nanos, Ordering::AcqRel)
            .saturating_add(nanos);
        let now = MonotonicInstant::from_elapsed(Duration::from_nanos(now));
        let wakers: Vec<_> = self
            .shared
            .sleepers
            .lock()
            .expect("sleeper lock")
            .values()
            .filter(|sleeper| sleeper.deadline <= now)
            .map(|sleeper| sleeper.waker.clone())
            .collect();
        for waker in wakers {
            waker.wake();
        }
    }

    /// Joins every task, including tasks launched while earlier handles are joined.
    fn join_all(&self) {
        loop {
            let handles =
                std::mem::take(&mut *self.shared.running.lock().expect("runtime handle lock"));
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

#[derive(Clone, Copy)]
enum Action {
    Immediate,
    Run,
    Pending(usize),
    Cancel,
    ZeroTimers(usize),
    FarTimer,
    Interval,
    Output,
    Tick,
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
enum Key {
    Work,
}

/// Selects the benchmark's finite, pending and actor-owned timer workloads.
struct EffectsReducer;

impl Reducer for EffectsReducer {
    type State = usize;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            Action::Immediate => Effect::send(Action::Output),
            Action::Run => Effect::run(|sender| async move {
                let _ = sender.send(Action::Output).await;
            }),
            Action::Pending(count) => Effect::merge(
                (0..*count)
                    .map(|_| Effect::run(|_sender| std::future::pending()).cancellable(Key::Work)),
            ),
            Action::Cancel => Effect::cancel(Key::Work),
            Action::ZeroTimers(count) => Effect::merge(
                (0..*count).map(|_| Effect::send_after(Duration::ZERO, Action::Output)),
            ),
            Action::FarTimer => {
                Effect::send_after(Duration::MAX, Action::Output).cancellable(Key::Work)
            }
            Action::Interval => {
                Effect::interval(Duration::from_nanos(1), || Action::Tick).cancellable(Key::Work)
            }
            Action::Output | Action::Tick => {
                *state += 1;
                Effect::none()
            }
        }
    }
}

/// Owns one Store and joins its task threads only after shutdown.
struct Fixture {
    /// Counts effect outputs and interval ticks actually reduced.
    store: Store<usize, Action>,
    /// Controls virtual deadlines and retains every accepted task handle.
    runtime: BenchRuntime,
}

impl Fixture {
    /// Starts the actor outside persistent-row timing.
    fn new() -> Self {
        let runtime = BenchRuntime::new();
        let store = Store::new(0, EffectsReducer, runtime.runtime());
        futures_executor::block_on(store.read(|state| *state)).expect("actor start");
        Self { store, runtime }
    }

    /// Measures admission and complete causal work, not admission alone.
    fn send_and_finish(&self, action: Action) {
        futures_executor::block_on(async {
            self.store
                .send(action)
                .await
                .expect("action commit")
                .finish()
                .await;
        });
    }

    /// Creates a generation cohort, cancels its key and awaits all descendants.
    fn cancel_generations(&self, count: usize) {
        futures_executor::block_on(async {
            let work = self
                .store
                .send(Action::Pending(count))
                .await
                .expect("pending commit");
            self.store
                .send(Action::Cancel)
                .await
                .expect("cancel commit")
                .finish()
                .await;
            work.finish().await;
        });
    }

    /// Removes a saturated actor timer without advancing virtual time.
    fn cancel_far_timer(&self) {
        futures_executor::block_on(async {
            let timer = self
                .store
                .send(Action::FarTimer)
                .await
                .expect("timer commit");
            self.store
                .send(Action::Cancel)
                .await
                .expect("cancel commit")
                .finish()
                .await;
            timer.finish().await;
        });
    }

    /// Advances fixed-rate actor timers and verifies every overdue cadence position.
    fn catch_up(&self, ticks: usize) {
        futures_executor::block_on(async {
            let before = self.store.read(|state| *state).await.expect("before count");
            let interval = self
                .store
                .send(Action::Interval)
                .await
                .expect("interval commit");
            self.runtime
                .advance_by(u64::try_from(ticks).expect("tick count fits u64"));
            loop {
                let current = self.store.read(|state| *state).await.expect("tick count");
                if current - before >= ticks {
                    break;
                }
                std::thread::yield_now();
            }
            self.store
                .send(Action::Cancel)
                .await
                .expect("cancel commit")
                .finish()
                .await;
            interval.finish().await;
            assert_eq!(
                self.store
                    .read(|state| *state)
                    .await
                    .expect("final tick count")
                    - before,
                ticks,
                "catch-up must neither skip nor duplicate ticks"
            );
        });
    }

    /// Reads final semantic work outside the row's timed iterations.
    fn assert_count(&self, expected: usize) {
        let actual = futures_executor::block_on(self.store.read(|state| *state))
            .expect("final effect count");
        assert_eq!(actual, expected);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        futures_executor::block_on(self.store.shutdown()).expect("clean shutdown");
        self.runtime.join_all();
    }
}

#[divan::bench(sample_count = 20)]
fn immediate_send_round_trip(bencher: divan::Bencher) {
    let fixture = Fixture::new();
    let mut count = 0;
    bencher.bench_local(|| {
        fixture.send_and_finish(Action::Immediate);
        count += 1;
    });
    fixture.assert_count(count);
}

#[divan::bench(sample_count = 20)]
fn run_effect_round_trip(bencher: divan::Bencher) {
    let fixture = Fixture::new();
    let mut count = 0;
    bencher.bench_local(|| {
        fixture.send_and_finish(Action::Run);
        count += 1;
    });
    fixture.assert_count(count);
}

#[divan::bench(sample_count = 20)]
fn action_task_cancel(bencher: divan::Bencher) {
    let fixture = Fixture::new();
    bencher.bench_local(|| {
        let task = futures_executor::block_on(fixture.store.send(Action::Pending(1)))
            .expect("pending commit");
        task.cancel();
        futures_executor::block_on(task.finish());
    });
    fixture.assert_count(0);
}

#[divan::bench(args = [1_usize, 16, 256], sample_count = 10)]
fn generation_cancellation(bencher: divan::Bencher, count: usize) {
    let fixture = Fixture::new();
    bencher.bench_local(|| fixture.cancel_generations(count));
    fixture.assert_count(0);
}

#[divan::bench(args = [1_usize, 16, 256], sample_count = 10)]
fn zero_deadline_timers(bencher: divan::Bencher, count: usize) {
    let fixture = Fixture::new();
    let mut outputs = 0;
    bencher.bench_local(|| {
        fixture.send_and_finish(Action::ZeroTimers(count));
        outputs += count;
    });
    fixture.assert_count(outputs);
}

#[divan::bench(sample_count = 20)]
fn far_timer_removal(bencher: divan::Bencher) {
    let fixture = Fixture::new();
    bencher.bench_local(|| fixture.cancel_far_timer());
    fixture.assert_count(0);
}

#[divan::bench(sample_count = 10)]
fn overdue_interval_catch_up(bencher: divan::Bencher) {
    let fixture = Fixture::new();
    bencher.bench_local(|| fixture.catch_up(256));
}

#[cfg(test)]
mod tests {
    #[test]
    fn advance_between_clock_read_and_registration_cannot_lose_wake() {
        use super::*;

        struct WakeCount(std::sync::atomic::AtomicUsize);

        impl std::task::Wake for WakeCount {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        let runtime = BenchRuntime::new();
        let mut sleep = BenchSleep {
            shared: Arc::clone(&runtime.shared),
            id: 0,
            deadline: MonotonicInstant::from_elapsed(Duration::from_nanos(1)),
            registered: false,
        };
        let wakes = Arc::new(WakeCount(std::sync::atomic::AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&wakes));
        let mut context = Context::from_waker(&waker);

        let result = sleep.poll_inner(&mut context, || runtime.advance_by(1));

        assert!(
            result.is_ready() || wakes.0.load(Ordering::Acquire) > 0,
            "a due wait must complete or receive another poll"
        );
    }
}
