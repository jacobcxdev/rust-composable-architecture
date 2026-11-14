//! Keeps shared value retention separate from a store or child's output lifetime.

use std::sync::mpsc;

use futures_executor::block_on;
use rca_core::{
    ActionPath, CaseStatePath, Effect, EffectSender, KeyedAction, KeyedActionPath, KeyedState,
    KeyedStatePath, OptionalStatePath, ReduceContext, Reducer, ReducerExt, ScopedStore, Shared,
    Store,
};
use rca_test::DeterministicRuntime;

/// Starts a shared read or delivers an owned value through an effect sender.
enum ChildAction {
    Start,
    Value(u32),
}

/// Captures the shared reader while the real effect retains its output handle.
struct Client {
    shared: Shared<u32>,
    senders: mpsc::Sender<EffectSender<ChildAction>>,
}

impl Reducer for Client {
    type State = Vec<u32>;
    type Action = ChildAction;

    fn reduce(
        &mut self,
        state: &mut Vec<u32>,
        action: &ChildAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<ChildAction> {
        match action {
            ChildAction::Start => {
                let reader = self.shared.reader().unwrap();
                let senders = self.senders.clone();
                Effect::run(move |sender| async move {
                    let value = reader.read(|value| *value).unwrap();
                    sender.send(ChildAction::Value(value)).await.unwrap();
                    senders.send(sender).unwrap();
                    std::future::pending::<()>().await;
                })
            }
            ChildAction::Value(value) => {
                state.push(*value);
                Effect::none()
            }
        }
    }
}

/// Selects a real optional, case or keyed lifetime wrapper.
#[derive(Clone, Copy)]
enum Family {
    Optional,
    Case,
    Keyed,
}

/// Gives the case child an explicit absent state.
enum Screen {
    Off,
    On(Vec<u32>),
}

/// Records delivered values independently for each child lifetime.
struct State {
    optional: Option<Vec<u32>>,
    screen: Screen,
    rows: KeyedState<u8, Vec<u32>>,
}

/// Removes and reinserts children without replacing the externally owned shared value.
enum Action {
    Optional(ChildAction),
    Case(ChildAction),
    Row(KeyedAction<u8, ChildAction>),
    Remove(Family),
    Insert(Family),
}

/// Changes child membership while each client retains its shared handle.
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
            Action::Remove(Family::Optional) => state.optional = None,
            Action::Remove(Family::Case) => state.screen = Screen::Off,
            Action::Remove(Family::Keyed) => {
                state.rows.remove(&1);
            }
            Action::Insert(Family::Optional) => state.optional = Some(Vec::new()),
            Action::Insert(Family::Case) => state.screen = Screen::On(Vec::new()),
            Action::Insert(Family::Keyed) => state.rows.insert(1, Vec::new()),
            _ => {}
        }
        Effect::none()
    }
}

/// Recreates the optional projection without naming its closure types.
macro_rules! optional_state {
    () => {
        OptionalStatePath::new(
            |s: &State| s.optional.as_ref(),
            |s: &mut State| s.optional.as_mut(),
        )
    };
}

/// Routes only optional-child actions.
macro_rules! optional_action {
    () => {
        ActionPath::new(
            |a: &Action| match a {
                Action::Optional(a) => Some(a),
                _ => None,
            },
            Action::Optional,
        )
    };
}

/// Selects the active case without constructing replacement values.
macro_rules! case_state {
    () => {
        CaseStatePath::new(
            |s: &State| match &s.screen {
                Screen::On(s) => Some(s),
                Screen::Off => None,
            },
            |s: &mut State| match &mut s.screen {
                Screen::On(s) => Some(s),
                Screen::Off => None,
            },
        )
    };
}

/// Routes only case-child actions.
macro_rules! case_action {
    () => {
        ActionPath::new(
            |a: &Action| match a {
                Action::Case(a) => Some(a),
                _ => None,
            },
            Action::Case,
        )
    };
}

/// Reuses the keyed projection for reducer composition and public scopes.
macro_rules! keyed_state {
    () => {
        KeyedStatePath::new(|s: &State| &s.rows, |s: &mut State| &mut s.rows)
    };
}

/// Preserves row identity when routing the child's output.
macro_rules! keyed_action {
    () => {
        KeyedActionPath::new(
            |a: &Action| match a {
                Action::Row(a) => Some(a),
                _ => None,
            },
            Action::Row,
        )
    };
}

/// Acquires the selected child's current lifetime through its public scope API.
async fn scope(store: &Store<State, Action>, family: Family) -> ScopedStore<Vec<u32>, ChildAction> {
    match family {
        Family::Optional => store
            .scope_some(optional_state!(), optional_action!())
            .await
            .unwrap(),
        Family::Case => store
            .scope_case(case_state!(), case_action!())
            .await
            .unwrap(),
        Family::Keyed => store
            .scope_keyed(keyed_state!(), keyed_action!(), 1)
            .await
            .unwrap(),
    }
}

