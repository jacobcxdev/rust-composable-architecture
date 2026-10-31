//! Checks owned stream items across child removal and same-identity replacement.

use std::cell::Cell;
use std::collections::VecDeque;
use std::future::poll_fn;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use futures_core::Stream;
use futures_executor::block_on;
use rca_core::{
    ActionPath, CaseStatePath, Effect, EffectSender, KeyedAction, KeyedActionPath, KeyedState,
    KeyedStatePath, OptionalStatePath, ReduceContext, Reducer, ReducerExt, ScopedStore, Store,
};
use rca_test::{ControlledStream, DeterministicRuntime, TryControlledSendError, controlled_stream};

/// A move-only value with a visible ID and an independent destruction counter.
struct Value(Cell<u64>, Arc<AtomicUsize>);

/// A move-only error that remains distinct from source cancellation.
struct DomainError(Cell<u64>, Arc<AtomicUsize>);

impl Drop for Value {
    fn drop(&mut self) {
        self.1.fetch_add(1, Ordering::SeqCst);
    }
}

impl Drop for DomainError {
    fn drop(&mut self) {
        self.1.fetch_add(1, Ordering::SeqCst);
    }
}

/// Keeps a typed domain error inside the stream payload.
type Item = Result<Box<Value>, Box<DomainError>>;

/// Records ordered domain results and counts source cancellations separately.
type ChildState = (Vec<Result<u64, u64>>, usize);

/// Starts one owned stream or delivers its domain item or cancellation.
enum ChildAction {
    Start,
    Item(Item),
    Cancelled,
}

/// Moves one finite stream plan into each started child effect.
struct Client {
    /// Supplies the old and replacement streams exactly once, without cloning receivers.
    streams: Arc<Mutex<VecDeque<ControlledStream<Item>>>>,
    /// Retains output senders for explicit attempts after their owner ends.
    senders: mpsc::Sender<EffectSender<ChildAction>>,
}

impl Reducer for Client {
    type State = ChildState;
    type Action = ChildAction;

    fn reduce(
        &mut self,
        state: &mut ChildState,
        action: &ChildAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<ChildAction> {
        match action {
            ChildAction::Start => {
                let mut stream = self.streams.lock().unwrap().pop_front().unwrap();
                let senders = self.senders.clone();
                // This is the application's explicit error-preserving consumer,
                // not a production forwarding adapter or tuple-factory promise.
                Effect::run(move |sender| async move {
                    senders.send(sender.clone()).unwrap();
                    while let Some(item) = poll_fn(|cx| Pin::new(&mut stream).poll_next(cx)).await {
                        let action = match item {
                            Ok(domain) => ChildAction::Item(domain),
                            Err(_) => ChildAction::Cancelled,
                        };
                        if sender.send(action).await.is_err() {
                            break;
                        }
                    }
                })
            }
            ChildAction::Item(item) => {
                state.0.push(match item {
                    Ok(value) => Ok(value.0.get()),
                    Err(error) => Err(error.0.get()),
                });
                Effect::none()
            }
            ChildAction::Cancelled => {
                state.1 += 1;
                Effect::none()
            }
        }
    }
}

/// Selects optional, case or keyed ownership for the same replacement protocol.
#[derive(Clone, Copy)]
enum Family {
    Optional,
    Case,
    Keyed,
}

/// Separates an absent screen from an active child with its own result history.
enum Screen {
    Off,
    On(ChildState),
}

/// Keeps all three composition families available to the selected scenario.
struct State {
    optional: Option<ChildState>,
    screen: Screen,
    rows: KeyedState<u8, ChildState>,
}

/// Routes stream output and performs explicit owner removal or replacement.
enum Action {
    Optional(ChildAction),
    Case(ChildAction),
    Row(KeyedAction<u8, ChildAction>),
    Remove(Family),
    Insert(Family),
}

/// Changes child lifetimes without fabricating stream output.
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
            Action::Insert(Family::Optional) => state.optional = Some((Vec::new(), 0)),
            Action::Insert(Family::Case) => state.screen = Screen::On((Vec::new(), 0)),
            Action::Insert(Family::Keyed) => state.rows.insert(1, (Vec::new(), 0)),
            _ => {}
        }
        Effect::none()
    }
}

