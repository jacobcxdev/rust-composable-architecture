//! Measures complete Store operations on current-thread and multi-thread Tokio hosts.
//!
//! Divan measures each closure, including `block_on`, producer spawning, joins,
//! and the small completed-operation counter. Host construction and final state
//! checks are outside that closure. Commit helpers include a state query.
//! Observation delivery and pending-work assertions are measured.
//! `timer_wake` uses an expired deadline, not a pending native timer.
//! `shutdown` includes Store construction. Smoke runs check execution, not speed.

use std::time::Duration;

use rca_core::{Effect, ReduceContext, Reducer, Store};
use rca_tokio::TokioRuntime;

fn main() {
    divan::main();
}

#[derive(Clone, Copy)]
/// Selects fixed workload shapes without payload allocation in the benchmark driver.
enum Action {
    Commit,
    Effect,
    Output,
    Timer,
    Pending(usize),
    Cancel,
}

/// Counts committed outputs and provides cancellable work for lifecycle measurements.
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
            Action::Commit | Action::Output => {
                *state += 1;
                Effect::none()
            }
            Action::Effect => Effect::run(|sender| async move {
                sender.send(Action::Output).await.expect("effect output");
            }),
            Action::Timer => Effect::send_after(Duration::ZERO, Action::Output),
            Action::Pending(count) => {
                Effect::merge((0..*count).map(|_| {
                    Effect::run(|_sender| std::future::pending()).cancellable("bench-work")
                }))
            }
            Action::Cancel => Effect::cancel("bench-work"),
        }
    }
}

/// Owns one native host and a Store across Divan samples.
///
/// `Drop` closes the Store before the host stops its workers.
struct Fixture {
    /// Runs all Store work and outlives its explicit shutdown.
    host: tokio::runtime::Runtime,
    /// Retains the cumulative output count across every benchmark invocation.
    store: Store<usize, Action>,
}

impl Fixture {
    fn current() -> Self {
        Self::new(false)
    }

    fn multi() -> Self {
        Self::new(true)
    }

    /// Constructs the host outside Divan's measured closure.
    ///
    /// Multi-thread workloads use four workers. Changing this count changes the workload configuration.
    fn new(multi: bool) -> Self {
        let host = if multi {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(4)
                .enable_all()
                .build()
                .expect("multi-thread runtime")
        } else {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("current-thread runtime")
        };
        let store = host.block_on(async {
            Store::new(
                0,
                Feature,
                TokioRuntime::current().expect("active Tokio runtime"),
            )
        });
        Self { host, store }
    }

    /// Completes the action's causal work, then queries its cumulative output count.
    ///
    /// Callers measure both operations, not dispatch alone.
    fn commit(&self, action: Action) -> usize {
        self.host.block_on(async {
            self.store
                .send(action)
                .await
                .expect("benchmark commit")
                .finish()
                .await;
            self.store.read(|state| *state).await.expect("state query")
        })
    }

    /// Checks cumulative outputs after Divan stops measuring the workload.
    fn assert_count(&self, expected: usize) {
        assert_eq!(
            self.host
                .block_on(self.store.read(|state| *state))
                .expect("final Store state"),
            expected,
            "every completed operation must update state exactly once"
        );
    }

    /// Measures actions that each produce exactly one committed output.
    ///
    /// The counter includes every closure invocation, including Divan calibration.
    /// Its increment is measured. The final Store query and assertion are not.
    fn bench_commits(&self, bencher: divan::Bencher, action: Action) {
        let mut completed = 0;
        bencher.bench_local(|| {
            let value = self.commit(action);
            completed += 1;
            divan::black_box(value)
        });
        self.assert_count(completed);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if !self.store.is_closed() {
            self.host
                .block_on(self.store.shutdown())
                .expect("benchmark shutdown");
        }
    }
}

#[divan::bench(sample_count = 20)]
fn current_action_round_trip(bencher: divan::Bencher) {
    let fixture = Fixture::current();
    fixture.bench_commits(bencher, Action::Commit);
}

#[divan::bench(sample_count = 20)]
fn multi_action_round_trip(bencher: divan::Bencher) {
    let fixture = Fixture::multi();
    fixture.bench_commits(bencher, Action::Commit);
}

#[divan::bench(sample_count = 20)]
fn current_effect_round_trip(bencher: divan::Bencher) {
    let fixture = Fixture::current();
    fixture.bench_commits(bencher, Action::Effect);
}

#[divan::bench(sample_count = 20)]
fn multi_effect_round_trip(bencher: divan::Bencher) {
    let fixture = Fixture::multi();
    fixture.bench_commits(bencher, Action::Effect);
}

#[divan::bench(sample_count = 20)]
fn query_round_trip(bencher: divan::Bencher) {
    let fixture = Fixture::multi();
    bencher.bench_local(|| {
        divan::black_box(
            fixture
                .host
                .block_on(fixture.store.read(|state| *state))
                .expect("state query"),
        )
    });
    fixture.assert_count(0);
}

