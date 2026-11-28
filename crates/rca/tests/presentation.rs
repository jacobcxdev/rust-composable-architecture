//! Exercises headless child dismissal after the parent inspects move-only child state.

#![cfg(feature = "test-util")]

use rca::{
    ActionPath, DeterministicRuntime, Effect, PresentationAction, ReduceContext, Reducer,
    StatePath, Store, presentation,
};

/// Keeps child identity separate from the value inspected during dismissal.
struct ChildState {
    id: u8,
    value: u8,
}

/// Records what the parent sees before composition removes the child.
struct State {
    child: Option<ChildState>,
    inspected: Option<u8>,
}

enum Action {
    Child(PresentationAction<()>),
}

/// Requests dismissal through its reduction context rather than mutating the parent.
struct Child;

impl Reducer for Child {
    type State = ChildState;
    type Action = ();

    fn reduce(
        &mut self,
        _: &mut ChildState,
        (): &(),
        context: &mut ReduceContext<'_>,
    ) -> Effect<()> {
        context.dismiss().effect()
    }
}

/// Reads the still-present child's value when the dismissal action arrives.
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
        if matches!(action, Action::Child(PresentationAction::Dismiss)) {
            state.inspected = state.child.as_ref().map(|child| child.value);
        }
        Effect::none()
    }
}

#[test]
fn facade_dismisses_through_parent_inspection_without_domain_cloning() {
    let runtime = DeterministicRuntime::new();
    let reducer = presentation(
        Parent,
        StatePath::new(
            |state: &State| &state.child,
            |state: &mut State| &mut state.child,
        ),
        ActionPath::new(
            |action: &Action| match action {
                Action::Child(action) => Some(action),
            },
            Action::Child,
        ),
        |child: &ChildState| &child.id,
        Child,
    );
    let store = Store::new(
        State {
            child: Some(ChildState { id: 1, value: 7 }),
            inspected: None,
        },
        reducer,
        runtime.runtime(),
    );
    futures_executor::block_on(runtime.run(async {
        store
            .send(Action::Child(PresentationAction::Presented(())))
            .await
            .unwrap()
            .finish()
            .await;
        assert!(
            store
                .read(|state| state.child.is_none() && state.inspected == Some(7))
                .await
                .unwrap()
        );
        store.shutdown().await.unwrap();
    }))
    .unwrap();
}