// These local expressions reuse the same concrete paths for reduction and
// scoped access, without introducing generated items or hidden task work.
/// Borrows the optional child's state.
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
            |a: &Action| {
                if let Action::Optional(a) = a {
                    Some(a)
                } else {
                    None
                }
            },
            Action::Optional,
        )
    };
}

/// Borrows the case child only while the parent case is present.
macro_rules! case_state {
    () => {
        CaseStatePath::new(
            |s: &State| {
                if let Screen::On(s) = &s.screen {
                    Some(s)
                } else {
                    None
                }
            },
            |s: &mut State| {
                if let Screen::On(s) = &mut s.screen {
                    Some(s)
                } else {
                    None
                }
            },
        )
    };
}

/// Routes only case-child actions.
macro_rules! case_action {
    () => {
        ActionPath::new(
            |a: &Action| {
                if let Action::Case(a) = a {
                    Some(a)
                } else {
                    None
                }
            },
            Action::Case,
        )
    };
}

/// Borrows the keyed collection while preserving each entry's lifetime.
macro_rules! keyed_state {
    () => {
        KeyedStatePath::new(|s: &State| &s.rows, |s: &mut State| &mut s.rows)
    };
}

/// Preserves the key when extracting or embedding a child action.
macro_rules! keyed_action {
    () => {
        KeyedActionPath::new(
            |a: &Action| {
                if let Action::Row(a) = a {
                    Some(a)
                } else {
                    None
                }
            },
            Action::Row,
        )
    };
}

/// Captures the selected child's current public scope.
async fn scope(
    store: &Store<State, Action>,
    family: Family,
) -> ScopedStore<ChildState, ChildAction> {
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

/// Allows the small finite trace without making queue exhaustion the removal oracle.
fn capacity() -> NonZeroUsize {
    NonZeroUsize::new(3).unwrap()
}

/// Removes old work, rejects historical output, then verifies the replacement's ordered items.
#[expect(
    clippy::too_many_lines,
    reason = "keeps one generation's removal, stale-output rejection and replacement in order"
)]
fn check_child(family: Family) {
    let host = DeterministicRuntime::new();
    let drops = Arc::new(AtomicUsize::new(0));
    let (old_control, old_stream) = controlled_stream::<Item>(capacity());
    let (fresh_control, fresh_stream) = controlled_stream::<Item>(capacity());
    let streams = Arc::new(Mutex::new(VecDeque::from([old_stream, fresh_stream])));
    let (senders, received) = mpsc::channel();
    let client = || Client {
        streams: Arc::clone(&streams),
        senders: senders.clone(),
    };
    let reducer = Parent
        .if_some(optional_state!(), optional_action!(), client())
        .if_case(case_state!(), case_action!(), client())
        .for_each(keyed_state!(), keyed_action!(), client());
    let store = Store::new(
        State {
            optional: Some((Vec::new(), 0)),
            screen: Screen::On((Vec::new(), 0)),
            rows: [(1, (Vec::new(), 0))].into_iter().collect(),
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
        old_control
            .try_send(Ok(Box::new(Value(Cell::new(11), Arc::clone(&drops)))))
            .unwrap();
        host.run_until_stalled().unwrap();
        assert_eq!(old.read(Clone::clone).await.unwrap(), (vec![Ok(11)], 0));

        store
            .send(Action::Remove(family))
            .await
            .unwrap()
            .finish()
            .await;
        host.run_until_stalled().unwrap();
        task.finish().await;
        store
            .send(Action::Insert(family))
            .await
            .unwrap()
            .finish()
            .await;
        assert!(
            old.read(|state| state.1)
                .await
                .unwrap_err()
                .is_scope_ended()
        );
        let fresh = scope(&store, family).await;
        assert_eq!(fresh.read(Clone::clone).await.unwrap(), (vec![], 0));

        // Actual scoped stream ownership ends; producer rejection retains Err.
        let error = Box::new(DomainError(Cell::new(23), Arc::clone(&drops)));
        let address = std::ptr::from_ref(error.as_ref()).addr();
        let rejected = old_control.try_send(Err(error)).unwrap_err();
        assert!(matches!(
            &rejected,
            TryControlledSendError::ReceiverDropped(_)
        ));
        let Err(error) = rejected.into_inner() else {
            panic!("late producer Err must be retained");
        };
        assert_eq!(std::ptr::from_ref(error.as_ref()).addr(), address);
        assert_eq!(error.0.get(), 23);
        drop(error);

        // Retained already-granted output authority is not fresh root input.
        for item in [
            Ok(Box::new(Value(Cell::new(31), Arc::clone(&drops)))),
            Err(Box::new(DomainError(Cell::new(47), Arc::clone(&drops)))),
        ] {
            assert!(
                stale
                    .send(ChildAction::Item(item))
                    .await
                    .unwrap_err()
                    .is_cancelled()
            );
        }
        host.run_until_stalled().unwrap();
        assert_eq!(fresh.read(Clone::clone).await.unwrap(), (vec![], 0));
        assert_eq!(drops.load(Ordering::SeqCst), 4);

        let task = fresh.send(ChildAction::Start).await.unwrap();
        host.run_until_stalled().unwrap();
        fresh_control
            .try_send(Err(Box::new(DomainError(
                Cell::new(53),
                Arc::clone(&drops),
            ))))
            .unwrap();
        fresh_control
            .try_send(Ok(Box::new(Value(Cell::new(59), Arc::clone(&drops)))))
            .unwrap();
        fresh_control.close();
        task.finish().await;
        assert_eq!(
            fresh.read(Clone::clone).await.unwrap(),
            (vec![Err(53), Ok(59)], 0)
        );
        assert_eq!(drops.load(Ordering::SeqCst), 6);
        store.shutdown().await.unwrap();
    }))
    .unwrap();
}

