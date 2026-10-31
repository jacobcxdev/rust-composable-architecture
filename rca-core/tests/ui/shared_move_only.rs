use rca_core::{
    ActionPath, Effect, ReduceContext, Reducer, SharedReducer, SharedScope, StatePath, scope,
    scope_shared,
};
use std::cell::Cell;

struct Private(Cell<usize>);
struct Settings(Cell<usize>);
struct Action(String);
struct State {
    private: Private,
    settings: Settings,
}
struct Child;
impl SharedReducer for Child {
    type State = Private;
    type Shared = Settings;
    type Action = Action;
    fn reduce(
        &mut self,
        state: &mut Private,
        shared: &mut Settings,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        state.0.set(action.0.len());
        shared.0.set(state.0.get());
        Effect::none()
    }
}
fn assert_reducer<R: Reducer<State = State, Action = Action>>(_: &R) {}
fn assert_send_sync<T: Send + Sync>(_: &T) {}
fn assert_debug<T: std::fmt::Debug>(_: &T) {}
fn main() {
    let child = SharedScope::new(
        |s: &mut State| (&mut s.private, &mut s.settings),
        ActionPath::new(|a: &Action| Some(a), |a| a),
        Child,
    );
    assert_reducer(&child);
    assert_send_sync(&child); // Domain !Sync does not infect a non-owning marker.
    assert_debug(&child); // No Debug bound on child, split, state or settings.
    struct Outer(State);
    let outer = scope(
        StatePath::new(|s: &Outer| &s.0, |s: &mut Outer| &mut s.0),
        ActionPath::new(|a: &Action| Some(a), |a| a),
        child,
    );
    fn assert_outer<R: Reducer<State = Outer, Action = Action>>(_: R) {}
    assert_outer(outer);
    let calls = Cell::new(0);
    let child = scope_shared(
        move |s: &mut State| {
            calls.set(calls.get() + 1);
            (&mut s.private, &mut s.settings)
        },
        ActionPath::new(|a: &Action| Some(a), |a| a),
        Child,
    );
    assert_reducer(&child); // Stored split need only Send, not Sync.
}
