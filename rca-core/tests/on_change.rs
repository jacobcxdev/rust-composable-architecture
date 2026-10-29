//! Checks change reactions through real Stores, child routes and dependency scopes.

mod support;

use rca_core::{
    ActionPath, CaseStatePath, DependencyKey, Effect, KeyedAction, KeyedActionPath, KeyedState,
    KeyedStatePath, OptionalStatePath, ReduceContext, Reducer, ReducerExt, StatePath, Store,
    combine, on_change, scope, with_dependency,
};
use support::ManualRuntime;

/// Retains the edited name, base-call count, change history and dependency trace.
#[derive(Default)]
struct State {
    name: String,
    base_calls: usize,
    changes: Vec<(String, String)>,
    contexts: Vec<&'static str>,
}

/// Separates an edit from an action that leaves the selected name unchanged.
enum Action {
    SetName(String),
    Unrelated,
}

/// Counts every base reduction and changes the name only for an edit.
struct Edit;

impl Reducer for Edit {
    type State = State;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut State,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        state.base_calls += 1;
        if let Action::SetName(value) = action {
            state.name.clone_from(value);
        }
        Effect::none()
    }
}

/// Moves the before-and-after names into the state during one reaction.
struct RecordChange(String, String);

impl Reducer for RecordChange {
    type State = State;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut State,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        assert!(matches!(action, Action::SetName(_)));
        state
            .changes
            .push((std::mem::take(&mut self.0), std::mem::take(&mut self.1)));
        Effect::none()
    }
}

#[test]
fn reducer_extension_commits_once_and_skips_equal_or_unrelated_edits() {
    let runtime = ManualRuntime::new();
    let store = Store::new(
        State::default(),
        Edit.on_change(|state: &State| state.name.clone(), RecordChange),
        runtime.runtime(),
    );
    runtime.start_all();
    futures_executor::block_on(async {
        store
            .send(Action::SetName("new".into()))
            .await
            .unwrap()
            .finish()
            .await;
        store
            .send(Action::SetName("new".into()))
            .await
            .unwrap()
            .finish()
            .await;
        store.send(Action::Unrelated).await.unwrap().finish().await;
        store
            .read(|state| {
                assert_eq!(state.name, "new");
                assert_eq!(state.base_calls, 3);
                assert_eq!(state.changes, [(String::new(), "new".into())]);
            })
            .await
            .unwrap();
        store.shutdown().await.unwrap();
    });
    for result in runtime.join_all() {
        result.unwrap();
    }
}

/// Keeps a child's current value and independent reaction count.
#[derive(Default)]
struct Child {
    value: usize,
    changes: usize,
}

/// Makes the detail child's presence depend on the selected case.
enum Screen {
    Detail(Child),
    Idle,
}

/// Holds sibling, optional, case and keyed children under one parent.
struct App {
    left: Child,
    right: Child,
    optional: Option<Child>,
    screen: Screen,
    keyed: KeyedState<u8, Child>,
}

/// Carries the same selected value to the child and its reaction.
enum ChildAction {
    Set(usize),
}

/// Routes child edits and retires or replaces all conditional children.
enum AppAction {
    Left(ChildAction),
    Right(ChildAction),
    Optional(ChildAction),
    Detail(ChildAction),
    Keyed(KeyedAction<u8, ChildAction>),
    Remove,
    Reinsert,
}

/// Writes the child's selected value before its reaction runs.
struct ChildEdit;

/// Checks the old and new values before counting one child reaction.
struct ChildReaction(usize, usize);

/// Removes and reinserts conditional children without changing their siblings.
struct Parent;

