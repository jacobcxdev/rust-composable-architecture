#[expect(dead_code, reason = "this target manually polls the captured actor")]
mod support;

use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};

use futures_util::FutureExt as _;
use rca_core::{Effect, Reduce, RuntimeFuture, Store};

// A failed consumer wake must not leave cancelled selector captures in a sleeping actor.
#[test]
fn panicking_observation_waker_still_schedules_actor_cleanup() {
    struct RecordWake {
        events: Arc<Mutex<Vec<&'static str>>>,
        label: &'static str,
        panic: bool,
    }

    impl Wake for RecordWake {
        fn wake(self: Arc<Self>) {
            self.events.lock().unwrap().push(self.label);
            assert!(!self.panic, "consumer wake panic");
        }
    }

    let runtime = support::ManualRuntime::new();
    let store = Store::new(
        7,
        Reduce::new(|_: &mut usize, (): &(), _| Effect::none()),
        runtime.runtime(),
    );
    let mut actor = runtime.take_only_queued_task();
    let capture = Arc::new(());
    let weak_capture = Arc::downgrade(&capture);
    let events = Arc::new(Mutex::new(Vec::new()));
    let actor_waker = Waker::from(Arc::new(RecordWake {
        events: Arc::clone(&events),
        label: "actor",
        panic: false,
    }));
    let mut actor_context = Context::from_waker(&actor_waker);
    let mut context = Context::from_waker(Waker::noop());
    let mut values = {
        let mut registration = std::pin::pin!(store.observe(move |state| {
            std::hint::black_box(&capture);
            *state
        }));
        assert!(registration.as_mut().poll(&mut context).is_pending());
        assert!(actor.as_mut().poll(&mut actor_context).is_pending());
        let Poll::Ready(values) = registration.as_mut().poll(&mut context) else {
            panic!("the actor must acknowledge registration");
        };
        values.unwrap()
    };
    assert_eq!(values.next().now_or_never(), Some(Some(7)));
    let observer_waker = Waker::from(Arc::new(RecordWake {
        events: Arc::clone(&events),
        label: "observer",
        panic: true,
    }));
    let waiting = {
        let mut next = std::pin::pin!(values.next());
        next.as_mut()
            .poll(&mut Context::from_waker(&observer_waker))
            .is_pending()
    };
    assert!(waiting);
    events.lock().unwrap().clear();
    assert!(weak_capture.upgrade().is_some());

    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(values)));
    assert!(panic.is_err(), "the consumer panic remains visible");
    assert_eq!(*events.lock().unwrap(), ["observer", "actor"]);
    assert!(actor.as_mut().poll(&mut actor_context).is_pending());
    assert!(weak_capture.upgrade().is_none());
    assert!(!store.is_closed(), "the actor remains usable after cleanup");
    drop(store);
    assert!(actor.as_mut().poll(&mut actor_context).is_ready());
}

// Replacing a task waker can cancel its actor. The same observation poll must
// report closure instead of returning Pending after that cancellation.
#[test]
fn replacing_observation_waker_can_cancel_actor_without_losing_closure() {
    struct CancelActorOnDrop(Mutex<Option<RuntimeFuture>>);

    #[expect(
        clippy::manual_noop_waker,
        reason = "its final-owner drop cancels the actor; a noop waker cannot own it"
    )]
    impl Wake for CancelActorOnDrop {
        fn wake(self: Arc<Self>) {}
    }

    impl Drop for CancelActorOnDrop {
        fn drop(&mut self) {
            drop(self.0.get_mut().unwrap().take());
        }
    }

    let runtime = support::ManualRuntime::new();
    let store = Store::new(
        7,
        Reduce::new(|_: &mut usize, (): &(), _| Effect::none()),
        runtime.runtime(),
    );
    let mut actor = runtime.take_only_queued_task();
    let capture = Arc::new(());
    let weak_capture = Arc::downgrade(&capture);
    let mut context = Context::from_waker(Waker::noop());
    let mut values = {
        let mut registration = std::pin::pin!(store.observe(move |state| {
            std::hint::black_box(&capture);
            *state
        }));
        assert!(registration.as_mut().poll(&mut context).is_pending());
        assert!(actor.as_mut().poll(&mut context).is_pending());
        let Poll::Ready(values) = registration.as_mut().poll(&mut context) else {
            panic!("the actor must acknowledge registration");
        };
        values.unwrap()
    };
    assert_eq!(values.next().now_or_never(), Some(Some(7)));

    let waker = Waker::from(Arc::new(CancelActorOnDrop(Mutex::new(Some(actor)))));
    let mut next = Box::pin(values.next());
    assert!(
        next.as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    // Leave the registered listener as the waker's final owner.
    drop(waker);
    assert!(!store.is_closed());
    assert!(weak_capture.upgrade().is_some());

    // A different task replaces that registration. Its old waker is dropped
    // outside the listener lock, cancelling the real actor and closing values.
    assert_eq!(next.as_mut().poll(&mut context), Poll::Ready(None));
    drop(next);
    assert!(store.is_closed());
    assert!(weak_capture.upgrade().is_none());
    assert_eq!(values.next().now_or_never(), Some(None));
    store.try_send(()).unwrap_err();
}
