//! Presentation ordering, replacement and scoped dismissal through public store APIs.

#[path = "support/thread_runtime.rs"]
mod thread_runtime;

use rca_core::{
    ActionPath, CaseStatePath, Dismiss, Effect, KeyedAction, KeyedActionPath, KeyedState,
    KeyedStatePath, OptionalStatePath, PresentationAction, ReduceContext, Reducer, StatePath,
    Store, for_each, if_case, if_some, presentation,
};
use std::sync::{Arc, Mutex};
use thread_runtime::ThreadRuntime;

/// Move-only child state whose explicit ID distinguishes replacement from mutation.
struct ChildState {
    id: u8,
    count: usize,
}

/// Parent observations and an optional replacement to install during reduction.
struct State {
    child: Option<ChildState>,
    seen: Vec<usize>,
    replacement: Option<u8>,
}

/// Child work, including captured clients that must not outlive their original scope.
///
/// Domain state and actions deliberately implement neither `Clone` nor `Debug`.
enum ChildAction {
    Increment,
    Close,
    NamedClose,
    Capture(Arc<Mutex<Option<Dismiss>>>),
    Replay(Dismiss),
    CaptureOutput(std::sync::mpsc::Sender<rca_core::EffectSender<ChildAction>>),
}

/// Presented actions and parent-controlled changes to the child lifetime.
enum Action {
    Child(PresentationAction<ChildAction>),
    Replace(u8),
    Clear,
}

/// Runs child work and exports dismissal or output clients for lifetime checks.
struct Child;

impl Reducer for Child {
    type State = ChildState;
    type Action = ChildAction;

    fn reduce(
        &mut self,
        state: &mut ChildState,
        action: &ChildAction,
        context: &mut ReduceContext<'_>,
    ) -> Effect<ChildAction> {
        match action {
            ChildAction::Increment => {
                state.count += 1;
                Effect::none()
            }
            ChildAction::Close => {
                let dismiss = context.dismiss();
                assert_eq!(format!("{dismiss:?}"), "Dismiss { bound: true, .. }");
                let effect = dismiss.effect();
                assert!(format!("{effect:?}").starts_with("Effect::"));
                effect
            }
            ChildAction::NamedClose => context
                .dismiss()
                .effect()
                .named("close", rca_core::OperationPolicy::Coexist),
            ChildAction::Capture(slot) => {
                *slot.lock().unwrap() = Some(context.dismiss());
                Effect::none()
            }
            ChildAction::Replay(dismiss) => Effect::merge([
                Effect::none(),
                dismiss
                    .effect()
                    .delay(std::time::Duration::from_millis(1))
                    .cancellable_in_flight("dismiss")
                    .without_inherited_cancellation()
                    .named("replay", rca_core::OperationPolicy::Coexist),
            ]),
            ChildAction::CaptureOutput(captured) => {
                let captured = captured.clone();
                Effect::run(move |sender| async move {
                    captured.send(sender).unwrap();
                    std::future::pending::<()>().await;
                })
            }
        }
    }
}

/// Records the child value seen by the parent, then applies requested replacements.
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
        state
            .seen
            .push(state.child.as_ref().map_or(99, |child| child.count));
        if matches!(
            action,
            Action::Child(
                PresentationAction::Dismiss | PresentationAction::Presented(ChildAction::Increment)
            )
        ) && let Some(id) = state.replacement.take()
        {
            state.child = Some(ChildState { id, count: 42 });
        }
        match action {
            Action::Replace(id) => state.child = Some(ChildState { id: *id, count: 42 }),
            Action::Clear => state.child = None,
            Action::Child(_) => {}
        }
        Effect::none()
    }
}

/// Builds a presentation whose child remains installed after ordinary actions.
fn reducer() -> impl Reducer<State = State, Action = Action> + Send + std::fmt::Debug {
    reducer_with_ephemeral(false)
}

