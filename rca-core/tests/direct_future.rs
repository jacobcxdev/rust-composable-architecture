//! Checks direct-future ownership at polling, admission, commit and cancellation.
//!
//! Manual polling keeps each boundary observable without wall-clock waits.

#[expect(dead_code, reason = "future tests manually poll runtime tasks")]
mod support;

use std::cell::Cell;
use std::marker::PhantomPinned;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};

use rca_core::{Effect, ReduceContext, Reducer, Store};

/// Selects the result of polling the owned input future.
#[derive(Clone, Copy)]
enum Outcome {
    Pending,
    Some,
    None,
    Panic,
}

/// Counts polls and drops, and detects movement after the first pinned poll.
struct Input {
    polls: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
    address: Cell<Option<usize>>,
    outcome: Outcome,
    _pinned: PhantomPinned,
}

impl Future for Input {
    type Output = Option<Cell<usize>>;

    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.as_ref().get_ref();
        let address = std::ptr::from_ref(this).addr();
        if let Some(previous) = this.address.replace(Some(address)) {
            assert_eq!(previous, address, "pinned input moved");
        }
        this.polls.fetch_add(1, Ordering::SeqCst);
        match this.outcome {
            Outcome::Pending => Poll::Pending,
            Outcome::Some => Poll::Ready(Some(Cell::new(42))),
            Outcome::None => Poll::Ready(None),
            Outcome::Panic => panic!("intentional input panic"),
        }
    }
}

impl Drop for Input {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

/// Starts the future, carries its non-`Sync` result or fills the bounded mailbox.
enum Action {
    Start,
    Value(Cell<usize>),
    Fill,
}

/// Moves one input into an effect and separately counts mapping and commits.
struct Feature {
    input: Option<Input>,
    maps: Arc<AtomicUsize>,
    commits: Arc<AtomicUsize>,
}

impl Reducer for Feature {
    type State = ();
    type Action = Action;

