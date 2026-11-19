// Modified for Rust Composable Architecture; see UPSTREAM.md for provenance and changes.

#![cfg(not(loom))]

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::Context;

use event_listener::{Event, EventListener};
use waker_fn::waker_fn;

#[cfg(all(feature = "std", not(target_family = "wasm")))]
mod support;

#[cfg(target_family = "wasm")]
use wasm_bindgen_test::wasm_bindgen_test as test;

fn is_notified(listener: &mut EventListener) -> bool {
    let waker = waker_fn(|| ());
    Pin::new(listener)
        .poll(&mut Context::from_waker(&waker))
        .is_ready()
}

#[test]
fn notification_snapshot_is_false_without_listeners() {
    let event = Event::new();
    assert!(!event.is_notified());
    assert_eq!(event.notify(1), 0);
    assert!(!event.is_notified());
}

#[test]
fn notification_snapshot_is_false_after_unpolled_listener_drop() {
    let event = Event::new();
    let listener = event.listen();
    assert!(!event.is_notified());
    drop(listener);
    assert!(!event.is_notified());
}

#[test]
fn notification_snapshot_tracks_notification_propagation_and_drop() {
    let event = Event::new();
    let first = event.listen();
    let second = event.listen();
    assert_eq!(event.notify(1), 1);
    assert!(event.is_notified());
    drop(first);
    assert!(event.is_notified());
    drop(second);
    assert!(!event.is_notified());
}

#[test]
fn notification_snapshot_is_false_after_consumption_and_reuse() {
    let event = Event::new();
    for _ in 0..2 {
        let mut listener = event.listen();
        assert!(!is_notified(&mut listener));
        assert!(!event.is_notified());
        assert_eq!(event.notify(1), 1);
        assert!(event.is_notified());
        assert!(is_notified(&mut listener));
        assert!(!event.is_notified());
    }
    assert_eq!(event.notify(1), 0);
    assert!(!event.is_notified());
}

#[test]
#[cfg(all(feature = "std", not(target_family = "wasm")))]
fn notification_snapshot_is_false_after_timeout() {
    use event_listener::Listener;
    use std::time::Duration;

    let event = Event::new();
    assert_eq!(event.listen().wait_timeout(Duration::ZERO), None);
    assert!(!event.is_notified());
}

#[test]
#[cfg(all(feature = "std", not(target_family = "wasm")))]
fn notification_snapshot_is_false_after_expired_deadline() {
    use event_listener::Listener;
    use std::time::Instant;

    let event = Event::new();
    assert_eq!(event.listen().wait_deadline(Instant::now()), None);
    assert!(!event.is_notified());
}

#[test]
#[cfg(all(feature = "std", not(target_family = "wasm")))]
fn expired_deadline_returns_tag_from_replaced_waker_drop() {
    if support::run_in_subprocess("expired_deadline_returns_tag_from_replaced_waker_drop") {
        return;
    }
    use event_listener::{IntoNotification, Listener};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Wake, Waker};
    use std::time::Instant;

    struct NotifyOnDrop {
        event: Arc<Event<usize>>,
        drops: Arc<AtomicUsize>,
    }

    #[expect(
        clippy::manual_noop_waker,
        reason = "The custom Drop callback triggers the deadline race; Waker::noop would erase it."
    )]
    impl Wake for NotifyOnDrop {
        fn wake(self: Arc<Self>) {}
    }

    impl Drop for NotifyOnDrop {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
            self.event.notify(1.tag(71));
        }
    }

    let event = Arc::new(Event::with_tag());
    let drops = Arc::new(AtomicUsize::new(0));
    let mut listener = event.listen();
    let waker = Waker::from(Arc::new(NotifyOnDrop {
        event: event.clone(),
        drops: drops.clone(),
    }));
    assert!(Pin::new(&mut listener)
        .poll(&mut Context::from_waker(&waker))
        .is_pending());
    drop(waker);
    assert_eq!(drops.load(Ordering::SeqCst), 0);

    // Replacing the sole remaining waker with an unparker notifies this listener.
    // The expired-deadline removal must return that tag instead of losing it.
    assert_eq!(listener.wait_deadline(Instant::now()), Some(71));
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(event.total_listeners(), 0);
    assert!(!event.is_notified());

    // Reuse the event without a notifying drop callback. An expired wait must
    // return no tag, not replay the notification consumed by the first listener.
    assert_eq!(event.listen().wait_deadline(Instant::now()), None);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(event.total_listeners(), 0);
    assert!(!event.is_notified());
}

#[test]
fn notification_snapshot_debug_reports_actual_count() {
    let event = Event::new();
    let first = event.listen();
    let second = event.listen();
    assert_eq!(event.notify(1), 1);
    assert!(format!("{event:?}").contains("listeners_notified: 1"));
    assert_eq!(event.notify(2), 1);
    assert!(format!("{event:?}").contains("listeners_notified: 2"));
    drop(first);
    drop(second);
    assert!(format!("{event:?}").contains("listeners_notified: 0"));
}

