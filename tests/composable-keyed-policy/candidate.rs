//! Runs the shared policy workload through RCA's native keyed composition.

#[path = "common.rs"]
mod common;

use common::{ChildAction, ChildData, Input, Mode, INPUTS};
use rca_core::{
    for_each, Effect, KeyedAction, KeyedActionPath, KeyedState, KeyedStatePath, ReduceContext,
    Reducer, Store,
};
use rca_tokio::TokioRuntime;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::Duration;

/// Routes child work or replaces a child through the parent alone.
enum Action {
    Child(KeyedAction<u8, ChildAction>),
    Replace(u8),
}

/// Preserves the originating key except in the deliberately wrong-key control.
fn embed(child: KeyedAction<u8, ChildAction>) -> Action {
    let (mut id, action) = child.into_parts();
    if matches!(
        action,
        ChildAction::Completed {
            wrong_key: true,
            ..
        }
    ) {
        id = 3 - id;
    }
    Action::Child(KeyedAction::new(id, action))
}

/// Owns independent child state and the parent observations used by the oracle.
struct State {
    children: KeyedState<u8, ChildData>,
    trace: Vec<(u8, u64)>,
    reductions: usize,
}

/// Records completed effects without inspecting child state or reducer order.
struct Parent {
    mode: Mode,
    completed: Arc<AtomicU64>,
}

impl Reducer for Parent {
    type State = State;
    type Action = Action;
    fn reduce(
        &mut self,
        state: &mut State,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        state.reductions += 1;
        match action {
            Action::Child(child) => {
                if let ChildAction::Completed { value, .. } = child.action() {
                    state.trace.push((*child.id(), *value));
                    self.completed.fetch_add(1, Ordering::SeqCst);
                }
            }
            Action::Replace(id) => {
                let old = state.children.remove(id).unwrap();
                let step = if self.mode == Mode::RetainPolicy {
                    old.step
                } else {
                    3
                };
                state.children.insert(*id, ChildData::new(7, step));
            }
        }
        Effect::none()
    }
}

/// Shares behaviour, but uses reducer-owned policy only for the failing control.
struct Child {
    mode: Mode,
    shared_step: u64,
}

impl Reducer for Child {
    type State = ChildData;
    type Action = ChildAction;
    fn reduce(
        &mut self,
        state: &mut ChildData,
        action: &ChildAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<ChildAction> {
        match *action {
            ChildAction::SetStep(step) => {
                state.step = step;
                self.shared_step = step;
            }
            ChildAction::Step => {
                let value = state.next;
                state.next += if self.mode == Mode::SharedPolicy {
                    self.shared_step
                } else {
                    state.step
                };
                return Effect::send(ChildAction::Completed {
                    value,
                    wrong_key: self.mode == Mode::WrongKey,
                });
            }
            ChildAction::Completed { value, .. } => state.completed.push(value),
        }
        Effect::none()
    }
}

/// Completes every effect, extracts state, stops the owned runtime, then checks it.
fn exercise(mode: Mode) -> Result<(), &'static str> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let result = runtime.block_on(async {
        let completed = Arc::new(AtomicU64::new(0));
        let reducer = for_each(
            Parent {
                mode,
                completed: Arc::clone(&completed),
            },
            KeyedStatePath::new(|s: &State| &s.children, |s: &mut State| &mut s.children),
            KeyedActionPath::new(
                |action: &Action| match action {
                    Action::Child(child) => Some(child),
                    Action::Replace(_) => None,
                },
                embed,
            ),
            Child {
                mode,
                shared_step: 2,
            },
        );
        let children = [(1, ChildData::new(10, 2)), (2, ChildData::new(100, 10))]
            .into_iter()
            .collect();
        let store = Store::new(
            State {
                children,
                trace: Vec::new(),
                reductions: 0,
            },
            reducer,
            TokioRuntime::current().unwrap(),
        );
        let operations = async {
            for (input, expected) in INPUTS.iter().zip(common::COMPLETED) {
                let action = match *input {
                    Input::SetStep(id, step) => {
                        Action::Child(KeyedAction::new(id, ChildAction::SetStep(step)))
                    }
                    Input::Step(id) => Action::Child(KeyedAction::new(id, ChildAction::Step)),
                    Input::Replace(id) => Action::Replace(id),
                };
                // A committed root alone does not prove its child effect completed.
                store
                    .send(action)
                    .await
                    .map_err(|_| "send failed")?
                    .finish()
                    .await;
                if completed.load(Ordering::SeqCst) != expected {
                    return Err("incomplete operation");
                }
            }
            Ok::<_, &'static str>(())
        };
        let completion = tokio::time::timeout(Duration::from_secs(3), operations).await;
        let state = tokio::time::timeout(Duration::from_secs(3), store.shutdown_into_state())
            .await
            .map_err(|_| "shutdown timed out")?
            .map_err(|_| "extraction failed")?;
        completion.map_err(|_| "operations timed out")??;
        Ok::<_, &'static str>(state)
    });
    drop(runtime);
    let state = result?;
    let rows = state
        .children
        .into_iter()
        .map(|(id, child)| (id, child.next, child.step, child.completed))
        .collect();
    common::verify(&state.trace, rows, state.reductions)
}

include!("tests.rs");
