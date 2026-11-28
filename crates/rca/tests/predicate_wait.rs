//! Checks root and child predicate waits against live effects and explicit cancellation.

#![cfg(feature = "test-util")]

use std::pin::Pin;
use std::sync::mpsc;
use std::task::{Context, Poll, Waker};

use futures_executor::block_on;
use rca::{
    ActionPath, DeterministicRuntime, Effect, EffectSender, PredicateWait,
    PredicateWaitOutcome as Outcome, ReduceContext, Reducer, StatePath, Store,
};

/// Keeps the predicate value independent of effect-handle observations.
struct State {
    busy: bool,
}

enum Action {
    Set(bool),
    Start,
}

/// Exposes each live effect's sender so the test can probe cancellation independently.
struct Feature(mpsc::Sender<EffectSender<Action>>);

impl Reducer for Feature {
    type State = State;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut State,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        match action {
            Action::Set(value) => {
                state.busy = *value;
                Effect::none()
            }
            Action::Start => {
                state.busy = true;
                let outputs = self.0.clone();
                Effect::run(move |sender| async move {
                    outputs.send(sender).unwrap();
                    std::future::pending::<()>().await;
                })
            }
        }
    }
}

/// Drives actor work on the same deterministic host as the tested wait.
fn drive<F: Future + Send>(host: &DeterministicRuntime, future: F) -> F::Output
where
    F::Output: Send,
{
    block_on(host.run(future)).unwrap()
}

/// Inspects a wait without driving the actor or introducing a separate wake source.
fn poll(wait: &mut PredicateWait) -> Poll<Outcome> {
    Pin::new(wait).poll(&mut Context::from_waker(Waker::noop()))
}

/// Projects only the predicate value while retaining the parent's action route.
fn child(store: &Store<State, Action>) -> rca::ScopedStore<bool, Action> {
    store.scope(
        StatePath::new(|state: &State| &state.busy, |state| &mut state.busy),
        ActionPath::new(|action: &Action| Some(action), |action| action),
    )
}

// Exercises the exported owned result through real root and child transactions.
#[test]
fn facade_root_and_child_waits_consume_current_and_post_commit_state() {
    let host = DeterministicRuntime::new();
    let (outputs, _received) = mpsc::channel();
    let store = Store::new(State { busy: false }, Feature(outputs), host.runtime());
    assert_eq!(
        drive(&host, store.wait_while(|state| state.busy)),
        Outcome::Satisfied
    );
    assert_eq!(
        drive(
            &host,
            store.send_and_wait_while(Action::Set(false), |state| state.busy)
        ),
        Outcome::Satisfied
    );
    let child = child(&store);
    drop(drive(&host, child.send(Action::Set(true))).unwrap());
    assert_eq!(
        drive(
            &host,
            child.send_and_wait_while(Action::Set(false), |busy| *busy)
        ),
        Outcome::Satisfied
    );
    drop(drive(&host, child.send(Action::Set(true))).unwrap());
    let mut wait = child.wait_while(|busy| *busy);
    assert_eq!(poll(&mut wait), Poll::Pending);
    host.run_until_stalled().unwrap();
    assert_eq!(poll(&mut wait), Poll::Pending);
    drop(child);
    drop(store);
    assert_eq!(drive(&host, wait), Outcome::StoreStopped);
}

// Catches facade users accidentally losing the distinct cancel and drop laws.
#[test]
fn facade_cancel_retires_only_owned_work_and_drop_keeps_it_live() {
    for scoped in [false, true] {
        for cancel in [false, true] {
            let host = DeterministicRuntime::new();
            let (outputs, received) = mpsc::channel();
            let store = Store::new(State { busy: false }, Feature(outputs), host.runtime());
            let sibling = drive(&host, store.send(Action::Start)).unwrap();
            host.run_until_stalled().unwrap();
            let sibling_sender = received.try_recv().unwrap();
            let mut wait = if scoped {
                child(&store).send_and_wait_while(Action::Start, |busy| *busy)
            } else {
                store.send_and_wait_while(Action::Start, |state| state.busy)
            };
            assert_eq!(poll(&mut wait), Poll::Pending);
            host.run_until_stalled().unwrap();
            let selected_sender = received.try_recv().unwrap();
            assert_eq!(poll(&mut wait), Poll::Pending);
            host.run_until_stalled().unwrap();
            assert_eq!(poll(&mut wait), Poll::Pending);
            if cancel {
                wait.cancel();
                assert_eq!(poll(&mut wait), Poll::Ready(Outcome::Cancelled));
            }
            drop(wait);
            host.run_until_stalled().unwrap();
            let sent = drive(&host, selected_sender.send(Action::Set(false)));
            if cancel {
                assert!(sent.unwrap_err().is_cancelled());
            } else {
                sent.unwrap();
                assert!(!drive(&host, store.read(|state| state.busy)).unwrap());
            }
            assert!(!sibling.is_cancelled());
            assert!(!sibling.is_finished());
            drive(&host, sibling_sender.send(Action::Set(true))).unwrap();
            let snapshot = drive(&host, store.runtime_snapshot()).unwrap();
            assert_eq!(snapshot.observation_count, 0);
            assert_eq!(snapshot.live_task_ids.len(), if cancel { 1 } else { 2 });
            drive(&host, store.shutdown()).unwrap();
        }
    }
}
