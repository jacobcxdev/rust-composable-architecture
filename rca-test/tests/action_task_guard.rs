//! Distinguishes opt-in guard cancellation from ordinary handles and host teardown.

use std::task::{Context, Poll, Waker};
use std::time::Duration;

use futures_executor::block_on;
use rca_core::{ActionTask, Effect, ReduceContext, Reducer, Store};
use rca_test::DeterministicRuntime;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Action {
    Start(u8),
    Follow(u8),
    Done(u8),
}

/// Records a causal chain whose two descendants sleep at separate deadlines.
struct Feature;

impl Reducer for Feature {
    type State = Vec<Action>;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        state.push(*action);
        match *action {
            Action::Start(id) => Effect::send_after(Duration::from_secs(1), Action::Follow(id)),
            Action::Follow(id) => Effect::send_after(Duration::from_secs(1), Action::Done(id)),
            Action::Done(_) => Effect::none(),
        }
    }
}

/// Commits the root action and returns the handle for its full causal chain.
fn start(host: &DeterministicRuntime, store: &Store<Vec<Action>, Action>, id: u8) -> ActionTask {
    block_on(host.run(store.send(Action::Start(id))))
        .unwrap()
        .unwrap()
}

/// Reads the actual committed trace through the actor.
fn state(host: &DeterministicRuntime, store: &Store<Vec<Action>, Action>) -> Vec<Action> {
    block_on(host.run(store.read(Clone::clone)))
        .unwrap()
        .unwrap()
}

#[test]
fn dropping_unpolled_or_pending_guard_cancels_only_selected_causal_work() {
    for poll_first in [false, true] {
        let host = DeterministicRuntime::new();
        let store = Store::new(Vec::new(), Feature, host.runtime());
        let selected = start(&host, &store, 1);
        let sibling = start(&host, &store, 2);
        let mut guard = Box::pin(selected.clone().finish_cancelling_on_drop());
        if poll_first {
            assert!(
                guard
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
        }
        drop(guard);
        assert!(selected.is_cancelled());
        assert!(!sibling.is_cancelled());
        host.run_until_stalled().unwrap();
        assert!(selected.is_finished());
        host.advance(Duration::from_secs(2)).unwrap();
        assert_eq!(
            state(&host, &store),
            [
                Action::Start(1),
                Action::Start(2),
                Action::Follow(2),
                Action::Done(2)
            ]
        );
        assert!(sibling.is_finished());
        assert!(!sibling.is_cancelled());
    }
}

#[test]
fn guard_drop_retires_later_sleeping_descendants_without_retracting_commits() {
    let host = DeterministicRuntime::new();
    let store = Store::new(Vec::new(), Feature, host.runtime());
    let selected = start(&host, &store, 1);
    host.advance(Duration::from_secs(1)).unwrap();
    assert_eq!(state(&host, &store), [Action::Start(1), Action::Follow(1)]);
    drop(selected.clone().finish_cancelling_on_drop());
    host.run_until_stalled().unwrap();
    host.advance(Duration::from_secs(1)).unwrap();
    assert_eq!(state(&host, &store), [Action::Start(1), Action::Follow(1)]);
    assert!(selected.is_finished());
    assert!(selected.is_cancelled());
}

#[test]
fn ordinary_task_and_pending_finish_drop_remain_noncancelling() {
    let host = DeterministicRuntime::new();
    let store = Store::new(Vec::new(), Feature, host.runtime());
    let task = start(&host, &store, 1);
    drop(task.clone());
    let mut ordinary_wait = Box::pin(task.finish());
    assert!(
        ordinary_wait
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    drop(ordinary_wait);
    assert!(!task.is_cancelled());
    host.advance(Duration::from_secs(2)).unwrap();
    assert_eq!(
        state(&host, &store),
        [Action::Start(1), Action::Follow(1), Action::Done(1)]
    );
    assert!(task.is_finished());
    assert!(!task.is_cancelled());
}

#[test]
fn successful_or_already_completed_guard_keeps_original_completion() {
    for poll_first in [false, true] {
        let host = DeterministicRuntime::new();
        let store = Store::new(Vec::new(), Feature, host.runtime());
        let task = start(&host, &store, 1);
        let mut guard = Box::pin(task.clone().finish_cancelling_on_drop());
        if poll_first {
            assert!(
                guard
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
        }
        host.advance(Duration::from_secs(2)).unwrap();
        assert_eq!(
            guard.as_mut().poll(&mut Context::from_waker(Waker::noop())),
            Poll::Ready(())
        );
        drop(guard);
        assert!(!task.is_cancelled());
        drop(task.clone().finish_cancelling_on_drop());
        assert!(!task.is_cancelled());
        assert!(task.is_finished());
    }
}

#[test]
fn no_effect_task_cancellation_and_owned_wait_preserve_completion() {
    let host = DeterministicRuntime::new();
    let store = Store::new(Vec::new(), Feature, host.runtime());
    let task = block_on(host.run(store.send(Action::Done(1))))
        .unwrap()
        .unwrap();
    assert_eq!(format!("{task:?}"), "ActionTask::Completed");

    task.cancel();
    assert!(task.is_finished());
    assert!(!task.is_cancelled());
    let mut guard = Box::pin(task.clone().finish_cancelling_on_drop());
    assert_eq!(
        guard.as_mut().poll(&mut Context::from_waker(Waker::noop())),
        Poll::Ready(())
    );
    drop(guard);

    assert!(task.is_finished());
    assert!(!task.is_cancelled());
    assert_eq!(state(&host, &store), [Action::Done(1)]);
    block_on(host.run(store.shutdown())).unwrap().unwrap();
}

#[test]
fn explicit_cancel_and_host_teardown_preserve_task_completion_semantics() {
    for cancel_first in [false, true] {
        let mut outcomes = Vec::new();
        for guarded in [false, true] {
            let host = DeterministicRuntime::new();
            let store = Store::new(Vec::new(), Feature, host.runtime());
            let task = start(&host, &store, 1);
            if cancel_first {
                task.cancel();
                host.run_until_stalled().unwrap();
            }
            let owned = task.clone();
            let wait: std::pin::Pin<Box<dyn Future<Output = ()> + Send + 'static>> = if guarded {
                Box::pin(owned.finish_cancelling_on_drop())
            } else {
                Box::pin(async move { owned.finish().await })
            };
            // Host teardown already cancels unfinished actor-owned work. The
            // opt-in waiter preserves that original completion/cancellation.
            drop(host);
            block_on(wait);
            outcomes.push((task.is_finished(), task.is_cancelled()));
        }
        assert_eq!(outcomes, [(true, true), (true, true)]);
    }
}

#[test]
fn cancel_before_task_acquisition_does_not_retract_admitted_input() {
    let host = DeterministicRuntime::new();
    let store = Store::new(Vec::new(), Feature, host.runtime());
    drop(store.send(Action::Start(0)));
    host.run_until_stalled().unwrap();
    assert!(state(&host, &store).is_empty());

    let receipt = store.try_send(Action::Start(1)).unwrap();
    let waiting = async move {
        receipt
            .finish()
            .await
            .unwrap()
            .finish_cancelling_on_drop()
            .await;
    };
    drop(waiting);
    host.advance(Duration::from_secs(2)).unwrap();
    assert_eq!(
        state(&host, &store),
        [Action::Start(1), Action::Follow(1), Action::Done(1)]
    );
}