/// Optionally makes an increment dismiss the child after both reducers run.
fn reducer_with_ephemeral(
    ephemeral: bool,
) -> impl Reducer<State = State, Action = Action> + Send + std::fmt::Debug {
    presentation(
        Parent,
        StatePath::new(
            |state: &State| &state.child,
            |state: &mut State| &mut state.child,
        ),
        ActionPath::new(
            |action: &Action| match action {
                Action::Child(action) => Some(action),
                Action::Replace(_) | Action::Clear => None,
            },
            Action::Child,
        ),
        |child: &ChildState| &child.id,
        Child,
    )
    .ephemeral(move |_: &ChildState, action: &ChildAction| {
        ephemeral && matches!(action, ChildAction::Increment)
    })
}

#[test]
fn presented_child_reduces_before_parent_without_cloning_domain_values() {
    let runtime = ThreadRuntime::new();
    let reducer = reducer();
    assert_eq!(format!("{reducer:?}"), "Presentation { .. }");
    let store = Store::new(
        State {
            child: Some(ChildState { id: 1, count: 7 }),
            seen: Vec::new(),
            replacement: None,
        },
        reducer,
        runtime.runtime(),
    );
    futures_executor::block_on(async {
        drop(
            store
                .send(Action::Child(PresentationAction::Presented(
                    ChildAction::Increment,
                )))
                .await
                .unwrap(),
        );
        assert_eq!(store.read(|state| state.seen.clone()).await.unwrap(), [8]);
        store.shutdown().await.unwrap();
    });
}

#[test]
fn dismiss_parent_inspects_value_before_same_identity_is_cleared() {
    let runtime = ThreadRuntime::new();
    let store = Store::new(
        State {
            child: Some(ChildState { id: 1, count: 7 }),
            seen: Vec::new(),
            replacement: Some(1),
        },
        reducer(),
        runtime.runtime(),
    );
    futures_executor::block_on(async {
        drop(
            store
                .send(Action::Child(PresentationAction::Dismiss))
                .await
                .unwrap(),
        );
        assert_eq!(
            store
                .read(|state| (state.seen.clone(), state.child.is_none()))
                .await
                .unwrap(),
            (vec![7], true)
        );
        store.shutdown().await.unwrap();
    });
}

#[test]
fn dismiss_preserves_different_replacement_identity() {
    let runtime = ThreadRuntime::new();
    let store = Store::new(
        State {
            child: Some(ChildState { id: 1, count: 7 }),
            seen: Vec::new(),
            replacement: Some(2),
        },
        reducer(),
        runtime.runtime(),
    );
    futures_executor::block_on(async {
        drop(
            store
                .send(Action::Child(PresentationAction::Dismiss))
                .await
                .unwrap(),
        );
        assert_eq!(
            store
                .read(|state| state.child.as_ref().map(|child| (child.id, child.count)))
                .await
                .unwrap(),
            Some((2, 42))
        );
        store.shutdown().await.unwrap();
    });
}

#[test]
fn owned_dismiss_client_delivers_a_parent_visible_dismiss_action() {
    for action in [ChildAction::Close, ChildAction::NamedClose] {
        let runtime = ThreadRuntime::new();
        let issues = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&issues);
        let store = Store::builder(
            State {
                child: Some(ChildState { id: 1, count: 7 }),
                seen: Vec::new(),
                replacement: None,
            },
            reducer(),
            runtime.runtime(),
        )
        .with_issue_handler(move |issue| recorded.lock().unwrap().push(issue.code))
        .build()
        .unwrap();
        futures_executor::block_on(async {
            let task = store
                .send(Action::Child(PresentationAction::Presented(action)))
                .await
                .unwrap();
            task.finish().await;
            assert_eq!(
                store
                    .read(|state| (state.seen.clone(), state.child.is_none()))
                    .await
                    .unwrap(),
                (vec![7, 7], true)
            );
            store.shutdown().await.unwrap();
        });
        assert!(issues.lock().unwrap().is_empty());
    }
}