impl Reducer for ChildEdit {
    type State = Child;
    type Action = ChildAction;
    fn reduce(
        &mut self,
        state: &mut Child,
        action: &ChildAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<ChildAction> {
        let ChildAction::Set(value) = action;
        state.value = *value;
        Effect::none()
    }
}

impl Reducer for ChildReaction {
    type State = Child;
    type Action = ChildAction;
    fn reduce(
        &mut self,
        state: &mut Child,
        action: &ChildAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<ChildAction> {
        let ChildAction::Set(value) = action;
        assert_ne!(self.0, self.1);
        assert_eq!(*value, self.1);
        assert_eq!(state.value, self.1);
        state.changes += 1;
        Effect::none()
    }
}

/// Gives each composed child its own selector and reaction state.
fn child() -> impl Reducer<State = Child, Action = ChildAction> {
    on_change(ChildEdit, |state: &Child| state.value, ChildReaction)
}

impl Reducer for Parent {
    type State = App;
    type Action = AppAction;
    fn reduce(
        &mut self,
        state: &mut App,
        action: &AppAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<AppAction> {
        match action {
            AppAction::Remove => {
                state.optional = None;
                state.screen = Screen::Idle;
                let _ = state.keyed.remove(&1);
            }
            AppAction::Reinsert => {
                state.optional = Some(Child::default());
                state.screen = Screen::Detail(Child::default());
                state.keyed.insert(1, Child::default());
            }
            _ => {}
        }
        Effect::none()
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "keeps same-root sibling and optional/case/keyed lifetime transitions in one scenario"
)]
fn nested_siblings_react_independently_and_retired_edit_routes_stay_retired() {
    let reducer = combine((
        scope(
            StatePath::new(|s: &App| &s.left, |s: &mut App| &mut s.left),
            ActionPath::new(
                |a: &AppAction| match a {
                    AppAction::Left(a) => Some(a),
                    _ => None,
                },
                AppAction::Left,
            ),
            child(),
        ),
        scope(
            StatePath::new(|s: &App| &s.right, |s: &mut App| &mut s.right),
            ActionPath::new(
                |a: &AppAction| match a {
                    AppAction::Right(a) => Some(a),
                    _ => None,
                },
                AppAction::Right,
            ),
            child(),
        ),
        Parent,
    ))
    .if_some(
        OptionalStatePath::new(
            |s: &App| s.optional.as_ref(),
            |s: &mut App| s.optional.as_mut(),
        ),
        ActionPath::new(
            |a: &AppAction| match a {
                AppAction::Optional(a) => Some(a),
                _ => None,
            },
            AppAction::Optional,
        ),
        child(),
    )
    .if_case(
        CaseStatePath::new(
            |s: &App| match &s.screen {
                Screen::Detail(c) => Some(c),
                Screen::Idle => None,
            },
            |s: &mut App| match &mut s.screen {
                Screen::Detail(c) => Some(c),
                Screen::Idle => None,
            },
        ),
        ActionPath::new(
            |a: &AppAction| match a {
                AppAction::Detail(a) => Some(a),
                _ => None,
            },
            AppAction::Detail,
        ),
        child(),
    )
    .for_each(
        KeyedStatePath::new(|s: &App| &s.keyed, |s: &mut App| &mut s.keyed),
        KeyedActionPath::new(
            |a: &AppAction| match a {
                AppAction::Keyed(a) => Some(a),
                _ => None,
            },
            AppAction::Keyed,
        ),
        child(),
    );
    let runtime = ManualRuntime::new();
    let mut keyed = KeyedState::new();
    keyed.insert(1, Child::default());
    let store = Store::new(
        App {
            left: Child::default(),
            right: Child::default(),
            optional: Some(Child::default()),
            screen: Screen::Detail(Child::default()),
            keyed,
        },
        reducer,
        runtime.runtime(),
    );
    runtime.start_all();
    futures_executor::block_on(async {
        let optional = store
            .scope_some(
                OptionalStatePath::new(
                    |s: &App| s.optional.as_ref(),
                    |s: &mut App| s.optional.as_mut(),
                ),
                ActionPath::new(
                    |a: &AppAction| match a {
                        AppAction::Optional(a) => Some(a),
                        _ => None,
                    },
                    AppAction::Optional,
                ),
            )
            .await
            .unwrap();
        let detail = store
            .scope_case(
                CaseStatePath::new(
                    |s: &App| match &s.screen {
                        Screen::Detail(c) => Some(c),
                        Screen::Idle => None,
                    },
                    |s: &mut App| match &mut s.screen {
                        Screen::Detail(c) => Some(c),
                        Screen::Idle => None,
                    },
                ),
                ActionPath::new(
                    |a: &AppAction| match a {
                        AppAction::Detail(a) => Some(a),
                        _ => None,
                    },
                    AppAction::Detail,
                ),
            )
            .await
            .unwrap();
        let keyed = store
            .scope_keyed(
                KeyedStatePath::new(|s: &App| &s.keyed, |s: &mut App| &mut s.keyed),
                KeyedActionPath::new(
                    |a: &AppAction| match a {
                        AppAction::Keyed(a) => Some(a),
                        _ => None,
                    },
                    AppAction::Keyed,
                ),
                1,
            )
            .await
            .unwrap();
        for action in [
            AppAction::Left(ChildAction::Set(7)),
            AppAction::Left(ChildAction::Set(7)),
            AppAction::Right(ChildAction::Set(9)),
        ] {
            store.send(action).await.unwrap().finish().await;
        }
        optional
            .send(ChildAction::Set(2))
            .await
            .unwrap()
            .finish()
            .await;
        detail
            .send(ChildAction::Set(3))
            .await
            .unwrap()
            .finish()
            .await;
        keyed
            .send(ChildAction::Set(4))
            .await
            .unwrap()
            .finish()
            .await;
        store
            .read(|s| {
                let optional = s.optional.as_ref().unwrap();
                assert_eq!((optional.value, optional.changes), (2, 1));
                let Screen::Detail(detail) = &s.screen else {
                    panic!("initial detail");
                };
                assert_eq!((detail.value, detail.changes), (3, 1));
                assert_eq!((s.keyed[&1].value, s.keyed[&1].changes), (4, 1));
            })
            .await
            .unwrap();
        store.send(AppAction::Remove).await.unwrap().finish().await;
        store
            .send(AppAction::Reinsert)
            .await
            .unwrap()
            .finish()
            .await;
        assert!(
            optional
                .send(ChildAction::Set(99))
                .await
                .unwrap_err()
                .is_scope_ended()
        );
        assert!(
            detail
                .send(ChildAction::Set(99))
                .await
                .unwrap_err()
                .is_scope_ended()
        );
        assert!(
            keyed
                .send(ChildAction::Set(99))
                .await
                .unwrap_err()
                .is_scope_ended()
        );
        store
            .read(|s| {
                assert_eq!((s.left.value, s.left.changes), (7, 1));
                assert_eq!((s.right.value, s.right.changes), (9, 1));
                assert_eq!(
                    (
                        s.optional.as_ref().unwrap().value,
                        s.optional.as_ref().unwrap().changes
                    ),
                    (0, 0)
                );
                let Screen::Detail(detail) = &s.screen else {
                    panic!("reinserted detail")
                };
                assert_eq!((detail.value, detail.changes), (0, 0));
                assert_eq!((s.keyed[&1].value, s.keyed[&1].changes), (0, 0));
            })
            .await
            .unwrap();
        store.shutdown().await.unwrap();
    });
    for result in runtime.join_all() {
        result.unwrap();
    }
}

/// Supplies a default that differs from every explicit dependency override.
struct Locale;
impl DependencyKey for Locale {
    type Value = &'static str;
    fn live_value() -> Self::Value {
        "live"
    }
}

/// Carries a name edit and a temporary action-level locale override.
struct ContextEdit {
    name: String,
    locale: &'static str,
}
/// Records dependency selection before and inside the temporary override.
struct ContextBase;

/// Repeats the dependency check and counts any unexpected mapped effect output.
struct ContextReaction(std::sync::Arc<std::sync::atomic::AtomicUsize>);

impl Reducer for ContextBase {
    type State = State;
    type Action = ContextEdit;
    fn reduce(
        &mut self,
        state: &mut State,
        action: &ContextEdit,
        context: &mut ReduceContext<'_>,
    ) -> Effect<ContextEdit> {
        state.name.clone_from(&action.name);
        state.contexts.push(*context.dependency::<Locale>());
        context.with_dependency_override::<Locale, _>(action.locale, |context| {
            state.contexts.push(*context.dependency::<Locale>());
        });
        Effect::none()
    }
}

impl Reducer for ContextReaction {
    type State = State;
    type Action = ContextEdit;
    fn reduce(
        &mut self,
        state: &mut State,
        action: &ContextEdit,
        context: &mut ReduceContext<'_>,
    ) -> Effect<ContextEdit> {
        state.contexts.push(*context.dependency::<Locale>());
        context.with_dependency_override::<Locale, _>(action.locale, |context| {
            state.contexts.push(*context.dependency::<Locale>());
        });
        let calls = std::sync::Arc::clone(&self.0);
        Effect::future(async { None::<String> }).map(move |name| {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            ContextEdit {
                name,
                locale: "unexpected",
            }
        })
    }
}

#[test]
fn typed_context_keeps_override_precedence_and_none_reaction_output_is_intentional() {
    let runtime = ManualRuntime::new();
    let mapped = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let captured = std::sync::Arc::clone(&mapped);
    let reducer = with_dependency::<Locale, _>(
        "outer",
        with_dependency::<Locale, _>(
            "inner",
            on_change(
                ContextBase,
                |s: &State| s.name.clone(),
                move |old, new| {
                    assert_eq!(old, "");
                    assert_eq!(new, "edited");
                    ContextReaction(std::sync::Arc::clone(&captured))
                },
            ),
        ),
    );
    let store = Store::builder(State::default(), reducer, runtime.runtime())
        .with_dependency::<Locale>("store")
        .build()
        .unwrap();
    runtime.start_all();
    futures_executor::block_on(async {
        let task = store
            .send(ContextEdit {
                name: "edited".into(),
                locale: "action",
            })
            .await
            .unwrap();
        // The receipt confirms the reduction, not acceptance of its effect.
        // Wait for the descendant before starting this host's next task batch.
        runtime.wait_for_queued_task();
        runtime.start_all();
        task.finish().await;
        assert_eq!(mapped.load(std::sync::atomic::Ordering::SeqCst), 0);
        store
            .read(|s| {
                assert_eq!(s.name, "edited");
                assert_eq!(s.contexts, ["inner", "action", "inner", "action"]);
            })
            .await
            .unwrap();
        store.shutdown().await.unwrap();
    });
    for result in runtime.join_all() {
        result.unwrap();
    }
}
