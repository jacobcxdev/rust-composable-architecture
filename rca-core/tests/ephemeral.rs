//! Checks parent-only ephemeral reduction with ordinary data payloads.

#[path = "support/thread_runtime.rs"]
mod thread_runtime;

use rca_core::{
    ActionPath, Effect, PresentationAction, ReduceContext, Reducer, StatePath, Store, ephemeral,
};
use thread_runtime::ThreadRuntime;

/// A plain data payload deliberately implements neither `Reducer`, `Clone` nor `Debug`.
struct Data {
    id: u8,
    value: u8,
}

/// Records parent observations and an optional replacement during the same action.
struct State {
    data: Option<Data>,
    seen: Vec<u8>,
    replacement: Option<u8>,
    dismissals: usize,
}

/// Separates predicate clearing from keeping data and effect-driven dismissal.
enum DataAction {
    Close,
    Keep,
    RequestDismiss,
    RequestNamedDismiss,
}

/// Distinguishes routed presentation actions from unrelated parent input.
enum Action {
    Data(PresentationAction<DataAction>),
    Unmatched,
}

/// Reads data before optional replacement and records explicit dismissal delivery.
struct Parent;

impl Reducer for Parent {
    type State = State;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut State,
        action: &Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        state
            .seen
            .push(state.data.as_ref().map_or(99, |data| data.value));
        if let Some(id) = state.replacement.take() {
            state.data = Some(Data { id, value: 42 });
        }
        match action {
            Action::Data(PresentationAction::Presented(DataAction::RequestDismiss)) => {
                return context.dismiss().effect();
            }
            Action::Data(PresentationAction::Presented(DataAction::RequestNamedDismiss)) => {
                return context
                    .dismiss()
                    .effect()
                    .named("close", rca_core::OperationPolicy::Coexist);
            }
            Action::Data(PresentationAction::Dismiss) => state.dismissals += 1,
            _ => {}
        }
        Effect::none()
    }
}

/// Composes the parent with data identity and a single clearing predicate.
fn reducer() -> impl Reducer<State = State, Action = Action> + Send + std::fmt::Debug {
    ephemeral(
        Parent,
        StatePath::new(
            |state: &State| &state.data,
            |state: &mut State| &mut state.data,
        ),
        ActionPath::new(
            |action: &Action| match action {
                Action::Data(action) => Some(action),
                Action::Unmatched => None,
            },
            Action::Data,
        ),
        |data: &Data| &data.id,
        |_: &Data, action: &DataAction| matches!(action, DataAction::Close),
    )
}

// Clearing before the parent, calling it twice, or routing all actions breaks this.
#[test]
fn parent_inspects_data_once_before_selected_or_explicit_dismissal() {
    for (action, cleared) in [
        (
            Action::Data(PresentationAction::Presented(DataAction::Close)),
            true,
        ),
        (Action::Data(PresentationAction::Dismiss), true),
        (
            Action::Data(PresentationAction::Presented(DataAction::Keep)),
            false,
        ),
        (Action::Unmatched, false),
    ] {
        let runtime = ThreadRuntime::new();
        let reducer = reducer();
        assert_eq!(format!("{reducer:?}"), "Ephemeral { .. }");
        let store = Store::new(
            State {
                data: Some(Data { id: 1, value: 7 }),
                seen: Vec::new(),
                replacement: None,
                dismissals: 0,
            },
            reducer,
            runtime.runtime(),
        );
        futures_executor::block_on(async {
            store.send(action).await.unwrap().finish().await;
            assert_eq!(
                store
                    .read(|state| (state.seen.clone(), state.data.is_none()))
                    .await
                    .unwrap(),
                (vec![7], cleared),
            );
            store.shutdown().await.unwrap();
        });
    }
}

// Unconditional clearing loses a genuinely different same-action replacement.
#[test]
fn different_same_action_replacement_survives_but_equal_identity_clears() {
    for dismiss in [false, true] {
        for replacement in [1, 2] {
            let runtime = ThreadRuntime::new();
            let store = Store::new(
                State {
                    data: Some(Data { id: 1, value: 7 }),
                    seen: Vec::new(),
                    replacement: Some(replacement),
                    dismissals: 0,
                },
                reducer(),
                runtime.runtime(),
            );
            futures_executor::block_on(async {
                let action = if dismiss {
                    PresentationAction::Dismiss
                } else {
                    PresentationAction::Presented(DataAction::Close)
                };
                store
                    .send(Action::Data(action))
                    .await
                    .unwrap()
                    .finish()
                    .await;
                let expected = if replacement == 1 {
                    None
                } else {
                    Some((2, 42))
                };
                assert_eq!(
                    store
                        .read(|state| {
                            (
                                state.seen.clone(),
                                state.data.as_ref().map(|data| (data.id, data.value)),
                            )
                        })
                        .await
                        .unwrap(),
                    (vec![7], expected),
                );
                store.shutdown().await.unwrap();
            });
        }
    }
}

#[test]
fn parent_dismiss_effect_routes_once_and_clears_after_parent_observes_data() {
    for action in [DataAction::RequestDismiss, DataAction::RequestNamedDismiss] {
        let runtime = ThreadRuntime::new();
        let store = Store::new(
            State {
                data: Some(Data { id: 1, value: 7 }),
                seen: Vec::new(),
                replacement: None,
                dismissals: 0,
            },
            reducer(),
            runtime.runtime(),
        );
        futures_executor::block_on(async {
            let task = store
                .send(Action::Data(PresentationAction::Presented(action)))
                .await
                .unwrap();
            task.finish().await;
            assert_eq!(
                store
                    .read(|state| (state.seen.clone(), state.data.is_none(), state.dismissals))
                    .await
                    .unwrap(),
                (vec![7, 7], true, 1),
            );
            store.shutdown().await.unwrap();
        });
    }
}

#[cfg(feature = "test-util")]
#[test]
fn absent_presented_action_reports_once_and_still_runs_parent() {
    let runtime = ThreadRuntime::new();
    let collector = rca_core::IssueCollector::new();
    let reporter = collector.reporter();
    let store = Store::builder(
        State {
            data: None,
            seen: Vec::new(),
            replacement: None,
            dismissals: 0,
        },
        reducer(),
        runtime.runtime(),
    )
    .with_issue_handler(move |issue| reporter.report(issue.clone()).unwrap())
    .build()
    .unwrap();
    futures_executor::block_on(async {
        store
            .send(Action::Data(PresentationAction::Presented(
                DataAction::Keep,
            )))
            .await
            .unwrap()
            .finish()
            .await;
        assert_eq!(
            store
                .read(|state| (state.seen.clone(), state.data.is_none(), state.dismissals))
                .await
                .unwrap(),
            (vec![99], true, 0),
        );
        store.shutdown().await.unwrap();
    });
    let issues = collector.finish();
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].code, "rca.missing_child");
}
