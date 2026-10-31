//! Runs change reactions and missing-child policy through the façade's `TestStore`.

#![cfg(feature = "test-util")]

use futures_executor::block_on;
use rca::{
    ActionPath, Effect, MissingChildPolicy, OnChange, OptionalStatePath, ReduceContext, Reducer,
    ReducerExt, TestStore, on_change,
};

/// Records base reduction separately from change callbacks and optional child state.
#[derive(Clone, Debug, Default, PartialEq)]
struct State {
    value: u8,
    base_calls: u8,
    changes: Vec<(u8, u8)>,
    child: Option<u8>,
}

#[derive(Debug, PartialEq)]
enum Action {
    Set(u8),
    Child(u8),
}

/// Counts every base reduction but changes the observed value only for `Set`.
struct Edit;

impl Reducer for Edit {
    type State = State;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut State,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        state.base_calls += 1;
        if let Action::Set(value) = action {
            state.value = *value;
        }
        Effect::none()
    }
}

/// Records the captured old/new pair and checks the action that caused it.
struct Record(u8, u8);

impl Reducer for Record {
    type State = State;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut State,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        assert_eq!(action, &Action::Set(self.1));
        state.changes.push((self.0, self.1));
        Effect::none()
    }
}

/// Checks one changed value and one equal value for either public constructor.
fn assert_reacts(reducer: impl Reducer<State = State, Action = Action> + Send + 'static) {
    let mut store = TestStore::new(State::default(), reducer);
    block_on(async {
        let _ = store
            .send_with(Action::Set(7), |state| {
                state.value = 7;
                state.base_calls = 1;
                state.changes.push((0, 7));
            })
            .await
            .unwrap();
        let _ = store
            .send_with(Action::Set(7), |state| state.base_calls = 2)
            .await
            .unwrap();
        store.finish().await.unwrap();
    });
}

#[test]
fn facade_change_reactions_work_through_both_construction_paths() {
    let reducer: OnChange<_, _, _> = Edit.on_change(|state: &State| state.value, Record);
    assert_reacts(reducer);
    assert_reacts(on_change(Edit, |state: &State| state.value, Record));
}

/// Makes accidental execution against absent child state observable.
struct Child;

impl Reducer for Child {
    type State = u8;
    type Action = u8;

    fn reduce(&mut self, state: &mut u8, action: &u8, _: &mut ReduceContext<'_>) -> Effect<u8> {
        *state += action;
        Effect::none()
    }
}

#[test]
fn facade_missing_child_policy_requires_evidence_and_keeps_base_reduction() {
    let reducer = Edit
        .if_some(
            OptionalStatePath::new(
                |state: &State| state.child.as_ref(),
                |state: &mut State| state.child.as_mut(),
            ),
            ActionPath::new(
                |action: &Action| match action {
                    Action::Child(value) => Some(value),
                    Action::Set(_) => None,
                },
                Action::Child,
            ),
            Child,
        )
        .missing_child_policy(MissingChildPolicy::ReportAndBase);
    let mut store = TestStore::new(State::default(), reducer);
    block_on(async {
        let _ = store
            .send_with(Action::Child(7), |state| state.base_calls = 1)
            .await
            .unwrap();
        let issue = store
            .expect_issue(|issue| issue.code == "rca.missing_child")
            .await
            .unwrap();
        assert!(issue.message.contains("IfSome"));
        let _ = store
            .send_with(Action::Set(9), |state| {
                state.value = 9;
                state.base_calls = 2;
            })
            .await
            .unwrap();
        store.finish().await.unwrap();
    });
}
