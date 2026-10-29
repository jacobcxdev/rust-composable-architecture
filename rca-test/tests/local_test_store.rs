//! Local assertions preserve owned values and release resources on their owner thread.

#![cfg(feature = "local")]

#[path = "local_test_store/clock.rs"]
mod clock;
#[path = "local_test_store/output.rs"]
mod output;

use std::cell::Cell;
use std::rc::Rc;
use std::thread;

use futures_executor::block_on;
use rca_core::ReduceContext;
use rca_core::local::{Effect, Reducer};
use rca_test::local::{CaptureKey, TestStore};

#[test]
fn failed_initial_capture_returns_the_original_rc_cause_and_drops_builder_on_owner() {
    let drops = Rc::new(Cell::new(0));
    let cause = Rc::new(Cell::new(7));
    let captured = Rc::clone(&cause);
    let result = TestStore::builder(
        Rc::new(0),
        Counter {
            drops: Rc::clone(&drops),
            owner: thread::current().id(),
        },
    )
    .with_capture::<Count>(move |_| Err(Rc::clone(&captured)));
    let error = result.expect_err("initial capture fails before a runtime is created");
    let recovered = error.into_capture_error::<Count>().unwrap();
    assert!(Rc::ptr_eq(&recovered, &cause));
    assert_eq!(drops.get(), 1);
    assert_eq!(Rc::strong_count(&cause), 2);
}

#[test]
fn panicking_local_matcher_restores_the_same_owned_action_for_retry() {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    let drops = Rc::new(Cell::new(0));
    let mut store = TestStore::new(
        Rc::new(0),
        Counter {
            drops: Rc::clone(&drops),
            owner: thread::current().id(),
        },
    );
    let start =
        block_on(store.send_with(Action::Set(Rc::new(5)), |value| *value = Rc::new(5))).unwrap();
    let mut original = None;
    let panic = catch_unwind(AssertUnwindSafe(|| {
        block_on(store.receive_with(
            |action| {
                if let Action::Follow(value) = action {
                    original = Some(Rc::as_ptr(value));
                }
                panic!("local matcher");
            },
            |_| {},
        ))
    }));
    let panic = panic.unwrap_err();
    assert_eq!(panic.downcast_ref::<&str>(), Some(&"local matcher"));
    let received = block_on(store.receive_with(
        |action| matches!(action, Action::Follow(value) if Some(Rc::as_ptr(value)) == original),
        |value| *value = Rc::new(6),
    ))
    .unwrap();
    block_on(received.finish()).unwrap();
    block_on(start.finish()).unwrap();
    block_on(store.finish()).unwrap();
    assert_eq!(drops.get(), 1);
}

/// Carries non-`Send` values through external and effect-originated actions.
#[derive(Debug)]
enum Action {
    Set(Rc<usize>),
    Follow(Rc<usize>),
}

#[test]
fn local_renderer_panic_preserves_assertion_debt_and_teardown_releases_owner() {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    let drops = Rc::new(Cell::new(0));
    let render_calls = Rc::new(Cell::new(0));
    let called = Rc::clone(&render_calls);
    let mut store = TestStore::builder(
        Rc::new(0),
        Counter {
            drops: Rc::clone(&drops),
            owner: thread::current().id(),
        },
    )
    .with_snapshot_renderer(move |_| {
        called.set(called.get() + 1);
        panic!("local renderer");
    })
    .build();
    let comparison = catch_unwind(AssertUnwindSafe(|| {
        block_on(store.send_with(Action::Follow(Rc::new(7)), |_| {}))
    }));
    let panic = comparison.unwrap_err();
    assert_eq!(panic.downcast_ref::<&str>(), Some(&"local renderer"));
    assert_eq!(render_calls.get(), 1);
    let error = block_on(store.finish()).unwrap_err();
    assert!(error.is_incomplete_assertion());
    let panic = catch_unwind(AssertUnwindSafe(|| drop(store)))
        .expect_err("teardown must report the incomplete send assertion");
    assert!(
        panic
            .downcast_ref::<String>()
            .unwrap()
            .starts_with("TestStore teardown failed: TestStore incomplete assertion for send at ")
    );
    assert_eq!(drops.get(), 1);
}

