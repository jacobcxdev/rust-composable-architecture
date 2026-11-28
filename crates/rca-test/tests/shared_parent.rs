//! Qualifies parent-owned sharing without introducing shared aliases.

use std::task::{Context, Poll, Waker};
use std::time::Duration;

use futures_executor::block_on;
use rca_core::{ActionPath, Effect, ReduceContext, Reducer, StatePath, Store, combine, scope};
use rca_test::{DeterministicRuntime, TestStore};

/// Keeps shared data move-only while tests copy independent scalar snapshots.
struct Shared {
    digits: u64,
}

/// Owns shared and unrelated values in one root state.
struct State {
    shared: Shared,
    unrelated: u64,
}

impl Default for State {
    fn default() -> Self {
        Self {
            shared: Shared { digits: 0 },
            unrelated: 0,
        }
    }
}

/// Appends digits immediately or through a cancellable delayed action.
enum ChildAction {
    Append(u64),
    Start(u64),
    Cancel,
}

/// Gives two required scopes distinct action routes to the same value.
enum Action {
    Left(ChildAction),
    Right(ChildAction),
    Unrelated,
}

/// Uses digit order to expose delivery and cancellation mistakes.
struct Child;

impl Reducer for Child {
    type State = Shared;
    type Action = ChildAction;

    fn reduce(
        &mut self,
        state: &mut Shared,
        action: &ChildAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<ChildAction> {
        match action {
            ChildAction::Append(digit) => state.digits = state.digits * 10 + digit,
            ChildAction::Start(digit) => {
                return Effect::send_after(Duration::from_secs(1), ChildAction::Append(*digit))
                    .cancellable("pending");
            }
            ChildAction::Cancel => return Effect::cancel("pending"),
        }
        Effect::none()
    }
}

/// Changes unrelated state without touching the shared value.
struct Parent;

impl Reducer for Parent {
    type State = State;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut State,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        if matches!(action, Action::Unrelated) {
            state.unrelated += 1;
        }
        Effect::none()
    }
}

/// Borrows the one parent-owned value through either required scope.
fn shared_path()
-> StatePath<State, Shared, impl Fn(&State) -> &Shared, impl Fn(&mut State) -> &mut Shared> {
    StatePath::new(
        |state: &State| &state.shared,
        |state: &mut State| &mut state.shared,
    )
}

/// Routes left-child input and output without changing the state projection.
fn left_path() -> ActionPath<
    Action,
    ChildAction,
    impl Fn(&Action) -> Option<&ChildAction>,
    impl Fn(ChildAction) -> Action + Clone,
> {
    ActionPath::new(
        |action: &Action| match action {
            Action::Left(child) => Some(child),
            _ => None,
        },
        Action::Left,
    )
}

/// Gives right-child work a separate identity despite the shared state projection.
fn right_path() -> ActionPath<
    Action,
    ChildAction,
    impl Fn(&Action) -> Option<&ChildAction>,
    impl Fn(ChildAction) -> Action + Clone,
> {
    ActionPath::new(
        |action: &Action| match action {
            Action::Right(child) => Some(child),
            _ => None,
        },
        Action::Right,
    )
}

/// Combines two action-scoped children that borrow the same state sequentially.
fn reducer() -> impl Reducer<State = State, Action = Action> {
    combine((
        scope(shared_path(), left_path(), Child),
        scope(shared_path(), right_path(), Child),
        Parent,
    ))
}

/// Selects shared and unrelated values for independent exhaustive assertions.
fn snapshot(state: &State) -> (u64, u64) {
    (state.shared.digits, state.unrelated)
}

// Catches copied child state, incorrect routing and unrelated distinct notifications.
#[test]
fn both_scoped_handles_read_one_value_and_observe_committed_updates() {
    let host = DeterministicRuntime::new();
    let store = Store::new(State::default(), reducer(), host.runtime());
    let left = store.scope(shared_path(), left_path());
    let right = store.scope(shared_path(), right_path());
    block_on(host.run(async {
        let mut values = right.observe_distinct(|state| state.digits).await.unwrap();
        let mut unrelated = store
            .observe_distinct(|state| state.unrelated)
            .await
            .unwrap();
        assert_eq!(values.next().await, Some(0));
        assert_eq!(unrelated.next().await, Some(0));

        let _ = left.send(ChildAction::Append(1)).await.unwrap();
        assert_eq!(right.read(|state| state.digits).await.unwrap(), 1);
        assert_eq!(values.next().await, Some(1));
        let _ = right.send(ChildAction::Append(2)).await.unwrap();
        assert_eq!(left.read(|state| state.digits).await.unwrap(), 12);
        assert_eq!(values.next().await, Some(12));

        let _ = store.send(Action::Unrelated).await.unwrap();
        assert_eq!(unrelated.next().await, Some(1));
        let mut next = std::pin::pin!(values.next());
        assert!(matches!(
            next.as_mut().poll(&mut Context::from_waker(Waker::noop())),
            Poll::Pending
        ));
        store.shutdown().await.unwrap();
    }))
    .unwrap();
}

// Catches cancellation keyed only by state location or user ID instead of scope.
#[test]
fn shared_value_does_not_merge_child_effect_lifetimes() {
    block_on(async {
        let mut store = TestStore::projected(State::default(), reducer(), snapshot);
        let _ = store
            .send(Action::Left(ChildAction::Start(1)))
            .await
            .unwrap();
        store.advance(Duration::from_secs(1)).await.unwrap();
        let _ = store
            .receive_with(
                |action| matches!(action, Action::Left(ChildAction::Append(1))),
                |state| state.0 = 1,
            )
            .await
            .unwrap();
        let _ = store
            .send(Action::Right(ChildAction::Start(2)))
            .await
            .unwrap();
        store.advance(Duration::from_secs(1)).await.unwrap();
        let _ = store
            .receive_with(
                |action| matches!(action, Action::Right(ChildAction::Append(2))),
                |state| state.0 = 12,
            )
            .await
            .unwrap();

        let _ = store
            .send(Action::Left(ChildAction::Start(3)))
            .await
            .unwrap();
        let _ = store
            .send(Action::Right(ChildAction::Start(4)))
            .await
            .unwrap();
        let _ = store.send(Action::Left(ChildAction::Cancel)).await.unwrap();
        store.advance(Duration::from_secs(1)).await.unwrap();
        let _ = store
            .receive_with(
                |action| matches!(action, Action::Right(ChildAction::Append(4))),
                |state| state.0 = 124,
            )
            .await
            .unwrap();
        store.finish().await.unwrap();
    });
}

// Negative control: omitting the common value must not silently pass a root assertion.
#[test]
#[should_panic(expected = "state mismatch")]
fn omitted_shared_change_fails_independent_snapshot_comparison() {
    block_on(async {
        let mut store = TestStore::projected(State::default(), reducer(), snapshot);
        let _ = store
            .send(Action::Left(ChildAction::Append(1)))
            .await
            .unwrap();
    });
}
