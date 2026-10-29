#![cfg(all(not(loom), not(target_family = "wasm")))]

mod support;

use event_listener::{Event, IntoNotification, Listener};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::task::{Context, Poll, Wake, Waker};
use std::thread;
use std::time::{Duration, Instant};

#[test]
fn committed_notification_wins_over_elapsed_deadlines() {
    if support::run_in_subprocess("committed_notification_wins_over_elapsed_deadlines") {
        return;
    }

    let event = Event::<usize>::with_tag();
    let listener = event.listen();
    assert_eq!(event.notify(1.tag(11)), 1);
    assert_eq!(listener.wait(), 11);
    assert_eq!(event.total_listeners(), 0);

    let listener = event.listen();
    assert_eq!(event.notify(1.tag(12)), 1);
    assert_eq!(listener.wait_timeout(Duration::ZERO), Some(12));
    assert_eq!(event.total_listeners(), 0);

    let listener = event.listen();
    assert_eq!(event.notify(1.tag(13)), 1);
    assert_eq!(listener.wait_deadline(Instant::now()), Some(13));
    assert_eq!(event.total_listeners(), 0);

    let listener = event.listen();
    assert_eq!(event.notify(1.tag(14)), 1);
    assert_eq!(listener.wait_timeout(Duration::MAX), Some(14));
    assert_eq!(event.total_listeners(), 0);
}

#[test]
fn stack_waits_deliver_committed_tags_before_elapsed_deadlines() {
    if support::run_in_subprocess("stack_waits_deliver_committed_tags_before_elapsed_deadlines") {
        return;
    }

    let event = Event::<usize>::with_tag();
    event_listener::listener!(event => listener);
    assert_eq!(event.notify(1.tag(21)), 1);
    assert_eq!(listener.wait(), 21);
    assert_eq!(event.total_listeners(), 0);

    event_listener::listener!(event => listener);
    assert_eq!(event.notify(1.tag(22)), 1);
    assert_eq!(listener.wait_timeout(Duration::ZERO), Some(22));
    assert_eq!(event.total_listeners(), 0);

    event_listener::listener!(event => listener);
    assert_eq!(event.notify(1.tag(23)), 1);
    assert_eq!(listener.wait_deadline(Instant::now()), Some(23));
    assert_eq!(event.total_listeners(), 0);

    event_listener::listener!(event => listener);
    assert_eq!(event.notify(1.tag(24)), 1);
    assert_eq!(listener.wait_timeout(Duration::MAX), Some(24));
    assert_eq!(event.total_listeners(), 0);
}

#[test]
fn stack_timeout_and_deadline_unlink_before_reregistration() {
    if support::run_in_subprocess("stack_timeout_and_deadline_unlink_before_reregistration") {
        return;
    }

    let event = Event::<usize>::with_tag();
    event_listener::listener!(event => listener);
    assert_eq!(event.total_listeners(), 1);
    assert_eq!(listener.wait_timeout(Duration::ZERO), None);
    assert_eq!(event.total_listeners(), 0);
    assert_eq!(event.notify(1.tag(31)), 0);

    event_listener::listener!(event => listener);
    assert_eq!(event.total_listeners(), 1);
    assert_eq!(listener.wait_deadline(Instant::now()), None);
    assert_eq!(event.total_listeners(), 0);
    assert_eq!(event.notify(1.tag(32)), 0);

    event_listener::listener!(event => listener);
    assert_eq!(event.notify(1.tag(33)), 1);
    assert_eq!(listener.wait(), 33);
    assert_eq!(event.total_listeners(), 0);
}

#[test]
fn elapsed_timeout_and_deadline_unregister_without_notification() {
    if support::run_in_subprocess("elapsed_timeout_and_deadline_unregister_without_notification") {
        return;
    }

    let event = Event::new();
    assert_eq!(event.listen().wait_timeout(Duration::ZERO), None);
    assert_eq!(event.total_listeners(), 0);
    assert_eq!(event.listen().wait_deadline(Instant::now()), None);
    assert_eq!(event.total_listeners(), 0);

    let listener = event.listen();
    assert_eq!(event.notify(1), 1);
    assert_eq!(listener.wait_timeout(Duration::ZERO), Some(()));
}

#[test]
fn future_timeout_and_deadline_expire_and_unlink() {
    if support::run_in_subprocess("future_timeout_and_deadline_expire_and_unlink") {
        return;
    }

    let event = Event::new();
    assert_eq!(event.listen().wait_timeout(Duration::from_millis(5)), None);
    assert_eq!(event.total_listeners(), 0);
    assert_eq!(
        event
            .listen()
            .wait_deadline(Instant::now() + Duration::from_millis(5)),
        None
    );
    assert_eq!(event.total_listeners(), 0);
    assert_eq!(event.notify(1), 0);
}

// Once the listener replaces its registered async waker with a blocking
// unparker, releasing that waker tells the producer it may deliver a tag.
// There are no other strong references when a blocking wait begins.
struct RegistrationReleased {
    event: Arc<Event<usize>>,
    released: mpsc::Sender<usize>,
    wakes: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
}

impl Wake for RegistrationReleased {
    fn wake(self: Arc<Self>) {
        self.wakes.fetch_add(1, Ordering::SeqCst);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.wakes.fetch_add(1, Ordering::SeqCst);
    }
}

