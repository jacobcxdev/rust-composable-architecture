//! Public count, composition and cancellation contracts for notification policies.

#![cfg(all(not(loom), not(target_family = "wasm")))]

mod support;

use event_listener::{Event, EventListener, IntoNotification};
#[cfg(feature = "std")]
use std::cell::Cell;
use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

fn poll<T>(listener: &mut EventListener<T>) -> Poll<T> {
    Pin::new(listener).poll(&mut Context::from_waker(Waker::noop()))
}

#[test]
fn every_signed_negative_count_preserves_pending_listeners() {
    if support::run_in_subprocess("every_signed_negative_count_preserves_pending_listeners") {
        return;
    }

    macro_rules! check {
        ($($ty:ty),*) => {$(
            for count in [<$ty>::MIN, -1 as $ty] {
                let event = Event::new();
                let mut listener = event.listen();
                assert_eq!(poll(&mut listener), Poll::Pending);
                let panic = catch_unwind(AssertUnwindSafe(|| event.notify(count))).unwrap_err();
                assert_eq!(panic.downcast_ref::<&str>(), Some(&"negative notification count"));
                #[cfg(feature = "std")]
                assert_eq!(event.total_listeners(), 1);
                assert_eq!(poll(&mut listener), Poll::Pending);
                assert_eq!(event.notify(1usize), 1);
                assert_eq!(poll(&mut listener), Poll::Ready(()));
                #[cfg(feature = "std")]
                assert_eq!(event.total_listeners(), 0);
            }
        )*};
    }
    check!(isize, i8, i16, i32, i64, i128);
}

#[test]
fn every_numeric_type_preserves_nonnegative_range_and_count_policy() {
    if support::run_in_subprocess("every_numeric_type_preserves_nonnegative_range_and_count_policy")
    {
        return;
    }

    macro_rules! check {
        ($($ty:ty),*) => {$(
            for count in [0 as $ty, 1 as $ty, <$ty>::MAX / 2 + 1, <$ty>::MAX] {
                let event = Event::new();
                let mut listeners: Vec<_> = (0..3).map(|_| event.listen()).collect();
                let result = catch_unwind(AssertUnwindSafe(|| event.notify(count)));
                // The public numeric range is target-width usize, not a saturating cast.
                let notified = if count as u128 > usize::MAX as u128 {
                    let panic = result.unwrap_err();
                    let message = panic.downcast_ref::<String>().expect("overflow diagnostic");
                    assert!(message.starts_with("overflow"));
                    #[cfg(feature = "std")]
                    assert_eq!(event.total_listeners(), 3);
                    0
                } else {
                    let expected = (count as u128).min(3) as usize;
                    assert_eq!(result.unwrap(), expected, "type={}, count={count}", stringify!($ty));
                    expected
                };
                for (index, listener) in listeners.iter_mut().enumerate() {
                    assert_eq!(poll(listener).is_ready(), index < notified);
                }
                assert_eq!(event.notify(usize::MAX), 3 - notified);
                for listener in &mut listeners[notified..] {
                    assert_eq!(poll(listener), Poll::Ready(()));
                }
                #[cfg(feature = "std")]
                assert_eq!(event.total_listeners(), 0);
            }
        )*};
    }
    check!(usize, u8, u16, u32, u64, u128, isize, i8, i16, i32, i64, i128);
}

#[test]
fn minimum_and_additional_counts_respect_existing_notifications() {
    if support::run_in_subprocess("minimum_and_additional_counts_respect_existing_notifications") {
        return;
    }

    // Keep the first two notifications unconsumed. Expected new/total counts
    // are literal public outcomes, not the implementation's count arithmetic.
    for (request, minimum_new, additional_new, minimum_total, additional_total) in [
        (0, 0, 0, 2, 2),
        (1, 0, 1, 2, 3),
        (2, 0, 2, 2, 4),
        (3, 1, 3, 3, 5),
        (5, 3, 3, 5, 5),
        (usize::MAX, 3, 3, 5, 5),
    ] {
        for (additional, expected_new, expected_total) in [
            (false, minimum_new, minimum_total),
            (true, additional_new, additional_total),
        ] {
            let event = Event::new();
            let mut listeners: Vec<_> = (0..5).map(|_| event.listen()).collect();
            assert_eq!(event.notify(2), 2);
            let actual = if additional {
                event.notify(request.additional())
            } else {
                event.notify(request)
            };
            assert_eq!(
                actual, expected_new,
                "request={request}, additional={additional}"
            );
            for (index, listener) in listeners.iter_mut().enumerate() {
                assert_eq!(
                    poll(listener).is_ready(),
                    index < expected_total,
                    "request={request}, additional={additional}, listener={index}"
                );
            }
            assert!(!event.is_notified());
        }
    }
}

#[cfg(feature = "std")]
#[test]
fn tag_and_relaxed_wrapper_order_preserves_notification_policy() {
    if support::run_in_subprocess("tag_and_relaxed_wrapper_order_preserves_notification_policy") {
        return;
    }

    for order in 0..6 {
        let event = Event::<usize>::with_tag();
        let mut listeners: Vec<_> = (0..5).map(|_| event.listen()).collect();
        assert_eq!(event.notify(2usize.tag(7)), 2);
        let actual = match order {
            0 => event.notify(3usize.tag(9)),
            1 => event.notify(3usize.relaxed().tag(9).relaxed()),
            2 => event.notify(3usize.tag(9).relaxed()),
            3 => event.notify(1usize.additional().relaxed().tag(9)),
            4 => event.notify(1usize.tag(9).relaxed().additional()),
            5 => event.notify(1usize.additional().tag(9).additional().relaxed()),
            _ => unreachable!(),
        };
        assert_eq!(actual, 1, "wrapper order {order}");
        for (listener, expected) in listeners.iter_mut().zip([
            Poll::Ready(7),
            Poll::Ready(7),
            Poll::Ready(9),
            Poll::Pending,
            Poll::Pending,
        ]) {
            assert_eq!(poll(listener), expected, "wrapper order {order}");
        }
    }
}

