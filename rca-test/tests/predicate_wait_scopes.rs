//! Checks predicate waits at send, observation, cancellation and scope-ending boundaries.

use std::cell::Cell;
use std::pin::Pin;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::task::{Context, Poll, Waker};

use futures_executor::block_on;
use rca_core::{
    Effect, PredicateWait, PredicateWaitOutcome as Outcome, ReduceContext, Reducer, Store,
};
use rca_test::DeterministicRuntime;

/// Advances only the caller future so each actor-admission gap stays observable.
fn poll<F: Future + Unpin>(future: &mut F) -> Poll<F::Output> {
    Pin::new(future).poll(&mut Context::from_waker(Waker::noop()))
}

/// Runs one caller operation without advancing virtual time.
fn drive<F: Future + Send>(host: &DeterministicRuntime, future: F) -> F::Output
where
    F::Output: Send,
{
    block_on(host.run(future)).unwrap()
}

/// Holds a non-`Sync` value without implementing `Clone` or `Debug` for the state.
struct State(Cell<bool>);

/// Sends owned state without adding shared-reference or cloning requirements.
struct Set(Cell<bool>);

/// Commits the requested value without creating effects.
struct Toggle;
impl Reducer for Toggle {
    type State = State;
    type Action = Set;
    fn reduce(
        &mut self,
        state: &mut State,
        action: &Set,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Set> {
        state.0.set(action.0.get());
        Effect::none()
    }
}

/// Builds the non-`Sync` domain used by root and required-child waits.
fn store(host: &DeterministicRuntime, value: bool) -> Store<State, Set> {
    Store::new(State(Cell::new(value)), Toggle, host.runtime())
}

/// Commits a value through the public API and discards its effect-free task handle.
fn set(host: &DeterministicRuntime, store: &Store<State, Set>, value: bool) {
    drop(drive(host, store.send(Set(Cell::new(value)))).unwrap());
}

/// Consumes registration while requiring the selected predicate to remain true.
fn register(host: &DeterministicRuntime, wait: &mut PredicateWait) {
    assert_eq!(poll(wait), Poll::Pending);
    host.run_until_stalled().unwrap();
    assert_eq!(poll(wait), Poll::Pending);
}

/// Reads the actor's observation count after earlier queued work completes.
fn observations(host: &DeterministicRuntime, store: &Store<State, Set>) -> usize {
    drive(host, store.runtime_snapshot())
        .unwrap()
        .observation_count
}

/// Leaves the initial predicate reply unread; sends first consume their commit receipt.
fn buffer_registration(host: &DeterministicRuntime, wait: &mut PredicateWait, sending: bool) {
    assert_eq!(poll(wait), Poll::Pending);
    host.run_until_stalled().unwrap();
    if sending {
        assert_eq!(poll(wait), Poll::Pending);
        host.run_until_stalled().unwrap();
    }
}

// A completed take is not callback admission: root closure still revokes its value.
#[test]
fn callback_rechecks_root_authority_after_publication_take() {
    for close_before_callback in [false, true] {
        let host = DeterministicRuntime::new();
        let store = store(&host, false);
        let observation = drive(&host, store.observe(|state| state.0.get())).unwrap();
        set(&host, &store, true);
        let calls = Cell::new(0);
        let mut callback = Box::pin(observation.on_change(|value| {
            assert!(value);
            calls.set(calls.get() + 1);
        }));
        assert_eq!(poll(&mut callback), Poll::Pending);
        host.run_until_stalled().unwrap();
        assert_eq!(calls.get(), 0);
        if close_before_callback {
            drive(&host, store.shutdown()).unwrap();
        }
        assert_eq!(poll(&mut callback), Poll::Ready(!close_before_callback));
        assert_eq!(calls.get(), usize::from(!close_before_callback));
        if !close_before_callback {
            drive(&host, store.shutdown()).unwrap();
        }
    }
}

// Catches accepting a committed false value after closure before root registration.
#[test]
fn root_send_shutdown_between_commit_and_registration_is_not_success() {
    for (close_before_registration, expected, expected_calls) in [
        (false, Outcome::Satisfied, 1),
        (true, Outcome::StoreStopped, 0),
    ] {
        let host = DeterministicRuntime::new();
        let store = store(&host, true);
        let calls = Arc::new(AtomicUsize::new(0));
        let predicate_calls = Arc::clone(&calls);
        let mut wait = store.send_and_wait_while(Set(Cell::new(false)), move |state| {
            predicate_calls.fetch_add(1, Ordering::SeqCst);
            state.0.get()
        });

        assert_eq!(poll(&mut wait), Poll::Pending);
        host.run_until_stalled().unwrap();

        // Inspect the committed state without consuming the wait's send receipt.
        let mut committed = Box::pin(store.read(|state| state.0.get()));
        assert!(poll(&mut committed).is_pending());
        host.run_until_stalled().unwrap();
        assert_eq!(poll(&mut committed).map(Result::unwrap), Poll::Ready(false));
        drop(committed);
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        if close_before_registration {
            let mut shutdown = Box::pin(store.shutdown());
            assert!(poll(&mut shutdown).is_pending());
            host.run_until_stalled().unwrap();
            assert_eq!(poll(&mut shutdown).map(Result::unwrap), Poll::Ready(()));
        } else {
            assert_eq!(poll(&mut wait), Poll::Pending);
            host.run_until_stalled().unwrap();
        }

        assert_eq!(poll(&mut wait), Poll::Ready(expected));
        assert_eq!(calls.load(Ordering::SeqCst), expected_calls);

        if !close_before_registration {
            let mut shutdown = Box::pin(store.shutdown());
            assert!(poll(&mut shutdown).is_pending());
            host.run_until_stalled().unwrap();
            assert_eq!(poll(&mut shutdown).map(Result::unwrap), Poll::Ready(()));
        }
    }
}

// R1: root closure must beat initial and completed-take false, on both APIs.
#[test]
fn buffered_false_after_root_shutdown_is_not_success() {
    for required in [false, true] {
        for sending in [false, true] {
            for taking in [false, true] {
                let host = DeterministicRuntime::new();
                let store = store(&host, taking);
                let mut wait = if required {
                    let child = store.scope(
                        rca_core::StatePath::new(|value: &State| value, |value| value),
                        rca_core::ActionPath::new(|action: &Set| Some(action), |action| action),
                    );
                    if sending {
                        child.send_and_wait_while(Set(Cell::new(taking)), |state| state.0.get())
                    } else {
                        child.wait_while(|state| state.0.get())
                    }
                } else if sending {
                    store.send_and_wait_while(Set(Cell::new(taking)), |state| state.0.get())
                } else {
                    store.wait_while(|state| state.0.get())
                };
                buffer_registration(&host, &mut wait, sending);
                if taking {
                    assert_eq!(poll(&mut wait), Poll::Pending);
                    set(&host, &store, false);
                    assert_eq!(poll(&mut wait), Poll::Pending);
                    host.run_until_stalled().unwrap();
                }
                drive(&host, store.shutdown()).unwrap();
                assert_eq!(
                    drive(&host, &mut wait),
                    Outcome::StoreStopped,
                    "required: {required}, sending: {sending}, taking: {taking}"
                );
                wait.cancel();
                assert_eq!(drive(&host, &mut wait), Outcome::StoreStopped);
            }
        }
    }
}

// R1: a completed take must not retain the root or hide its abortive closure.
#[test]
fn buffered_false_take_after_last_root_drop_is_not_success() {
    for sending in [false, true] {
        let host = DeterministicRuntime::new();
        let store = store(&host, true);
        let mut wait = if sending {
            store.send_and_wait_while(Set(Cell::new(true)), |state| state.0.get())
        } else {
            store.wait_while(|state| state.0.get())
        };
        buffer_registration(&host, &mut wait, sending);
        assert_eq!(poll(&mut wait), Poll::Pending);
        set(&host, &store, false);
        assert_eq!(poll(&mut wait), Poll::Pending);
        host.run_until_stalled().unwrap();
        drop(store);
        host.run_until_stalled().unwrap();
        assert_eq!(drive(&host, wait), Outcome::StoreStopped);
    }
}

// R1 positive control: live false succeeds and observed success stays sticky.
#[test]
fn live_buffered_false_succeeds_and_observed_success_survives_shutdown_and_cancel() {
    for sending in [false, true] {
        for taking in [false, true] {
            let host = DeterministicRuntime::new();
            let store = store(&host, taking);
            let mut wait = if sending {
                store.send_and_wait_while(Set(Cell::new(taking)), |state| state.0.get())
            } else {
                store.wait_while(|state| state.0.get())
            };
            buffer_registration(&host, &mut wait, sending);
            if taking {
                assert_eq!(poll(&mut wait), Poll::Pending);
                set(&host, &store, false);
                assert_eq!(poll(&mut wait), Poll::Pending);
                host.run_until_stalled().unwrap();
            }
            assert_eq!(drive(&host, &mut wait), Outcome::Satisfied);
            drive(&host, store.shutdown()).unwrap();
            wait.cancel();
            assert_eq!(poll(&mut wait), Poll::Ready(Outcome::Satisfied));
        }
    }
}

// Catches stale pre-send false and accidentally waiting for only future changes.
#[test]
fn send_reads_post_commit_state_and_later_false_completes() {
    let host = DeterministicRuntime::new();
    let store = store(&host, false);
    let mut wait = store.send_and_wait_while(Set(Cell::new(true)), |state| state.0.get());
    register(&host, &mut wait);
    host.run_until_stalled().unwrap();
    assert_eq!(poll(&mut wait), Poll::Pending);
    assert_eq!(observations(&host, &store), 1);
    set(&host, &store, false);
    assert_eq!(drive(&host, &mut wait), Outcome::Satisfied);
    assert_eq!(observations(&host, &store), 0);
    wait.cancel();
    assert_eq!(poll(&mut wait), Poll::Ready(Outcome::Satisfied));
}

// Catches a transition latch silently replacing documented latest-state waiting.
#[test]
fn transient_false_is_coalesced_until_a_consumed_false() {
    let host = DeterministicRuntime::new();
    let store = store(&host, true);
    let mut wait = store.wait_while(|state| state.0.get());
    register(&host, &mut wait);
    set(&host, &store, false);
    set(&host, &store, true);
    assert_eq!(poll(&mut wait), Poll::Pending);
    host.run_until_stalled().unwrap();
    assert_eq!(poll(&mut wait), Poll::Pending);
    set(&host, &store, false);
    assert_eq!(drive(&host, wait), Outcome::Satisfied);
    assert_eq!(observations(&host, &store), 0);
}

// Catches a check-then-register gap between send commit and observation admission.
#[test]
fn current_registration_snapshot_covers_transition_after_commit() {
    let host = DeterministicRuntime::new();
    let store = store(&host, false);
    let mut wait = store.send_and_wait_while(Set(Cell::new(true)), |state| state.0.get());
    assert_eq!(poll(&mut wait), Poll::Pending);
    host.run_until_stalled().unwrap(); // Send committed, receipt not consumed.
    set(&host, &store, false);
    assert_eq!(drive(&host, wait), Outcome::Satisfied);
    assert_eq!(observations(&host, &store), 0);
}

// Catches an eager constructor, retained observation, or double completion.
#[test]
fn cancel_before_poll_never_selects_or_reduces() {
    let host = DeterministicRuntime::new();
    let store = store(&host, true);
    let calls = Arc::new(AtomicUsize::new(0));
    let selected = Arc::clone(&calls);
    let mut wait = store.send_and_wait_while(Set(Cell::new(false)), move |_| {
        selected.fetch_add(1, Ordering::SeqCst);
        true
    });
    wait.cancel();
    wait.cancel();
    assert_eq!(poll(&mut wait), Poll::Ready(Outcome::Cancelled));
    host.run_until_stalled().unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(drive(&host, store.read(|state| state.0.get())).unwrap());
    assert_eq!(observations(&host, &store), 0);
}

// Catches constructing an owned wait eagerly admitting registration or action.
#[test]
fn ordinary_unpolled_drop_has_no_registration_or_action_side_effects() {
    let host = DeterministicRuntime::new();
    let store = store(&host, true);
    let wait = store.send_and_wait_while(Set(Cell::new(false)), |_| panic!("unpolled predicate"));
    drop(wait);
    drop(store.wait_while(|_| panic!("unpolled observation")));
    host.run_until_stalled().unwrap();
    assert!(drive(&host, store.read(|value| value.0.get())).unwrap());
    assert_eq!(observations(&host, &store), 0);
}

// Catches cancellation that drops only the receiver and leaves the registration alive.
#[test]
fn cancel_during_each_registration_phase_retires_observation() {
    for phase in 0..3 {
        let host = DeterministicRuntime::new();
        let store = store(&host, true);
        let calls = Arc::new(AtomicUsize::new(0));
        let selected = Arc::clone(&calls);
        let mut wait = store.wait_while(move |state| {
            selected.fetch_add(1, Ordering::SeqCst);
            state.0.get()
        });
        assert_eq!(poll(&mut wait), Poll::Pending);
        if phase > 0 {
            host.run_until_stalled().unwrap();
        }
        if phase > 1 {
            assert_eq!(poll(&mut wait), Poll::Pending);
        }
        wait.cancel();
        assert_eq!(poll(&mut wait), Poll::Ready(Outcome::Cancelled));
        host.run_until_stalled().unwrap();
        let before = calls.load(Ordering::SeqCst);
        set(&host, &store, false);
        assert_eq!(calls.load(Ordering::SeqCst), before);
        assert_eq!(observations(&host, &store), 0);
    }
}

// Catches eager cancellation of another admitted action or a leaked pending send.
#[test]
fn cancel_while_admission_is_full_or_before_actor_decision_suppresses_only_owned_action() {
    for blocked in [false, true] {
        let host = DeterministicRuntime::new();
        let store = Store::builder(State(Cell::new(true)), Toggle, host.runtime())
            .mailbox_capacity(1)
            .build()
            .unwrap();
        let ordinary = blocked.then(|| store.try_send(Set(Cell::new(true))).unwrap());
        let mut wait = store.send_and_wait_while(Set(Cell::new(false)), |state| state.0.get());
        assert_eq!(poll(&mut wait), Poll::Pending);
        wait.cancel();
        assert_eq!(poll(&mut wait), Poll::Ready(Outcome::Cancelled));
        host.run_until_stalled().unwrap();
        if let Some(receipt) = ordinary {
            drop(drive(&host, receipt.finish()).unwrap());
        }
        assert!(drive(&host, store.read(|state| state.0.get())).unwrap());
        assert_eq!(observations(&host, &store), 0);
    }
}

// Catches registration admitted after its owner cancels while the mailbox is full.
#[test]
fn cancel_during_blocked_registration_admission_drops_selector() {
    let host = DeterministicRuntime::new();
    let store = Store::builder(State(Cell::new(true)), Toggle, host.runtime())
        .mailbox_capacity(1)
        .build()
        .unwrap();
    let ordinary = store.try_send(Set(Cell::new(true))).unwrap();
    let selected = Arc::new(AtomicUsize::new(0));
    let calls = Arc::clone(&selected);
    let mut wait = store.wait_while(move |_| {
        calls.fetch_add(1, Ordering::SeqCst);
        true
    });
    assert_eq!(poll(&mut wait), Poll::Pending);
    wait.cancel();
    assert_eq!(poll(&mut wait), Poll::Ready(Outcome::Cancelled));
    drop(drive(&host, ordinary.finish()).unwrap());
    assert_eq!(selected.load(Ordering::SeqCst), 0);
    assert_eq!(observations(&host, &store), 0);
}

// Catches borrowed/non-Send futures and hidden root retention during long waits.
#[test]
fn owned_wait_transports_without_domain_clone_debug_sync_and_does_not_retain_root() {
    fn transport<T: Send + 'static>(value: T) -> T {
        value
    }
    let host = DeterministicRuntime::new();
    let store = store(&host, true);
    let mut wait = transport(store.wait_while(|state| state.0.get()));
    assert!(format!("{wait:?}").contains("PredicateWait"));
    register(&host, &mut wait);
    drop(store);
    host.run_until_stalled().unwrap();
    assert_eq!(drive(&host, wait), Outcome::StoreStopped);
}

