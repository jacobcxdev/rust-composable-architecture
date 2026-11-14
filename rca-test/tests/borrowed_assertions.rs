//! Local assertion callbacks borrow expectations without weakening transport.

use std::cell::Cell;
use std::convert::Infallible;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use futures_executor::block_on;
use rca_core::{Effect, ReduceContext, Reducer, Shared};
use rca_test::{CaptureKey, TestStore};

/// Declares the shared value separately from the root snapshot.
struct Value;

impl CaptureKey for Value {
    type Value = usize;
    type Error = Infallible;
}

/// Holds a `Send`-only domain value without `Clone`, `Debug` or equality.
struct State(Cell<usize>);

/// Carries an owned value or requests its later effect delivery.
enum Action {
    Set(Box<usize>),
    Start(usize),
}

/// Applies the same value to root state and the declared shared capture.
struct Feature(Shared<usize>);

impl Reducer for Feature {
    type State = State;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut State,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        match action {
            Action::Set(value) => {
                state.0.set(**value);
                self.0
                    .try_update(
                        |current| {
                            *current = **value;
                            Ok::<_, Infallible>(())
                        },
                        || {},
                    )
                    .unwrap();
                Effect::none()
            }
            Action::Start(value) => Effect::send(Action::Set(Box::new(*value))),
        }
    }
}

/// Builds independent root history and a directly inspectable shared capture.
fn fixture() -> (TestStore<State, Action, Cell<usize>>, Shared<usize>) {
    let shared = Shared::new(0);
    let captured = shared.clone();
    let store =
        TestStore::projected_builder(State(Cell::new(0)), Feature(shared.clone()), |state| {
            Cell::new(state.0.get())
        })
        .with_capture::<Value>(move |_| Ok(captured.read(|value| *value).unwrap()))
        .unwrap()
        .build();
    (store, shared)
}

/// Requires a `Send` future without changing when it runs.
fn send_future<T: Send>(value: T) -> T {
    value
}

/// Extracts an assertion message without accepting unrelated panic payload types.
fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    match payload.downcast::<String>() {
        Ok(message) => *message,
        Err(payload) => payload.downcast_ref::<&str>().unwrap().to_string(),
    }
}

#[test]
fn update_accepts_non_send_key_send_only_value_and_borrowed_callback() {
    /// Deliberately lacks `Send` while its captured value remains `Send`-only.
    struct LocalKey(std::marker::PhantomData<std::rc::Rc<()>>);

    impl CaptureKey for LocalKey {
        type Value = Cell<usize>;
        type Error = Infallible;
    }
    let shared = Shared::new(0);
    let mut store = TestStore::projected_builder(State(Cell::new(0)), Feature(shared), |_| ())
        .with_capture::<LocalKey>(|state| Ok(Cell::new(state.0.get())))
        .unwrap()
        .build();
    let expected = 3;
    let mut calls = 0;
    let _task = block_on(send_future(store.send_with_captures(
        Action::Set(Box::new(expected)),
        |()| {},
        |values| {
            values.update::<LocalKey>(|value| {
                calls += 1;
                value.set(expected);
            })
        },
    )))
    .unwrap();
    assert_eq!(calls, 1);
    block_on(store.finish()).unwrap();
}

#[test]
fn panicking_capture_update_admits_no_send_and_retains_original_receive_payload() {
    for receive in [false, true] {
        let (mut store, shared) = fixture();
        if receive {
            let _task = block_on(store.send(Action::Start(9))).unwrap();
        }
        let mut address = None;
        let mut calls = 0;
        let failure = catch_unwind(AssertUnwindSafe(|| {
            if receive {
                block_on(store.receive_with_captures(
                    |action| {
                        if let Action::Set(value) = action {
                            address = Some((&raw const **value).addr());
                        }
                        true
                    },
                    |snapshot| snapshot.set(9),
                    |values| {
                        values.update::<Value>(|value| {
                            calls += 1;
                            *value = 9;
                            panic!("capture edit");
                        })
                    },
                ))
            } else {
                block_on(store.send_with_captures(
                    Action::Set(Box::new(9)),
                    |snapshot| snapshot.set(9),
                    |values| {
                        values.update::<Value>(|value| {
                            calls += 1;
                            *value = 9;
                            panic!("capture edit");
                        })
                    },
                ))
            }
        }))
        .unwrap_err();
        assert_eq!(panic_message(failure), "capture edit");
        assert_eq!(calls, 1);
        assert_eq!(shared.read(|value| *value).unwrap(), 0);
        block_on(store.assert_state(|state| assert_eq!(state.0.get(), 0))).unwrap();
        if receive {
            let _task = block_on(store.receive_with_captures(
                |action| {
                    matches!(action, Action::Set(value) if Some((&raw const **value).addr()) == address && **value == 9)
                },
                |snapshot| snapshot.set(9),
                |values| values.update::<Value>(|value| *value = 9),
            ))
            .unwrap();
        }
        block_on(store.finish()).unwrap();
    }
}

