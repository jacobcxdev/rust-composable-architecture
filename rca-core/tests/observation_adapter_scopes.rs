#[path = "support/thread_runtime.rs"]
mod support;

use std::cell::RefCell;
use std::rc::Rc;

use rca_core::{
    ActionPath, CaseStatePath, Effect, ExternalObservation, KeyedAction, KeyedActionPath,
    KeyedState, KeyedStatePath, OptionalStatePath, ReduceContext, Reducer, ReducerExt, ScopedStore,
    Store,
};
use support::ThreadRuntime;

enum Screen {
    Detail(usize),
    Idle,
}

struct State {
    optional: Option<usize>,
    screen: Screen,
    keyed: KeyedState<u8, usize>,
}

enum Action {
    Optional(usize),
    Detail(usize),
    Keyed(KeyedAction<u8, usize>),
    Remove,
    Reinsert,
}

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
        match action {
            Action::Remove => {
                state.optional = None;
                state.screen = Screen::Idle;
                state.keyed.remove(&7);
            }
            Action::Reinsert => {
                state.optional = Some(0);
                state.screen = Screen::Detail(0);
                state.keyed.insert(7, 0);
            }
            _ => {}
        }
        Effect::none()
    }
}

struct Child;
impl Reducer for Child {
    type State = usize;
    type Action = usize;

    fn reduce(
        &mut self,
        state: &mut usize,
        action: &usize,
        _: &mut ReduceContext<'_>,
    ) -> Effect<usize> {
        *state = *action;
        Effect::none()
    }
}

type OptionalPath = OptionalStatePath<
    State,
    usize,
    fn(&State) -> Option<&usize>,
    fn(&mut State) -> Option<&mut usize>,
>;
type CasePath =
    CaseStatePath<State, usize, fn(&State) -> Option<&usize>, fn(&mut State) -> Option<&mut usize>>;
type ChildActionPath =
    ActionPath<Action, usize, fn(&Action) -> Option<&usize>, fn(usize) -> Action>;
type KeyedPath = KeyedStatePath<
    State,
    u8,
    usize,
    fn(&State) -> &KeyedState<u8, usize>,
    fn(&mut State) -> &mut KeyedState<u8, usize>,
>;
type ChildKeyedActionPath = KeyedActionPath<
    Action,
    u8,
    usize,
    fn(&Action) -> Option<&KeyedAction<u8, usize>>,
    fn(KeyedAction<u8, usize>) -> Action,
>;

fn optional_path() -> OptionalPath {
    OptionalStatePath::new(
        |s: &State| s.optional.as_ref(),
        |s: &mut State| s.optional.as_mut(),
    )
}

fn optional_action() -> ChildActionPath {
    ActionPath::new(
        |a| {
            if let Action::Optional(value) = a {
                Some(value)
            } else {
                None
            }
        },
        Action::Optional,
    )
}

fn case_path() -> CasePath {
    CaseStatePath::new(
        |s: &State| {
            if let Screen::Detail(value) = &s.screen {
                Some(value)
            } else {
                None
            }
        },
        |s: &mut State| {
            if let Screen::Detail(value) = &mut s.screen {
                Some(value)
            } else {
                None
            }
        },
    )
}

fn case_action() -> ChildActionPath {
    ActionPath::new(
        |a| {
            if let Action::Detail(value) = a {
                Some(value)
            } else {
                None
            }
        },
        Action::Detail,
    )
}

fn keyed_path() -> KeyedPath {
    KeyedStatePath::new(|s: &State| &s.keyed, |s: &mut State| &mut s.keyed)
}

fn keyed_action() -> ChildKeyedActionPath {
    KeyedActionPath::new(
        |a| {
            if let Action::Keyed(value) = a {
                Some(value)
            } else {
                None
            }
        },
        Action::Keyed,
    )
}

fn store(runtime: &ThreadRuntime) -> Store<State, Action> {
    let mut keyed = KeyedState::new();
    keyed.insert(7, 0);
    Store::new(
        State {
            optional: Some(0),
            screen: Screen::Detail(0),
            keyed,
        },
        Parent
            .if_some(optional_path(), optional_action(), Child)
            .if_case(case_path(), case_action(), Child)
            .for_each(keyed_path(), keyed_action(), Child),
        runtime.runtime(),
    )
}

#[derive(Clone, Copy)]
enum Kind {
    Optional,
    Case,
    Keyed,
}

async fn child(store: &Store<State, Action>, kind: Kind) -> ScopedStore<usize, usize> {
    match kind {
        Kind::Optional => store
            .scope_some(optional_path(), optional_action())
            .await
            .unwrap(),
        Kind::Case => store.scope_case(case_path(), case_action()).await.unwrap(),
        Kind::Keyed => store
            .scope_keyed(keyed_path(), keyed_action(), 7)
            .await
            .unwrap(),
    }
}

// Reusing application identity must not revive an unpolled stale callback or producer.
#[test]
fn optional_case_and_keyed_callbacks_and_producers_preserve_generation_authority() {
    for kind in [Kind::Optional, Kind::Case, Kind::Keyed] {
        let runtime = ThreadRuntime::new();
        let store = store(&runtime);
        futures_executor::block_on(async {
            let old = child(&store, kind).await;
            let live = old.observe(|value| *value).await.unwrap();
            let source = Rc::new(RefCell::new(None));
            let mut values = ExternalObservation::<_, ()>::new({
                let source = Rc::clone(&source);
                move |producer| {
                    *source.borrow_mut() = Some(producer);
                    (5, || {})
                }
            });
            old.send(values.next().await.unwrap().unwrap())
                .await
                .unwrap()
                .finish()
                .await;
            assert!(live.on_change(|value| assert_eq!(value, 5)).await);
            let stale = old.observe(|value| *value).await.unwrap();
            old.send(6).await.unwrap().finish().await;
            let stale_callback = stale.on_change(|_| panic!("retired callback must not execute"));
            store.send(Action::Remove).await.unwrap().finish().await;
            store.send(Action::Reinsert).await.unwrap().finish().await;
            assert!(!stale_callback.await);
            source.borrow().as_ref().unwrap().publish(99).unwrap();
            let late = values.next().await.unwrap().unwrap();
            assert!(old.send(late).await.unwrap_err().is_scope_ended());
            let fresh = child(&store, kind).await;
            assert_eq!(fresh.read(|value| *value).await.unwrap(), 0);
            let current = fresh.observe(|value| *value).await.unwrap();
            fresh.send(7).await.unwrap().finish().await;
            assert!(current.on_change(|value| assert_eq!(value, 7)).await);
            values.cancel();
            assert_eq!(source.borrow().as_ref().unwrap().publish(100), Err(100));
            store.shutdown().await.unwrap();
        });
    }
}
