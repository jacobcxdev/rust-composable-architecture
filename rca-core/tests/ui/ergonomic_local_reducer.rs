use std::{cell::Cell, rc::Rc};

use rca_core::{
    Effect, KeyedAction, KeyedState, Reduce, ReduceContext, Reducer, identity_action_path,
    identity_keyed_action_path, identity_keyed_state_path, identity_state_path,
};

struct Local<'a>(&'a Cell<usize>, Rc<()>);

fn assert_send_sync(_: impl Send + Sync) {}
fn assert_copy(_: impl Copy) {}

fn reduce_locally<'a>(value: &'a Cell<usize>, context: &mut ReduceContext<'_>) {
    let capture = Local(value, Rc::new(()));
    let mut calls = 0;
    let mut reducer = Reduce::<Local<'a>, Local<'a>, _>::new(move |state, action, context| {
        calls += 1;
        state.0.set(action.0.get() + capture.0.get() + calls);
        let _ = (&capture.1, context.now());
        Effect::none()
    });
    let mut state = Local(value, Rc::new(()));
    let action = Local(value, Rc::new(()));
    let _ = reducer.reduce(&mut state, &action, context);
    let _ = format!("{reducer:?}");
}

fn main() {
    let value = Cell::new(3);
    // Ordinary typed-closure construction needs no domain or carrier turbofish.
    let inferred =
        Reduce::new(|_state: &mut Local<'_>, _action: &Local<'_>, _context| Effect::none());
    let _: &dyn Reducer<State = Local<'_>, Action = Local<'_>> = &inferred;
    let path = identity_state_path::<Local<'_>>();
    assert_copy(path);
    let mut state = Local(&value, Rc::new(()));
    assert!(std::ptr::eq(path.get(&state), &state));
    path.get_mut(&mut state).0.set(5);
    let action_path = identity_action_path::<Local<'_>>();
    assert_copy(action_path);
    let action = action_path.embed(state);
    assert!(std::ptr::eq(action_path.extract(&action).unwrap(), &action));
    let keyed_state = identity_keyed_state_path::<Local<'_>, Local<'_>>();
    assert_copy(keyed_state);
    let mut rows = KeyedState::new();
    assert!(std::ptr::eq(keyed_state.get_mut(&mut rows), &rows));
    let keyed_action = identity_keyed_action_path::<Local<'_>, Local<'_>>();
    assert_copy(keyed_action);
    let embedded = keyed_action.embed(Local(&value, Rc::new(())), action);
    let _: &Local<'_> = keyed_action.extract(&embedded).unwrap().1;
    let _: KeyedAction<Local<'_>, Local<'_>> = embedded;
    // Domain auto traits must not infect a callback that owns no domain values.
    assert_send_sync(Reduce::<Local<'_>, Local<'_>, _>::new(|_, _, _| {
        Effect::none()
    }));
    assert_send_sync(path);
    assert_send_sync(action_path);
    assert_send_sync(keyed_state);
    assert_send_sync(keyed_action);
    let _ = reduce_locally;
}
