use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures_executor::block_on;
use rca_core::{ActionPath, Effect, OptionalStatePath, ReduceContext, Reducer, ReducerExt};
use rca_test::{ControlledGate, GateDropped, TestStore, controlled_gate};

#[test]
fn shared_client_future_can_borrow_and_return_a_non_send_value() {
    fn require_send<T: Send>(value: T) -> T {
        value
    }

    let host = rca_test::DeterministicRuntime::new();
    let borrowed = String::from("borrowed client output");
    let client = async { std::rc::Rc::new(borrowed.as_str()) };

    let output = block_on(require_send(host.run(client))).unwrap();

    assert_eq!(*output, borrowed.as_str());
    assert_eq!(output.as_ptr(), borrowed.as_ptr());
    assert_eq!(std::rc::Rc::strong_count(&output), 1);
}

#[derive(Debug, PartialEq)]
enum Action {
    Start,
    Number(u64),
    Remove,
}

enum Mode {
    Some,
    None,
    Mapped,
    Sequence,
    Waiting(Option<ControlledGate>),
}

struct Feature {
    mode: Mode,
    calls: Arc<AtomicUsize>,
}

impl Reducer for Feature {
    type State = Vec<u64>;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        match action {
            Action::Start => {
                let calls = Arc::clone(&self.calls);
                match &mut self.mode {
                    Mode::Some => Effect::future(async { Some(Action::Number(17)) }),
                    Mode::None => Effect::future(async { None::<u64> }).map(move |value| {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Action::Number(value)
                    }),
                    Mode::Mapped => {
                        let value = Cell::new(4_u64);
                        Effect::future(async move { Some(value.get()) })
                            .map(|value| value * 3)
                            .map(move |value| {
                                calls.fetch_add(1, Ordering::SeqCst);
                                Action::Number(value + 2)
                            })
                    }
                    Mode::Sequence => Effect::concatenate([
                        Effect::future(async { Some(Action::Number(11)) }),
                        Effect::future(async move {
                            calls.fetch_add(1, Ordering::SeqCst);
                            Some(Action::Number(23))
                        }),
                    ]),
                    Mode::Waiting(gate) => {
                        let gate = gate.take().expect("start only once");
                        Effect::future(async move {
                            gate.await.expect("release future");
                            calls.fetch_add(1, Ordering::SeqCst);
                            Some(Action::Number(31))
                        })
                    }
                }
            }
            Action::Number(value) => {
                state.push(*value);
                Effect::none()
            }
            Action::Remove => Effect::none(),
        }
    }
}

#[test]
fn some_waits_for_asserted_commit_and_finishes_causal_work() {
    block_on(async {
        let mut store = TestStore::new(
            Vec::new(),
            Feature {
                mode: Mode::Some,
                calls: Arc::new(AtomicUsize::new(0)),
            },
        );
        let task = store.send(Action::Start).await.unwrap();
        task.finish()
            .await
            .expect_err("output still awaits assertion");
        assert!(!task.is_finished());
        let _ = store
            .receive_with(
                |action| *action == Action::Number(17),
                |state| state.push(17),
            )
            .await
            .unwrap();
        task.finish().await.unwrap();
        store.finish().await.unwrap();
    });
}

#[test]
fn none_finishes_without_mapping_or_emitting_an_action() {
    block_on(async {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut store = TestStore::new(
            Vec::new(),
            Feature {
                mode: Mode::None,
                calls: Arc::clone(&calls),
            },
        );
        let task = store.send(Action::Start).await.unwrap();
        task.finish().await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        store.finish().await.unwrap();
    });
}

#[test]
fn nested_mapping_delivers_one_owned_output_in_order() {
    block_on(async {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut store = TestStore::new(
            Vec::new(),
            Feature {
                mode: Mode::Mapped,
                calls: Arc::clone(&calls),
            },
        );
        let task = store.send(Action::Start).await.unwrap();
        let _ = store
            .receive_with(
                |action| *action == Action::Number(14),
                |state| state.push(14),
            )
            .await
            .unwrap();
        task.finish().await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        store.finish().await.unwrap();
    });
}

#[test]
fn concatenate_does_not_poll_next_future_before_asserted_commit() {
    block_on(async {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut store = TestStore::new(
            Vec::new(),
            Feature {
                mode: Mode::Sequence,
                calls: Arc::clone(&calls),
            },
        );
        let task = store.send(Action::Start).await.unwrap();
        task.finish()
            .await
            .expect_err("first output awaits assertion");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let _ = store
            .receive_with(
                |action| *action == Action::Number(11),
                |state| state.push(11),
            )
            .await
            .unwrap();
        task.finish()
            .await
            .expect_err("second output awaits assertion");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let _ = store
            .receive_with(
                |action| *action == Action::Number(23),
                |state| state.push(23),
            )
            .await
            .unwrap();
        task.finish().await.unwrap();
        store.finish().await.unwrap();
    });
}

#[test]
fn cancellation_drops_pending_future_without_emitting() {
    block_on(async {
        let (release, gate) = controlled_gate();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut store = TestStore::new(
            Vec::new(),
            Feature {
                mode: Mode::Waiting(Some(gate)),
                calls: Arc::clone(&calls),
            },
        );
        let task = store.send(Action::Start).await.unwrap();
        task.finish().await.expect_err("future is pending");
        release.wait_until_waiting().await.unwrap();
        task.cancel().await.unwrap();
        assert!(task.is_cancelled());
        assert_eq!(release.release(), Err(GateDropped));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        task.finish().await.unwrap();
        store.finish().await.unwrap();
    });
}

struct Parent;

impl Reducer for Parent {
    type State = Option<Vec<u64>>;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        if *action == Action::Remove {
            *state = None;
        }
        Effect::none()
    }
}

#[test]
fn optional_removal_drops_child_future_without_output() {
    block_on(async {
        let (release, gate) = controlled_gate();
        let calls = Arc::new(AtomicUsize::new(0));
        let reducer = Parent.if_some(
            OptionalStatePath::new(
                |state: &Option<Vec<u64>>| state.as_ref(),
                |state: &mut Option<Vec<u64>>| state.as_mut(),
            ),
            ActionPath::new(
                |action: &Action| (*action != Action::Remove).then_some(action),
                |action| action,
            ),
            Feature {
                mode: Mode::Waiting(Some(gate)),
                calls: Arc::clone(&calls),
            },
        );
        let mut store = TestStore::new(Some(Vec::new()), reducer);
        let task = store.send(Action::Start).await.unwrap();
        task.finish().await.expect_err("child future is pending");
        let _ = store
            .send_with(Action::Remove, |state| *state = None)
            .await
            .unwrap();
        task.finish().await.unwrap();
        assert_eq!(release.release(), Err(GateDropped));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        store.finish().await.unwrap();
    });
}