#[test]
fn absent_presented_and_dismiss_actions_report_before_base_reduction() {
    let runtime = ThreadRuntime::new();
    let issues = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&issues);
    let store = Store::builder(
        State {
            child: None,
            seen: Vec::new(),
            replacement: None,
        },
        reducer(),
        runtime.runtime(),
    )
    .with_issue_handler(move |issue| recorded.lock().unwrap().push(issue.code))
    .build()
    .unwrap();
    futures_executor::block_on(async {
        drop(
            store
                .send(Action::Child(PresentationAction::Presented(
                    ChildAction::Increment,
                )))
                .await
                .unwrap(),
        );
        drop(
            store
                .send(Action::Child(PresentationAction::Dismiss))
                .await
                .unwrap(),
        );
        assert_eq!(
            issues.lock().unwrap().as_slice(),
            ["rca.missing_child", "rca.missing_child"]
        );
        assert_eq!(
            store.read(|state| state.seen.clone()).await.unwrap(),
            [99, 99]
        );
        store.shutdown().await.unwrap();
    });
}

#[test]
fn root_dismiss_reports_a_required_issue_without_creating_output() {
    /// Requests dismissal without any enclosing presentation.
    struct Root {
        /// Records the public call's coordinates independently of the reported issue.
        call_site: Arc<std::sync::OnceLock<(u32, u32)>>,
    }
    impl Reducer for Root {
        type State = usize;
        type Action = ();
        fn reduce(
            &mut self,
            state: &mut usize,
            (): &(),
            context: &mut ReduceContext<'_>,
        ) -> Effect<()> {
            *state += 1;
            let dismiss = context.dismiss();
            assert_eq!(format!("{dismiss:?}"), "Dismiss { bound: false, .. }");
            let (effect, expected_line, expected_column) = (
                Dismiss::effect(&dismiss), // The aligned macros locate this public call.
                line!() - 1,
                column!(),
            );
            self.call_site
                .set((expected_line, expected_column))
                .expect("one dismissal call");
            effect
        }
    }
    let runtime = ThreadRuntime::new();
    let call_site = Arc::new(std::sync::OnceLock::new());
    let root = Root {
        call_site: Arc::clone(&call_site),
    };
    let issues = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&issues);
    let store = Store::builder(0, root, runtime.runtime())
        .with_issue_handler(move |issue| recorded.lock().unwrap().push(issue.clone()))
        .build()
        .unwrap();
    futures_executor::block_on(async {
        store.send(()).await.unwrap().finish().await;
        {
            let reports = issues.lock().unwrap();
            let [issue] = reports.as_slice() else {
                panic!("one unavailable-dismissal issue is required: {reports:?}");
            };
            assert_eq!(issue.code, "rca.dismiss_unavailable");
            assert_eq!(
                issue.message,
                "dismissal requires its matching presentation reducer and captured lifetime"
            );
            assert_eq!(issue.severity, rca_core::IssueSeverity::Error);
            assert_eq!(issue.cause, None);
            let &(expected_line, expected_column) = call_site.get().unwrap();
            assert_eq!(issue.location.file(), file!());
            assert_eq!(issue.location.line(), expected_line);
            assert_eq!(issue.location.column(), expected_column);
        };
        assert_eq!(store.read(|state| *state).await.unwrap(), 1);
        store.shutdown().await.unwrap();
    });
}

#[test]
fn ephemeral_matching_action_reduces_child_and_base_before_clearing() {
    let runtime = ThreadRuntime::new();
    let store = Store::new(
        State {
            child: Some(ChildState { id: 1, count: 7 }),
            seen: Vec::new(),
            replacement: None,
        },
        reducer_with_ephemeral(true),
        runtime.runtime(),
    );
    futures_executor::block_on(async {
        drop(
            store
                .send(Action::Child(PresentationAction::Presented(
                    ChildAction::Increment,
                )))
                .await
                .unwrap(),
        );
        assert_eq!(
            store
                .read(|state| (state.seen.clone(), state.child.is_none()))
                .await
                .unwrap(),
            (vec![8], true)
        );
        store.shutdown().await.unwrap();
    });
}

