#![deny(warnings)]

use rca_core::{
    EntryMount, Feature, FeatureContext, InitialPolicy, ReactionChange, RootMount, Task,
};
use std::{cell::Cell, rc::Rc};
struct State(Cell<i32>);
struct Action(Cell<i32>);
struct FeatureValue(Cell<i32>);
impl Feature for FeatureValue {
    type State = State;
    type Action = Action;
    fn reduce(
        &mut self,
        state: &mut State,
        action: &Action,
        _: &mut FeatureContext<'_, State, Action>,
    ) {
        self.0.set(self.0.get() + 1);
        state.0.set(state.0.get() + action.0.get());
    }
}
#[derive(PartialEq)]
struct Projection(Rc<i32>);
fn main() {
    let capture = Cell::new(0);
    let root = RootMount::new(FeatureValue(Cell::new(0))).on_change(
        move |state| {
            capture.set(capture.get() + 1);
            Projection(Rc::new(state.0.get()))
        },
        InitialPolicy::Skip,
        |change: ReactionChange<Projection>, _| {
            drop(change);
            Task::run(|_| async {})
        },
    );
    let entry = EntryMount::new(FeatureValue(Cell::new(0))).on_change(
        |state| Projection(Rc::new(state.0.get())),
        InitialPolicy::Run,
        |change: ReactionChange<Projection>, _| {
            drop(change);
            Task::run(|_| async {})
        },
    );
    fn send<T: Send>(_: T) {}
    send(root);
    send(entry);
}
