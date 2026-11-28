//! Captures check ordered private values and shared data independently of root snapshots.

use std::convert::Infallible;
use std::panic::{AssertUnwindSafe, catch_unwind};

use futures_executor::block_on;
use rca_core::{
    ActionPath, Effect, ReduceContext, Reducer, Shared, SharedAccessError, StackAction, StackId,
    StackState, StatePath, stack,
};
use rca_test::{CaptureKey, Exhaustivity, TestStore};

/// Holds domain state without `Clone`, `Debug` or equality requirements.
struct ChildState {
    private: u64,
}

/// Changes private and shared inputs independently in one child reduction.
struct ChildAction {
    private: u64,
    shared: u64,
}

/// Separates child mutations from parent-owned changes to entry order and lifetime.
enum Action {
    Path(StackAction<u64, ChildAction>),
    ReverseByReinsertion,
    RecreateTail,
}

/// Reorders or recreates entries through their public removal and insertion APIs.
struct Parent;
impl Reducer for Parent {
    type State = StackState<ChildState>;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        match action {
            Action::Path(_) => {}
            Action::ReverseByReinsertion => {
                let tail = state.pop().expect("fixture has two entries");
                let first = state.pop().expect("fixture has two entries");
                // Public StackState has no lifetime-preserving reorder operation.
                // These insertions reuse logical IDs, but renew entry authority.
                for (id, value) in [tail, first] {
                    state.try_push(id, value).unwrap_or_else(|_| {
                        panic!("the fixture removed both entries before reinsertion")
                    });
                }
            }
            Action::RecreateTail => {
                let (id, value) = state.pop().expect("fixture has a tail");
                state
                    .try_push(id, value)
                    .unwrap_or_else(|_| panic!("the same logical ID was removed"));
            }
        }
        Effect::none()
    }
}

/// Shares one value while each stack entry retains its own private state.
struct Child(Shared<u64>);
impl Reducer for Child {
    type State = ChildState;
    type Action = ChildAction;