// Catches treating root closure before/during registration as satisfaction.
#[test]
fn shutdown_before_or_after_registration_reports_root_closure() {
    for registered in [false, true] {
        let host = DeterministicRuntime::new();
        let store = store(&host, true);
        let mut wait = store.wait_while(|state| state.0.get());
        if registered {
            register(&host, &mut wait);
        }
        drive(&host, store.shutdown()).unwrap();
        assert_eq!(drive(&host, wait), Outcome::StoreStopped);
    }
}

/// Separates work owned by a selected send from independent sibling work.
mod causal {
    use super::*;
    use rca_core::{ActionTask, EffectSender};
    use std::sync::mpsc;

    /// Identifies unfinished effects independently of their predicate values.
    enum Action {
        Start(u8, bool),
        Set(bool),
    }

    /// Exposes each effect's output handle and an independent drop counter.
    struct Client {
        outputs: mpsc::Sender<(u8, EffectSender<Action>)>,
        retired: Arc<AtomicUsize>,
    }

    /// Counts actual effect destruction, not a cancellation request alone.
    struct Retire(Arc<AtomicUsize>);
    impl Drop for Retire {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    impl Reducer for Client {
        type State = bool;
        type Action = Action;
        fn reduce(
            &mut self,
            state: &mut bool,
            action: &Action,
            _: &mut ReduceContext<'_>,
        ) -> Effect<Action> {
            match action {
                Action::Set(value) => {
                    *state = *value;
                    Effect::none()
                }
                Action::Start(id, value) => {
                    *state = *value;
                    let id = *id;
                    let outputs = self.outputs.clone();
                    let guard = Retire(Arc::clone(&self.retired));
                    Effect::run(move |sender| async move {
                        let _guard = guard;
                        outputs.send((id, sender)).unwrap();
                        std::future::pending::<()>().await;
                    })
                }
            }
        }
    }

