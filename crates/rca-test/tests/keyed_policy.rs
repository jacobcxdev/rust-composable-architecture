//! Qualifies mutable per-entry policy without per-entry reducer objects.

use std::time::Duration;

use futures_executor::block_on;
use rca_core::{
    Effect, KeyedAction, KeyedActionPath, KeyedState, KeyedStatePath, ReduceContext, Reducer,
    for_each,
};
use rca_test::TestStore;

/// Holds per-child policy and progress without implementing `Clone`.
struct ChildState {
    next: u64,
    step: u64,
    completed: Vec<u64>,
}

/// Creates fresh per-child policy and empty completion history.
fn child(next: u64, step: u64) -> ChildState {
    ChildState {
        next,
        step,
        completed: Vec::new(),
    }
}

enum ChildAction {
    Start,
    SetStep(u64),
    Completed(u64),
}

enum Action {
    Child(KeyedAction<u8, ChildAction>),
    Replace(u8),
}

/// Reads changing policy from child state, not a per-entry reducer instance.
struct Child;

impl Reducer for Child {
    type State = ChildState;
    type Action = ChildAction;

    fn reduce(
        &mut self,
        state: &mut ChildState,
        action: &ChildAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<ChildAction> {
        match action {
            ChildAction::Start => {
                let output = state.next;
                state.next += state.step;
                return Effect::send_after(Duration::from_secs(1), ChildAction::Completed(output));
            }
            ChildAction::SetStep(step) => state.step = *step,
            ChildAction::Completed(output) => state.completed.push(*output),
        }
        Effect::none()
    }
}

/// Supplies fresh policy explicitly when replacing a child with the same ID.
struct Parent;

impl Reducer for Parent {
    type State = KeyedState<u8, ChildState>;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        if let Action::Replace(id) = action {
            state.remove(id).expect("existing child");
            state.insert(*id, child(7, 3));
        }
        Effect::none()
    }
}

/// Routes one shared child reducer through state-owned policies.
fn reducer() -> impl Reducer<State = KeyedState<u8, ChildState>, Action = Action> {
    for_each(
        Parent,
        KeyedStatePath::new(|state| state, |state| state),
        KeyedActionPath::new(
            |action| match action {
                Action::Child(child) => Some(child),
                Action::Replace(_) => None,
            },
            Action::Child,
        ),
        Child,
    )
}

/// Copies ordered IDs, next values, steps and completed outputs for independent expectations.
fn snapshot(state: &KeyedState<u8, ChildState>) -> Vec<(u8, u64, u64, Vec<u64>)> {
    state
        .iter()
        .map(|(id, child)| (*id, child.next, child.step, child.completed.clone()))
        .collect()
}

/// Wraps a child action with its destination ID.
fn action(id: u8, action: ChildAction) -> Action {
    Action::Child(KeyedAction::new(id, action))
}

// Wrong-key routing or shared mutable policy makes these literal snapshots fail.
#[test]
fn each_child_owns_its_mutable_policy_and_progress() {
    block_on(async {
        let state = [(1, child(10, 2)), (2, child(100, 10))]
            .into_iter()
            .collect();
        let mut store = TestStore::projected(state, reducer(), snapshot);
        let _ = store
            .send_with(action(1, ChildAction::SetStep(4)), |state| state[0].2 = 4)
            .await
            .unwrap();
        let _ = store
            .send_with(action(1, ChildAction::Start), |state| state[0].1 = 14)
            .await
            .unwrap();
        let _ = store
            .send_with(action(2, ChildAction::Start), |state| state[1].1 = 110)
            .await
            .unwrap();
        store.advance(Duration::from_secs(1)).await.unwrap();
        let _ = store
            .receive_with(
                |value| {
                    matches!(
                        value,
                        Action::Child(value)
                            if *value.id() == 1
                                && matches!(value.action(), ChildAction::Completed(10))
                    )
                },
                |state| state[0].3 = vec![10],
            )
            .await
            .unwrap();
        let _ = store
            .receive_with(
                |value| {
                    matches!(
                        value,
                        Action::Child(value)
                            if *value.id() == 2
                                && matches!(value.action(), ChildAction::Completed(100))
                    )
                },
                |state| state[1].3 = vec![100],
            )
            .await
            .unwrap();
        store.finish().await.unwrap();
    });
}

// Missing generation teardown lets the retired child's output reach its replacement.
#[test]
fn replacement_resets_policy_and_cancels_only_retired_work() {
    block_on(async {
        let state = [(1, child(10, 2)), (2, child(100, 10))]
            .into_iter()
            .collect();
        let mut store = TestStore::projected(state, reducer(), snapshot);
        let _ = store
            .send_with(action(1, ChildAction::Start), |state| state[0].1 = 12)
            .await
            .unwrap();
        let _ = store
            .send_with(action(2, ChildAction::Start), |state| state[1].1 = 110)
            .await
            .unwrap();
        let _ = store
            .send_with(Action::Replace(1), |state| {
                *state = vec![(2, 110, 10, vec![]), (1, 7, 3, vec![])];
            })
            .await
            .unwrap();
        let _ = store
            .send_with(action(1, ChildAction::Start), |state| state[1].1 = 10)
            .await
            .unwrap();
        store.advance(Duration::from_secs(1)).await.unwrap();
        let _ = store
            .receive_with(
                |value| {
                    matches!(
                        value,
                        Action::Child(value)
                            if *value.id() == 2
                                && matches!(value.action(), ChildAction::Completed(100))
                    )
                },
                |state| state[0].3 = vec![100],
            )
            .await
            .unwrap();
        let _ = store
            .receive_with(
                |value| {
                    matches!(
                        value,
                        Action::Child(value)
                            if *value.id() == 1
                                && matches!(value.action(), ChildAction::Completed(7))
                    )
                },
                |state| state[1].3 = vec![7],
            )
            .await
            .unwrap();
        store.finish().await.unwrap();
    });
}
