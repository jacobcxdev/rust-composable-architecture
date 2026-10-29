//! Runs the same policy workload through the pinned historical keyed reducer.

#[path = "common.rs"]
mod common;

use common::{ChildAction, ChildData, Input, Mode, INPUTS};
use composable::{Effects, Keyed, KeyedState, RecursiveReducer, Reducer, Store};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

/// Owns state inside each reducer, as required by the historical engine.
struct Child {
    data: ChildData,
    mode: Mode,
    shared_step: Arc<AtomicU64>,
}

impl Reducer for Child {
    type Action = ChildAction;
    type Output = Self;
    fn reduce(&mut self, action: ChildAction, effects: impl Effects<ChildAction>) {
        match action {
            ChildAction::SetStep(step) => {
                self.data.step = step;
                self.shared_step.store(step, Ordering::Relaxed);
            }
            ChildAction::Step => {
                let value = self.data.next;
                self.data.next += if self.mode == Mode::SharedPolicy {
                    self.shared_step.load(Ordering::Relaxed)
                } else {
                    self.data.step
                };
                effects.action(ChildAction::Completed {
                    value,
                    wrong_key: self.mode == Mode::WrongKey,
                });
            }
            ChildAction::Completed { value, .. } => self.data.completed.push(value),
        }
    }
}

/// Routes child work or replaces a child without child reduction.
#[derive(Clone)]
enum Action {
    Child(Keyed<u8, ChildAction>),
    Replace(u8),
}

impl From<Keyed<u8, ChildAction>> for Action {
    fn from(mut value: Keyed<u8, ChildAction>) -> Self {
        if matches!(
            value.action,
            ChildAction::Completed {
                wrong_key: true,
                ..
            }
        ) {
            value.key = 3 - value.key;
        }
        Self::Child(value)
    }
}

impl TryFrom<Action> for Keyed<u8, ChildAction> {
    type Error = ();
    fn try_from(value: Action) -> Result<Self, Self::Error> {
        match value {
            Action::Child(child) => Ok(child),
            Action::Replace(_) => Err(()),
        }
    }
}

/// Holds native child reducers and observations that do not depend on their order.
#[derive(RecursiveReducer)]
struct Parent {
    children: KeyedState<u8, Child>,
    #[reducer(skip)]
    trace: Vec<(u8, u64)>,
    #[reducer(skip)]
    reductions: usize,
    #[reducer(skip)]
    completed: Arc<AtomicU64>,
}

impl RecursiveReducer for Parent {
    type Action = Action;
    fn reduce(&mut self, action: Action, _: impl Effects<Action>) {
        self.reductions += 1;
        match action {
            Action::Child(Keyed {
                key,
                action: ChildAction::Completed { value, .. },
            }) => {
                self.trace.push((key, value));
                self.completed.fetch_add(1, Ordering::SeqCst);
            }
            Action::Replace(id) => {
                let old = self.children.remove(&id).unwrap();
                let step = if old.mode == Mode::RetainPolicy {
                    old.data.step
                } else {
                    3
                };
                self.children.insert(
                    id,
                    Child {
                        data: ChildData::new(7, step),
                        mode: old.mode,
                        shared_step: old.shared_step,
                    },
                );
            }
            _ => {}
        }
    }
}

/// Drains native follow-up actions, joins the owned worker, then checks final state.
fn exercise(mode: Mode) -> Result<(), &'static str> {
    let shared_step = Arc::new(AtomicU64::new(2));
    let mut children: KeyedState<u8, Child> = KeyedState::default();
    for (id, next, step) in [(1, 10, 2), (2, 100, 10)] {
        children.insert(
            id,
            Child {
                data: ChildData::new(next, step),
                mode,
                shared_step: Arc::clone(&shared_step),
            },
        );
    }
    let completed = Arc::new(AtomicU64::new(0));
    let store = Store::with_initial(Parent {
        children,
        trace: Vec::new(),
        reductions: 0,
        completed: Arc::clone(&completed),
    });
    let mut boundary_error = None;
    for (input, expected) in INPUTS.iter().zip(common::COMPLETED) {
        let action = match *input {
            Input::SetStep(id, step) => Action::Child(Keyed::new(id, ChildAction::SetStep(step))),
            Input::Step(id) => Action::Child(Keyed::new(id, ChildAction::Step)),
            Input::Replace(id) => Action::Replace(id),
        };
        // Native sync drains these synchronous follow-up actions before returning.
        store.sync(action);
        if completed.load(Ordering::SeqCst) != expected {
            boundary_error = Some("incomplete operation");
            break;
        }
    }
    let parent = store.into_inner(); // joins the owned LocalPool worker
    if let Some(error) = boundary_error {
        return Err(error);
    }
    let rows = parent
        .children
        .iter()
        .map(|(id, child)| {
            (
                *id,
                child.data.next,
                child.data.step,
                child.data.completed.clone(),
            )
        })
        .collect();
    common::verify(&parent.trace, rows, parent.reductions)
}

include!("tests.rs");