#[test]
fn historical_dismiss_cannot_clear_replaced_or_recreated_optional_child() {
    for replacement in [1, 2] {
        let runtime = ThreadRuntime::new();
        let store = Store::new(
            State {
                child: Some(ChildState { id: 1, count: 7 }),
                seen: Vec::new(),
                replacement: None,
            },
            reducer(),
            runtime.runtime(),
        );
        let captured = Arc::new(Mutex::new(None));
        futures_executor::block_on(async {
            drop(
                store
                    .send(Action::Child(PresentationAction::Presented(
                        ChildAction::Capture(Arc::clone(&captured)),
                    )))
                    .await
                    .unwrap(),
            );
            if replacement == 1 {
                drop(store.send(Action::Clear).await.unwrap());
            }
            drop(store.send(Action::Replace(replacement)).await.unwrap());
            let dismiss = captured.lock().unwrap().take().unwrap();
            assert_eq!(format!("{dismiss:?}"), "Dismiss { bound: true, .. }");
            store
                .send(Action::Child(PresentationAction::Presented(
                    ChildAction::Replay(dismiss),
                )))
                .await
                .unwrap()
                .finish()
                .await;
            assert_eq!(
                store
                    .read(|state| state.child.as_ref().map(|child| (child.id, child.count)))
                    .await
                    .unwrap(),
                Some((replacement, 42))
            );
            store.shutdown().await.unwrap();
        });
    }
}

#[test]
fn dismiss_client_cannot_retarget_another_store_sharing_the_adapter() {
    /// Shares one adapter while each store must retain its own dismissal authority.
    struct Shared<Inner>(Arc<Mutex<Inner>>);

    impl<Inner: Reducer> Reducer for Shared<Inner> {
        type State = Inner::State;
        type Action = Inner::Action;

        fn reduce(
            &mut self,
            state: &mut Self::State,
            action: &Self::Action,
            context: &mut ReduceContext<'_>,
        ) -> Effect<Self::Action> {
            self.0.lock().unwrap().reduce(state, action, context)
        }
    }

    let runtime = ThreadRuntime::new();
    let initial = || State {
        child: Some(ChildState { id: 1, count: 7 }),
        seen: Vec::new(),
        replacement: None,
    };
    let shared = Arc::new(Mutex::new(reducer()));
    let source = Store::new(initial(), Shared(Arc::clone(&shared)), runtime.runtime());
    let issues = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&issues);
    let destination = Store::builder(initial(), Shared(shared), runtime.runtime())
        .with_issue_handler(move |issue| recorded.lock().unwrap().push(issue.code))
        .build()
        .unwrap();
    let captured = Arc::new(Mutex::new(None));
    futures_executor::block_on(async {
        drop(
            source
                .send(Action::Child(PresentationAction::Presented(
                    ChildAction::Capture(Arc::clone(&captured)),
                )))
                .await
                .unwrap(),
        );
        let dismiss = captured.lock().unwrap().take().unwrap();
        destination
            .send(Action::Child(PresentationAction::Presented(
                ChildAction::Replay(dismiss.clone()),
            )))
            .await
            .unwrap()
            .finish()
            .await;
        assert_eq!(
            destination
                .read(|state| state.child.as_ref().map(|child| child.count))
                .await
                .unwrap(),
            Some(7),
            "foreign dismissal must not clear a Store sharing the same adapter"
        );
        assert_eq!(
            issues.lock().unwrap().as_slice(),
            ["rca.dismiss_unavailable"]
        );
        source
            .send(Action::Child(PresentationAction::Presented(
                ChildAction::Replay(dismiss),
            )))
            .await
            .unwrap()
            .finish()
            .await;
        assert!(source.read(|state| state.child.is_none()).await.unwrap());
        source.shutdown().await.unwrap();
        destination.shutdown().await.unwrap();
    });
}