    /// Starts a named sibling and proves its captured handle belongs to that send.
    fn sibling(
        host: &DeterministicRuntime,
        store: &Store<bool, Action>,
        received: &mpsc::Receiver<(u8, EffectSender<Action>)>,
    ) -> (ActionTask, EffectSender<Action>) {
        let task = drive(host, store.send(Action::Start(1, true))).unwrap();
        host.run_until_stalled().unwrap();
        let (id, sender) = received.try_recv().unwrap();
        assert_eq!(id, 1);
        (task, sender)
    }

    // Catches losing the task when cancellation drops an unconsumed commit receipt.
    #[test]
    fn cancellation_in_receipt_gap_or_observing_retires_only_selected_causal_work() {
        for observing in [false, true] {
            let host = DeterministicRuntime::new();
            let (outputs, received) = mpsc::channel();
            let retired = Arc::new(AtomicUsize::new(0));
            let store = Store::new(
                true,
                Client {
                    outputs,
                    retired: Arc::clone(&retired),
                },
                host.runtime(),
            );
            let (sibling, sibling_sender) = sibling(&host, &store, &received);
            let mut wait = store.send_and_wait_while(Action::Start(2, true), |value| *value);
            assert_eq!(poll(&mut wait), Poll::Pending);
            host.run_until_stalled().unwrap();
            let (id, selected_sender) = received.try_recv().unwrap();
            assert_eq!(id, 2);
            if observing {
                assert_eq!(poll(&mut wait), Poll::Pending);
                host.run_until_stalled().unwrap();
                assert_eq!(poll(&mut wait), Poll::Pending);
            }
            let mut queued = Box::pin(selected_sender.send(Action::Set(false)));
            assert!(poll(&mut queued).is_pending());
            wait.cancel();
            wait.cancel();
            assert_eq!(poll(&mut wait), Poll::Ready(Outcome::Cancelled));
            host.run_until_stalled().unwrap();
            assert!(drive(&host, queued).unwrap_err().is_cancelled());
            assert!(drive(&host, store.read(|value| *value)).unwrap());
            assert_eq!(retired.load(Ordering::SeqCst), 1);
            assert!(!sibling.is_cancelled());
            assert!(!sibling.is_finished());
            assert!(
                drive(&host, selected_sender.send(Action::Set(false)))
                    .unwrap_err()
                    .is_cancelled()
            );
            drive(&host, sibling_sender.send(Action::Set(false))).unwrap();
            let snapshot = drive(&host, store.runtime_snapshot()).unwrap();
            assert_eq!(snapshot.live_task_ids.len(), 1);
            assert_eq!(snapshot.observation_count, 0);
            assert!(!drive(&host, store.read(|value| *value)).unwrap());
            drive(&host, store.shutdown()).unwrap();
            host.run_until_stalled().unwrap();
            assert_eq!(retired.load(Ordering::SeqCst), 2);
            assert!(sibling.is_finished());
        }
    }

