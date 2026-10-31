//! Benchmarks deterministic host operations and complete test workflows.
//!
//! The isolated scheduler cases use `with_inputs` and return their fixtures, so
//! setup and fixture destruction stay outside Divan's measured interval. Other
//! cases include their explicit construction, work and closure-local cleanup.
//! `skip_ext_time` only changes the sample time budget, not those boundaries.
//! These local timing samples are diagnostic, not load-independent release proof.

use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use rca_core::testing::AssertionGate;
use rca_core::{Effect, MonotonicInstant, ReduceContext, Reducer, Store};
use rca_test::{DEFAULT_STEP_BUDGET, DeterministicRuntime, TestStore};

fn main() {
    divan::main();
}

#[divan::bench(sample_count = 20, max_time = 0.5)]
/// Measures construction; the returned host is dropped after the sample.
fn scheduler_host_construction(bencher: divan::Bencher) {
    bencher.bench_local(|| divan::black_box(DeterministicRuntime::new()));
}

#[divan::bench(args = [1_usize, 16, 256], sample_count = 20, max_time = 0.5, skip_ext_time = true)]
/// Measures admission of preallocated tasks, without polling or teardown.
fn scheduler_enqueue_only(bencher: divan::Bencher, count: usize) {
    bencher
        .with_inputs(|| {
            let host = DeterministicRuntime::new();
            let tasks: Vec<rca_core::RuntimeFuture> = (0..count)
                .map(|_| Box::pin(async {}) as rca_core::RuntimeFuture)
                .collect();
            (host, tasks)
        })
        .bench_local_values(|(host, mut tasks)| {
            for task in tasks.drain(..) {
                host.spawn_task(task);
            }
            (host, tasks)
        });
}

#[divan::bench(sample_count = 20, max_time = 0.5, skip_ext_time = true)]
/// Measures 256 task polls after registration and initial wake outside timing.
fn scheduler_steady_polls(bencher: divan::Bencher) {
    bencher
        .with_inputs(|| {
            let (host, wake) = scheduler_paused_probe();
            wake.wake();
            host
        })
        .bench_local_values(|host| {
            assert_eq!(host.run_until_stalled().expect("256 polls").polls(), 256);
            host
        });
}

#[divan::bench(sample_count = 20, max_time = 0.5, skip_ext_time = true)]
/// Measures one wake of a registered task that is not already ready.
fn scheduler_ready_notification(bencher: divan::Bencher) {
    bencher
        .with_inputs(scheduler_paused_probe)
        .bench_local_values(|(host, wake)| {
            wake.wake_by_ref();
            (host, wake)
        });
}

#[divan::bench(sample_count = 20, max_time = 0.5, skip_ext_time = true)]
/// Measures 256 duplicate wakes after the first wake outside timing.
fn scheduler_ready_deduplication(bencher: divan::Bencher) {
    bencher
        .with_inputs(|| {
            let pair = scheduler_paused_probe();
            pair.1.wake_by_ref();
            pair
        })
        .bench_local_values(|(host, wake)| {
            for _ in 0..256 {
                wake.wake_by_ref();
            }
            (host, wake)
        });
}

