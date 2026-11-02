//! Tagged listeners retain their contracts across async and blocking use.

#![cfg(all(feature = "std", not(loom), not(target_family = "wasm")))]

mod support;

use event_listener::{Event, IntoNotification, Listener};
use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::task::{Context, Poll, Wake, Waker};
use std::time::{Duration, Instant};

struct CountWake(AtomicUsize);

impl Wake for CountWake {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn tagged_diagnostics_and_reregistration_preserve_delivery() {
    if support::run_in_subprocess("tagged_diagnostics_and_reregistration_preserve_delivery") {
        return;
    }
    let event = Event::<usize>::with_tag();
    assert!(format!("{event:?}").contains("<uninitialized>"));
    let mut listener = event.listen();
    let initial = format!("{event:?}");
    assert!(initial.contains("listeners_total: 1"));
    assert!(initial.contains("listeners_notified: 0"));

    let old = Arc::new(CountWake(AtomicUsize::new(0)));
    let replacement = Arc::new(CountWake(AtomicUsize::new(0)));
    for target in [&old, &old, &replacement] {
        let waker = Waker::from(target.clone());
        assert_eq!(
            Pin::new(&mut listener).poll(&mut Context::from_waker(&waker)),
            Poll::Pending
        );
    }
    let mut locked = None;
    assert_eq!(
        event.notify(1.tag_with(|| {
            locked = Some(format!("{event:?}"));
            41
        })),
        1
    );
    assert!(locked.unwrap().contains("<locked>"));
    assert_eq!(old.0.load(Ordering::SeqCst), 0);
    assert_eq!(replacement.0.load(Ordering::SeqCst), 1);
    let notified = format!("{event:?}");
    assert!(notified.contains("listeners_total: 1"));
    assert!(notified.contains("listeners_notified: 1"));
    let mut cx = Context::from_waker(Waker::noop());
    assert_eq!(Pin::new(&mut listener).poll(&mut cx), Poll::Ready(41));
    assert_eq!(event.total_listeners(), 0);
    assert!(!event.is_notified());
    // Future permits a panic after completion; this notifier documents one.
    let repoll = catch_unwind(AssertUnwindSafe(|| Pin::new(&mut listener).poll(&mut cx)));
    let panic = repoll.expect_err("completed listener must reject a second poll");
    let message = panic
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| panic.downcast_ref::<String>().map(String::as_str));
    assert_eq!(message, Some("EventListener was not inserted into the linked list, make sure you're not polling EventListener/listener! after it has finished"));
}

struct ReleaseRegistration(mpsc::Sender<()>);

impl Wake for ReleaseRegistration {
    fn wake(self: Arc<Self>) {
        panic!("the async registration must be replaced before notification");
    }
}

impl Drop for ReleaseRegistration {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

#[test]
fn tagged_event_remains_reusable_after_expiry_and_blocking_handoffs() {
    if support::run_in_subprocess(
        "tagged_event_remains_reusable_after_expiry_and_blocking_handoffs",
    ) {
        return;
    }
    let event = Arc::new(Event::<usize>::with_tag());
    assert_eq!(event.listen().wait_timeout(Duration::ZERO), None);
    assert_eq!(event.listen().wait_deadline(Instant::now()), None);
    assert_eq!(event.listen().wait_timeout(Duration::from_millis(2)), None);
    assert_eq!(event.total_listeners(), 0);
    assert_eq!(event.notify(1.tag(99)), 0);

    for kind in 0..3 {
        let listener = event.listen();
        assert_eq!(event.notify(1.tag(kind)), 1);
        let ready = match kind {
            0 => Some(listener.wait()),
            1 => listener.wait_timeout(Duration::ZERO),
            _ => listener.wait_deadline(Instant::now()),
        };
        assert_eq!(ready, Some(kind));

        let (released, receive) = mpsc::channel();
        let waker = Waker::from(Arc::new(ReleaseRegistration(released)));
        let mut listener = event.listen();
        assert_eq!(
            Pin::new(&mut listener).poll(&mut Context::from_waker(&waker)),
            Poll::Pending
        );
        drop(waker);
        let producer_event = event.clone();
        let producer = std::thread::spawn(move || {
            let handoff = receive.recv_timeout(Duration::from_secs(3));
            // Release even after a failed handshake so the untimed wait can end.
            let delivered = producer_event.notify(1.tag(kind + 10));
            handoff.expect("blocking wait must release the old async registration");
            assert_eq!(delivered, 1);
        });
        let received = match kind {
            0 => Some(listener.wait()),
            1 => listener.wait_timeout(Duration::from_secs(5)),
            _ => listener.wait_deadline(Instant::now() + Duration::from_secs(5)),
        };
        producer.join().unwrap();
        assert_eq!(received, Some(kind + 10));
        assert_eq!(event.total_listeners(), 0);
        assert!(!event.is_notified());
    }
}