#[divan::bench(sample_count = 20)]
fn observation_round_trip(bencher: divan::Bencher) {
    let fixture = Fixture::multi();
    let mut observation = fixture
        .host
        .block_on(fixture.store.observe(|state| *state))
        .expect("observation");
    assert_eq!(fixture.host.block_on(observation.next()), Some(0));
    let mut completed = 0;
    bencher.bench_local(|| {
        fixture.host.block_on(async {
            fixture
                .store
                .send(Action::Commit)
                .await
                .expect("observation commit")
                .finish()
                .await;
            completed += 1;
            let value = observation.next().await.expect("projected value");
            // Delivery correctness is part of this measured round trip.
            assert_eq!(value, completed);
            divan::black_box(value)
        })
    });
    fixture.assert_count(completed);
}

#[divan::bench(args = [1_usize, 2, 4, 8], sample_count = 20)]
fn concurrent_producers(bencher: divan::Bencher, producers: usize) {
    let fixture = Fixture::multi();
    let mut completed = 0;
    bencher.bench_local(|| {
        fixture.host.block_on(async {
            let mut tasks = Vec::with_capacity(producers);
            for _ in 0..producers {
                let store = fixture.store.clone();
                tasks.push(tokio::spawn(async move {
                    store
                        .send(Action::Commit)
                        .await
                        .expect("producer commit")
                        .finish()
                        .await;
                }));
            }
            for task in tasks {
                task.await.expect("producer task");
            }
        });
        completed += producers;
    });
    fixture.assert_count(completed);
}

#[divan::bench(args = [1_usize, 2, 4, 8], sample_count = 20)]
fn independent_stores(bencher: divan::Bencher, count: usize) {
    let host = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("multi-thread runtime");
    let stores: Vec<_> = host.block_on(async {
        let runtime = TokioRuntime::current().expect("active Tokio runtime");
        (0..count)
            .map(|_| Store::new(0, Feature, runtime.clone()))
            .collect()
    });
    // Each outer invocation commits once to every Store.
    let mut completed_per_store = 0;
    bencher.bench_local(|| {
        host.block_on(async {
            let mut tasks = Vec::with_capacity(count);
            for store in &stores {
                let store = store.clone();
                tasks.push(tokio::spawn(async move {
                    store
                        .send(Action::Commit)
                        .await
                        .expect("independent commit")
                        .finish()
                        .await;
                }));
            }
            for task in tasks {
                task.await.expect("independent task");
            }
        });
        completed_per_store += 1;
    });
    host.block_on(async {
        for store in stores {
            assert_eq!(
                store.read(|state| *state).await.expect("final Store state"),
                completed_per_store
            );
            store.shutdown().await.expect("independent shutdown");
        }
    });
}

#[divan::bench(sample_count = 20)]
fn saturated_mailbox(bencher: divan::Bencher) {
    // Capacity-one contention does not guarantee a full queue on every send.
    let host = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("multi-thread runtime");
    let store = host.block_on(async {
        Store::builder(
            0,
            Feature,
            TokioRuntime::current().expect("active Tokio runtime"),
        )
        .mailbox_capacity(1)
        .build()
        .expect("bounded Store")
    });
    let mut completed = 0;
    bencher.bench_local(|| {
        host.block_on(async {
            let mut tasks = Vec::with_capacity(32);
            for _ in 0..32 {
                let store = store.clone();
                tasks.push(tokio::spawn(async move {
                    store
                        .send(Action::Commit)
                        .await
                        .expect("saturated commit")
                        .finish()
                        .await;
                }));
            }
            for task in tasks {
                task.await.expect("saturated task");
            }
        });
        completed += 32;
    });
    assert_eq!(
        host.block_on(store.read(|state| *state))
            .expect("final Store state"),
        completed
    );
    host.block_on(store.shutdown()).expect("bounded shutdown");
}

#[divan::bench(sample_count = 20)]
fn timer_wake(bencher: divan::Bencher) {
    let fixture = Fixture::multi();
    fixture.bench_commits(bencher, Action::Timer);
}

#[divan::bench(sample_count = 20)]
fn cancellation_churn(bencher: divan::Bencher) {
    let fixture = Fixture::multi();
    bencher.bench_local(|| {
        fixture.host.block_on(async {
            let pending = fixture
                .store
                .send(Action::Pending(64))
                .await
                .expect("pending work");
            assert!(!pending.is_finished(), "pending work must remain live");
            fixture
                .store
                .send(Action::Cancel)
                .await
                .expect("cancel work")
                .finish()
                .await;
            pending.finish().await;
        });
    });
    fixture.assert_count(0);
}

#[divan::bench(sample_count = 20)]
fn shutdown(bencher: divan::Bencher) {
    let host = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread runtime");
    bencher.bench_local(|| {
        host.block_on(async {
            let store = Store::new(
                0,
                Feature,
                TokioRuntime::current().expect("active Tokio runtime"),
            );
            store.shutdown().await.expect("benchmark shutdown");
        });
    });
}