    fn reduce(
        &mut self,
        state: &mut ChildState,
        action: &ChildAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<ChildAction> {
        state.private = action.private;
        self.0
            .try_update(
                |value| {
                    *value = action.shared;
                    Ok::<_, Infallible>(())
                },
                || {},
            )
            .unwrap();
        Effect::none()
    }
}

/// Captures ordered logical IDs and private values without copying domain state.
struct PrivateValues;
impl CaptureKey for PrivateValues {
    type Value = Vec<(StackId, u64)>;
    type Error = Infallible;
}

/// Captures shared data separately from each entry's private value.
struct SharedValue;
impl CaptureKey for SharedValue {
    type Value = u64;
    type Error = SharedAccessError;
}

/// Leaves the root snapshot empty so declared captures carry every value assertion.
type Store = TestStore<StackState<ChildState>, Action, ()>;

/// Builds two entries with separate private and shared captures under the selected assertion mode.
fn fixture(mode: Exhaustivity) -> (Store, StackId, StackId, Shared<u64>) {
    let mut state = StackState::new();
    let first = state.push(ChildState { private: 10 });
    let second = state.push(ChildState { private: 20 });
    let shared = Shared::new(5);
    let captured = shared.clone();
    let reducer = stack(
        Parent,
        StatePath::new(
            |state: &StackState<ChildState>| state,
            |state: &mut StackState<ChildState>| state,
        ),
        ActionPath::new(
            |action: &Action| match action {
                Action::Path(action) => Some(action),
                Action::ReverseByReinsertion | Action::RecreateTail => None,
            },
            Action::Path,
        ),
        |value: &u64, _: &mut ReduceContext<'_>| ChildState { private: *value },
        Child(shared.clone()),
    );
    let store = TestStore::projected_builder(state, reducer, |_| ())
        .with_exhaustivity(mode)
        .with_capture::<PrivateValues>(|state| {
            Ok(state
                .iter()
                .map(|(id, value)| (id.clone(), value.private))
                .collect())
        })
        .unwrap()
        .with_capture::<SharedValue>(move |_| captured.read(|value| *value))
        .unwrap()
        .build();
    (store, first, second, shared)
}

/// Routes independent private and shared values to the selected logical ID.
fn mutate(id: StackId, private: u64, shared: u64) -> Action {
    Action::Path(StackAction::Element {
        id,
        action: ChildAction { private, shared },
    })
}

/// Requires an admitted capture failure to survive finish and normal teardown.
fn assert_sticky_failure(mut store: Store) {
    assert!(
        block_on(store.finish())
            .unwrap_err()
            .is_incomplete_assertion()
    );
    let panic = catch_unwind(AssertUnwindSafe(|| drop(store)))
        .expect_err("an admitted stack capture failure must survive normal Drop");
    let message = panic
        .downcast_ref::<String>()
        .expect("teardown supplies a diagnostic message");
    assert!(message.contains("incomplete assertion"), "{message}");
}

// Ignoring either declared capture, or exempting non-exhaustive stores, breaks this.
#[test]
fn omitted_stack_private_or_shared_change_fails_in_both_modes() {
    for mode in [Exhaustivity::Exhaustive, Exhaustivity::NonExhaustive] {
        for omit_private in [false, true] {
            let (mut store, first, second, shared) = fixture(mode);
            let before = shared.read(|value| *value).unwrap();
            let failure = block_on(store.send_with_captures(
                mutate(first.clone(), 11, 7),
                |()| {},
                move |expected| {
                    if omit_private {
                        expected.set::<SharedValue>(7)
                    } else {
                        expected.set::<PrivateValues>(vec![(first, 11), (second, 20)])
                    }
                },
            ))
            .unwrap_err();
            assert!(failure.is_capture_mismatch());
            assert_eq!(before, 5, "the scalar capture is independent history");
            assert_eq!(shared.read(|value| *value).unwrap(), 7);
            assert_sticky_failure(store);
        }
    }
}

// Capturing only values or stale positions loses the requested logical-ID order.
#[test]
fn omitted_stack_reorder_is_not_hidden_by_equal_root_projection() {
    for mode in [Exhaustivity::Exhaustive, Exhaustivity::NonExhaustive] {
        let (mut store, _, _, _) = fixture(mode);
        assert!(
            block_on(store.send(Action::ReverseByReinsertion))
                .unwrap_err()
                .is_capture_mismatch()
        );
        assert_sticky_failure(store);
    }
}

// Comparing transient positions or treating repeated IDs as old values breaks this.
#[test]
fn explicit_stack_captures_follow_reordered_and_reused_logical_ids() {
    for mode in [Exhaustivity::Exhaustive, Exhaustivity::NonExhaustive] {
        let (mut store, first, second, _) = fixture(mode);
        let wanted = vec![(first.clone(), 11), (second.clone(), 20)];
        let _task = block_on(store.send_with_captures(
            mutate(first.clone(), 11, 7),
            |()| {},
            move |expected| {
                expected.set::<PrivateValues>(wanted)?;
                expected.set::<SharedValue>(7)
            },
        ))
        .unwrap();
        let wanted = vec![(second.clone(), 20), (first.clone(), 11)];
        let _task = block_on(store.send_with_captures(
            Action::ReverseByReinsertion,
            |()| {},
            move |expected| expected.set::<PrivateValues>(wanted),
        ))
        .unwrap();
        // Equal selected values legitimately compare equal across a new lifetime.
        // Captures do not grant authority; existing stack scope witnesses cover it.
        let _task = block_on(store.send(Action::RecreateTail)).unwrap();
        let wanted = vec![(second.clone(), 20), (first.clone(), 13)];
        let _task = block_on(store.send_with_captures(
            mutate(first.clone(), 13, 9),
            |()| {},
            move |expected| {
                expected.set::<PrivateValues>(wanted)?;
                expected.set::<SharedValue>(9)
            },
        ))
        .unwrap();
        let wanted = vec![(second.clone(), 23), (first, 13)];
        let _task = block_on(store.send_with_captures(
            mutate(second, 23, 10),
            |()| {},
            move |expected| {
                expected.set::<PrivateValues>(wanted)?;
                expected.set::<SharedValue>(10)
            },
        ))
        .unwrap();
        block_on(store.finish()).unwrap();
    }
}
