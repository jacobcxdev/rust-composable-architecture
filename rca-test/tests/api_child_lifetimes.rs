//! Checks scoped lifetime cancellation without revoking independently held clocks or fresh scopes.

use std::sync::{Arc, mpsc};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use futures_executor::block_on;
use rca_core::{
    ActionPath, CaseStatePath, Clock, Effect, EffectSender, KeyedAction, KeyedActionPath,
    KeyedState, KeyedStatePath, OptionalStatePath, ReduceContext, Reducer, ReducerExt, ScopedStore,
    Store,
};
use rca_test::DeterministicRuntime;

/// Selects the public composition family used by the shared lifetime scenario.
#[derive(Clone, Copy)]
enum Family {
    Optional,
    Case,
    Keyed,
}

/// Makes leaving and re-entering the same enum case observable.
enum Screen {
    Off,
    On(u8),
}

/// Holds independently removable optional, enum-case and keyed children.
struct State {
    optional: Option<u8>,
    screen: Screen,
    rows: KeyedState<u8, u8>,
}

enum ChildAction {
    Start,
    Response,
}

enum Action {
    Optional(ChildAction),
    Case(ChildAction),
    Row(KeyedAction<u8, ChildAction>),
    Remove(Family),
    Insert(Family),
}

/// Removes or reinserts one selected child without changing the other families.
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
            Action::Insert(Family::Optional) => state.optional = Some(0),
            Action::Insert(Family::Case) => state.screen = Screen::On(0),
            Action::Insert(Family::Keyed) => state.rows.insert(1, 0),
            _ => {}
        }
        Effect::none()
    }
}

/// Exposes a scoped sender and retains a capture while its timer is pending.
struct Client {
    /// Remains usable independently of a particular scoped effect's lifetime.
    clock: Clock,
    /// Lets the test distinguish cancellation from merely hiding an old output.
    capture: Arc<()>,
    /// Returns an escaped sender so the test can attempt stale output directly.
    senders: mpsc::Sender<EffectSender<ChildAction>>,
}

impl Reducer for Client {
    type State = u8;
    type Action = ChildAction;

    fn reduce(
        &mut self,
        state: &mut u8,
        action: &ChildAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<ChildAction> {
        match action {
            ChildAction::Start => {
                let wait = self.clock.sleep(Duration::from_secs(1));
                let capture = Arc::clone(&self.capture);
                let senders = self.senders.clone();
                Effect::run(move |sender| async move {
                    senders.send(sender.clone()).unwrap();
                    wait.await;
                    drop(capture);
                    let _ = sender.send(ChildAction::Response).await;
                })
            }
            ChildAction::Response => {
                *state += 1;
                Effect::none()
            }
        }
    }
}

// Each projection is a real public composition seam; the child/client is shared
// by keyed entries, not constructed afresh for each logical ID.
/// Projects the optional child without moving its state.
macro_rules! optional_state {
    () => {
        OptionalStatePath::new(
            |s: &State| s.optional.as_ref(),
            |s: &mut State| s.optional.as_mut(),
        )
    };
}

/// Extracts and embeds optional-child actions through their public path.
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

/// Projects only the active enum case.
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

/// Extracts and embeds enum-case child actions through their public path.
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

/// Projects keyed storage without selecting a particular child.
macro_rules! keyed_state {
    () => {
        KeyedStatePath::new(|s: &State| &s.rows, |s: &mut State| &mut s.rows)
    };
}

/// Keeps each child's key attached while routing its action.
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

/// Acquires the selected child through its family's public scope API.
///
/// # Panics
///
/// Panics if the fixture's child is absent or the store cannot acquire it.
async fn scope(store: &Store<State, Action>, family: Family) -> ScopedStore<u8, ChildAction> {
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

/// Checks cancellation, stale authority and independent clock use across one replacement.
fn check_clock_client(family: Family) {
    let host = DeterministicRuntime::new();
    let clock = host.runtime().clock();
    let capture = Arc::new(());
    let (senders, received) = mpsc::channel();
    let client = || Client {
        clock: clock.clone(),
        capture: Arc::clone(&capture),
        senders: senders.clone(),
    };
    let reducer = Parent
        .if_some(optional_state!(), optional_action!(), client())
        .if_case(case_state!(), case_action!(), client())
        .for_each(keyed_state!(), keyed_action!(), client());
    let store = Store::new(
        State {
            optional: Some(0),
            screen: Screen::On(0),
            rows: [(1, 0)].into_iter().collect(),
        },
        reducer,
        host.runtime(),
    );
    let driver = host.clone();
    block_on(driver.run(async move {
        let old = scope(&store, family).await;
        let mut values = old.observe(|value| *value).await.unwrap();
        assert_eq!(values.next().await, Some(0));
        let baseline = Arc::strong_count(&capture);
        let task = old.send(ChildAction::Start).await.unwrap();
        host.run_until_stalled().unwrap();
        let stale = received.try_recv().unwrap();
        assert_eq!(host.pending_sleeps().len(), 1);
        assert_eq!(Arc::strong_count(&capture), baseline + 1);

        store
            .send(Action::Remove(family))
            .await
            .unwrap()
            .finish()
            .await;
        host.run_until_stalled().unwrap();
        task.finish().await;
        assert!(host.pending_sleeps().is_empty());
        assert_eq!(Arc::strong_count(&capture), baseline);

        store
            .send(Action::Insert(family))
            .await
            .unwrap()
            .finish()
            .await;
        assert!(old.read(|value| *value).await.unwrap_err().is_scope_ended());
        assert!(
            old.send(ChildAction::Response)
                .await
                .unwrap_err()
                .is_scope_ended()
        );
        assert_eq!(values.next().await, None);
        assert!(
            stale
                .send(ChildAction::Response)
                .await
                .unwrap_err()
                .is_cancelled()
        );

        let fresh = scope(&store, family).await;
        let mut current = fresh.observe(|value| *value).await.unwrap();
        assert_eq!(current.next().await, Some(0));
        let fresh_task = fresh.send(ChildAction::Start).await.unwrap();
        host.run_until_stalled().unwrap();
        assert_eq!(host.pending_sleeps().len(), 1);
        // The independently retained clock is still usable: generation fencing
        // revokes scoped work/output, not the clock or newly authorised input.
        let mut independent = clock.sleep(Duration::from_secs(1));
        assert_eq!(
            independent
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop())),
            Poll::Pending
        );
        assert_eq!(host.pending_sleeps().len(), 2);
        host.advance(Duration::from_secs(1)).unwrap();
        independent.await;
        fresh_task.finish().await;
        assert_eq!(current.next().await, Some(1));
        assert_eq!(fresh.read(|value| *value).await.unwrap(), 1);
        assert!(host.pending_sleeps().is_empty());
        assert_eq!(Arc::strong_count(&capture), baseline);
        store.shutdown().await.unwrap();
    }))
    .unwrap();
}