#[test]
fn cloned_dismiss_client_cannot_retarget_an_independent_store() {
    let runtime = ThreadRuntime::new();
    let initial = || State {
        child: Some(ChildState { id: 1, count: 7 }),
        seen: Vec::new(),
        replacement: None,
    };
    let source = Store::new(initial(), reducer(), runtime.runtime());
    let issues = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&issues);
    let destination = Store::builder(initial(), reducer(), runtime.runtime())
        .with_issue_handler(move |issue| recorded.lock().unwrap().push(issue.code))
        .build()
        .unwrap();
    let captured = Arc::new(Mutex::new(None));
    futures_executor::block_on(async {
        drop(
            source
                .send(Action::Child(PresentationAction::Presented(
                    ChildAction::Capture(Arc::clone(&captured)),
                )))
                .await
                .unwrap(),
        );
        let dismiss = captured.lock().unwrap().take().unwrap();
        destination
            .send(Action::Child(PresentationAction::Presented(
                ChildAction::Replay(dismiss.clone()),
            )))
            .await
            .unwrap()
            .finish()
            .await;
        assert_eq!(
            destination
                .read(|state| state.child.as_ref().map(|child| child.count))
                .await
                .unwrap(),
            Some(7)
        );
        assert_eq!(
            issues.lock().unwrap().as_slice(),
            ["rca.dismiss_unavailable"]
        );
        source
            .send(Action::Child(PresentationAction::Presented(
                ChildAction::Replay(dismiss),
            )))
            .await
            .unwrap()
            .finish()
            .await;
        assert!(source.read(|state| state.child.is_none()).await.unwrap());
        source.shutdown().await.unwrap();
        destination.shutdown().await.unwrap();
    });
}

/// Routes child actions or replaces a keyed row with a fresh same-ID row.
enum RowsAction {
    Row(KeyedAction<u8, Action>),
    Recreate,
}

/// Recreates rows so an old client cannot act on a new lifetime with the same ID.
struct Rows;

/// Creates the same visible value for each distinct keyed lifetime.
fn initial_row() -> State {
    State {
        child: Some(ChildState { id: 1, count: 7 }),
        seen: Vec::new(),
        replacement: None,
    }
}

impl Reducer for Rows {
    type State = KeyedState<u8, State>;
    type Action = RowsAction;
    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &RowsAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<RowsAction> {
        if matches!(action, RowsAction::Recreate) {
            state.remove(&1);
            state.insert(1, initial_row());
        }
        Effect::none()
    }
}

#[test]
fn historical_dismiss_cannot_clear_a_same_id_fresh_keyed_generation_or_sibling() {
    let runtime = ThreadRuntime::new();
    let mut rows = KeyedState::new();
    rows.insert(1, initial_row());
    rows.insert(2, initial_row());
    let reducer = for_each(
        Rows,
        KeyedStatePath::new(
            |state: &KeyedState<u8, State>| state,
            |state: &mut KeyedState<u8, State>| state,
        ),
        KeyedActionPath::new(
            |action: &RowsAction| match action {
                RowsAction::Row(action) => Some(action),
                RowsAction::Recreate => None,
            },
            RowsAction::Row,
        ),
        reducer(),
    );
    let issues = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&issues);
    let store = Store::builder(rows, reducer, runtime.runtime())
        .with_issue_handler(move |issue| recorded.lock().unwrap().push(issue.code))
        .build()
        .unwrap();
    let captured = Arc::new(Mutex::new(None));
    let child = |id, action| {
        RowsAction::Row(KeyedAction::new(
            id,
            Action::Child(PresentationAction::Presented(action)),
        ))
    };
    futures_executor::block_on(async {
        drop(
            store
                .send(child(1, ChildAction::Capture(Arc::clone(&captured))))
                .await
                .unwrap(),
        );
        let dismiss = captured.lock().unwrap().take().unwrap();
        store
            .send(child(2, ChildAction::Replay(dismiss.clone())))
            .await
            .unwrap()
            .finish()
            .await;
        assert_eq!(
            issues.lock().unwrap().as_slice(),
            ["rca.dismiss_unavailable"]
        );
        drop(store.send(RowsAction::Recreate).await.unwrap());
        store
            .send(child(1, ChildAction::Replay(dismiss.clone())))
            .await
            .unwrap()
            .finish()
            .await;
        store
            .send(child(2, ChildAction::Replay(dismiss)))
            .await
            .unwrap()
            .finish()
            .await;
        assert_eq!(
            store
                .read(|rows| rows
                    .iter()
                    .map(|(id, state)| (*id, state.child.as_ref().map(|child| child.count)))
                    .collect::<Vec<_>>())
                .await
                .unwrap(),
            [(2, Some(7)), (1, Some(7))]
        );
        store.shutdown().await.unwrap();
    });
}