    // Catches accidental cancellation by ordinary Drop at any acquired stage.
    #[test]
    fn ordinary_drop_keeps_admitted_actions_and_causal_work_alive() {
        for phase in 0..4 {
            let host = DeterministicRuntime::new();
            let (outputs, received) = mpsc::channel();
            let retired = Arc::new(AtomicUsize::new(0));
            let store = Store::new(
                true,
                Client {
                    outputs,
                    retired: Arc::clone(&retired),
                },
                host.runtime(),
            );
            let mut wait = store.send_and_wait_while(Action::Start(2, true), |value| *value);
            assert_eq!(poll(&mut wait), Poll::Pending);
            if phase > 0 {
                host.run_until_stalled().unwrap();
            }
            if phase > 1 {
                assert_eq!(poll(&mut wait), Poll::Pending);
            }
            if phase > 2 {
                host.run_until_stalled().unwrap();
                assert_eq!(poll(&mut wait), Poll::Pending);
            }
            drop(wait);
            host.run_until_stalled().unwrap();
            let (id, sender) = received.try_recv().unwrap();
            assert_eq!(id, 2);
            assert_eq!(retired.load(Ordering::SeqCst), 0);
            drive(&host, sender.send(Action::Set(false))).unwrap();
            let snapshot = drive(&host, store.runtime_snapshot()).unwrap();
            assert_eq!(snapshot.live_task_ids.len(), 1);
            assert_eq!(snapshot.observation_count, 0);
            drive(&host, store.shutdown()).unwrap();
            host.run_until_stalled().unwrap();
            assert_eq!(retired.load(Ordering::SeqCst), 1);
        }
    }

