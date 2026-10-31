#![deny(warnings)]

use rca_core::local::{EntryMount, Feature, FeatureContext, RootMount, Task};
use rca_core::{InitialPolicy, ReactionChange};
use std::{cell::Cell, rc::Rc};
struct State(Rc<Cell<i32>>);
struct FeatureValue(Rc<Cell<i32>>);
impl Feature for FeatureValue {
    type State = State;
    type Action = Rc<i32>;
    fn update(
        &mut self,
        state: &mut State,
        action: &Rc<i32>,
        _: &mut FeatureContext<'_, State, Rc<i32>>,
    ) {
        self.0.set(self.0.get() + 1);
        state.0.set(state.0.get() + **action);
    }
}
#[derive(PartialEq)]
struct Projection(Rc<i32>);
fn main() {
    let capture = Rc::new(Cell::new(0));
    let _root = RootMount::new(FeatureValue(capture.clone())).on_change(
        move |state| {
            capture.set(capture.get() + 1);
            Projection(Rc::new(state.0.get()))
        },
        InitialPolicy::Skip,
        |change: ReactionChange<Projection>, _| {
            Task::run(move |_| async move {
                drop(change);
            })
        },
    );
    let _entry = EntryMount::new(FeatureValue(Rc::new(Cell::new(0)))).on_change(
        |state| Projection(Rc::new(state.0.get())),
        InitialPolicy::Run,
        |change: ReactionChange<Projection>, _| {
            Task::run(move |_| async move {
                drop(change);
            })
        },
    );
}