#[test]
fn projected_feature_keeps_immutable_rc_history_and_borrowed_expectations() {
    use rca_core::local::{Feature, FeatureContext};
    /// Keeps live state free of `Clone`, `Debug` and `PartialEq` requirements.
    struct State {
        /// Copy-on-write history whose old projection must remain unchanged.
        history: Rc<Vec<Rc<str>>>,
        /// Mutable domain data excluded from the asserted projection.
        private: Rc<Cell<usize>>,
    }

    /// Appends history and counts reductions through a non-`Send` capture.
    struct Append(Rc<Cell<usize>>);

    impl Feature for Append {
        type State = State;
        type Action = Rc<str>;
        fn reduce(
            &mut self,
            state: &mut State,
            action: &Rc<str>,
            _: &mut FeatureContext<'_, State, Rc<str>>,
        ) {
            Rc::make_mut(&mut state.history).push(Rc::clone(action));
            state.private.set(state.private.get() + 1);
            self.0.set(self.0.get() + 1);
        }
    }
    let calls = Rc::new(Cell::new(0));
    let history = Rc::new(Vec::new());
    let capture_calls = Rc::new(Cell::new(0));
    let captured = Rc::clone(&capture_calls);
    let mut store = TestStore::projected_feature_builder(
        State {
            history: Rc::clone(&history),
            private: Rc::new(Cell::new(0)),
        },
        Append(Rc::clone(&calls)),
        move |state| {
            captured.set(captured.get() + 1);
            Rc::clone(&state.history)
        },
    )
    .build();
    let expected: Rc<str> = Rc::from("first");
    let task = block_on(store.send_with(Rc::clone(&expected), |snapshot| {
        Rc::make_mut(snapshot).push(Rc::clone(&expected));
    }))
    .unwrap();
    block_on(task.finish()).unwrap();
    assert!(
        history.is_empty(),
        "retained history must not change with live state"
    );
    assert_eq!(calls.get(), 1);
    assert!(capture_calls.get() >= 2);
    block_on(store.finish()).unwrap();
}

/// Updates owned local state and checks its reducer and destructor thread.
struct Counter {
    /// Counts reducer destruction independently of state and action ownership.
    drops: Rc<Cell<usize>>,
    /// Thread that constructs, drives and releases this fixture.
    owner: thread::ThreadId,
}

impl Drop for Counter {
    fn drop(&mut self) {
        assert_eq!(thread::current().id(), self.owner);
        self.drops.set(self.drops.get() + 1);
    }
}

impl Reducer for Counter {
    type State = Rc<usize>;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        assert_eq!(thread::current().id(), self.owner);
        match action {
            Action::Set(value) => {
                *state = Rc::clone(value);
                Effect::send(Action::Follow(Rc::new(**value + 1)))
            }
            Action::Follow(value) => {
                *state = Rc::clone(value);
                Effect::none()
            }
        }
    }
}

/// Captures local state or preserves an owned, non-`Send` capture failure.
struct Count;

impl CaptureKey for Count {
    type Value = Rc<usize>;
    type Error = Rc<Cell<usize>>;
}

#[test]
fn public_local_host_sends_receives_captures_and_finishes_on_owner() {
    block_on(async {
        let owner = thread::current().id();
        let drops = Rc::new(Cell::new(0));
        let captures = Rc::new(Cell::new(0));
        let capture_count = Rc::clone(&captures);
        let mut store = TestStore::builder(
            Rc::new(0),
            Counter {
                drops: Rc::clone(&drops),
                owner,
            },
        )
        .with_capture::<Count>(move |state| {
            assert_eq!(thread::current().id(), owner);
            capture_count.set(capture_count.get() + 1);
            Ok(Rc::clone(state))
        })
        .unwrap()
        .build();
        let sent = Rc::new(3);
        let expected = Rc::clone(&sent);
        let expected_capture = Rc::clone(&sent);
        let task = store
            .send_with_captures(
                Action::Set(sent),
                move |state| *state = expected,
                move |values| values.set::<Count>(expected_capture),
            )
            .await
            .unwrap();
        let _follow = store
            .receive_with_captures(
                |action: &Action| matches!(action, Action::Follow(value) if **value == 4),
                |state| *state = Rc::new(4),
                |values| values.set::<Count>(Rc::new(4)),
            )
            .await
            .unwrap();
        task.finish().await.unwrap();
        let observed = Rc::new(Cell::new(0));
        let output = Rc::clone(&observed);
        store
            .assert_state(move |state| output.set(**state))
            .await
            .unwrap();
        assert_eq!(observed.get(), 4);
        assert!(captures.get() >= 3);
        store.finish().await.unwrap();
        assert_eq!(drops.get(), 1);
    });
}