    // Catches success implicitly joining or cancelling an otherwise unfinished effect.
    #[test]
    fn success_does_not_finish_or_cancel_remaining_causal_work() {
        let host = DeterministicRuntime::new();
        let (outputs, received) = mpsc::channel();
        let retired = Arc::new(AtomicUsize::new(0));
        let store = Store::new(
            true,
            Client {
                outputs,
                retired: Arc::clone(&retired),
            },
            host.runtime(),
        );
        let mut wait = store.send_and_wait_while(Action::Start(2, false), |value| *value);
        assert_eq!(drive(&host, &mut wait), Outcome::Satisfied);
        wait.cancel();
        let (_, sender) = received.try_recv().unwrap();
        drive(&host, sender.send(Action::Set(true))).unwrap();
        let snapshot = drive(&host, store.runtime_snapshot()).unwrap();
        assert_eq!(snapshot.observation_count, 0);
        assert_eq!(snapshot.live_task_ids.len(), 1);
        assert_eq!(retired.load(Ordering::SeqCst), 0);
        drive(&host, store.shutdown()).unwrap();
        host.run_until_stalled().unwrap();
        assert_eq!(retired.load(Ordering::SeqCst), 1);
    }
}

/// Repeats lifetime-sensitive waits through optional, case and keyed child APIs.
mod scopes {
    use super::*;
    use rca_core::{
        ActionPath, CaseStatePath, KeyedAction, KeyedActionPath, KeyedState, KeyedStatePath,
        OptionalStatePath, ReducerExt, ScopedStore, StatePath,
    };

    /// Selects a real lifetime-owning wrapper, not a mocked scope classifier.
    #[derive(Clone, Copy, Debug)]
    enum Family {
        Optional,
        Case,
        Keyed,
    }

    /// Represents an active or absent enum-case child.
    enum Screen {
        Off,
        On(bool),
    }

    /// Keeps three equal-valued children in independent lifetimes.
    struct State {
        optional: Option<bool>,
        screen: Screen,
        rows: KeyedState<u8, bool>,
    }

    /// Routes child values or removes and reinserts one selected lifetime.
    enum Action {
        Optional(bool),
        Case(bool),
        Row(KeyedAction<u8, bool>),
        Remove(Family),
        Insert(Family),
    }

    /// Changes child presence without handling the child's predicate action.
    struct Parent;

    /// Commits one predicate value without starting work.
    struct Child;
    impl Reducer for Child {
        type State = bool;
        type Action = bool;
        fn reduce(
            &mut self,
            state: &mut bool,
            action: &bool,
            _: &mut ReduceContext<'_>,
        ) -> Effect<bool> {
            *state = *action;
            Effect::none()
        }
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
            match action {
                Action::Remove(Family::Optional) => state.optional = None,
                Action::Remove(Family::Case) => state.screen = Screen::Off,
                Action::Remove(Family::Keyed) => {
                    state.rows.remove(&1);
                }
                Action::Insert(Family::Optional) => state.optional = Some(true),
                Action::Insert(Family::Case) => state.screen = Screen::On(true),
                Action::Insert(Family::Keyed) => state.rows.insert(1, true),
                _ => {}
            }
            Effect::none()
        }
    }

    /// Recreates the optional projection without naming closure types.
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

    /// Selects the active enum case without constructing replacement values.
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

    /// Routes only active-case actions.
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

    /// Recreates the keyed projection for composition and scoped handles.
    macro_rules! keyed_state {
        () => {
            KeyedStatePath::new(|s: &State| &s.rows, |s: &mut State| &mut s.rows)
        };
    }

    /// Preserves the row ID while routing a child's predicate action.
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

    /// Starts each wrapper with one live child whose predicate is true.
    fn store(host: &DeterministicRuntime) -> Store<State, Action> {
        Store::new(
            State {
                optional: Some(true),
                screen: Screen::On(true),
                rows: [(1, true)].into_iter().collect(),
            },
            Parent
                .if_some(optional_state!(), optional_action!(), Child)
                .if_case(case_state!(), case_action!(), Child)
                .for_each(keyed_state!(), keyed_action!(), Child),
            host.runtime(),
        )
    }

    /// Acquires the selected child's current lifetime through its public API.
    fn scope(
        host: &DeterministicRuntime,
        store: &Store<State, Action>,
        family: Family,
    ) -> ScopedStore<bool, bool> {
        match family {
            Family::Optional => drive(
                host,
                store.scope_some(optional_state!(), optional_action!()),
            )
            .unwrap(),
            Family::Case => drive(host, store.scope_case(case_state!(), case_action!())).unwrap(),
            Family::Keyed => {
                drive(host, store.scope_keyed(keyed_state!(), keyed_action!(), 1)).unwrap()
            }
        }
    }

    /// Sends directly from the root so the test need not retain a scoped handle.
    fn action(family: Family, value: bool) -> Action {
        match family {
            Family::Optional => Action::Optional(value),
            Family::Case => Action::Case(value),
            Family::Keyed => Action::Row(KeyedAction::new(1, value)),
        }
    }

    // R1: both buffered-value paths retain the original generation's ending.
    #[test]
    fn buffered_false_after_scope_or_root_end_is_not_success_or_rebound() {
        for family in [Family::Optional, Family::Case, Family::Keyed] {
            for sending in [false, true] {
                for taking in [false, true] {
                    for root_ended in [false, true] {
                        let host = DeterministicRuntime::new();
                        let store = store(&host);
                        let child = scope(&host, &store, family);
                        drop(drive(&host, child.send(taking)).unwrap());
                        let mut wait = if sending {
                            child.send_and_wait_while(taking, |value| *value)
                        } else {
                            child.wait_while(|value| *value)
                        };
                        buffer_registration(&host, &mut wait, sending);
                        if taking {
                            assert_eq!(poll(&mut wait), Poll::Pending);
                            drop(drive(&host, child.send(false)).unwrap());
                            assert_eq!(poll(&mut wait), Poll::Pending);
                            host.run_until_stalled().unwrap();
                        }
                        drop(child);
                        drop(drive(&host, store.send(Action::Remove(family))).unwrap());
                        drop(drive(&host, store.send(Action::Insert(family))).unwrap());
                        if root_ended {
                            drive(&host, store.shutdown()).unwrap();
                        }
                        let expected = if root_ended {
                            Outcome::StoreStopped
                        } else {
                            Outcome::ScopeEnded
                        };
                        assert_eq!(
                            drive(&host, &mut wait),
                            expected,
                            "case (family, sending, taking, root ended): {:?}",
                            (family, sending, taking, root_ended)
                        );
                        if !root_ended {
                            let fresh = scope(&host, &store, family);
                            assert_eq!(
                                drive(&host, fresh.send_and_wait_while(false, |value| *value)),
                                Outcome::Satisfied
                            );
                            assert_eq!(
                                drive(&host, store.runtime_snapshot())
                                    .unwrap()
                                    .observation_count,
                                0
                            );
                        }
                    }
                }
            }
        }
    }

