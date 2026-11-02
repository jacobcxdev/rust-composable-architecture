//! Cached readers retain their subscription, not a removed child's permission to send output.

use std::convert::Infallible;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
    mpsc,
};

use futures_executor::block_on;
use rca_core::{
    ActionPath, CaseStatePath, Effect, EffectSender, KeyedAction, KeyedActionPath, KeyedState,
    KeyedStatePath, OptionalStatePath, ReduceContext, Reducer, ReducerExt, ScopedStore,
    SharedKeyCache, SharedKeyIdentity, SharedLoadContext, SharedReader, SharedReaderKey,
    SharedSubscribeContext, SharedSubscriber, Store,
};
use rca_test::DeterministicRuntime;

/// Creates one counted subscription for readers that share a cache entry.
struct Key(Arc<AtomicUsize>);

/// Records actual release of the retained backend resource.
struct Subscription(Arc<AtomicUsize>);
impl Drop for Subscription {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
impl SharedReaderKey for Key {
    type Value = u32;
    type Error = Infallible;
    type Subscription = Subscription;

    fn identity(&self) -> SharedKeyIdentity {
        SharedKeyIdentity::new::<u32>("scope", "v1")
    }

    fn load(
        &self,
        _: SharedLoadContext<'_, u32>,
    ) -> impl Future<Output = Result<Option<u32>, Infallible>> + Send {
        std::future::ready(Ok(None))
    }

    fn subscribe(
        &self,
        _: SharedSubscribeContext,
        _: SharedSubscriber<u32>,
    ) -> Result<Subscription, Infallible> {
        Ok(Subscription(Arc::clone(&self.0)))
    }
}

/// Starts a cached read or delivers its value through a real effect sender.
enum ChildAction {
    Start,
    Value(u32),
}

/// Retains cached data while its effect owns an independently scoped output handle.
struct Client {
    reader: SharedReader<u32>,
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
                let reader = self.reader.clone();
                let senders = self.senders.clone();
                Effect::run(move |sender| async move {
                    sender
                        .send(ChildAction::Value(reader.read(|value| *value).unwrap()))
                        .await
                        .unwrap();
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

/// Selects the structural wrapper whose lifetime the test retires.
#[derive(Clone, Copy)]
enum Family {
    Optional,
    Case,
    Keyed,
}

/// Distinguishes a present case child from an absent child.
enum Screen {
    Off,
    On(Vec<u32>),
}

/// Stores delivered values separately for each structural wrapper.
struct State {
    optional: Option<Vec<u32>>,
    screen: Screen,
    rows: KeyedState<u8, Vec<u32>>,
}

/// Routes child output and removes or inserts the selected child.
enum Action {
    Optional(ChildAction),
    Case(ChildAction),
    Row(KeyedAction<u8, ChildAction>),
    Remove(Family),
    Insert(Family),
}

/// Changes child membership without replacing the externally owned cached reader.
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

/// Reuses the optional projection without naming its closure types.
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

/// Selects the active case for composition and public scoped handles.
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

/// Reuses the keyed state projection for both public consumers.
macro_rules! keyed_state {
    () => {
        KeyedStatePath::new(|s: &State| &s.rows, |s: &mut State| &mut s.rows)
    };
}

/// Preserves row identity when embedding child output.
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

/// Retains cached data across retirement, then checks its final resource release.
fn check_child(family: Family) {
    let host = DeterministicRuntime::new();
    let cache = SharedKeyCache::new();
    let drops = Arc::new(AtomicUsize::new(0));
    let reader = block_on(cache.reader(Key(Arc::clone(&drops)), || 11)).unwrap();
    let (senders, received) = mpsc::channel();
    let client = || Client {
        reader: reader.clone(),
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
    let task_drops = Arc::clone(&drops);
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
        cache.reset().unwrap();
        assert_eq!(reader.read(|value| *value).unwrap(), 11);
        assert_eq!(task_drops.load(Ordering::SeqCst), 0);
        store
            .send(Action::Insert(family))
            .await
            .unwrap()
            .finish()
            .await;
        assert!(old.read(Vec::len).await.unwrap_err().is_scope_ended());
        let fresh = scope(&store, family).await;
        assert!(
            stale
                .send(ChildAction::Value(reader.read(|value| *value).unwrap()))
                .await
                .unwrap_err()
                .is_cancelled()
        );
        assert_eq!(fresh.read(Clone::clone).await.unwrap(), Vec::<u32>::new());
        let fresh_task = fresh.send(ChildAction::Start).await.unwrap();
        host.run_until_stalled().unwrap();
        assert_eq!(fresh.read(Clone::clone).await.unwrap(), vec![11]);
        store.shutdown().await.unwrap();
        fresh_task.finish().await;
        assert_eq!(reader.read(|value| *value).unwrap(), 11);
        assert_eq!(task_drops.load(Ordering::SeqCst), 0);
    }))
    .unwrap();
    drop(driver);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn optional_cached_reader_cannot_revive_a_retired_generation() {
    check_child(Family::Optional);
}

#[test]
fn case_cached_reader_cannot_revive_a_retired_generation() {
    check_child(Family::Case);
}

#[test]
fn keyed_cached_reader_cannot_revive_a_retired_generation() {
    check_child(Family::Keyed);
}

// The cache and Store may both disappear while a captured reader retains its resource.
#[test]
fn root_cached_reader_outlives_shutdown_without_reopening_output() {
    let host = DeterministicRuntime::new();
    let cache = SharedKeyCache::new();
    let drops = Arc::new(AtomicUsize::new(0));
    let reader = block_on(cache.reader(Key(Arc::clone(&drops)), || 3)).unwrap();
    let retained = reader.clone();
    let (senders, received) = mpsc::channel();
    let store = Store::new(Vec::new(), Client { reader, senders }, host.runtime());
    let driver = host.clone();
    block_on(driver.run(async move {
        let task = store.send(ChildAction::Start).await.unwrap();
        host.run_until_stalled().unwrap();
        let output = received.try_recv().unwrap();
        assert_eq!(store.read(Clone::clone).await.unwrap(), vec![3]);
        store.shutdown().await.unwrap();
        task.finish().await;
        assert!(
            output
                .send(ChildAction::Value(3))
                .await
                .unwrap_err()
                .is_cancelled()
        );
    }))
    .unwrap();
    drop(cache);
    drop(driver);
    assert_eq!(retained.read(|value| *value).unwrap(), 3);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(retained);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}
