use rca_core::{
    Effect, ForEachShared, KeyedAction, KeyedActionPath, KeyedState, ReduceContext, Reducer,
    SharedReducer, for_each_shared,
};
use std::cell::Cell;
struct Private(Cell<u64>);
struct Settings(Cell<u64>);
struct ChildAction(String);
struct State {
    rows: KeyedState<u8, Private>,
    settings: Settings,
}
struct Child;
impl SharedReducer for Child {
    type State = Private;
    type Shared = Settings;
    type Action = ChildAction;
    fn reduce(
        &mut self,
        state: &mut Private,
        shared: &mut Settings,
        action: &ChildAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<ChildAction> {
        state.0.set(action.0.len() as u64);
        shared.0.set(state.0.get());
        Effect::none()
    }
}
struct Parent;
impl Reducer for Parent {
    type State = State;
    type Action = KeyedAction<u8, ChildAction>;
    fn reduce(
        &mut self,
        _: &mut State,
        _: &Self::Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        Effect::none()
    }
}
fn assert_reducer<R: Reducer<State = State, Action = KeyedAction<u8, ChildAction>>>(_: &R) {}
fn assert_send_sync<T: Send + Sync>(_: &T) {}
fn assert_debug<T: std::fmt::Debug>(_: &T) {}
fn main() {
    let reducer = ForEachShared::new(
        Parent,
        |s: &State| &s.rows,
        |s: &mut State| (&mut s.rows, &mut s.settings),
        KeyedActionPath::new(|a: &KeyedAction<u8, ChildAction>| Some(a), |a| a),
        Child,
    );
    assert_reducer(&reducer);
    assert_send_sync(&reducer);
    assert_debug(&reducer);
    let calls = Cell::new(0);
    let get_calls = Cell::new(0);
    let reducer = for_each_shared(
        Parent,
        move |s: &State| {
            get_calls.set(get_calls.get() + 1);
            &s.rows
        },
        move |s: &mut State| {
            calls.set(calls.get() + 1);
            (&mut s.rows, &mut s.settings)
        },
        KeyedActionPath::new(|a: &KeyedAction<u8, ChildAction>| Some(a), |a| a),
        Child,
    );
    assert_reducer(&reducer); // Owned Cell captures do not require Sync.
}
