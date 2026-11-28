//! Checks composed effect order and scoped actions without cloning their owned payloads.

use rca_core::{ActionPath, Effect, ReduceContext, Reducer, StatePath, combine, scope};
use rca_test::TestStore;

enum Action {
    Start(Vec<u8>),
    Output(u8),
}

/// Emits a distinct value, optionally wrapped in nested merges with empty effects.
struct Send<const OUTPUT: u8> {
    nested: bool,
}

impl<const OUTPUT: u8> Reducer for Send<OUTPUT> {
    type State = ();
    type Action = Action;

    fn reduce(
        &mut self,
        _state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        let Action::Start(bytes) = action else {
            return Effect::none();
        };
        assert_eq!(bytes, &[9]);
        let output = Effect::send(Action::Output(OUTPUT));
        if self.nested {
            Effect::merge([
                Effect::none(),
                Effect::merge([Effect::none(), output, Effect::none()]),
                Effect::none(),
            ])
        } else {
            output
        }
    }
}

struct Empty;

impl Reducer for Empty {
    type State = ();
    type Action = Action;

    fn reduce(
        &mut self,
        _state: &mut Self::State,
        _action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        Effect::none()
    }
}

#[test]
fn combine_one_child_effect_reaches_runtime() {
    futures_executor::block_on(async {
        let reducer = combine((Send::<1> { nested: false },));
        let mut store = TestStore::new((), reducer);

        let _ = store
            .send(Action::Start(vec![9]))
            .await
            .expect("start commit");
        let _ = store
            .receive(|action| matches!(action, Action::Output(1)))
            .await
            .expect("first output commit");
        store.finish().await.expect("clean finish");
    });
}

#[test]
fn combine_mixed_none_effects_reach_runtime_in_source_order() {
    futures_executor::block_on(async {
        let reducer = combine((
            Send::<1> { nested: false },
            Empty,
            Send::<2> { nested: false },
        ));
        let mut store = TestStore::new((), reducer);

        let _ = store
            .send(Action::Start(vec![9]))
            .await
            .expect("start commit");
        let _ = store
            .receive(|action| matches!(action, Action::Output(1)))
            .await
            .expect("first output commit");
        let _ = store
            .receive(|action| matches!(action, Action::Output(2)))
            .await
            .expect("second output commit");
        store.finish().await.expect("clean finish");
    });
}

#[test]
fn combine_nested_merges_reach_runtime_in_source_order() {
    futures_executor::block_on(async {
        let reducer = combine((Send::<1> { nested: true }, Send::<2> { nested: true }));
        let mut store = TestStore::new((), reducer);

        let _ = store
            .send(Action::Start(vec![9]))
            .await
            .expect("start commit");
        let _ = store
            .receive(|action| matches!(action, Action::Output(1)))
            .await
            .expect("first nested output commit");
        let _ = store
            .receive(|action| matches!(action, Action::Output(2)))
            .await
            .expect("second nested output commit");
        store.finish().await.expect("clean finish");
    });
}

/// Records reducer order and payload addresses without owning the effect's allocation.
struct ChildState {
    calls: Vec<&'static str>,
    effect_pointer: Option<usize>,
    received_pointer: Option<usize>,
}

/// Keeps parent and child call histories separate for ordering assertions.
struct AppState {
    child: ChildState,
    calls: Vec<&'static str>,
}

/// Compares stable observations instead of embedding allocation addresses in expected state.
#[derive(Clone, Debug, PartialEq)]
struct AppSnapshot {
    child_calls: Vec<&'static str>,
    calls: Vec<&'static str>,
    effect_allocated: bool,
    received_original_allocation: Option<bool>,
}

/// Projects call history and allocation preservation without copying the action payload.
fn app_snapshot(state: &AppState) -> AppSnapshot {
    AppSnapshot {
        child_calls: state.child.calls.clone(),
        calls: state.calls.clone(),
        effect_allocated: state.child.effect_pointer.is_some(),
        received_original_allocation: state
            .child
            .received_pointer
            .map(|pointer| Some(pointer) == state.child.effect_pointer),
    }
}

enum ChildAction {
    Load,
    Loaded(Box<[u8]>),
}

enum AppAction {
    Child(ChildAction),
}

struct Child;

impl Reducer for Child {
    type State = ChildState;
    type Action = ChildAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            ChildAction::Load => {
                state.calls.push("child load");
                let bytes: Box<[u8]> = Box::from([1_u8]);
                state.effect_pointer = Some(bytes.as_ptr() as usize);
                Effect::send(ChildAction::Loaded(bytes))
            }
            ChildAction::Loaded(bytes) => {
                state.calls.push("child loaded");
                state.received_pointer = Some(bytes.as_ptr() as usize);
                Effect::none()
            }
        }
    }
}

struct Parent;

impl Reducer for Parent {
    type State = AppState;
    type Action = AppAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            AppAction::Child(ChildAction::Load) => {
                assert_eq!(state.child.calls, ["child load"]);
                state.calls.push("parent load");
            }
            AppAction::Child(ChildAction::Loaded(bytes)) => {
                assert_eq!(state.child.calls, ["child load", "child loaded"]);
                assert_eq!(state.child.received_pointer, Some(bytes.as_ptr() as usize));
                state.calls.push("parent loaded");
            }
        }
        Effect::none()
    }
}

#[test]
fn scope_runs_child_before_parent_and_maps_owned_effect() {
    futures_executor::block_on(async {
        let child_state = StatePath::new(
            |state: &AppState| &state.child,
            |state: &mut AppState| &mut state.child,
        );
        let child_action = ActionPath::new(
            |action: &AppAction| match action {
                AppAction::Child(action) => Some(action),
            },
            AppAction::Child,
        );
        let reducer = combine((scope(child_state, child_action, Child), Parent));
        let mut store = TestStore::projected(
            AppState {
                child: ChildState {
                    calls: vec![],
                    effect_pointer: None,
                    received_pointer: None,
                },
                calls: vec![],
            },
            reducer,
            app_snapshot,
        );

        let _ = store
            .send_with(AppAction::Child(ChildAction::Load), |state| {
                state.child_calls.push("child load");
                state.calls.push("parent load");
                state.effect_allocated = true;
            })
            .await
            .unwrap();
        store
            .assert_state(|state| {
                assert_eq!(state.calls, ["parent load"]);
                assert_eq!(state.child.calls, ["child load"]);
            })
            .await
            .unwrap();
        let _ = store
            .receive_with(
                |action| {
                    matches!(
                        action,
                        AppAction::Child(ChildAction::Loaded(bytes)) if bytes.as_ref() == [1]
                    )
                },
                |state| {
                    state.child_calls.push("child loaded");
                    state.calls.push("parent loaded");
                    state.received_original_allocation = Some(true);
                },
            )
            .await
            .unwrap();
        store
            .assert_state(|state| {
                assert_eq!(state.calls, ["parent load", "parent loaded"]);
                assert_eq!(state.child.calls, ["child load", "child loaded"]);
                assert_eq!(state.child.effect_pointer, state.child.received_pointer);
            })
            .await
            .unwrap();
        store.finish().await.unwrap();
    });
}
