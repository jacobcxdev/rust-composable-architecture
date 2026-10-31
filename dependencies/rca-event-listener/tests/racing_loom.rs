//! Minimal public-API races missing from upstream's single-threaded Loom cases.
#![cfg(loom)]

use event_listener::{Event, EventListener, IntoNotification};
use loom::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use loom::sync::Arc;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};
use waker_fn::waker_fn;

fn model(name: &str, body: impl Fn() + Send + Sync + 'static) {
    // Ordinary atomics count completed model invocations outside the model's
    // synchronisation domain; they do not influence any schedule or assertion.
    let completed = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let record = std::sync::Arc::clone(&completed);
    let mut builder = loom::model::Builder::new();
    builder.max_threads = 2;
    builder.max_branches = 1000;
    builder.preemption_bound = Some(2);
    builder.max_permutations = None;
    builder.max_duration = None;
    builder.check(move || {
        body();
        record.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    });
    eprintln!(
        "model={name}; completed_permutations={}; threads=2; branches=1000; preemptions=2; no internal time/permutation cutoff",
        completed.load(std::sync::atomic::Ordering::Relaxed)
    );
}

fn poll(listener: &mut EventListener, waker: &Waker) -> Poll<()> {
    Pin::new(listener).poll(&mut Context::from_waker(waker))
}

fn counting_waker(event: &Arc<Event>, count: &Arc<AtomicUsize>) -> Waker {
    let event = Arc::clone(event);
    let count = Arc::clone(count);
    waker_fn(move || {
        // Re-entry also checks that wake dispatch releases the list lock.
        let _ = event.total_listeners();
        count.fetch_add(1, Ordering::SeqCst);
    })
}

#[test]
fn condition_recheck_or_notification_prevents_a_lost_wake() {
    model(
        "condition_recheck_or_notification_prevents_a_lost_wake",
        || {
            let event = Arc::new(Event::new());
            let condition = Arc::new(AtomicBool::new(false));
            let notifying = Arc::clone(&event);
            let changed = Arc::clone(&condition);
            let producer = loom::thread::spawn(move || {
                changed.store(true, Ordering::Relaxed);
                notifying.notify(usize::MAX);
            });

            let mut listener = event.listen();
            let count = Arc::new(AtomicUsize::new(0));
            let waker = counting_waker(&event, &count);
            // The condition has no independent release/acquire synchronisation.
            // Registration must either see the earlier notification's write, or
            // retain a listener for the later notification.
            let observed = condition.load(Ordering::Relaxed);
            let first_poll = (!observed).then(|| poll(&mut listener, &waker));
            producer.join().unwrap();

            if let Some(Poll::Pending) = first_poll {
                // A pending executor gets another poll only after a real wake.
                assert_eq!(count.load(Ordering::SeqCst), 1);
                assert_eq!(poll(&mut listener, &waker), Poll::Ready(()));
            }
            // Ready already consumed the notification; an observed condition needs
            // no wait. Neither branch polls a completed or unwoken future again.
            drop(listener);
            assert_eq!(event.total_listeners(), 0);
        },
    );
}

#[test]
fn initial_registration_races_notification() {
    model("initial_registration_races_notification", || {
        let event = Arc::new(Event::new());
        let mut listener = event.listen();
        let count = Arc::new(AtomicUsize::new(0));
        let waker = counting_waker(&event, &count);
        let notifying = Arc::clone(&event);
        let thread = loom::thread::spawn(move || assert_eq!(notifying.notify(1), 1));
        let first_poll = poll(&mut listener, &waker);
        thread.join().unwrap();
        if first_poll.is_pending() {
            assert_eq!(count.load(Ordering::SeqCst), 1);
            assert!(poll(&mut listener, &waker).is_ready());
        } else {
            // Notification may land before registration or after installation
            // but before the recheck completes. Either wake count is valid.
            assert!(count.load(Ordering::SeqCst) <= 1);
        }
        assert_eq!(event.total_listeners(), 0);
    });
}

#[test]
fn replacement_registration_races_notification() {
    model("replacement_registration_races_notification", || {
        let event = Arc::new(Event::new());
        let mut listener = event.listen();
        let old_count = Arc::new(AtomicUsize::new(0));
        let new_count = Arc::new(AtomicUsize::new(0));
        let old = counting_waker(&event, &old_count);
        let new = counting_waker(&event, &new_count);
        assert!(poll(&mut listener, &old).is_pending());
        let notifying = Arc::clone(&event);
        let thread = loom::thread::spawn(move || assert_eq!(notifying.notify(1), 1));
        let replacement = poll(&mut listener, &new);
        thread.join().unwrap();
        assert_eq!(
            old_count.load(Ordering::SeqCst) + new_count.load(Ordering::SeqCst),
            1
        );
        if replacement.is_pending() {
            assert!(poll(&mut listener, &new).is_ready());
        }
        assert_eq!(event.total_listeners(), 0);
    });
}