/// Leaves ancestor state unchanged while nested composition handles the action.
struct Noop<State, Action>(std::marker::PhantomData<fn(State, Action)>);

impl<State, Action> Reducer for Noop<State, Action> {
    type State = State;
    type Action = Action;
    fn reduce(&mut self, _: &mut State, _: &Action, _: &mut ReduceContext<'_>) -> Effect<Action> {
        Effect::none()
    }
}

/// Starts pending work whose destructor records cancellation by ancestor dismissal.
struct NestedWorker {
    started: Option<futures_channel::oneshot::Sender<()>>,
    dropped: Arc<std::sync::atomic::AtomicBool>,
}

impl Reducer for NestedWorker {
    type State = ();
    type Action = ();
    fn reduce(&mut self, (): &mut (), (): &(), _: &mut ReduceContext<'_>) -> Effect<()> {
        /// Marks physical disposal of the pending future, not just cancellation request.
        struct Finished(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for Finished {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let started = self.started.take().unwrap();
        let finished = Finished(Arc::clone(&self.dropped));
        Effect::run(move |_| async move {
            let _finished = finished;
            started.send(()).unwrap();
            std::future::pending::<()>().await;
        })
    }
}

/// Dismisses the parent, then requires the child's work receipt to finish after cleanup.
fn assert_nested_retirement(
    child: impl Reducer<State = Option<()>, Action = ()> + Send + 'static,
    started: futures_channel::oneshot::Receiver<()>,
    dropped: &std::sync::atomic::AtomicBool,
) {
    let runtime = ThreadRuntime::new();
    let reducer = presentation(
        Noop::<Option<Option<()>>, PresentationAction<()>>(std::marker::PhantomData),
        StatePath::new(
            |state: &Option<Option<()>>| state,
            |state: &mut Option<Option<()>>| state,
        ),
        ActionPath::new(
            |action: &PresentationAction<()>| Some(action),
            |action| action,
        ),
        |_: &Option<()>| &(),
        child,
    );
    let store = Store::new(Some(Some(())), reducer, runtime.runtime());
    futures_executor::block_on(async {
        let work = store.send(PresentationAction::Presented(())).await.unwrap();
        started.await.unwrap();
        drop(store.send(PresentationAction::Dismiss).await.unwrap());
        work.finish().await;
        assert!(work.is_finished());
        assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
        assert!(store.read(Option::is_none).await.unwrap());
        store.shutdown().await.unwrap();
    });
}

#[test]
fn dismissal_retires_running_work_beneath_optional_and_case_children() {
    let (send, receive) = futures_channel::oneshot::channel();
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let child = if_some(
        Noop::<Option<()>, ()>(std::marker::PhantomData),
        OptionalStatePath::new(
            |state: &Option<()>| state.as_ref(),
            |state: &mut Option<()>| state.as_mut(),
        ),
        ActionPath::new(|action: &()| Some(action), |action| action),
        NestedWorker {
            started: Some(send),
            dropped: Arc::clone(&dropped),
        },
    );
    assert_nested_retirement(child, receive, &dropped);

    let (send, receive) = futures_channel::oneshot::channel();
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let child = if_case(
        Noop::<Option<()>, ()>(std::marker::PhantomData),
        CaseStatePath::new(
            |state: &Option<()>| state.as_ref(),
            |state: &mut Option<()>| state.as_mut(),
        ),
        ActionPath::new(|action: &()| Some(action), |action| action),
        NestedWorker {
            started: Some(send),
            dropped: Arc::clone(&dropped),
        },
    );
    assert_nested_retirement(child, receive, &dropped);
}

#[test]
fn old_output_cannot_reach_same_id_fresh_keyed_generation() {
    let runtime = ThreadRuntime::new();
    let mut rows = KeyedState::new();
    rows.insert(1, initial_row());
    let reducer = for_each(
        Rows,
        KeyedStatePath::new(
            |state: &KeyedState<u8, State>| state,
            |state: &mut KeyedState<u8, State>| state,
        ),
        KeyedActionPath::new(
            |action: &RowsAction| match action {
                RowsAction::Row(action) => Some(action),
                RowsAction::Recreate => None,
            },
            RowsAction::Row,
        ),
        reducer(),
    );
    let store = Store::new(rows, reducer, runtime.runtime());
    let (send, receive) = std::sync::mpsc::channel();
    futures_executor::block_on(async {
        let work = store
            .send(RowsAction::Row(KeyedAction::new(
                1,
                Action::Child(PresentationAction::Presented(ChildAction::CaptureOutput(
                    send,
                ))),
            )))
            .await
            .unwrap();
        let old = receive
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        drop(store.send(RowsAction::Recreate).await.unwrap());
        work.finish().await;
        assert!(
            old.send(ChildAction::Increment)
                .await
                .unwrap_err()
                .is_cancelled()
        );
        assert_eq!(
            store
                .read(|rows| rows[&1].child.as_ref().map(|child| child.count))
                .await
                .unwrap(),
            Some(7)
        );
        store.shutdown().await.unwrap();
    });
}

#[test]
fn ephemeral_action_preserves_the_replacement_installed_by_parent() {
    let runtime = ThreadRuntime::new();
    let mut initial = initial_row();
    initial.replacement = Some(2);
    let store = Store::new(initial, reducer_with_ephemeral(true), runtime.runtime());
    futures_executor::block_on(async {
        drop(
            store
                .send(Action::Child(PresentationAction::Presented(
                    ChildAction::Increment,
                )))
                .await
                .unwrap(),
        );
        assert_eq!(
            store
                .read(|state| (
                    state.seen.clone(),
                    state.child.as_ref().map(|child| (child.id, child.count))
                ))
                .await
                .unwrap(),
            (vec![8], Some((2, 42)))
        );
        store.shutdown().await.unwrap();
    });
}

#[test]
fn nested_presentation_dismisses_only_the_nearest_target() {
    let runtime = ThreadRuntime::new();
    let reducer = presentation(
        Noop::<Option<State>, PresentationAction<Action>>(std::marker::PhantomData),
        StatePath::new(
            |state: &Option<State>| state,
            |state: &mut Option<State>| state,
        ),
        ActionPath::new(
            |action: &PresentationAction<Action>| Some(action),
            |action| action,
        ),
        |_: &State| &0_u8,
        reducer(),
    );
    let store = Store::new(Some(initial_row()), reducer, runtime.runtime());
    futures_executor::block_on(async {
        store
            .send(PresentationAction::Presented(Action::Child(
                PresentationAction::Presented(ChildAction::Close),
            )))
            .await
            .unwrap()
            .finish()
            .await;
        assert_eq!(
            store
                .read(|state| state.as_ref().map(|state| state.child.is_none()))
                .await
                .unwrap(),
            Some(true)
        );
        store.shutdown().await.unwrap();
    });
}