    // R1 positive controls reject a classifier that treats every child as ended.
    #[test]
    fn live_scoped_buffered_false_succeeds_and_observed_success_stays_sticky() {
        for family in [Family::Optional, Family::Case, Family::Keyed] {
            for sending in [false, true] {
                for taking in [false, true] {
                    let host = DeterministicRuntime::new();
                    let store = store(&host);
                    let child = scope(&host, &store, family);
                    drop(drive(&host, child.send(taking)).unwrap());
                    let mut wait = if sending {
                        child.send_and_wait_while(taking, |value| *value)
                    } else {
                        child.wait_while(|value| *value)
                    };
                    buffer_registration(&host, &mut wait, sending);
                    if taking {
                        assert_eq!(poll(&mut wait), Poll::Pending);
                        drop(drive(&host, child.send(false)).unwrap());
                        assert_eq!(poll(&mut wait), Poll::Pending);
                        host.run_until_stalled().unwrap();
                    }
                    drop(child);
                    assert_eq!(drive(&host, &mut wait), Outcome::Satisfied);
                    drop(drive(&host, store.send(Action::Remove(family))).unwrap());
                    drive(&host, store.shutdown()).unwrap();
                    wait.cancel();
                    assert_eq!(poll(&mut wait), Poll::Ready(Outcome::Satisfied));
                }
            }
        }
    }

    // Catches weak-only child ownership and stale observation rebinding to replacements.
    #[test]
    fn optional_case_keyed_owned_waits_survive_handle_drop_but_end_on_reincarnation() {
        for family in [Family::Optional, Family::Case, Family::Keyed] {
            let host = DeterministicRuntime::new();
            let store = store(&host);
            let child = scope(&host, &store, family);
            let mut wait = child.wait_while(|value| *value);
            register(&host, &mut wait);
            drop(child);
            drop(drive(&host, store.send(action(family, false))).unwrap());
            assert_eq!(drive(&host, wait), Outcome::Satisfied);
            let old = scope(&host, &store, family);
            drop(drive(&host, old.send(true)).unwrap());
            let mut stale = old.wait_while(|value| *value);
            register(&host, &mut stale);
            drop(drive(&host, store.send(Action::Remove(family))).unwrap());
            drop(drive(&host, store.send(Action::Insert(family))).unwrap());
            assert_eq!(drive(&host, stale), Outcome::ScopeEnded);
            assert_eq!(
                drive(&host, old.send_and_wait_while(false, |value| *value)),
                Outcome::ScopeEnded
            );
            let fresh = scope(&host, &store, family);
            assert_eq!(
                drive(&host, fresh.send_and_wait_while(false, |value| *value)),
                Outcome::Satisfied
            );
            assert_eq!(
                drive(&host, store.runtime_snapshot())
                    .unwrap()
                    .observation_count,
                0
            );
            assert!(drive(&host, store.read(|_| true)).unwrap());
        }
    }

    // Catches invalid lifetime checked after deferred embedding or reduction.
    #[test]
    fn scope_ended_after_send_admission_is_explicit_and_does_not_reach_replacement() {
        for family in [Family::Optional, Family::Case, Family::Keyed] {
            let host = DeterministicRuntime::new();
            let store = store(&host);
            let old = scope(&host, &store, family);
            let remove = store.try_send(Action::Remove(family)).unwrap();
            let mut wait = old.send_and_wait_while(false, |value| *value);
            assert_eq!(poll(&mut wait), Poll::Pending);
            drop(drive(&host, remove.finish()).unwrap());
            assert_eq!(drive(&host, wait), Outcome::ScopeEnded);
            drop(drive(&host, store.send(Action::Insert(family))).unwrap());
            let fresh = scope(&host, &store, family);
            assert!(drive(&host, fresh.read(|value| *value)).unwrap());
        }
    }

    // Catches retaining root authority via the owned scoped wait or its classifier.
    #[test]
    fn scoped_wait_does_not_retain_root_and_root_closure_is_not_scope_ended() {
        for family in [Family::Optional, Family::Case, Family::Keyed] {
            let host = DeterministicRuntime::new();
            let store = store(&host);
            let child = scope(&host, &store, family);
            let mut wait = child.wait_while(|value| *value);
            register(&host, &mut wait);
            drop(child);
            drop(store);
            host.run_until_stalled().unwrap();
            assert_eq!(drive(&host, wait), Outcome::StoreStopped);
        }
    }

    // Catches inventing a scope-end outcome for a non-lifetime-qualified projection.
    #[test]
    fn required_child_wait_supports_send_and_reports_root_closure() {
        let host = DeterministicRuntime::new();
        let store = Store::new(true, Child, host.runtime());
        let child = store.scope(
            StatePath::new(|v: &bool| v, |v: &mut bool| v),
            ActionPath::new(|a: &bool| Some(a), |a| a),
        );
        assert_eq!(
            drive(&host, child.send_and_wait_while(false, |value| *value)),
            Outcome::Satisfied
        );
        drop(drive(&host, child.send(true)).unwrap());
        let mut wait = child.wait_while(|value| *value);
        register(&host, &mut wait);
        drive(&host, store.shutdown()).unwrap();
        assert_eq!(drive(&host, wait), Outcome::StoreStopped);
    }

