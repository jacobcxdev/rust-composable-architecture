//! Receipt conversion preserves commit wakes, causal work and weak Store ownership.

#[expect(dead_code, reason = "receipt tests manually poll runtime tasks")]
mod support;

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use rca_core::{
    ActionPath, ActionReceiptFuture, ActionTask, Effect, OptionalStatePath, ReduceContext, Reducer,
    Store, StoreStopped,
};

enum Action {
    Start,
    Follow,
}

/// Counts the committed starter and its later effect action separately.
struct Feature(Arc<AtomicUsize>);

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
                self.0.fetch_add(1, Ordering::SeqCst);
                Effect::run(|sender| async move {
                    sender.send(Action::Follow).await.unwrap();
                })
            }
            Action::Follow => {
                self.0.fetch_add(10, Ordering::SeqCst);
                Effect::none()
            }
        }
    }
}

#[derive(Default)]
/// Observes a receipt wake without driving the actor from the waker.
struct WakeCount(AtomicUsize);

impl Wake for WakeCount {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn diagnostics_store_and_receipt_formatting_does_not_drive_admitted_work() {
    let runtime = support::ManualRuntime::new();
    let commits = Arc::new(AtomicUsize::new(0));
    let builder =
        Store::builder((), Feature(Arc::clone(&commits)), runtime.runtime()).mailbox_capacity(3);
    let configured = format!("{builder:?}");
    assert!(configured.starts_with("StoreBuilder {"));
    assert!(configured.contains("mailbox_capacity: 3"));
    assert_eq!(runtime.spawn_count(), 0);
    assert_eq!(commits.load(Ordering::SeqCst), 0);

    let store = builder.build().unwrap();
    let mut actor = runtime.take_only_queued_task();
    assert_eq!(format!("{store:?}"), "Store { closed: false, .. }");
    let receipt = store.try_send(Action::Follow).ok().unwrap();
    assert_eq!(format!("{receipt:?}"), "ActionReceipt { .. }");
    let mut future = receipt.into_future();
    assert_eq!(format!("{future:?}"), "ActionReceiptFuture { .. }");
    assert_eq!(commits.load(Ordering::SeqCst), 0);
    let mut cx = Context::from_waker(Waker::noop());
    assert!(Pin::new(&mut future).poll(&mut cx).is_pending());
    assert_eq!(format!("{future:?}"), "ActionReceiptFuture { .. }");
    assert_eq!(commits.load(Ordering::SeqCst), 0);