/// Rejects stale output while retained shared values and fresh child output remain usable.
#[expect(
    clippy::too_many_lines,
    reason = "keeps retirement, retained data and fresh output in one causal trace"
)]
fn check_child(family: Family) {
    let host = DeterministicRuntime::new();
    let shared = Shared::new(11);
    let alias = shared.clone();
    let captured = shared
        .project(|value| Some(value), |value| Some(value))
        .unwrap();
    let (senders, received) = mpsc::channel();
    let client = || Client {
        shared: shared.clone(),
        senders: senders.clone(),
    };
    let reducer = Parent
        .if_some(optional_state!(), optional_action!(), client())
        .if_case(case_state!(), case_action!(), client())
        .for_each(keyed_state!(), keyed_action!(), client());
    let store = Store::new(
        State {
            optional: Some(Vec::new()),
            screen: Screen::On(Vec::new()),
            rows: [(1, Vec::new())].into_iter().collect(),
        },
        reducer,
        host.runtime(),
    );
    let driver = host.clone();
    block_on(driver.run(async move {
        let old = scope(&store, family).await;
        let task = old.send(ChildAction::Start).await.unwrap();
        host.run_until_stalled().unwrap();
        let stale = received.try_recv().unwrap();
        assert_eq!(old.read(Clone::clone).await.unwrap(), vec![11]);
        store
            .send(Action::Remove(family))
            .await
            .unwrap()
            .finish()
            .await;
        host.run_until_stalled().unwrap();
        task.finish().await;
        captured
            .try_update(
                |value| {
                    *value = 23;
                    Ok::<_, ()>(())
                },
                || {},
            )
            .unwrap();
        let retained = captured.read(|value| *value).unwrap();
        assert_eq!(retained, 23);
        assert!(
            stale
                .send(ChildAction::Value(retained))
                .await
                .unwrap_err()
                .is_cancelled()
        );
        store
            .send(Action::Insert(family))
            .await
            .unwrap()
            .finish()
            .await;
        assert!(old.read(Vec::len).await.unwrap_err().is_scope_ended());
        let fresh = scope(&store, family).await;
        assert_eq!(fresh.read(Clone::clone).await.unwrap(), Vec::<u32>::new());
        shared.rebind(&Shared::new(31)).unwrap();
        alias
            .try_update(
                |value| {
                    *value = 37;
                    Ok::<_, ()>(())
                },
                || {},
            )
            .unwrap();
        assert_eq!(captured.read(|value| *value).unwrap(), 23);
        for value in [retained, alias.read(|value| *value).unwrap()] {
            assert!(
                stale
                    .send(ChildAction::Value(value))
                    .await
                    .unwrap_err()
                    .is_cancelled()
            );
        }
        assert_eq!(fresh.read(Clone::clone).await.unwrap(), Vec::<u32>::new());
        let fresh_task = fresh.send(ChildAction::Start).await.unwrap();
        host.run_until_stalled().unwrap();
        let live = received.try_recv().unwrap();
        assert_eq!(fresh.read(Clone::clone).await.unwrap(), vec![37]);
        // Explicit post-unlock publication stages an owned value; async output
        // still uses current generation authority rather than the shared handle.
        let mut published = None;
        alias
            .try_update(
                |value| {
                    *value = 41;
                    Ok::<_, ()>(())
                },
                || {
                    published = Some(alias.read(|value| *value).unwrap());
                },
            )
            .unwrap();
        live.send(ChildAction::Value(published.unwrap()))
            .await
            .unwrap();
        host.run_until_stalled().unwrap();
        assert_eq!(fresh.read(Clone::clone).await.unwrap(), vec![37, 41]);
        store.shutdown().await.unwrap();
        fresh_task.finish().await;
    }))
    .unwrap();
}

#[test]
fn optional_shared_value_cannot_revive_retired_output_authority() {
    check_child(Family::Optional);
}

#[test]
fn case_shared_value_cannot_revive_retired_output_authority() {
    check_child(Family::Case);
}

#[test]
fn keyed_shared_value_cannot_revive_retired_output_authority() {
    check_child(Family::Keyed);
}

#[test]
fn root_shared_reader_works_live_but_retention_does_not_reopen_stopped_store() {
    let host = DeterministicRuntime::new();
    let shared = Shared::new(3);
    let reader = shared.reader().unwrap();
    let (senders, received) = mpsc::channel();
    let store = Store::new(
        Vec::new(),
        Client {
            shared: shared.clone(),
            senders,
        },
        host.runtime(),
    );
    let driver = host.clone();
    block_on(driver.run(async move {
        let task = store.send(ChildAction::Start).await.unwrap();
        host.run_until_stalled().unwrap();
        let output = received.try_recv().unwrap();
        assert_eq!(store.read(Clone::clone).await.unwrap(), vec![3]);
        shared
            .try_update(
                |value| {
                    *value = 5;
                    Ok::<_, ()>(())
                },
                || {},
            )
            .unwrap();
        output
            .send(ChildAction::Value(reader.read(|value| *value).unwrap()))
            .await
            .unwrap();
        host.run_until_stalled().unwrap();
        assert_eq!(store.read(Clone::clone).await.unwrap(), vec![3, 5]);
        store.shutdown().await.unwrap();
        task.finish().await;
        shared
            .try_update(
                |value| {
                    *value = 7;
                    Ok::<_, ()>(())
                },
                || {},
            )
            .unwrap();
        assert_eq!(reader.read(|value| *value).unwrap(), 7);
        assert!(
            output
                .send(ChildAction::Value(7))
                .await
                .unwrap_err()
                .is_cancelled()
        );
    }))
    .unwrap();
}