#[test]
fn cancellation_races_ordinary_notification() {
    model("cancellation_races_ordinary_notification", || {
        let event = Arc::new(Event::new());
        let mut first = event.listen();
        let mut second = event.listen();
        let mut third = event.listen();
        let count = Arc::new(AtomicUsize::new(0));
        let waker = counting_waker(&event, &count);
        assert!(poll(&mut first, &waker).is_pending());
        assert!(poll(&mut second, &waker).is_pending());
        assert!(poll(&mut third, &waker).is_pending());
        let notifying = Arc::clone(&event);
        let thread = loom::thread::spawn(move || assert_eq!(notifying.notify(1), 1));
        drop(first);
        thread.join().unwrap();
        assert!(poll(&mut second, &waker).is_ready());
        assert!(poll(&mut third, &waker).is_pending());
        assert!((1..=2).contains(&count.load(Ordering::SeqCst)));
        drop(third);
        assert_eq!(event.total_listeners(), 0);
    });
}

#[test]
fn cancellation_races_additional_notification() {
    model("cancellation_races_additional_notification", || {
        let event = Arc::new(Event::new());
        let mut first = event.listen();
        let mut second = event.listen();
        let mut third = event.listen();
        let count = Arc::new(AtomicUsize::new(0));
        let waker = counting_waker(&event, &count);
        assert!(poll(&mut first, &waker).is_pending());
        assert!(poll(&mut second, &waker).is_pending());
        assert!(poll(&mut third, &waker).is_pending());
        let notifying = Arc::clone(&event);
        let thread = loom::thread::spawn(move || assert_eq!(notifying.notify(2.additional()), 2));
        drop(first);
        thread.join().unwrap();
        assert!(poll(&mut second, &waker).is_ready());
        assert!(poll(&mut third, &waker).is_ready());
        assert!((2..=3).contains(&count.load(Ordering::SeqCst)));
        assert_eq!(event.total_listeners(), 0);
    });
}

#[test]
fn tagged_initial_registration_races_notification() {
    model("tagged_initial_registration_races_notification", || {
        let event = Arc::new(Event::<usize>::with_tag());
        let mut listener = event.listen();
        let count = Arc::new(AtomicUsize::new(0));
        let waking = Arc::clone(&event);
        let wake_count = Arc::clone(&count);
        let waker = waker_fn(move || {
            // Tagged wake dispatch must also release the list lock before re-entry.
            let _ = waking.total_listeners();
            wake_count.fetch_add(1, Ordering::SeqCst);
        });
        let notifying = Arc::clone(&event);
        let thread = loom::thread::spawn(move || assert_eq!(notifying.notify(1.tag(71)), 1));
        let first_poll = Pin::new(&mut listener).poll(&mut Context::from_waker(&waker));
        thread.join().unwrap();

        // Registration must neither lose the tag nor strand a pending listener.
        // A completed listener is never polled a second time.
        match first_poll {
            Poll::Ready(tag) => {
                assert_eq!(tag, 71);
                assert!(count.load(Ordering::SeqCst) <= 1);
            }
            Poll::Pending => {
                assert_eq!(count.load(Ordering::SeqCst), 1);
                assert_eq!(
                    Pin::new(&mut listener).poll(&mut Context::from_waker(&waker)),
                    Poll::Ready(71)
                );
            }
        }
        assert_eq!(event.total_listeners(), 0);
    });
}

#[test]
fn wait_timeout_rejects_unnotified_listener_under_loom() {
    use event_listener::Listener;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::time::Duration;

    model(
        "wait_timeout_rejects_unnotified_listener_under_loom",
        || {
            let event = Event::new();
            let listener = event.listen();
            assert_eq!(event.total_listeners(), 1);

            // Catch inside the model so listener cleanup also uses Loom's state.
            let panic = catch_unwind(AssertUnwindSafe(|| listener.wait_timeout(Duration::ZERO)))
                .expect_err("an unnotified timed wait must panic under Loom");
            assert_eq!(
                panic.downcast_ref::<&str>().copied(),
                Some("parking does not support timeouts under loom")
            );
            assert_eq!(event.total_listeners(), 0);
            assert_eq!(event.notify(1), 0);
        },
    );
}