/// Pauses once, then completes in 256 polls after an explicit external wake.
fn scheduler_paused_probe() -> (DeterministicRuntime, Waker) {
    let host = DeterministicRuntime::new();
    let recorded = Arc::new(Mutex::new(None));
    let output = Arc::clone(&recorded);
    let mut first = true;
    let mut remaining = 256;
    host.spawn_task(Box::pin(std::future::poll_fn(move |cx| {
        if first {
            first = false;
            *output.lock().expect("record task wake") = Some(cx.waker().clone());
            return Poll::Pending;
        }
        remaining -= 1;
        if remaining == 0 {
            Poll::Ready(())
        } else {
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    })));
    assert_eq!(host.run_until_stalled().expect("first poll").polls(), 1);
    let wake = recorded.lock().expect("take task wake").take().unwrap();
    (host, wake)
}

#[divan::bench(args = [1_usize, 16, 256], sample_count = 20)]
/// Measures host construction, ready-task admission, polling and cleanup.
fn ready_tasks(bencher: divan::Bencher, count: usize) {
    bencher.bench_local(|| {
        let runtime = DeterministicRuntime::new();
        for _ in 0..count {
            runtime.spawn_task(Box::pin(async {}));
        }
        let polls = runtime
            .run_until_stalled()
            .expect("finite ready tasks")
            .polls();
        assert_eq!(polls, count);
        divan::black_box(polls)
    });
}

#[divan::bench(args = [1_usize, 16, 256], sample_count = 20)]
/// Measures a complete host lifecycle with exactly the requested task polls.
fn self_wake_accounting(bencher: divan::Bencher, polls: usize) {
    bencher.bench_local(|| {
        let runtime = DeterministicRuntime::new();
        runtime.spawn_task(Box::pin(FiniteSelfWake { remaining: polls }));
        let actual = runtime
            .run_until_stalled()
            .expect("finite self wake")
            .polls();
        assert_eq!(actual, polls);
        divan::black_box(actual)
    });
}

#[divan::bench(args = [1_usize, 1_000, 100_000], sample_count = 10)]
/// Measures registration, one deadline cohort, completion and host cleanup.
fn virtual_timers(bencher: divan::Bencher, count: usize) {
    bencher.bench_local(|| {
        // Each drive needs count polls. This keeps both finite phases below the
        // budget, including the 100,000-wait workload; it is not a latency limit.
        let runtime = DeterministicRuntime::with_step_budget(count.saturating_mul(3).max(16));
        let deadline = MonotonicInstant::from_elapsed(Duration::from_nanos(1));
        for _ in 0..count {
            let sleep = runtime.sleep_until(deadline);
            runtime.spawn_task(sleep);
        }
        let registration = runtime
            .run_until_stalled()
            .expect("timer registration within budget");
        assert_eq!(registration.polls(), count);
        let completion = runtime
            .advance(Duration::from_nanos(1))
            .expect("timer wake within budget");
        assert_eq!(completion.polls(), count);
        assert_eq!(completion.deadline_batches(), 1);
        divan::black_box(completion.polls())
    });
}

#[divan::bench(sample_count = 20)]
/// Measures one gated effect, checked final state and explicit Store shutdown.
fn assertion_gate_pause_release(bencher: divan::Bencher) {
    bencher.bench_local(|| {
        let runtime = DeterministicRuntime::new();
        let gate = AssertionGate::new();
        let store = Store::builder(0, Feature, runtime.runtime())
            .assertion_gate(gate.clone())
            .build()
            .expect("valid Store");
        let task = futures_executor::block_on(runtime.run(store.send(Action::Start)))
            .expect("finite send drive")
            .expect("start commit");
        runtime.run_until_stalled().expect("gate pause");
        assert!(gate.release_pending());
        runtime.run_until_stalled().expect("gate release");
        assert!(task.is_finished());
        let count = futures_executor::block_on(runtime.run(store.read(|state| *state)))
            .expect("finite state read drive")
            .expect("response state remains readable");
        assert_eq!(count, 1);
        futures_executor::block_on(runtime.run(store.shutdown()))
            .expect("finite shutdown drive")
            .expect("clean shutdown");
    });
}

#[divan::bench(sample_count = 20)]
/// Measures one send, asserted effect response and exhaustive finish.
fn test_store_send_receive(bencher: divan::Bencher) {
    bencher.bench_local(|| {
        let mut store = TestStore::new(0, Feature);
        futures_executor::block_on(async {
            let _start = store.send(Action::Start).await.expect("start commit");
            let _response = store
                .receive_with(
                    |action| matches!(action, Action::Response),
                    |state| *state = 1,
                )
                .await
                .expect("response commit");
            store.finish().await.expect("clean finish");
        });
    });
}

#[divan::bench(sample_count = 10)]
/// Measures exact default-budget exhaustion, diagnostic creation and cleanup.
fn default_budget_exhaustion(bencher: divan::Bencher) {
    bencher.bench_local(|| {
        let runtime = DeterministicRuntime::new();
        runtime.spawn_task(Box::pin(InfiniteSelfWake));
        let error = runtime
            .run_until_stalled()
            .expect_err("infinite self wake exhausts budget");
        assert_eq!(error.polls(), DEFAULT_STEP_BUDGET);
        divan::black_box(error.ready_task_ids().len())
    });
}

/// Completes on the last of a positive number of polls.
struct FiniteSelfWake {
    /// Remaining polls; benchmark arguments are always positive.
    remaining: usize,
}

impl Future for FiniteSelfWake {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.remaining -= 1;
        if self.remaining == 0 {
            Poll::Ready(())
        } else {
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

/// Keeps one task ready until the host's finite poll budget stops driving.
struct InfiniteSelfWake;

impl Future for InfiniteSelfWake {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

#[derive(Clone, Copy)]
/// One causal effect and its response, shared by both Store workflows.
enum Action {
    /// Emits one response without changing state.
    Start,
    /// Commits one scalar increment.
    Response,
}

/// Minimal state-changing effect workflow for Store-level measurements.
struct Feature;

impl Reducer for Feature {
    type State = usize;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            Action::Start => Effect::send(Action::Response),
            Action::Response => {
                *state += 1;
                Effect::none()
            }
        }
    }
}
