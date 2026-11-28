//! Checks error-route suppression when a child disappears during its final operation poll.

#[expect(dead_code, reason = "scope tests manually poll runtime tasks")]
mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Waker};

use rca_core::{
    ActionPath, CaseStatePath, Effect, EffectError, IssueReporter, KeyedAction, KeyedActionPath,
    KeyedState, KeyedStatePath, OptionalStatePath, ReduceContext, Reducer, RuntimeFuture, Store,
    for_each, if_case, if_some,
};

/// Provides a removable enum payload for the case-composition variant.
enum Case {
    Present(()),
    Absent,
}

/// Holds the optional, case and keyed child shapes tested by the same schedule.
struct State {
    optional: Option<()>,
    case: Case,
    rows: KeyedState<u8, ()>,
}

/// Starts one child operation or removes all child shapes through fresh root input.
enum Action {
    Child(u8),
    Row(KeyedAction<u8, u8>),
    Remove,
}

/// Removes children without producing another effect.
struct Parent;

impl Reducer for Parent {
    type State = State;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut State,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        if matches!(action, Action::Remove) {
            state.optional = None;
            state.case = Case::Absent;
            state.rows.remove(&1);
        }
        Effect::none()
    }
}

/// Transfers one prepared fallible operation into the selected child scope.
struct Child(Option<Effect<u8>>);

impl Reducer for Child {
    type State = ();
    type Action = u8;

    fn reduce(&mut self, (): &mut (), _: &u8, _: &mut ReduceContext<'_>) -> Effect<u8> {
        self.0.take().expect("one child operation")
    }
}

/// Removes the child inside its failing poll, then checks fresh root authority.
///
/// Error callbacks must not run after removal, including handler construction.
fn removed_during_error_poll<R: Reducer<State = State, Action = Action> + Send + 'static>(
    make_reducer: impl FnOnce(Child) -> R,
    start: Action,
    handled: bool,
) {
    let runtime = support::ManualRuntime::new();
    let actor = Arc::new(Mutex::new(None::<RuntimeFuture>));
    let store = Arc::new(Mutex::new(None::<Store<State, Action>>));
    let operations = Arc::new(AtomicUsize::new(0));
    let operation_count = Arc::clone(&operations);
    let operation_actor = Arc::clone(&actor);
    let operation_store = Arc::clone(&store);
    let operation = move |_| async move {
        operation_count.fetch_add(1, Ordering::SeqCst);
        let receipt = operation_store
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .try_send(Action::Remove)
            .ok()
            .unwrap();
        assert!(
            operation_actor
                .lock()
                .unwrap()
                .as_mut()
                .unwrap()
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        // The root External removal action is fresh input, not owned child output.
        drop(futures_executor::block_on(receipt.finish()).unwrap());
        Err(EffectError::Failed(7u8))
    };
    let effect = if handled {
        Effect::try_run(operation, |_, _| -> std::future::Ready<()> {
            panic!("removed child invoked handler")
        })
    } else {
        Effect::try_run_reported(
            operation,
            IssueReporter::new(|_| panic!("removed child reported")),
            |_| panic!("removed child made issue"),
            |_| panic!("removed child delivered outcome"),
        )
    };
    let mut rows = KeyedState::new();
    rows.insert(1, ());
    let root = Store::new(
        State {
            optional: Some(()),
            case: Case::Present(()),
            rows,
        },
        make_reducer(Child(Some(effect))),
        runtime.runtime(),
    );
    let mut root_actor = runtime.take_only_queued_task();
    let receipt = root.try_send(start).ok().unwrap();
    assert!(
        root_actor
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    let task = futures_executor::block_on(receipt.finish()).unwrap();
    *actor.lock().unwrap() = Some(root_actor);
    *store.lock().unwrap() = Some(root.clone());
    let mut run = runtime.take_only_queued_task();
    assert!(
        run.as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_ready()
    );
    assert_eq!(operations.load(Ordering::SeqCst), 1);
    // Fresh root authority remains usable after the child's effect is invalidated.
    let receipt = root.try_send(Action::Remove).ok().unwrap();
    assert!(
        actor
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    drop(futures_executor::block_on(receipt.finish()).unwrap());
    assert!(task.is_finished());
}

#[test]
fn optional_removal_inside_error_poll_suppresses_both_error_routes() {
    for handled in [false, true] {
        removed_during_error_poll(
            |child| {
                if_some(
                    Parent,
                    OptionalStatePath::new(
                        |s: &State| s.optional.as_ref(),
                        |s: &mut State| s.optional.as_mut(),
                    ),
                    ActionPath::new(
                        |a: &Action| {
                            if let Action::Child(value) = a {
                                Some(value)
                            } else {
                                None
                            }
                        },
                        Action::Child,
                    ),
                    child,
                )
            },
            Action::Child(0),
            handled,
        );
    }
}

#[test]
fn case_removal_inside_error_poll_suppresses_both_error_routes() {
    for handled in [false, true] {
        removed_during_error_poll(
            |child| {
                if_case(
                    Parent,
                    CaseStatePath::new(
                        |s: &State| match &s.case {
                            Case::Present(value) => Some(value),
                            Case::Absent => None,
                        },
                        |s: &mut State| match &mut s.case {
                            Case::Present(value) => Some(value),
                            Case::Absent => None,
                        },
                    ),
                    ActionPath::new(
                        |a: &Action| {
                            if let Action::Child(value) = a {
                                Some(value)
                            } else {
                                None
                            }
                        },
                        Action::Child,
                    ),
                    child,
                )
            },
            Action::Child(0),
            handled,
        );
    }
}

#[test]
fn keyed_removal_inside_error_poll_suppresses_both_error_routes() {
    for handled in [false, true] {
        removed_during_error_poll(
            |child| {
                for_each(
                    Parent,
                    KeyedStatePath::new(|s: &State| &s.rows, |s: &mut State| &mut s.rows),
                    KeyedActionPath::new(
                        |a: &Action| {
                            if let Action::Row(value) = a {
                                Some(value)
                            } else {
                                None
                            }
                        },
                        Action::Row,
                    ),
                    child,
                )
            },
            Action::Row(KeyedAction::new(1, 0)),
            handled,
        );
    }
}