    // Catches accepting a committed false value after closure before registration.
    #[test]
    fn scoped_send_shutdown_between_commit_and_registration_is_not_success() {
        for (close_before_registration, expected, expected_calls) in [
            (false, Outcome::Satisfied, 1),
            (true, Outcome::StoreStopped, 0),
        ] {
            let host = DeterministicRuntime::new();
            let store = Store::new(true, Child, host.runtime());
            let child = store.scope(
                StatePath::new(|value: &bool| value, |value: &mut bool| value),
                ActionPath::new(|action: &bool| Some(action), |action| action),
            );
            let calls = Arc::new(AtomicUsize::new(0));
            let predicate_calls = Arc::clone(&calls);
            let mut wait = child.send_and_wait_while(false, move |value| {
                predicate_calls.fetch_add(1, Ordering::SeqCst);
                *value
            });

            assert_eq!(poll(&mut wait), Poll::Pending);
            host.run_until_stalled().unwrap();

            // Read the commit without consuming the wait's send receipt.
            let mut committed = Box::pin(store.read(|value| *value));
            assert!(poll(&mut committed).is_pending());
            host.run_until_stalled().unwrap();
            assert_eq!(poll(&mut committed).map(Result::unwrap), Poll::Ready(false));
            drop(committed);
            assert_eq!(calls.load(Ordering::SeqCst), 0);

            if close_before_registration {
                let mut shutdown = Box::pin(store.shutdown());
                assert!(poll(&mut shutdown).is_pending());
                host.run_until_stalled().unwrap();
                assert_eq!(poll(&mut shutdown).map(Result::unwrap), Poll::Ready(()));
            } else {
                assert_eq!(poll(&mut wait), Poll::Pending);
                host.run_until_stalled().unwrap();
            }

            assert_eq!(poll(&mut wait), Poll::Ready(expected));
            assert_eq!(calls.load(Ordering::SeqCst), expected_calls);

            if !close_before_registration {
                let mut shutdown = Box::pin(store.shutdown());
                assert!(poll(&mut shutdown).is_pending());
                host.run_until_stalled().unwrap();
                assert_eq!(poll(&mut shutdown).map(Result::unwrap), Poll::Ready(()));
            }
        }
    }

    // Catches reporting a stale scope error instead of root closure before registration.
    #[test]
    fn scoped_registration_after_shutdown_reports_root_closure() {
        for family in [Family::Optional, Family::Case, Family::Keyed] {
            let host = DeterministicRuntime::new();
            let store = store(&host);
            let child = scope(&host, &store, family);
            drive(&host, store.shutdown()).unwrap();
            assert_eq!(
                drive(&host, child.wait_while(|value| *value)),
                Outcome::StoreStopped
            );
            assert_eq!(
                drive(&host, child.send_and_wait_while(false, |value| *value)),
                Outcome::StoreStopped
            );
        }
    }

    // Catches leaked scoped registration and handle ownership at cancellation boundaries.
    #[test]
    fn scoped_cancellation_cleans_registration_before_and_after_receipt() {
        for family in [Family::Optional, Family::Case, Family::Keyed] {
            for phase in 0..4 {
                for sending in [false, true] {
                    let host = DeterministicRuntime::new();
                    let store = store(&host);
                    let child = scope(&host, &store, family);
                    let mut wait = if sending {
                        child.send_and_wait_while(true, |value| *value)
                    } else {
                        child.wait_while(|value| *value)
                    };
                    drop(child);
                    if phase > 0 {
                        assert_eq!(poll(&mut wait), Poll::Pending);
                    }
                    if phase > 1 {
                        host.run_until_stalled().unwrap();
                    }
                    if phase > 2 {
                        assert_eq!(poll(&mut wait), Poll::Pending);
                        host.run_until_stalled().unwrap();
                    }
                    wait.cancel();
                    assert_eq!(poll(&mut wait), Poll::Ready(Outcome::Cancelled));
                    host.run_until_stalled().unwrap();
                    let diagnostics = drive(&host, store.runtime_diagnostics()).unwrap();
                    assert_eq!(diagnostics.runtime().observation_count, 0);
                    assert_eq!(diagnostics.handle_registration_count(), 0);
                    assert!(drive(&host, store.read(|_| true)).unwrap());
                }
            }
        }
    }

    /// Counts actual embedding calls to distinguish rejected from admitted sends.
    fn counted_scope(
        host: &DeterministicRuntime,
        store: &Store<State, Action>,
        family: Family,
        embeds: Arc<AtomicUsize>,
    ) -> ScopedStore<bool, bool> {
        match family {
            Family::Optional => drive(
                host,
                store.scope_some(
                    optional_state!(),
                    ActionPath::new(
                        |a: &Action| {
                            if let Action::Optional(a) = a {
                                Some(a)
                            } else {
                                None
                            }
                        },
                        move |value| {
                            embeds.fetch_add(1, Ordering::SeqCst);
                            Action::Optional(value)
                        },
                    ),
                ),
            )
            .unwrap(),
            Family::Case => drive(
                host,
                store.scope_case(
                    case_state!(),
                    ActionPath::new(
                        |a: &Action| {
                            if let Action::Case(a) = a {
                                Some(a)
                            } else {
                                None
                            }
                        },
                        move |value| {
                            embeds.fetch_add(1, Ordering::SeqCst);
                            Action::Case(value)
                        },
                    ),
                ),
            )
            .unwrap(),
            Family::Keyed => drive(
                host,
                store.scope_keyed(
                    keyed_state!(),
                    KeyedActionPath::new(
                        |a: &Action| {
                            if let Action::Row(a) = a {
                                Some(a)
                            } else {
                                None
                            }
                        },
                        move |value| {
                            embeds.fetch_add(1, Ordering::SeqCst);
                            Action::Row(value)
                        },
                    ),
                    1,
                ),
            )
            .unwrap(),
        }
    }