impl Drop for RegistrationReleased {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        // This query also requires that user-owned waker destruction occurs
        // outside the notifier's list lock.
        let registered = self.event.total_listeners();
        let _ = self.released.send(registered);
    }
}

#[test]
fn blocking_waits_replace_async_registration_and_receive_real_wakeups() {
    if support::run_in_subprocess(
        "blocking_waits_replace_async_registration_and_receive_real_wakeups",
    ) {
        return;
    }

    for wait_kind in 0..3 {
        let event = Arc::new(Event::<usize>::with_tag());
        let (released_tx, released_rx) = mpsc::channel();
        let wakes = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        let waker = Waker::from(Arc::new(RegistrationReleased {
            event: event.clone(),
            released: released_tx,
            wakes: wakes.clone(),
            drops: drops.clone(),
        }));
        let mut listener = event.listen();
        assert_eq!(
            Pin::new(&mut listener).poll(&mut Context::from_waker(&waker)),
            Poll::Pending
        );
        drop(waker);
        let producer_event = event.clone();
        let producer = thread::spawn(move || {
            let registered = released_rx.recv_timeout(Duration::from_secs(3));
            // Deliver even if the handshake times out, so the unbounded wait
            // cannot remain asleep merely because this assertion failed.
            let delivered = producer_event.notify(1.tag(71));
            assert_eq!(registered.unwrap(), 1);
            delivered
        });

        let received = match wait_kind {
            0 => Some(listener.wait()),
            1 => listener.wait_timeout(Duration::from_secs(5)),
            _ => listener.wait_deadline(Instant::now() + Duration::from_secs(5)),
        };
        let delivered = producer.join().unwrap();
        assert_eq!(received, Some(71));
        assert_eq!(delivered, 1);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(wakes.load(Ordering::SeqCst), 0);
        assert_eq!(event.total_listeners(), 0);
    }
}

#[test]
fn nested_blocking_wait_from_waker_drop_preserves_outer_notification() {
    if support::run_in_subprocess(
        "nested_blocking_wait_from_waker_drop_preserves_outer_notification",
    ) {
        return;
    }

    struct NotifyOnDrop(Arc<Event>);
    impl Wake for NotifyOnDrop {
        fn wake(self: Arc<Self>) {
            panic!("the nested async waker must be replaced before notification");
        }
    }
    impl Drop for NotifyOnDrop {
        fn drop(&mut self) {
            assert_eq!(self.0.notify(1), 1);
        }
    }

    struct WaitOnDrop {
        outer: Arc<Event>,
        inner: Option<event_listener::EventListener>,
        completed: Arc<AtomicUsize>,
        panic_after_inner: bool,
    }
    impl Wake for WaitOnDrop {
        fn wake(self: Arc<Self>) {
            panic!("the outer async waker must be replaced before notification");
        }
    }
    impl Drop for WaitOnDrop {
        fn drop(&mut self) {
            // The outer blocking registration is already installed. The nested
            // wait must not consume the outer wait's only wake token.
            assert_eq!(self.outer.notify(1), 1);
            self.inner.take().unwrap().wait();
            self.completed.fetch_add(1, Ordering::SeqCst);
            assert!(self.outer.is_notified());
            eprintln!("nested wait returned while the outer notification remains committed");
            if self.panic_after_inner {
                panic!("nested wait callback panic");
            }
        }
    }

    for panic_after_inner in [false, true] {
        let outer = Arc::new(Event::new());
        let inner = Arc::new(Event::new());
        let completed = Arc::new(AtomicUsize::new(0));
        let mut inner_listener = inner.listen();
        let inner_waker = Waker::from(Arc::new(NotifyOnDrop(Arc::clone(&inner))));
        assert_eq!(
            Pin::new(&mut inner_listener).poll(&mut Context::from_waker(&inner_waker)),
            Poll::Pending
        );
        drop(inner_waker);

        let mut outer_listener = outer.listen();
        let outer_waker = Waker::from(Arc::new(WaitOnDrop {
            outer: Arc::clone(&outer),
            inner: Some(inner_listener),
            completed: Arc::clone(&completed),
            panic_after_inner,
        }));
        assert_eq!(
            Pin::new(&mut outer_listener).poll(&mut Context::from_waker(&outer_waker)),
            Poll::Pending
        );
        drop(outer_waker);

        // Replacing the outer async registration runs both destructor callbacks
        // synchronously. No producer thread, timer or extra poll supplies a wake.
        let result = std::panic::catch_unwind(|| outer_listener.wait());
        if panic_after_inner {
            assert_eq!(
                result.unwrap_err().downcast_ref::<&str>(),
                Some(&"nested wait callback panic")
            );
        } else {
            result.unwrap();
        }
        assert_eq!(completed.load(Ordering::SeqCst), 1);
        assert_eq!(outer.total_listeners(), 0);
        assert_eq!(inner.total_listeners(), 0);
        assert!(!outer.is_notified());
        assert!(!inner.is_notified());

        // Ordinary waits still work on the same thread after return or unwind.
        let listener = outer.listen();
        assert_eq!(outer.notify(1), 1);
        listener.wait();
        assert_eq!(outer.total_listeners(), 0);
    }
}