    fn reduce(
        &mut self,
        (): &mut (),
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        match action {
            Action::Start => {
                let maps = Arc::clone(&self.maps);
                Effect::future(self.input.take().expect("one start"))
                    .map(move |value| {
                        maps.fetch_add(1, Ordering::SeqCst);
                        value
                    })
                    .map(Action::Value)
            }
            Action::Value(value) => {
                assert_eq!(value.get(), 42);
                self.commits.fetch_add(1, Ordering::SeqCst);
                Effect::none()
            }
            Action::Fill => Effect::none(),
        }
    }
}

/// Builds a fresh input with independently retained poll and drop counters.
fn feature(outcome: Outcome) -> (Feature, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let polls = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    (
        Feature {
            input: Some(Input {
                polls: Arc::clone(&polls),
                drops: Arc::clone(&drops),
                address: Cell::new(None),
                outcome,
                _pinned: PhantomPinned,
            }),
            maps: Arc::new(AtomicUsize::new(0)),
            commits: Arc::new(AtomicUsize::new(0)),
        },
        polls,
        drops,
    )
}

#[test]
fn ready_input_drops_before_admission_but_task_waits_for_commit() {
    let runtime = support::ManualRuntime::new();
    let (feature, polls, drops) = feature(Outcome::Some);
    let maps = Arc::clone(&feature.maps);
    let commits = Arc::clone(&feature.commits);
    let store = Store::builder((), feature, runtime.runtime())
        .mailbox_capacity(1)
        .build()
        .unwrap();
    let mut actor = runtime.take_only_queued_task();
    let receipt = store.try_send(Action::Start).ok().unwrap();
    let mut cx = Context::from_waker(Waker::noop());

    assert_eq!(polls.load(Ordering::SeqCst), 0);
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    let task = futures_executor::block_on(receipt.finish()).unwrap();
    let mut run = runtime.take_only_queued_task();
    assert_eq!(polls.load(Ordering::SeqCst), 0);
    assert_eq!(maps.load(Ordering::SeqCst), 0);

    let fill = store.try_send(Action::Fill).ok().unwrap();
    assert!(run.as_mut().poll(&mut cx).is_pending());
    assert_eq!(polls.load(Ordering::SeqCst), 1);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(maps.load(Ordering::SeqCst), 1);
    assert_eq!(commits.load(Ordering::SeqCst), 0);
    assert!(!task.is_finished());

    assert!(actor.as_mut().poll(&mut cx).is_pending());
    drop(futures_executor::block_on(fill.finish()).unwrap());
    assert!(run.as_mut().poll(&mut cx).is_pending());
    assert_eq!(commits.load(Ordering::SeqCst), 0);
    assert!(!task.is_finished());

    assert!(actor.as_mut().poll(&mut cx).is_pending());
    assert_eq!(commits.load(Ordering::SeqCst), 1);
    assert!(run.as_mut().poll(&mut cx).is_ready());
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    assert!(task.is_finished());
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn none_completes_without_mapping_or_sending() {
    let runtime = support::ManualRuntime::new();
    let (feature, polls, drops) = feature(Outcome::None);
    let maps = Arc::clone(&feature.maps);
    let commits = Arc::clone(&feature.commits);
    let store = Store::new((), feature, runtime.runtime());
    let mut actor = runtime.take_only_queued_task();
    let receipt = store.try_send(Action::Start).ok().unwrap();
    let mut cx = Context::from_waker(Waker::noop());
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    let task = futures_executor::block_on(receipt.finish()).unwrap();
    let mut run = runtime.take_only_queued_task();
    assert!(run.as_mut().poll(&mut cx).is_ready());
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    assert!(task.is_finished());
    assert_eq!(polls.load(Ordering::SeqCst), 1);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(maps.load(Ordering::SeqCst), 0);
    assert_eq!(commits.load(Ordering::SeqCst), 0);
}

#[test]
fn cancellation_and_owner_loss_reject_output_waiting_for_capacity() {
    for stop_store in [false, true] {
        let runtime = support::ManualRuntime::new();
        let (feature, polls, drops) = feature(Outcome::Some);
        let commits = Arc::clone(&feature.commits);
        let store = Store::builder((), feature, runtime.runtime())
            .mailbox_capacity(1)
            .build()
            .unwrap();
        let mut actor = runtime.take_only_queued_task();
        let receipt = store.try_send(Action::Start).ok().unwrap();
        let mut cx = Context::from_waker(Waker::noop());
        assert!(actor.as_mut().poll(&mut cx).is_pending());
        let task = futures_executor::block_on(receipt.finish()).unwrap();
        let mut run = runtime.take_only_queued_task();
        let fill = store.try_send(Action::Fill).ok().unwrap();
        assert!(run.as_mut().poll(&mut cx).is_pending());
        assert_eq!(polls.load(Ordering::SeqCst), 1);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(!task.is_finished());

        if stop_store {
            drop(store);
            assert!(actor.as_mut().poll(&mut cx).is_ready());
        } else {
            task.cancel();
            assert!(actor.as_mut().poll(&mut cx).is_pending());
        }
        assert!(run.as_mut().poll(&mut cx).is_ready());
        drop(run);
        drop(actor);
        drop(fill);
        assert!(task.is_finished());
        assert_eq!(commits.load(Ordering::SeqCst), 0);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn cancellation_releases_unpolled_and_pinned_pending_inputs_once() {
    for poll_before_cancel in [false, true] {
        let runtime = support::ManualRuntime::new();
        let (feature, polls, drops) = feature(Outcome::Pending);
        let store = Store::new((), feature, runtime.runtime());
        let mut actor = runtime.take_only_queued_task();
        let receipt = store.try_send(Action::Start).ok().unwrap();
        let mut cx = Context::from_waker(Waker::noop());
        assert!(actor.as_mut().poll(&mut cx).is_pending());
        let task = futures_executor::block_on(receipt.finish()).unwrap();
        let mut run = runtime.take_only_queued_task();
        if poll_before_cancel {
            assert!(run.as_mut().poll(&mut cx).is_pending());
        }
        // Moving the task handle must not move the input pinned inside it.
        let mut moved = Box::new(run);
        if poll_before_cancel {
            assert!(moved.as_mut().as_mut().poll(&mut cx).is_pending());
        }
        task.cancel();
        assert!(actor.as_mut().poll(&mut cx).is_pending());
        assert!(moved.as_mut().as_mut().poll(&mut cx).is_ready());
        drop(moved);
        assert_eq!(
            polls.load(Ordering::SeqCst),
            usize::from(poll_before_cancel) * 2
        );
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn runtime_drop_and_poll_panic_release_input_once() {
    for outcome in [Outcome::Pending, Outcome::Panic] {
        let runtime = support::ManualRuntime::new();
        let (feature, polls, drops) = feature(outcome);
        let store = Store::new((), feature, runtime.runtime());
        let mut actor = runtime.take_only_queued_task();
        let receipt = store.try_send(Action::Start).ok().unwrap();
        let mut cx = Context::from_waker(Waker::noop());
        assert!(actor.as_mut().poll(&mut cx).is_pending());
        let task = futures_executor::block_on(receipt.finish()).unwrap();
        let mut run = runtime.take_only_queued_task();
        if matches!(outcome, Outcome::Panic) {
            let panic = catch_unwind(AssertUnwindSafe(move || run.as_mut().poll(&mut cx)))
                .expect_err("input panic propagates");
            assert_eq!(
                panic.downcast_ref::<&str>(),
                Some(&"intentional input panic")
            );
        } else {
            drop(run);
        }
        assert_eq!(
            polls.load(Ordering::SeqCst),
            usize::from(matches!(outcome, Outcome::Panic))
        );
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        drop(actor);
        assert!(task.is_finished());
    }
}

#[test]
fn discarded_effect_owns_but_never_polls_input() {
    let (mut feature, polls, drops) = feature(Outcome::Pending);
    let effect = Effect::future(feature.input.take().unwrap());
    assert_eq!(format!("{effect:?}"), "Effect::Future");
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(effect);
    assert_eq!(polls.load(Ordering::SeqCst), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn last_owner_loss_cancels_pending_future() {
    let runtime = support::ManualRuntime::new();
    let (feature, polls, drops) = feature(Outcome::Pending);
    let store = Store::new((), feature, runtime.runtime());
    let mut actor = runtime.take_only_queued_task();
    let receipt = store.try_send(Action::Start).ok().unwrap();
    let mut cx = Context::from_waker(Waker::noop());
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    let task = futures_executor::block_on(receipt.finish()).unwrap();
    let mut run = runtime.take_only_queued_task();
    assert!(run.as_mut().poll(&mut cx).is_pending());
    drop(store);
    assert!(actor.as_mut().poll(&mut cx).is_ready());
    assert!(run.as_mut().poll(&mut cx).is_ready());
    assert!(task.is_finished());
    assert_eq!(polls.load(Ordering::SeqCst), 1);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}