    // Catches eagerly invoking a caller's embedder before lifetime/cancellation validation.
    #[test]
    fn ended_or_cancelled_owned_sends_never_embed_the_child_action() {
        for family in [Family::Optional, Family::Case, Family::Keyed] {
            for cancel in [false, true] {
                let host = DeterministicRuntime::new();
                let store = store(&host);
                let embeds = Arc::new(AtomicUsize::new(0));
                let child = counted_scope(&host, &store, family, Arc::clone(&embeds));
                let removal = (!cancel).then(|| store.try_send(Action::Remove(family)).unwrap());
                let mut wait = child.send_and_wait_while(false, |value| *value);
                assert_eq!(embeds.load(Ordering::SeqCst), 0);
                assert_eq!(poll(&mut wait), Poll::Pending);
                if cancel {
                    wait.cancel();
                }
                host.run_until_stalled().unwrap();
                if let Some(removal) = removal {
                    drop(drive(&host, removal.finish()).unwrap());
                }
                assert_eq!(
                    drive(&host, wait),
                    if cancel {
                        Outcome::Cancelled
                    } else {
                        Outcome::ScopeEnded
                    }
                );
                assert_eq!(embeds.load(Ordering::SeqCst), 0);
                if !cancel {
                    drop(drive(&host, store.send(Action::Insert(family))).unwrap());
                }
                let fresh = counted_scope(&host, &store, family, Arc::clone(&embeds));
                assert_eq!(
                    drive(&host, fresh.send_and_wait_while(false, |value| *value)),
                    Outcome::Satisfied
                );
                assert_eq!(embeds.load(Ordering::SeqCst), 1);
            }
        }
    }

    // Catches scope-error precedence hiding root closure before registration or send.
    #[test]
    fn root_closure_takes_precedence_over_already_ended_scope() {
        for family in [Family::Optional, Family::Case, Family::Keyed] {
            let host = DeterministicRuntime::new();
            let store = store(&host);
            let child = scope(&host, &store, family);
            drop(drive(&host, store.send(Action::Remove(family))).unwrap());
            drive(&host, store.shutdown()).unwrap();
            assert_eq!(
                drive(&host, child.wait_while(|value| *value)),
                Outcome::StoreStopped
            );
            assert_eq!(
                drive(&host, child.send_and_wait_while(false, |value| *value)),
                Outcome::StoreStopped
            );
        }
    }

    /// Starts unfinished work only for true, exposing its real output handle.
    struct ChildClient(std::sync::mpsc::Sender<rca_core::EffectSender<bool>>);
    impl Reducer for ChildClient {
        type State = bool;
        type Action = bool;
        fn reduce(
            &mut self,
            state: &mut bool,
            action: &bool,
            _: &mut ReduceContext<'_>,
        ) -> Effect<bool> {
            *state = *action;
            if !*action {
                return Effect::none();
            }
            let outputs = self.0.clone();
            Effect::run(move |sender| async move {
                outputs.send(sender).unwrap();
                std::future::pending::<()>().await;
            })
        }
    }

    // Catches root/sibling cancellation through registered child send ownership.
    #[test]
    fn optional_case_keyed_cancellation_removal_and_ordinary_drop_keep_sibling_work_live() {
        for family in [Family::Optional, Family::Case, Family::Keyed] {
            for policy in 0..3 {
                let host = DeterministicRuntime::new();
                let (outputs, received) = std::sync::mpsc::channel();
                let store = Store::new(
                    State {
                        optional: Some(true),
                        screen: Screen::On(true),
                        rows: [(1, true)].into_iter().collect(),
                    },
                    Parent
                        .if_some(
                            optional_state!(),
                            optional_action!(),
                            ChildClient(outputs.clone()),
                        )
                        .if_case(case_state!(), case_action!(), ChildClient(outputs.clone()))
                        .for_each(keyed_state!(), keyed_action!(), ChildClient(outputs)),
                    host.runtime(),
                );
                let other_family = match family {
                    Family::Optional => Family::Case,
                    Family::Case | Family::Keyed => Family::Optional,
                };
                let other = scope(&host, &store, other_family);
                let other_task = drive(&host, other.send(true)).unwrap();
                host.run_until_stalled().unwrap();
                let other_sender = received.try_recv().unwrap();
                let selected = scope(&host, &store, family);
                let mut wait = selected.send_and_wait_while(true, |value| *value);
                assert_eq!(poll(&mut wait), Poll::Pending);
                host.run_until_stalled().unwrap();
                let selected_sender = received.try_recv().unwrap();
                if policy == 1 {
                    assert_eq!(poll(&mut wait), Poll::Pending);
                    host.run_until_stalled().unwrap();
                    assert_eq!(poll(&mut wait), Poll::Pending);
                    drop(drive(&host, store.send(Action::Remove(family))).unwrap());
                    assert_eq!(drive(&host, wait), Outcome::ScopeEnded);
                } else if policy == 0 {
                    wait.cancel(); // Receipt still unconsumed.
                    assert_eq!(poll(&mut wait), Poll::Ready(Outcome::Cancelled));
                } else {
                    drop(wait);
                }
                host.run_until_stalled().unwrap();
                let selected_result = drive(&host, selected_sender.send(false));
                if policy == 2 {
                    selected_result.unwrap();
                } else {
                    assert!(selected_result.unwrap_err().is_cancelled());
                }
                assert!(!other_task.is_cancelled());
                assert!(!other_task.is_finished());
                drive(&host, other_sender.send(false)).unwrap();
                assert!(!drive(&host, other.read(|value| *value)).unwrap());
                let snapshot = drive(&host, store.runtime_snapshot()).unwrap();
                assert_eq!(
                    snapshot.live_task_ids.len(),
                    if policy == 2 { 2 } else { 1 }
                );
                assert_eq!(snapshot.observation_count, 0);
                drive(&host, store.shutdown()).unwrap();
                host.run_until_stalled().unwrap();
                assert!(other_task.is_finished());
            }
        }
    }
}
