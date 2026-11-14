//! Checks cancelled Local assertions around the real reducer-commit boundary.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use rca_core::ReduceContext;
use rca_core::local::{Effect, Runtime, RuntimeDriver, RuntimeFuture};

use super::*;

/// Pauses the fixture's single actor and retains its latest wake request.
#[derive(Debug, Default)]
struct Pause {
    paused: Cell<bool>,
    waker: RefCell<Option<Waker>>,
}

impl Pause {
    /// Resumes polling after releasing the borrow of the stored waker.
    fn resume(&self) {
        self.paused.set(false);
        let wake = self.waker.borrow_mut().take();
        if let Some(wake) = wake {
            wake.wake();
        }
    }
}

/// Gates actor polling while retaining the real host's clock and yield behaviour.
#[derive(Debug)]
struct PausedDriver {
    host: DeterministicRuntime,
    pause: Rc<Pause>,
}

impl RuntimeDriver for PausedDriver {
    fn spawn(&self, mut task: RuntimeFuture) {
        let pause = Rc::clone(&self.pause);
        self.host
            .spawn_task(Box::pin(std::future::poll_fn(move |cx| {
                if pause.paused.get() {
                    *pause.waker.borrow_mut() = Some(cx.waker().clone());
                    Poll::Pending
                } else {
                    task.as_mut().poll(cx)
                }
            })));
    }

    fn now(&self) -> MonotonicInstant {
        self.host.now()
    }

    fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
        self.host.sleep_until(deadline)
    }

    fn yield_now(&self) -> RuntimeFuture {
        Box::pin(crate::runtime::scheduler::yield_now())
    }
}

/// Starts an eager child action or commits the original owner-local payload.
enum Action {
    Start,
    Increment(Rc<usize>),
}

/// Pauses immediately after a commit so assertion cancellation can interrupt its read.
struct Feature {
    pause: Rc<Pause>,
    commits: Rc<Cell<usize>>,
    payload: Rc<usize>,
}

impl Reducer for Feature {
    type State = Rc<usize>;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut Rc<usize>,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        match action {
            Action::Start => Effect::send(Action::Increment(Rc::clone(&self.payload))),
            Action::Increment(value) => {
                *state = Rc::clone(value);
                self.commits.set(self.commits.get() + 1);
                self.pause.paused.set(true);
                Effect::none()
            }
        }
    }
}

#[test]
fn projected_accepts_a_feature_with_owner_local_task_captures() {
    struct State {
        value: usize,
        owner: Rc<()>,
    }

    struct Work;

    impl rca_core::local::Feature for Work {
        type State = State;
        type Action = ();

        fn update(
            &mut self,
            state: &mut State,
            (): &(),
            context: &mut rca_core::local::FeatureContext<'_, State, ()>,
        ) {
            let owner = Rc::clone(&state.owner);
            context.task(rca_core::local::Task::run(move |context| async move {
                context
                    .update(move |state: &mut State| {
                        assert!(Rc::ptr_eq(&state.owner, &owner));
                        state.value += 1;
                    })
                    .await
                    .unwrap();
            }));
        }
    }

    futures_executor::block_on(async {
        let mut store = TestStore::projected(
            State {
                value: 0,
                owner: Rc::new(()),
            },
            Work,
            |state| state.value,
        );
        let task = store.send(()).await.unwrap();

        let _ = store
            .receive_update(|expected| *expected = 1)
            .await
            .unwrap();

        task.finish().await.unwrap();
        store.finish().await.unwrap();
    });
}

#[test]
fn policy_constructors_accept_a_feature_with_owner_local_captures() {
    /// Records each mutation without requiring a thread-safe capture.
    struct Counter(Rc<Cell<usize>>);

    impl rca_core::local::Feature for Counter {
        type State = usize;
        type Action = ();

        fn update(
            &mut self,
            state: &mut usize,
            (): &(),
            _: &mut rca_core::local::FeatureContext<'_, usize, ()>,
        ) {
            self.0.set(self.0.get() + 1);
            *state += 1;
        }
    }

    futures_executor::block_on(async {
        let calls = Rc::new(Cell::new(0));
        let stores = [
            TestStore::with_exhaustivity(
                0,
                Counter(Rc::clone(&calls)),
                Exhaustivity::NonExhaustive,
            ),
            TestStore::with_step_budget(0, Counter(Rc::clone(&calls)), 64),
            TestStore::with_exhaustivity_and_step_budget(
                0,
                Counter(Rc::clone(&calls)),
                Exhaustivity::Exhaustive,
                64,
            ),
        ];

        for (index, mut store) in stores.into_iter().enumerate() {
            let task = store.send_with((), |expected| *expected = 1).await.unwrap();
            assert_eq!(calls.get(), index + 1);

            task.finish().await.unwrap();
            store.finish().await.unwrap();
        }
    });
}