#[test]
fn required_change_panics_before_send_or_receive_admission_at_every_user_stage() {
    /// Selects one failure stage while keeping production state untouched.
    #[derive(Debug)]
    struct Snapshot {
        value: usize,
        stage: Arc<AtomicUsize>,
        saved: bool,
    }

    impl Clone for Snapshot {
        fn clone(&self) -> Self {
            let stage = self.stage.load(Ordering::SeqCst);
            assert_ne!(stage, 1, "adapter clone");
            Self {
                value: self.value,
                stage: Arc::clone(&self.stage),
                saved: stage != 0,
            }
        }
    }

    impl PartialEq for Snapshot {
        fn eq(&self, other: &Self) -> bool {
            assert_ne!(self.stage.load(Ordering::SeqCst), 3, "adapter equality");
            self.value == other.value
        }
    }

    impl Drop for Snapshot {
        fn drop(&mut self) {
            if self.saved
                && self
                    .stage
                    .compare_exchange(4, 0, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
            {
                panic!("adapter saved clone destructor");
            }
        }
    }

    for receive in [false, true] {
        for (stage, message) in [
            (0, "expected value did not change"),
            (1, "adapter clone"),
            (2, "adapter edit"),
            (3, "adapter equality"),
            (4, "adapter saved clone destructor"),
        ] {
            let shared = Shared::new(0);
            let control = Arc::new(AtomicUsize::new(0));
            let capture_control = Arc::clone(&control);
            let mut store =
                TestStore::projected(State(Cell::new(0)), Feature(shared.clone()), move |state| {
                    Snapshot {
                        value: state.0.get(),
                        stage: Arc::clone(&capture_control),
                        saved: false,
                    }
                });
            if receive {
                let _task = block_on(store.send(Action::Start(9))).unwrap();
            }
            let mut address = None;
            let callback = |snapshot: &mut Snapshot| {
                // Arm only after the ordinary expectation clone and successful preflight.
                control.store(stage, Ordering::SeqCst);
                rca_test::requiring_change(|value: &mut Snapshot| {
                    assert_ne!(stage, 2, "adapter edit");
                    if stage != 0 {
                        value.value = 9;
                    }
                })(snapshot);
            };
            let failure = catch_unwind(AssertUnwindSafe(|| {
                if receive {
                    block_on(store.receive_with(
                        |action| {
                            if let Action::Set(value) = action {
                                address = Some((&raw const **value).addr());
                            }
                            true
                        },
                        callback,
                    ))
                } else {
                    block_on(store.send_with(Action::Set(Box::new(9)), callback))
                }
            }))
            .unwrap_err();
            control.store(0, Ordering::SeqCst);
            assert!(panic_message(failure).contains(message));
            assert_eq!(shared.read(|value| *value).unwrap(), 0);
            block_on(store.assert_state(|state| assert_eq!(state.0.get(), 0))).unwrap();
            if receive {
                let _task = block_on(store.receive_with(
                    |action| {
                        matches!(action, Action::Set(value) if Some((&raw const **value).addr()) == address && **value == 9)
                    },
                    |snapshot| snapshot.value = 9,
                ))
                .unwrap();
            }
            block_on(store.finish()).unwrap();
        }
    }
}

#[test]
fn required_changes_are_scoped_and_borrowed_send_callbacks_compose() {
    for require_root in [false, true] {
        let (mut store, shared) = fixture();
        let mut capture_calls = 0;
        let failure = catch_unwind(AssertUnwindSafe(|| {
            block_on(store.send_with_captures(
                Action::Set(Box::new(3)),
                |snapshot| {
                    if require_root {
                        rca_test::requiring_change(|_: &mut Cell<usize>| {})(snapshot);
                    } else {
                        snapshot.set(3);
                    }
                },
                |values| {
                    capture_calls += 1;
                    values.update::<Value>(rca_test::requiring_change(|value: &mut usize| {
                        if require_root {
                            *value = 3;
                        }
                    }))
                },
            ))
        }))
        .unwrap_err();
        assert_eq!(panic_message(failure), "expected value did not change");
        assert_eq!(capture_calls, usize::from(!require_root));
        assert_eq!(shared.read(|value| *value).unwrap(), 0);
        let expected = 3;
        let _task = block_on(send_future(store.send_with_captures(
            Action::Set(Box::new(expected)),
            rca_test::requiring_change(|snapshot: &mut Cell<usize>| snapshot.set(expected)),
            |values| {
                values.update::<Value>(rca_test::requiring_change(|value: &mut usize| {
                    *value = expected;
                }))
            },
        )))
        .unwrap();
        block_on(store.finish()).unwrap();
    }
}

#[test]
fn required_expected_change_cannot_hide_unchanged_production_or_clear_debt() {
    let (mut store, _) = fixture();
    let failure = catch_unwind(AssertUnwindSafe(|| {
        block_on(store.send_with(
            Action::Start(0),
            rca_test::requiring_change(|snapshot: &mut Cell<usize>| snapshot.set(1)),
        ))
    }))
    .unwrap_err();
    let message = panic_message(failure);
    assert!(message.starts_with("TestStore state mismatch at "));
    assert!(message.contains(" during send: projected coverage"));
    let mut calls = 0;
    let error = block_on(store.send_with(
        Action::Set(Box::new(1)),
        rca_test::requiring_change(|snapshot: &mut Cell<usize>| {
            calls += 1;
            snapshot.set(1);
        }),
    ))
    .unwrap_err();
    assert!(error.is_incomplete_assertion());
    assert_eq!(calls, 0);
    assert!(
        block_on(store.finish())
            .unwrap_err()
            .is_incomplete_assertion()
    );
    let teardown = catch_unwind(AssertUnwindSafe(|| drop(store))).unwrap_err();
    assert!(
        panic_message(teardown)
            .starts_with("TestStore teardown failed: TestStore incomplete assertion for send at ")
    );
}

#[test]
fn required_capture_delta_still_needs_production_to_match() {
    let (mut store, _) = fixture();
    let failure = block_on(store.send_with_captures(
        Action::Start(0),
        |_| {},
        |values| values.update::<Value>(rca_test::requiring_change(|value: &mut usize| *value = 1)),
    ))
    .unwrap_err();
    assert!(failure.is_capture_mismatch());
    let mut calls = 0;
    let failure = block_on(store.check_captures(|values| {
        values.update::<Value>(rca_test::requiring_change(|value: &mut usize| {
            calls += 1;
            *value = 1;
        }))
    }))
    .unwrap_err();
    assert!(failure.is_incomplete_assertion());
    assert_eq!(calls, 0);
    assert!(
        block_on(store.finish())
            .unwrap_err()
            .is_incomplete_assertion()
    );
    let teardown = catch_unwind(AssertUnwindSafe(|| drop(store))).unwrap_err();
    assert!(
        panic_message(teardown)
            .starts_with("TestStore teardown failed: TestStore incomplete assertion for send at ")
    );
}

// Reintroducing 'static or invoking callbacks twice breaks this test.
#[test]
fn all_local_callbacks_borrow_expected_values_and_run_once() {
    let (mut store, shared) = fixture();
    let expected = 3;
    let mut updates = 0;
    let mut captures = 0;
    let mut matches = 0;
    block_on(async {
        let _task = send_future(store.send_with_captures(
            Action::Set(Box::new(expected)),
            |snapshot| {
                updates += 1;
                snapshot.set(expected);
            },
            |values| {
                captures += 1;
                values.set::<Value>(expected)
            },
        ))
        .await
        .unwrap();
        let _task = send_future(store.send_with(Action::Start(expected), |snapshot| {
            updates += 1;
            snapshot.set(expected);
        }))
        .await
        .unwrap();
        let _task = send_future(store.receive_with_captures(
            |action| {
                matches += 1;
                matches!(action, Action::Set(value) if **value == expected)
            },
            |snapshot| {
                updates += 1;
                snapshot.set(expected);
            },
            |values| {
                captures += 1;
                values.set::<Value>(expected)
            },
        ))
        .await
        .unwrap();
        let _task = store.send(Action::Start(expected)).await.unwrap();
        let _task = send_future(store.receive_with(
            |action| {
                matches += 1;
                matches!(action, Action::Set(value) if **value == expected)
            },
            |snapshot| {
                updates += 1;
                snapshot.set(expected);
            },
        ))
        .await
        .unwrap();
        let _task = store.send(Action::Start(expected)).await.unwrap();
        let _task = send_future(store.receive(|action| {
            matches += 1;
            matches!(action, Action::Set(value) if **value == expected)
        }))
        .await
        .unwrap();
        shared
            .try_update(
                |value| {
                    *value = expected + 1;
                    Ok::<_, Infallible>(())
                },
                || {},
            )
            .unwrap();
        send_future(store.check_captures(|values| {
            captures += 1;
            values.set::<Value>(expected + 1)
        }))
        .await
        .unwrap();
        store.finish().await.unwrap();
    });
    assert_eq!((updates, captures, matches), (4, 3, 3));
}

// Rejecting an action must not release it or run state/capture updates.
#[test]
fn rejected_match_retains_move_only_action_and_skips_updates() {
    let (mut store, _) = fixture();
    let expected = 5;
    let mut matches = 0;
    let mut updates = 0;
    let mut captures = 0;
    let _task = block_on(store.send(Action::Start(expected))).unwrap();
    let error = block_on(store.receive_with_captures(
        |action| {
            matches += 1;
            matches!(action, Action::Set(value) if **value != expected)
        },
        |_| {
            updates += 1;
        },
        |_| {
            captures += 1;
            Ok(())
        },
    ))
    .unwrap_err();
    assert!(error.is_action_mismatch());
    assert_eq!((matches, updates, captures), (1, 0, 0));
    let failure = catch_unwind(AssertUnwindSafe(|| {
        block_on(store.receive(|action| {
            matches += 1;
            assert!(matches!(action, Action::Set(value) if **value == expected));
            panic!("borrowed matcher");
        }))
    }))
    .unwrap_err();
    assert_eq!(panic_message(failure), "borrowed matcher");
    assert_eq!(matches, 2);
    let _task = block_on(store.receive_with_captures(
        |action| matches!(action, Action::Set(value) if **value == expected),
        |snapshot| snapshot.set(expected),
        |values| values.set::<Value>(expected),
    ))
    .unwrap();
    block_on(store.finish()).unwrap();
}

/// Counts callback-owned resource destruction independently of state admission.
struct DropProbe(Arc<AtomicUsize>);

impl Drop for DropProbe {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

// Callback resources belong to the unpolled future; dropping it admits nothing.
#[test]
fn unpolled_and_panicking_borrowed_callbacks_drop_resources_without_admission() {
    let (mut store, _) = fixture();
    let drops = Arc::new(AtomicUsize::new(0));
    let expected = 2;
    let probe = DropProbe(Arc::clone(&drops));
    drop(
        store.send_with(Action::Set(Box::new(expected)), |snapshot| {
            drop(probe);
            snapshot.set(expected);
        }),
    );
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    let probe = DropProbe(Arc::clone(&drops));
    let failure = catch_unwind(AssertUnwindSafe(|| {
        block_on(store.send_with(Action::Set(Box::new(expected)), |_| {
            let _probe = probe;
            panic!("borrowed {expected}");
        }))
    }))
    .unwrap_err();
    assert_eq!(panic_message(failure), "borrowed 2");
    assert_eq!(drops.load(Ordering::SeqCst), 2);
    let probe = DropProbe(Arc::clone(&drops));
    let failure = catch_unwind(AssertUnwindSafe(|| {
        block_on(store.send_with_captures(
            Action::Set(Box::new(expected)),
            |snapshot| snapshot.set(expected),
            |_| {
                let _probe = probe;
                panic!("borrowed captures {expected}");
            },
        ))
    }))
    .unwrap_err();
    assert_eq!(panic_message(failure), "borrowed captures 2");
    assert_eq!(drops.load(Ordering::SeqCst), 3);
    block_on(store.finish()).unwrap();
}