// Missing generation fencing would commit stale typed items to a replacement.
#[test]
fn optional_stream_rejects_stale_items_before_same_child_returns() {
    check_child(Family::Optional);
}

#[test]
fn case_stream_rejects_stale_items_before_same_case_returns() {
    check_child(Family::Case);
}

#[test]
fn keyed_stream_rejects_stale_items_before_same_id_returns() {
    check_child(Family::Keyed);
}

// Catches swallowing domain failures or treating them as control cancellation.
#[test]
fn root_stream_commits_domain_failure_and_later_success_then_distinct_cancellation() {
    let host = DeterministicRuntime::new();
    let drops = Arc::new(AtomicUsize::new(0));
    let (control, stream) = controlled_stream::<Item>(capacity());
    let (senders, received) = mpsc::channel();
    let store = Store::new(
        (Vec::new(), 0),
        Client {
            streams: Arc::new(Mutex::new(VecDeque::from([stream]))),
            senders,
        },
        host.runtime(),
    );
    let driver = host.clone();
    block_on(driver.run(async move {
        let task = store.send(ChildAction::Start).await.unwrap();
        host.run_until_stalled().unwrap();
        let _sender = received.try_recv().unwrap();
        control
            .try_send(Err(Box::new(DomainError(
                Cell::new(11),
                Arc::clone(&drops),
            ))))
            .unwrap();
        control
            .try_send(Ok(Box::new(Value(Cell::new(23), Arc::clone(&drops)))))
            .unwrap();
        host.run_until_stalled().unwrap();
        assert_eq!(
            store.read(Clone::clone).await.unwrap(),
            (vec![Err(11), Ok(23)], 0)
        );
        control.cancel();
        task.finish().await;
        assert_eq!(
            store.read(Clone::clone).await.unwrap(),
            (vec![Err(11), Ok(23)], 1)
        );
        assert_eq!(drops.load(Ordering::SeqCst), 2);
        store.shutdown().await.unwrap();
    }))
    .unwrap();
}