/// Polls a finite fixture operation without advancing time.
///
/// # Panics
///
/// Panics if the operation is still pending after 64 polls.
fn ready<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    for _ in 0..64 {
        if let Poll::Ready(output) = future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        {
            return output;
        }
    }
    panic!("finite operation remained pending");
}

/// Builds a real Local actor, gate, host and `TestStore` with paused actor polling.
///
/// The eager child action does not spawn a second task: the one stored waker
/// belongs to the actor whose commit and assertion read the test separates.
fn paused_store(
    pause: &Rc<Pause>,
    commits: &Rc<Cell<usize>>,
    payload: &Rc<usize>,
) -> TestStore<Rc<usize>, Action> {
    let runtime = DeterministicRuntime::new();
    let gate = TransactionGate::new();
    let issues = IssueRecorder::new();
    let store = Store::builder(
        Rc::new(0),
        Feature {
            pause: Rc::clone(pause),
            commits: Rc::clone(commits),
            payload: Rc::clone(payload),
        },
        Runtime::new(PausedDriver {
            host: runtime.clone(),
            pause: Rc::clone(pause),
        }),
    )
    .transaction_gate(gate.clone())
    .issue_recorder(issues.clone())
    .build()
    .unwrap();
    TestStore {
        store,
        runtime,
        gate,
        issues,
        omissions: Vec::new(),
        mode: Exhaustivity::NonExhaustive,
        completed: false,
        shutdown: false,
        assertions: Assertions {
            baseline: std::panic::AssertUnwindSafe(parking_lot::Mutex::new(Rc::new(0))),
            pending: std::panic::AssertUnwindSafe(parking_lot::Mutex::new(None)),
            capture: Rc::new(Rc::clone),
            render: None,
            coverage: Coverage::Full,
            declared: DeclaredCaptures::default(),
        },
    }
}

#[test]
fn cancelled_local_send_and_receive_keep_real_boundary_debt_before_and_after_commit() {
    for receive in [false, true] {
        for after_commit in [false, true] {
            let pause = Rc::new(Pause::default());
            let commits = Rc::new(Cell::new(0));
            let payload = Rc::new(1);
            let mut store = paused_store(&pause, &commits, &payload);
            let initial = receive.then(|| ready(store.send(Action::Start)).unwrap());
            pause.paused.set(!after_commit);
            let expected = Rc::clone(&payload);
            let mut future: std::pin::Pin<Box<dyn Future<Output = _> + '_>> = if receive {
                Box::pin(store.receive_with(|_| true, move |state| *state = expected))
            } else {
                Box::pin(
                    store.send_with(Action::Increment(Rc::clone(&payload)), move |state| {
                        *state = expected;
                    }),
                )
            };
            assert!(
                future
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            assert_eq!(commits.get(), usize::from(after_commit));
            drop(future);
            assert!(ready(store.finish()).unwrap_err().is_incomplete_assertion());
            assert!(
                ready(store.receive(|_| panic!("debt blocks matcher")))
                    .unwrap_err()
                    .is_incomplete_assertion()
            );
            assert_eq!(**store.assertions.baseline.get_mut(), 0);

            pause.resume();
            let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(store)))
                .expect_err("cancelled assertion remains visible during teardown");
            let operation = if receive { "receive" } else { "send" };
            assert!(panic.downcast_ref::<String>().unwrap().starts_with(&format!(
                "TestStore teardown failed: TestStore incomplete assertion for {operation} at "
            )));
            drop(initial);
            assert_eq!(Rc::strong_count(&payload), 1);
        }
    }
}
