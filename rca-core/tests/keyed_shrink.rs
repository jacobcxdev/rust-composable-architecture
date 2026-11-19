//! Checks that shrinking storage leaves an existing child scope and effect alive.

use rca_core::{
    Effect, KeyedAction, KeyedActionPath, KeyedState, KeyedStatePath, ReduceContext, Reducer,
    Store, for_each,
};

mod support;

#[cfg(not(miri))]
#[path = "../src/allocation_test/child.rs"]
mod subprocess;

/// Holds a response that the test releases only after storage has been shrunk.
struct ChildState {
    value: u8,
    /// Moved into the single effect so the pending operation is not duplicated.
    response: Option<async_channel::Receiver<u8>>,
}

/// Starts a pending response or applies the response to the child.
enum ChildAction {
    Start,
    Response(u8),
}

/// Routes child work and separates actor startup from the storage change.
enum Action {
    Child(KeyedAction<u8, ChildAction>),
    Warm,
    Shrink,
}

/// Shrinks the existing collection instead of replacing its child identities.
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
        if matches!(action, Action::Shrink) {
            state.shrink_to_fit();
        }
        Effect::none()
    }
}

/// Waits for the test-controlled response through an ordinary effect.
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
                let response = state.response.take().unwrap();
                Effect::run(move |sender| async move {
                    let value = response.recv().await.unwrap();
                    sender.send(ChildAction::Response(value)).await.unwrap();
                })
            }
            ChildAction::Response(value) => {
                state.value = *value;
                Effect::none()
            }
        }
    }
}

// Rebuilding the collection would end both the pre-existing handle and pending response.
#[test]
fn parent_shrink_preserves_live_child_effect_and_scoped_handle() {
    // Native deadlocks are contained by the shared 60-second child watchdog.
    // Miri runs the same scenario directly under the surrounding job's limit.
    #[cfg(not(miri))]
    if subprocess::run_in_child("parent_shrink_preserves_live_child_effect_and_scoped_handle") {
        return;
    }
    check_live_child_after_shrink();
}

/// Uses the pre-shrink scope and effect sender, then stops and joins the runtime.
fn check_live_child_after_shrink() {
    let runtime = support::ManualRuntime::new();
    let (reply, response) = async_channel::bounded(1);
    let mut states = KeyedState::with_capacity(100);
    states.insert(
        7,
        ChildState {
            value: 0,
            response: Some(response),
        },
    );
    let state_path = KeyedStatePath::new(
        |s: &KeyedState<u8, ChildState>| s,
        |s: &mut KeyedState<u8, ChildState>| s,
    );
    let action_path = KeyedActionPath::new(
        |a: &Action| match a {
            Action::Child(a) => Some(a),
            _ => None,
        },
        Action::Child,
    );
    let reducer = for_each(Parent, state_path, action_path, Child);
    let store = Store::new(states, reducer, runtime.runtime());
    runtime.start_all();
    futures_executor::block_on(async {
        store.send(Action::Warm).await.unwrap().finish().await;
        let child = store.scope_keyed(state_path, action_path, 7).await.unwrap();
        let task = child.send(ChildAction::Start).await.unwrap();
        store.read(|_| ()).await.unwrap();
        runtime.start_all();
        store.send(Action::Shrink).await.unwrap().finish().await;
        assert_eq!(child.read(|s| s.value).await.unwrap(), 0);
        reply.try_send(42).unwrap();
        task.finish().await;
        assert_eq!(child.read(|s| s.value).await.unwrap(), 42);
        child
            .send(ChildAction::Response(43))
            .await
            .unwrap()
            .finish()
            .await;
        assert_eq!(child.read(|s| s.value).await.unwrap(), 43);
        store.shutdown().await.unwrap();
    });
    for result in runtime.join_all() {
        result.unwrap();
    }
}
