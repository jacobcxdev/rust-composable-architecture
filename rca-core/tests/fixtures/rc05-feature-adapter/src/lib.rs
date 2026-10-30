//! Checks store constructors with move-only, non-`Sync` state and actions.
//!
//! This compiler fixture deliberately omits extra traits that could hide stricter bounds.

use rca::{Effect, ReduceContext, Reducer, Runtime, Store};
use std::cell::Cell;

/// Mutable state that is `Send` but not `Sync`, with no `Clone` implementation.
pub struct State(Cell<usize>);
/// A move-only action whose interior cell prevents `Sync`.
pub struct Action(Cell<usize>);
/// Adds an action's value without requiring shared access to its interior cell.
pub struct Counter;

impl Reducer for Counter {
    type State = State;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut State,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        state.0.set(state.0.get() + action.0.get());
        Effect::none()
    }
}

/// Checks the ordinary constructor with the fixture's restricted state and action types.
pub fn ordinary(runtime: Runtime) -> Store<State, Action> {
    Store::new(State(Cell::new(0)), Counter, runtime)
}

/// Checks the builder with the same types and a valid bounded mailbox.
pub fn configured(runtime: Runtime) -> Store<State, Action> {
    Store::builder(State(Cell::new(0)), Counter, runtime)
        .mailbox_capacity(1)
        .build()
        .unwrap()
}
