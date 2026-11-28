//! Exercises closure leaves through root keyed paths and generated paths.

use futures_executor::block_on;
use rca_core::{
    Effect, KeyedAction, KeyedState, Reduce, Reducer, for_each, identity_action_path,
    identity_keyed_action_path, identity_keyed_state_path, identity_state_path, scope,
};
use rca_macros::{ActionPaths, StatePaths};
use rca_test::TestStore;

/// Keeps child data separate from the shared closure's call counter.
struct ChildState {
    value: usize,
}

enum ChildAction {
    Add(usize),
    Completed(usize),
}

/// Shares one persistent closure across keyed children, including their returned effect actions.
fn keyed_reducer()
-> impl Reducer<State = KeyedState<u8, ChildState>, Action = KeyedAction<u8, ChildAction>> {
    let mut calls = 0;
    let leaf = Reduce::<ChildState, ChildAction, _>::new(move |state, action, _| {
        calls += 1;
        match action {
            ChildAction::Add(amount) => {
                state.value += amount + calls;
                Effect::send(ChildAction::Completed(state.value))
            }
            ChildAction::Completed(value) => {
                assert_eq!(state.value, *value);
                Effect::none()
            }
        }
    });
    // Identity paths also interoperate with ordinary required composition.
    let leaf = scope(identity_state_path(), identity_action_path(), leaf);
    for_each(
        Reduce::<KeyedState<u8, ChildState>, KeyedAction<u8, ChildAction>, _>::new(|_, _, _| {
            Effect::none()
        }),
        identity_keyed_state_path(),
        identity_keyed_action_path(),
        leaf,
    )
}

/// Copies ordered IDs and values without requiring the domain state to implement `Clone`.
fn snapshot(rows: &KeyedState<u8, ChildState>) -> Vec<(u8, usize)> {
    rows.iter().map(|(id, child)| (*id, child.value)).collect()
}

#[test]
fn root_keyed_closure_leaf_keeps_move_only_actions_and_persistent_captures() {
    block_on(async {
        let rows = [(7, ChildState { value: 2 }), (9, ChildState { value: 10 })]
            .into_iter()
            .collect();
        let mut store = TestStore::projected(rows, keyed_reducer(), snapshot);
        let expected_id = 7;
        let _ = store
            .send_with(KeyedAction::new(7, ChildAction::Add(3)), |rows| {
                rows[0].1 = 6;
            })
            .await
            .unwrap();
        let _ = store
            .receive(|action| {
                matches!(action.action(), ChildAction::Completed(6)) && action.id() == &expected_id
            })
            .await
            .unwrap();
        let _ = store
            .send_with(KeyedAction::new(9, ChildAction::Add(4)), |rows| {
                rows[1].1 = 17;
            })
            .await
            .unwrap();
        let _ = store
            .receive(|action| {
                matches!(action.action(), ChildAction::Completed(17)) && *action.id() == 9
            })
            .await
            .unwrap();
        store.finish().await.unwrap();
    });
}

/// Embeds the same keyed workflow behind a generated state path.
#[derive(StatePaths)]
struct AppState {
    rows: KeyedState<u8, ChildState>,
}

/// Routes the same move-only actions through a generated action path.
#[derive(ActionPaths)]
enum AppAction {
    Rows(KeyedAction<u8, ChildAction>),
}

#[test]
fn generated_paths_embed_the_same_keyed_closure_workflow() {
    block_on(async {
        let reducer = scope(
            AppStatePaths::rows(),
            AppActionPaths::rows(),
            keyed_reducer(),
        );
        let state = AppState {
            rows: [(7, ChildState { value: 2 })].into_iter().collect(),
        };
        let mut store = TestStore::projected(state, reducer, |state| snapshot(&state.rows));
        let action = AppActionPaths::rows().embed(KeyedAction::new(7, ChildAction::Add(3)));
        let _ = store.send_with(action, |rows| rows[0].1 = 6).await.unwrap();
        let _ = store
            .receive(|action| {
                let AppAction::Rows(action) = action;
                *action.id() == 7 && matches!(action.action(), ChildAction::Completed(6))
            })
            .await
            .unwrap();
        store.finish().await.unwrap();
    });
}