#[test]
fn notify() {
    let event = Event::new();

    let mut l1 = event.listen();
    let mut l2 = event.listen();
    let mut l3 = event.listen();

    assert!(!is_notified(&mut l1));
    assert!(!is_notified(&mut l2));
    assert!(!is_notified(&mut l3));

    assert_eq!(event.notify(2), 2);
    assert_eq!(event.notify(1), 0);

    assert!(is_notified(&mut l1));
    assert!(is_notified(&mut l2));
    assert!(!is_notified(&mut l3));
}

#[test]
fn notify_additional() {
    let event = Event::new();

    let mut l1 = event.listen();
    let mut l2 = event.listen();
    let mut l3 = event.listen();

    assert_eq!(event.notify_additional(1), 1);
    assert_eq!(event.notify(1), 0);
    assert_eq!(event.notify_additional(1), 1);

    assert!(is_notified(&mut l1));
    assert!(is_notified(&mut l2));
    assert!(!is_notified(&mut l3));
}

#[test]
fn notify_one() {
    let event = Event::new();

    let mut l1 = event.listen();
    let mut l2 = event.listen();

    assert!(!is_notified(&mut l1));
    assert!(!is_notified(&mut l2));

    assert_eq!(event.notify(1), 1);
    assert!(is_notified(&mut l1));
    assert!(!is_notified(&mut l2));

    assert_eq!(event.notify(1), 1);
    assert!(is_notified(&mut l2));
}

#[test]
fn notify_all() {
    let event = Event::new();

    let mut l1 = event.listen();
    let mut l2 = event.listen();

    assert!(!is_notified(&mut l1));
    assert!(!is_notified(&mut l2));

    assert_eq!(event.notify(usize::MAX), 2);
    assert!(is_notified(&mut l1));
    assert!(is_notified(&mut l2));
}

#[test]
fn drop_notified() {
    let event = Event::new();

    let l1 = event.listen();
    let mut l2 = event.listen();
    let mut l3 = event.listen();

    assert_eq!(event.notify(1), 1);
    drop(l1);
    assert!(is_notified(&mut l2));
    assert!(!is_notified(&mut l3));
}

#[test]
fn drop_notified2() {
    let event = Event::new();

    let l1 = event.listen();
    let mut l2 = event.listen();
    let mut l3 = event.listen();

    assert_eq!(event.notify(2), 2);
    drop(l1);
    assert!(is_notified(&mut l2));
    assert!(!is_notified(&mut l3));
}

#[test]
fn drop_notified_additional() {
    let event = Event::new();

    let l1 = event.listen();
    let mut l2 = event.listen();
    let mut l3 = event.listen();
    let mut l4 = event.listen();

    assert_eq!(event.notify_additional(1), 1);
    assert_eq!(event.notify(2), 1);
    drop(l1);
    assert!(is_notified(&mut l2));
    assert!(is_notified(&mut l3));
    assert!(!is_notified(&mut l4));
}

#[test]
fn drop_non_notified() {
    let event = Event::new();

    let mut l1 = event.listen();
    let mut l2 = event.listen();
    let l3 = event.listen();

    assert_eq!(event.notify(1), 1);
    drop(l3);
    assert!(is_notified(&mut l1));
    assert!(!is_notified(&mut l2));
}

#[test]
fn notify_all_fair() {
    let event = Event::new();
    let v = Arc::new(Mutex::new(vec![]));

    let mut l1 = event.listen();
    let mut l2 = event.listen();
    let mut l3 = event.listen();

    let waker1 = {
        let v = v.clone();
        waker_fn(move || v.lock().unwrap().push(1))
    };
    let waker2 = {
        let v = v.clone();
        waker_fn(move || v.lock().unwrap().push(2))
    };
    let waker3 = {
        let v = v.clone();
        waker_fn(move || v.lock().unwrap().push(3))
    };

    assert!(Pin::new(&mut l1)
        .poll(&mut Context::from_waker(&waker1))
        .is_pending());
    assert!(Pin::new(&mut l2)
        .poll(&mut Context::from_waker(&waker2))
        .is_pending());
    assert!(Pin::new(&mut l3)
        .poll(&mut Context::from_waker(&waker3))
        .is_pending());

    assert_eq!(event.notify(usize::MAX), 3);
    assert_eq!(&*v.lock().unwrap(), &[1, 2, 3]);

    assert!(Pin::new(&mut l1)
        .poll(&mut Context::from_waker(&waker1))
        .is_ready());
    assert!(Pin::new(&mut l2)
        .poll(&mut Context::from_waker(&waker2))
        .is_ready());
    assert!(Pin::new(&mut l3)
        .poll(&mut Context::from_waker(&waker3))
        .is_ready());
}