    assert!(actor.as_mut().poll(&mut cx).is_pending());
    let Poll::Ready(Ok(task)) = Pin::new(&mut future).poll(&mut cx) else {
        panic!("actor committed the admitted action");
    };
    assert_eq!(format!("{future:?}"), "ActionReceiptFuture { .. }");
    assert_eq!(format!("{task:?}"), "ActionTask::Completed");
    assert!(task.is_finished());
    assert!(!task.is_cancelled());
    assert_eq!(commits.load(Ordering::SeqCst), 10);
    drop(actor);
    assert_eq!(format!("{store:?}"), "Store { closed: true, .. }");
    assert_eq!(commits.load(Ordering::SeqCst), 10);
}

#[test]
fn moved_pending_receipt_receives_commit_wake_and_task() {
    fn assert_traits<T: Send + Unpin + 'static>() {}
    assert_traits::<ActionReceiptFuture>();

    let runtime = support::ManualRuntime::new();
    let commits = Arc::new(AtomicUsize::new(0));
    let store = Store::new((), Feature(Arc::clone(&commits)), runtime.runtime());
    let mut actor = runtime.take_only_queued_task();
    let mut future = store.try_send(Action::Follow).ok().unwrap().into_future();
    let first = Arc::new(WakeCount::default());
    let first_waker = Waker::from(Arc::clone(&first));
    assert!(
        Pin::new(&mut future)
            .poll(&mut Context::from_waker(&first_waker))
            .is_pending()
    );

    let mut moved = Box::new(future);
    let second = Arc::new(WakeCount::default());
    let second_waker = Waker::from(Arc::clone(&second));
    let mut cx = Context::from_waker(&second_waker);
    assert!(Pin::new(&mut *moved).poll(&mut cx).is_pending());
    let mut erased: Pin<
        Box<dyn Future<Output = Result<ActionTask, StoreStopped>> + Send + 'static>,
    > = Box::pin(*moved);
    assert!(
        actor
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    assert!(second.0.load(Ordering::SeqCst) > 0);
    let Poll::Ready(Ok(task)) = erased.as_mut().poll(&mut cx) else {
        panic!("committed receipt must return its task");
    };
    assert!(task.is_finished());
    assert!(!task.is_cancelled());
    assert_eq!(commits.load(Ordering::SeqCst), 10);
}

#[test]
fn scoped_receipt_rejection_does_not_stop_root_store() {
    enum ScopedAction {
        Remove,
        Increment,
    }

    struct ScopedFeature(Arc<AtomicUsize>);

    impl Reducer for ScopedFeature {
        type State = Option<()>;
        type Action = ScopedAction;

        fn reduce(
            &mut self,
            state: &mut Option<()>,
            action: &ScopedAction,
            _: &mut ReduceContext<'_>,
        ) -> Effect<ScopedAction> {
            match action {
                ScopedAction::Remove => *state = None,
                ScopedAction::Increment => {
                    self.0.fetch_add(1, Ordering::SeqCst);
                }
            }
            Effect::none()
        }
    }

    let runtime = support::ManualRuntime::new();
    let commits = Arc::new(AtomicUsize::new(0));
    let store = Store::new(
        Some(()),
        ScopedFeature(Arc::clone(&commits)),
        runtime.runtime(),
    );
    let mut actor = runtime.take_only_queued_task();
    let mut cx = Context::from_waker(Waker::noop());
    let child = {
        let registration = store.scope_some(
            OptionalStatePath::new(Option::as_ref, Option::as_mut),
            ActionPath::new(|action: &ScopedAction| Some(action), |action| action),
        );
        let mut registration = std::pin::pin!(registration);
        assert!(registration.as_mut().poll(&mut cx).is_pending());
        assert!(actor.as_mut().poll(&mut cx).is_pending());
        let Poll::Ready(Ok(child)) = registration.as_mut().poll(&mut cx) else {
            panic!("scope registration must complete");
        };
        child
    };
    let removal = store.try_send(ScopedAction::Remove).ok().unwrap();
    let mut rejected = child
        .try_send(ScopedAction::Increment)
        .ok()
        .unwrap()
        .into_future();
    assert!(Pin::new(&mut rejected).poll(&mut cx).is_pending());
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    drop(futures_executor::block_on(removal.finish()).unwrap());
    assert!(matches!(
        Pin::new(&mut rejected).poll(&mut cx),
        Poll::Ready(Err(_))
    ));
    assert_eq!(commits.load(Ordering::SeqCst), 0);
    let accepted = store.try_send(ScopedAction::Increment).ok().unwrap();
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    drop(futures_executor::block_on(accepted.finish()).unwrap());
    assert_eq!(commits.load(Ordering::SeqCst), 1);
}

#[test]
fn dropping_converted_receipt_does_not_cancel_admitted_causal_work() {
    for poll_before_drop in [false, true] {
        let runtime = support::ManualRuntime::new();
        let commits = Arc::new(AtomicUsize::new(0));
        let store = Store::new((), Feature(Arc::clone(&commits)), runtime.runtime());
        let mut actor = runtime.take_only_queued_task();
        let mut future = store.try_send(Action::Start).ok().unwrap().into_future();
        let mut cx = Context::from_waker(Waker::noop());
        if poll_before_drop {
            assert!(Pin::new(&mut future).poll(&mut cx).is_pending());
        }
        drop(future);
        assert!(actor.as_mut().poll(&mut cx).is_pending());
        assert_eq!(commits.load(Ordering::SeqCst), 1);
        let mut run = runtime.take_only_queued_task();
        assert!(run.as_mut().poll(&mut cx).is_pending());
        assert!(actor.as_mut().poll(&mut cx).is_pending());
        assert!(run.as_mut().poll(&mut cx).is_ready());
        assert_eq!(commits.load(Ordering::SeqCst), 11);
    }
}

#[test]
fn pending_receipt_does_not_retain_final_store_authority() {
    let runtime = support::ManualRuntime::new();
    let commits = Arc::new(AtomicUsize::new(0));
    let store = Store::new((), Feature(Arc::clone(&commits)), runtime.runtime());
    let mut actor = runtime.take_only_queued_task();
    let mut future = store.try_send(Action::Follow).ok().unwrap().into_future();
    let notifications = Arc::new(WakeCount::default());
    let waker = Waker::from(Arc::clone(&notifications));
    let mut cx = Context::from_waker(&waker);
    assert!(Pin::new(&mut future).poll(&mut cx).is_pending());
    drop(store);
    assert!(
        actor
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_ready()
    );
    assert!(notifications.0.load(Ordering::SeqCst) > 0);
    assert!(matches!(
        Pin::new(&mut future).poll(&mut cx),
        Poll::Ready(Err(_))
    ));
    assert_eq!(commits.load(Ordering::SeqCst), 0);
}