// Missing generation cancellation would retain the wait/capture or deliver old
// output; a global clock revoke would prevent the fresh/independent waits.
#[test]
fn optional_clock_client_work_ends_without_revoking_clock() {
    check_clock_client(Family::Optional);
}

#[test]
fn case_clock_client_work_and_reader_end_before_same_case_returns() {
    check_clock_client(Family::Case);
}

#[test]
fn keyed_clock_client_work_and_reader_end_before_same_id_returns() {
    check_clock_client(Family::Keyed);
}

// A queued observation must not select an equal replacement through an old handle.
#[test]
fn queued_observation_registration_rejects_a_replaced_child_without_selecting_it() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Completes a finite lifecycle operation without relying on external wakes.
    ///
    /// # Panics
    ///
    /// Panics if the operation remains pending after 64 client polls.
    fn finish_without_external_wake<F: Future>(future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        let mut context = Context::from_waker(Waker::noop());
        for _ in 0..64 {
            if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
                return output;
            }
        }
        panic!("scoped lifecycle operation did not finish within 64 client polls");
    }

    let host = DeterministicRuntime::new();
    let store = Store::new(
        State {
            optional: Some(0),
            screen: Screen::Off,
            rows: [(1, 0)].into_iter().collect(),
        },
        Parent,
        host.runtime(),
    );
    let old = finish_without_external_wake(host.run(scope(&store, Family::Optional))).unwrap();
    let removal = store.try_send(Action::Remove(Family::Optional)).unwrap();
    let insertion = store.try_send(Action::Insert(Family::Optional)).unwrap();
    let selections = Arc::new(AtomicUsize::new(0));
    let selected = Arc::clone(&selections);
    let capture = Arc::new(());
    let held_capture = Arc::downgrade(&capture);
    let mut registration = Box::pin(old.observe(move |value| {
        std::hint::black_box(&capture);
        selected.fetch_add(1, Ordering::SeqCst);
        *value
    }));
    assert!(
        registration
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );

    host.run_until_stalled().unwrap();
    drop(
        finish_without_external_wake(host.run(removal.finish()))
            .unwrap()
            .unwrap(),
    );
    drop(
        finish_without_external_wake(host.run(insertion.finish()))
            .unwrap()
            .unwrap(),
    );
    let Poll::Ready(result) = registration
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    else {
        panic!("actor must resolve the rejected observation registration");
    };
    let error = result.unwrap_err();
    drop(registration);
    assert!(error.is_scope_ended());
    assert!(!error.is_store_stopped());
    assert_eq!(selections.load(Ordering::SeqCst), 0);
    assert!(held_capture.upgrade().is_none());
    assert!(!store.is_closed());

    let fresh = finish_without_external_wake(host.run(scope(&store, Family::Optional))).unwrap();
    let selected = Arc::clone(&selections);
    let mut observation = finish_without_external_wake(host.run(fresh.observe(move |value| {
        selected.fetch_add(1, Ordering::SeqCst);
        *value
    })))
    .unwrap()
    .unwrap();
    assert_eq!(
        finish_without_external_wake(host.run(observation.next())).unwrap(),
        Some(0)
    );
    assert_eq!(selections.load(Ordering::SeqCst), 1);
    drop(observation);
    finish_without_external_wake(host.run(store.shutdown()))
        .unwrap()
        .unwrap();
}
