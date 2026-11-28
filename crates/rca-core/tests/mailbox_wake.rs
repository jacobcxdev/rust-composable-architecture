//! Checks actor and producer notifications at explicit mailbox polling boundaries.

#[expect(dead_code, reason = "mailbox tests manually poll runtime tasks")]
mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use rca_core::{Effect, ReduceContext, Reducer, Store};

/// Records committed state independently of action receipts and actor wake counts.
struct Counter(Arc<AtomicUsize>);

impl Reducer for Counter {
    type State = usize;
    type Action = usize;

    fn reduce(
        &mut self,
        state: &mut usize,
        action: &usize,
        _: &mut ReduceContext<'_>,
    ) -> Effect<usize> {
        *state += action;
        self.0.fetch_add(*action, Ordering::SeqCst);
        Effect::none()
    }
}

/// Counts actual wakes without advancing the manually owned actor.
#[derive(Default)]
struct Notifications(AtomicUsize);

impl Notifications {
    /// Returns the current count for a before-and-after notification check.
    fn count(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

impl Wake for Notifications {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn idle_actor_is_woken_by_try_send_send_and_read() {
    let runtime = support::ManualRuntime::new();
    let commits = Arc::new(AtomicUsize::new(0));
    let store = Store::new(0, Counter(Arc::clone(&commits)), runtime.runtime());
    let mut actor = runtime.take_only_queued_task();
    let notifications = Arc::new(Notifications::default());
    let waker = Waker::from(Arc::clone(&notifications));
    let mut cx = Context::from_waker(&waker);
    assert!(actor.as_mut().poll(&mut cx).is_pending());

    let before = notifications.count();
    let receipt = store.try_send(1).unwrap();
    assert!(notifications.count() > before);
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    let mut receipt = std::pin::pin!(receipt.finish());
    assert!(matches!(
        receipt
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop())),
        Poll::Ready(Ok(_))
    ));

    let before = notifications.count();
    let mut send = Box::pin(store.send(2));
    let mut caller = Context::from_waker(Waker::noop());
    assert!(send.as_mut().poll(&mut caller).is_pending());
    assert!(notifications.count() > before);
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    assert!(matches!(
        send.as_mut().poll(&mut caller),
        Poll::Ready(Ok(_))
    ));

    let before = notifications.count();
    let mut read = Box::pin(store.read(|state| *state));
    assert!(read.as_mut().poll(&mut caller).is_pending());
    assert!(notifications.count() > before);
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    assert!(matches!(
        read.as_mut().poll(&mut caller),
        Poll::Ready(Ok(3))
    ));
    assert_eq!(commits.load(Ordering::SeqCst), 3);
}

#[test]
fn cancelled_full_mailbox_waiter_does_not_prevent_next_producer_waking_actor() {
    let runtime = support::ManualRuntime::new();
    let commits = Arc::new(AtomicUsize::new(0));
    let store = Store::builder(0, Counter(Arc::clone(&commits)), runtime.runtime())
        .mailbox_capacity(1)
        .build()
        .unwrap();
    let mut actor = runtime.take_only_queued_task();
    let actor_notifications = Arc::new(Notifications::default());
    let actor_waker = Waker::from(Arc::clone(&actor_notifications));
    let mut actor_cx = Context::from_waker(&actor_waker);
    assert!(actor.as_mut().poll(&mut actor_cx).is_pending());
    let before = actor_notifications.count();
    let first = store.try_send(1).unwrap();
    assert!(actor_notifications.count() > before);

    let mut cancelled = Box::pin(store.send(100));
    assert!(
        cancelled
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    let producer_notifications = Arc::new(Notifications::default());
    let producer_waker = Waker::from(Arc::clone(&producer_notifications));
    let mut producer_cx = Context::from_waker(&producer_waker);
    let mut waiting = Box::pin(store.send(2));
    assert!(waiting.as_mut().poll(&mut producer_cx).is_pending());
    drop(cancelled);
    let before = producer_notifications.count();
    assert!(actor.as_mut().poll(&mut actor_cx).is_pending());
    assert!(producer_notifications.count() > before);
    let mut first = std::pin::pin!(first.finish());
    assert!(matches!(
        first.as_mut().poll(&mut Context::from_waker(Waker::noop())),
        Poll::Ready(Ok(_))
    ));

    let before = actor_notifications.count();
    assert!(waiting.as_mut().poll(&mut producer_cx).is_pending());
    assert!(actor_notifications.count() > before);
    assert!(actor.as_mut().poll(&mut actor_cx).is_pending());
    assert!(matches!(
        waiting.as_mut().poll(&mut producer_cx),
        Poll::Ready(Ok(_))
    ));
    assert_eq!(commits.load(Ordering::SeqCst), 3);
}

#[test]
fn final_store_owner_wakes_idle_actor_for_shutdown() {
    let runtime = support::ManualRuntime::new();
    let store = Store::new(0, Counter(Arc::new(AtomicUsize::new(0))), runtime.runtime());
    let other_owner = store.clone();
    let mut actor = runtime.take_only_queued_task();
    let notifications = Arc::new(Notifications::default());
    let waker = Waker::from(Arc::clone(&notifications));
    let mut cx = Context::from_waker(&waker);
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    drop(other_owner);
    let before = notifications.count();
    drop(store);
    assert!(notifications.count() > before);
    assert!(actor.as_mut().poll(&mut cx).is_ready());
}
