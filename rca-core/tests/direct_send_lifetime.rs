//! Direct effect sends retain their action until delivery or real cancellation.

#[expect(dead_code, reason = "direct-send tests manually poll runtime tasks")]
mod support;

use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};

use rca_core::{ActionTask, Effect, ReduceContext, Reducer, RuntimeFuture, Store};

/// Counts exact destruction of the action held by a direct send.
struct Value {
    amount: Cell<usize>,
    drops: Arc<AtomicUsize>,
}

impl Drop for Value {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

enum Action {
    Start,
    Blocker,
    Value(Value),
}

struct Feature {
    commits: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
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
            Action::Start => Effect::send(Action::Value(Value {
                amount: Cell::new(7),
                drops: Arc::clone(&self.drops),
            })),
            Action::Blocker => {
                self.commits.fetch_add(100, Ordering::SeqCst);
                Effect::none()
            }
            Action::Value(value) => {
                self.commits.fetch_add(value.amount.get(), Ordering::SeqCst);
                Effect::none()
            }
        }
    }
}

/// Keeps actor and effect futures separate so each admission boundary is polled.
struct Fixture {
    _runtime: support::ManualRuntime,
    store: Store<(), Action>,
    actor: RuntimeFuture,
    effect: RuntimeFuture,
    task: ActionTask,
    commits: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
}

impl Fixture {
    /// Commits `Start` but leaves its direct effect unpolled.
    fn new() -> Self {
        let runtime = support::ManualRuntime::new();
        let commits = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        let store = Store::builder(
            (),
            Feature {
                commits: Arc::clone(&commits),
                drops: Arc::clone(&drops),
            },
            runtime.runtime(),
        )
        .mailbox_capacity(1)
        .build()
        .unwrap();
        let mut actor = runtime.take_only_queued_task();
        let receipt = store.try_send(Action::Start).ok().unwrap();
        let mut cx = Context::from_waker(Waker::noop());
        assert!(actor.as_mut().poll(&mut cx).is_pending());
        let mut receipt = std::pin::pin!(receipt.finish());
        let Poll::Ready(Ok(task)) = receipt.as_mut().poll(&mut cx) else {
            panic!("start must commit before its effect is polled");
        };
        let effect = runtime.take_only_queued_task();
        assert!(!task.is_finished());
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        Self {
            _runtime: runtime,
            store,
            actor,
            effect,
            task,
            commits,
            drops,
        }
    }
}

#[test]
fn cancellation_before_first_poll_releases_direct_action_without_delivery() {
    let mut fixture = Fixture::new();
    let mut cx = Context::from_waker(Waker::noop());
    fixture.task.cancel();
    assert!(fixture.actor.as_mut().poll(&mut cx).is_pending());
    assert!(fixture.effect.as_mut().poll(&mut cx).is_ready());
    assert_eq!(fixture.drops.load(Ordering::SeqCst), 1);
    drop(fixture.effect);
    assert!(fixture.actor.as_mut().poll(&mut cx).is_pending());
    assert!(fixture.task.is_finished());
    assert_eq!(fixture.commits.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.drops.load(Ordering::SeqCst), 1);
}

#[test]
fn cancelled_full_mailbox_direct_send_does_not_deliver_after_capacity_returns() {
    let mut fixture = Fixture::new();
    let blocker = fixture.store.try_send(Action::Blocker).ok().unwrap();
    let mut cx = Context::from_waker(Waker::noop());
    assert!(fixture.effect.as_mut().poll(&mut cx).is_pending());
    assert_eq!(fixture.drops.load(Ordering::SeqCst), 0);
    fixture.task.cancel();
    assert!(fixture.actor.as_mut().poll(&mut cx).is_pending());
    let mut blocker = std::pin::pin!(blocker.finish());
    assert!(matches!(blocker.as_mut().poll(&mut cx), Poll::Ready(Ok(_))));
    assert!(fixture.effect.as_mut().poll(&mut cx).is_ready());
    assert_eq!(fixture.drops.load(Ordering::SeqCst), 1);
    drop(fixture.effect);
    assert!(fixture.actor.as_mut().poll(&mut cx).is_pending());
    assert!(fixture.task.is_finished());
    assert_eq!(fixture.commits.load(Ordering::SeqCst), 100);
    assert_eq!(fixture.drops.load(Ordering::SeqCst), 1);
}

#[test]
fn final_owner_loss_releases_pending_direct_send_without_delivery() {
    for full in [false, true] {
        let mut fixture = Fixture::new();
        let blocker = full.then(|| fixture.store.try_send(Action::Blocker).ok().unwrap());
        let mut cx = Context::from_waker(Waker::noop());
        assert!(fixture.effect.as_mut().poll(&mut cx).is_pending());
        assert_eq!(fixture.drops.load(Ordering::SeqCst), 0);
        drop(fixture.store);
        assert!(fixture.actor.as_mut().poll(&mut cx).is_ready());
        assert!(fixture.effect.as_mut().poll(&mut cx).is_ready());
        drop(fixture.effect);
        if let Some(blocker) = blocker {
            let mut blocker = std::pin::pin!(blocker.finish());
            assert!(matches!(
                blocker.as_mut().poll(&mut cx),
                Poll::Ready(Err(_))
            ));
        }
        assert!(fixture.task.is_finished());
        assert_eq!(fixture.commits.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.drops.load(Ordering::SeqCst), 1);
    }
}