#[cfg(feature = "std")]
#[test]
fn tag_generators_run_once_per_new_notification_through_nested_wrappers() {
    if support::run_in_subprocess(
        "tag_generators_run_once_per_new_notification_through_nested_wrappers",
    ) {
        return;
    }

    for (request, minimum_new, additional_new) in [
        (0, 0, 0),
        (1, 0, 1),
        (2, 0, 2),
        (3, 1, 3),
        (usize::MAX, 3, 3),
    ] {
        for (order, expected_new) in [(0, minimum_new), (1, additional_new), (2, additional_new)] {
            let event = Event::<usize>::with_tag();
            let mut listeners: Vec<_> = (0..5).map(|_| event.listen()).collect();
            assert_eq!(event.notify(2usize.tag(7)), 2);
            let calls = Cell::new(0);
            let mut generate = || {
                calls.set(calls.get() + 1);
                100 + calls.get()
            };
            let actual = match order {
                0 => event.notify(request.relaxed().tag_with(&mut generate).relaxed()),
                1 => event.notify(
                    request
                        .additional()
                        .relaxed()
                        .tag_with(&mut generate)
                        .additional(),
                ),
                2 => event.notify(
                    request
                        .tag_with(&mut generate)
                        .relaxed()
                        .additional()
                        .relaxed(),
                ),
                _ => unreachable!(),
            };
            assert_eq!(actual, expected_new, "request={request}, order={order}");
            assert_eq!(
                calls.get(),
                expected_new,
                "request={request}, order={order}"
            );
            assert_eq!(poll(&mut listeners[0]), Poll::Ready(7));
            assert_eq!(poll(&mut listeners[1]), Poll::Ready(7));
            for (index, (listener, tag)) in
                listeners[2..].iter_mut().zip([101, 102, 103]).enumerate()
            {
                let expected = if index < expected_new {
                    Poll::Ready(tag)
                } else {
                    Poll::Pending
                };
                assert_eq!(poll(listener), expected, "request={request}, order={order}");
            }
        }
    }
}

#[cfg(feature = "std")]
#[test]
fn cancellation_preserves_additional_policy_and_moves_the_original_tag() {
    if support::run_in_subprocess(
        "cancellation_preserves_additional_policy_and_moves_the_original_tag",
    ) {
        return;
    }

    for additional in [false, true] {
        let event = Event::<usize>::with_tag();
        let first = event.listen();
        let second = event.listen();
        let mut third = event.listen();
        let mut fourth = event.listen();
        let calls = Cell::new(0);
        let mut generate = || {
            calls.set(calls.get() + 1);
            100 + calls.get()
        };
        let actual = if additional {
            event.notify(2usize.tag_with(&mut generate).relaxed().additional())
        } else {
            event.notify(2usize.relaxed().tag_with(&mut generate))
        };
        assert_eq!(actual, 2);
        drop(first);
        if additional {
            // Cancellation replaces this reservation even while another remains.
            assert_eq!(poll(&mut third), Poll::Ready(101));
            drop(second);
            assert_eq!(poll(&mut fourth), Poll::Ready(102));
        } else {
            // Another minimum notification still exists, so no replacement yet.
            assert_eq!(poll(&mut third), Poll::Pending);
            drop(second);
            assert_eq!(poll(&mut third), Poll::Ready(102));
            assert_eq!(poll(&mut fourth), Poll::Pending);
        }
        assert_eq!(
            calls.get(),
            2,
            "propagation must move, not regenerate, a tag"
        );
        assert!(!event.is_notified());
    }
}

#[cfg(all(feature = "std", panic = "unwind"))]
#[test]
fn panicking_tag_generation_preserves_completed_notifications_and_retry_count() {
    if support::run_in_subprocess(
        "panicking_tag_generation_preserves_completed_notifications_and_retry_count",
    ) {
        return;
    }

    use std::panic::{catch_unwind, AssertUnwindSafe};

    for additional in [false, true] {
        let event = Event::<usize>::with_tag();
        let mut listeners: Vec<_> = (0..4).map(|_| event.listen()).collect();
        assert_eq!(event.notify(1usize.tag(10)), 1);
        let calls = Cell::new(0);
        let mut generate = || {
            calls.set(calls.get() + 1);
            if calls.get() == 2 {
                panic!("policy tag generation failed");
            }
            20
        };
        let failure = catch_unwind(AssertUnwindSafe(|| {
            if additional {
                event.notify(2usize.tag_with(&mut generate).additional().relaxed());
            } else {
                event.notify(3usize.relaxed().tag_with(&mut generate));
            }
        }));
        let payload = failure.expect_err("the second generated tag must panic");
        assert_eq!(
            payload.downcast_ref::<&str>(),
            Some(&"policy tag generation failed")
        );
        assert_eq!(calls.get(), 2);

        // Do not consume the two successful notifications before retrying.
        // The failed tag did not reserve a listener or increment the count.
        let retried = if additional {
            event.notify(1usize.tag(30).additional())
        } else {
            event.notify(3usize.tag(30))
        };
        assert_eq!(retried, 1);
        for (listener, expected) in listeners.iter_mut().zip([
            Poll::Ready(10),
            Poll::Ready(20),
            Poll::Ready(30),
            Poll::Pending,
        ]) {
            assert_eq!(poll(listener), expected);
        }
        assert!(!event.is_notified());
    }
}
