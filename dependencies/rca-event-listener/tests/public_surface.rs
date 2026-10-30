//! Public notification, identity, diagnostic and stack-slot contracts.

#![cfg(all(not(loom), not(target_family = "wasm")))]

mod support;

use event_listener::{Event, IntoNotification, Listener};
use std::any::Any;
#[cfg(feature = "std")]
use std::cell::Cell;
use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::pin::Pin;
use std::sync::atomic::{fence, Ordering};
use std::task::{Context, Poll, Waker};

fn poll<F: Future + Unpin>(future: &mut F) -> Poll<F::Output> {
    Pin::new(future).poll(&mut Context::from_waker(Waker::noop()))
}

fn panic_text(payload: &(dyn Any + Send)) -> &str {
    if let Some(message) = payload.downcast_ref::<String>() {
        message
    } else if let Some(message) = payload.downcast_ref::<&str>() {
        message
    } else {
        panic!("expected a string panic diagnostic");
    }
}

#[test]
fn default_event_starts_empty_and_delivers_first_notification() {
    if support::run_in_subprocess("default_event_starts_empty_and_delivers_first_notification") {
        return;
    }

    let event = Event::default();
    #[cfg(feature = "std")]
    assert_eq!(event.total_listeners(), 0);
    assert!(!event.is_notified());

    let mut listener = event.listen();
    #[cfg(feature = "std")]
    assert_eq!(event.total_listeners(), 1);
    assert!(poll(&mut listener).is_pending());
    assert_eq!(event.notify(1), 1);
    assert_eq!(poll(&mut listener), Poll::Ready(()));
    #[cfg(feature = "std")]
    assert_eq!(event.total_listeners(), 0);
}

#[test]
fn is_notified_tracks_unconsumed_delivery_with_a_waiting_listener() {
    if support::run_in_subprocess("is_notified_tracks_unconsumed_delivery_with_a_waiting_listener")
    {
        return;
    }

    let event = Event::new();
    let mut first = event.listen();
    let mut second = event.listen();
    assert!(!event.is_notified());

    assert_eq!(event.notify(1), 1);
    assert!(event.is_notified());
    assert_eq!(poll(&mut first), Poll::Ready(()));
    #[cfg(feature = "std")]
    assert_eq!(event.total_listeners(), 1);
    assert!(!event.is_notified());

    assert_eq!(event.notify(1), 1);
    assert!(event.is_notified());
    assert_eq!(poll(&mut second), Poll::Ready(()));
    #[cfg(feature = "std")]
    assert_eq!(event.total_listeners(), 0);
}

#[test]
fn owned_listeners_keep_their_event_identity() {
    if support::run_in_subprocess("owned_listeners_keep_their_event_identity") {
        return;
    }

    let event = Event::new();
    let other_event = Event::new();
    let uninitialised_event = Event::new();
    let mut first = event.listen();
    let second = event.listen();
    let other = other_event.listen();

    assert!(first.listens_to(&event));
    assert!(!first.listens_to(&other_event));
    assert!(!first.listens_to(&uninitialised_event));
    assert!(first.same_event(&first));
    assert!(first.same_event(&second));
    assert!(!first.same_event(&other));
    assert!(format!("{first:?}").starts_with("EventListener"));
    #[cfg(feature = "std")]
    assert_eq!(event.total_listeners(), 2);

    assert_eq!(event.notify(1), 1);
    assert_eq!(poll(&mut first), Poll::Ready(()));
    assert!(first.listens_to(&event));
    assert!(first.same_event(&second));
    // Completion already consumed the notification.
    assert!(!first.discard());
    assert!(!second.discard());
    assert!(!other.discard());
    #[cfg(feature = "std")]
    assert_eq!(event.total_listeners(), 0);
    #[cfg(feature = "std")]
    assert_eq!(other_event.total_listeners(), 0);
}

#[test]
fn stack_listeners_keep_their_event_identity() {
    if support::run_in_subprocess("stack_listeners_keep_their_event_identity") {
        return;
    }

    let event = Event::new();
    let other_event = Event::new();
    let uninitialised_event = Event::new();
    let mut first_slot = Box::pin(event_listener::__private::StackSlot::new(&event));
    let mut second_slot = Box::pin(event_listener::__private::StackSlot::new(&event));
    let mut other_slot = Box::pin(event_listener::__private::StackSlot::new(&other_event));
    assert!(format!("{:?}", first_slot.as_ref().get_ref()).starts_with("StackSlot"));
    #[cfg(feature = "std")]
    assert_eq!(event.total_listeners(), 0);

    let first = first_slot.as_mut().listen();
    let second = second_slot.as_mut().listen();
    let other = other_slot.as_mut().listen();
    assert!(first.listens_to(&event));
    assert!(!first.listens_to(&other_event));
    assert!(!first.listens_to(&uninitialised_event));
    assert!(first.same_event(&first));
    assert!(first.same_event(&second));
    assert!(!first.same_event(&other));
    assert!(format!("{first:?}").starts_with("StackListener"));
    #[cfg(feature = "std")]
    assert_eq!(event.total_listeners(), 2);

    assert!(!first.discard());
    assert!(!second.discard());
    assert!(!other.discard());
    #[cfg(feature = "std")]
    assert_eq!(event.total_listeners(), 0);
    #[cfg(feature = "std")]
    assert_eq!(other_event.total_listeners(), 0);
}

#[test]
fn event_debug_reports_state_without_changing_it() {
    if support::run_in_subprocess("event_debug_reports_state_without_changing_it") {
        return;
    }

    let uninitialised = Event::new();
    assert!(format!("{uninitialised:?}").contains("<uninitialized>"));
    #[cfg(feature = "std")]
    assert_eq!(uninitialised.total_listeners(), 0);
    drop(uninitialised);

    let event = Event::new();
    let mut first = event.listen();
    let second = event.listen();
    let pending = format!("{event:?}");
    assert!(pending.contains("listeners_notified: 0"));
    assert!(pending.contains("listeners_total: 2"));
    assert_eq!(event.notify(1), 1);
    let notified = format!("{event:?}");
    assert!(notified.contains("listeners_notified: 1"));
    assert!(notified.contains("listeners_total: 2"));
    assert_eq!(poll(&mut first), Poll::Ready(()));
    assert!(!second.discard());
    #[cfg(feature = "std")]
    assert_eq!(event.total_listeners(), 0);
}

#[test]
#[cfg(feature = "std")]
fn event_debug_is_non_blocking_during_tag_generation() {
    if support::run_in_subprocess("event_debug_is_non_blocking_during_tag_generation") {
        return;
    }

    let event = Event::<u8>::with_tag();
    let mut listener = event.listen();
    let mut observed = None;
    assert_eq!(
        event.notify(1.tag_with(|| {
            observed = Some(format!("{event:?}"));
            7
        })),
        1
    );
    assert!(observed.unwrap().contains("<locked>"));
    assert_eq!(poll(&mut listener), Poll::Ready(7));
    assert_eq!(event.total_listeners(), 0);
}

#[test]
fn relaxed_notifications_preserve_minimum_and_additional_counts() {
    if support::run_in_subprocess("relaxed_notifications_preserve_minimum_and_additional_counts") {
        return;
    }

    let event = Event::new();
    let mut first = event.listen();
    let mut second = event.listen();
    let mut third = event.listen();
    fence(Ordering::SeqCst);

    assert_eq!(event.notify(1.relaxed()), 1);
    assert_eq!(event.notify(1.relaxed()), 0);
    assert_eq!(event.notify(1.additional().relaxed()), 1);
    assert_eq!(poll(&mut first), Poll::Ready(()));
    assert_eq!(poll(&mut second), Poll::Ready(()));
    assert!(poll(&mut third).is_pending());
    assert_eq!(event.notify(1.relaxed().additional()), 1);
    assert_eq!(poll(&mut third), Poll::Ready(()));
    #[cfg(feature = "std")]
    assert_eq!(event.total_listeners(), 0);
}

#[test]
#[cfg(feature = "std")]
fn relaxed_tagged_notifications_preserve_each_payload() {
    if support::run_in_subprocess("relaxed_tagged_notifications_preserve_each_payload") {
        return;
    }

    let event = Event::<u8>::with_tag();
    let mut first = event.listen();
    let mut second = event.listen();
    fence(Ordering::SeqCst);

    assert_eq!(event.notify(1.tag(7).relaxed()), 1);
    assert_eq!(event.notify(1.tag(9).relaxed().additional()), 1);
    assert_eq!(poll(&mut first), Poll::Ready(7));
    assert_eq!(poll(&mut second), Poll::Ready(9));
    assert_eq!(event.total_listeners(), 0);
}

#[test]
fn deprecated_relaxed_notify_keeps_minimum_semantics() {
    if support::run_in_subprocess("deprecated_relaxed_notify_keeps_minimum_semantics") {
        return;
    }

    let event = Event::new();
    let mut first = event.listen();
    let mut second = event.listen();
    fence(Ordering::SeqCst);

    assert_eq!(event.notify_relaxed(1), 1);
    assert_eq!(event.notify_relaxed(1), 0);
    assert_eq!(poll(&mut first), Poll::Ready(()));
    assert!(poll(&mut second).is_pending());
    assert_eq!(event.notify_relaxed(1), 1);
    assert_eq!(poll(&mut second), Poll::Ready(()));
    #[cfg(feature = "std")]
    assert_eq!(event.total_listeners(), 0);
}

#[test]
fn deprecated_additional_relaxed_notify_keeps_additional_semantics() {
    if support::run_in_subprocess("deprecated_additional_relaxed_notify_keeps_additional_semantics")
    {
        return;
    }

    let event = Event::new();
    let mut first = event.listen();
    let mut second = event.listen();
    fence(Ordering::SeqCst);

    assert_eq!(event.notify_additional_relaxed(1), 1);
    assert_eq!(event.notify_additional_relaxed(1), 1);
    assert_eq!(event.notify_additional_relaxed(1), 0);
    assert_eq!(poll(&mut first), Poll::Ready(()));
    assert_eq!(poll(&mut second), Poll::Ready(()));
    #[cfg(feature = "std")]
    assert_eq!(event.total_listeners(), 0);
}

#[test]
#[cfg(feature = "std")]
fn tag_with_debug_does_not_call_the_producer() {
    if support::run_in_subprocess("tag_with_debug_does_not_call_the_producer") {
        return;
    }

    let event = Event::<u8>::with_tag();
    let mut first = event.listen();
    let mut second = event.listen();
    let calls = Cell::new(0);
    let notification = 2.tag_with(|| {
        calls.set(calls.get() + 1);
        calls.get()
    });
    let debug = format!("{notification:?}");
    assert!(debug.contains("TagWith"));
    assert!(debug.contains(".."));
    assert_eq!(calls.get(), 0);

    assert_eq!(event.notify(notification), 2);
    assert_eq!(calls.get(), 2);
    assert_eq!(poll(&mut first), Poll::Ready(1));
    assert_eq!(poll(&mut second), Poll::Ready(2));
    assert_eq!(event.total_listeners(), 0);
}

#[test]
fn negative_notification_count_preserves_registered_listener() {
    if support::run_in_subprocess("negative_notification_count_preserves_registered_listener") {
        return;
    }

    let event = Event::new();
    let mut listener = event.listen();
    assert!(poll(&mut listener).is_pending());

    let panic = catch_unwind(|| event.notify(-1_i32)).expect_err("negative count must panic");
    assert!(panic_text(&*panic).contains("negative notification count"));
    #[cfg(feature = "std")]
    assert_eq!(event.total_listeners(), 1);
    assert!(poll(&mut listener).is_pending());
    assert_eq!(event.notify(1), 1);
    assert_eq!(poll(&mut listener), Poll::Ready(()));
    #[cfg(feature = "std")]
    assert_eq!(event.total_listeners(), 0);
}

#[test]
fn overflow_notification_count_preserves_registered_listener() {
    if support::run_in_subprocess("overflow_notification_count_preserves_registered_listener") {
        return;
    }

    let event = Event::new();
    let mut listener = event.listen();
    assert!(poll(&mut listener).is_pending());

    let panic = catch_unwind(|| event.notify(u128::MAX)).expect_err("oversized count must panic");
    assert!(panic_text(&*panic).contains("overflow"));
    #[cfg(feature = "std")]
    assert_eq!(event.total_listeners(), 1);
    assert!(poll(&mut listener).is_pending());
    assert_eq!(event.notify(1), 1);
    assert_eq!(poll(&mut listener), Poll::Ready(()));
    #[cfg(feature = "std")]
    assert_eq!(event.total_listeners(), 0);
}

#[test]
fn polling_completed_listener_panics_without_affecting_another() {
    if support::run_in_subprocess("polling_completed_listener_panics_without_affecting_another") {
        return;
    }

    let event = Event::new();
    let mut completed = event.listen();
    let mut waiting = event.listen();
    assert_eq!(event.notify(1), 1);
    assert_eq!(poll(&mut completed), Poll::Ready(()));

    let panic = catch_unwind(AssertUnwindSafe(|| poll(&mut completed)))
        .expect_err("a completed listener cannot be polled again");
    assert!(panic_text(&*panic).contains("after it has finished"));
    #[cfg(feature = "std")]
    assert_eq!(event.total_listeners(), 1);
    assert!(poll(&mut waiting).is_pending());
    assert_eq!(event.notify(1), 1);
    assert_eq!(poll(&mut waiting), Poll::Ready(()));
    #[cfg(feature = "std")]
    assert_eq!(event.total_listeners(), 0);
}

#[test]
#[cfg(feature = "std")]
fn completed_stack_slot_can_listen_again() {
    if support::run_in_subprocess("completed_stack_slot_can_listen_again") {
        return;
    }

    let event = Event::<u8>::with_tag();
    let mut slot = Box::pin(event_listener::__private::StackSlot::new(&event));

    for tag in [7, 9] {
        let mut listener = slot.as_mut().listen();
        assert_eq!(event.total_listeners(), 1);
        assert!(poll(&mut listener).is_pending());
        assert_eq!(event.notify(1.tag(tag)), 1);
        assert_eq!(poll(&mut listener), Poll::Ready(tag));
        assert_eq!(event.total_listeners(), 0);
    }
}

#[test]
#[cfg(feature = "std")]
fn discarded_stack_slot_can_listen_again() {
    if support::run_in_subprocess("discarded_stack_slot_can_listen_again") {
        return;
    }

    let event = Event::<u8>::with_tag();
    let mut slot = Box::pin(event_listener::__private::StackSlot::new(&event));

    for notified in [false, true] {
        let mut discarded = slot.as_mut().listen();
        assert!(poll(&mut discarded).is_pending());
        if notified {
            assert_eq!(event.notify(1.tag(7)), 1);
        }
        assert_eq!(discarded.discard(), notified);
        assert_eq!(event.total_listeners(), 0);

        let mut replacement = slot.as_mut().listen();
        assert!(poll(&mut replacement).is_pending());
        assert_eq!(event.notify(1.tag(9)), 1);
        assert_eq!(poll(&mut replacement), Poll::Ready(9));
        assert_eq!(event.total_listeners(), 0);
    }
}

#[test]
fn unit_stack_slot_can_complete_discard_and_listen_again() {
    if support::run_in_subprocess("unit_stack_slot_can_complete_discard_and_listen_again") {
        return;
    }

    let event = Event::new();
    let mut slot = Box::pin(event_listener::__private::StackSlot::new(&event));
    for (notify, discard) in [(true, false), (false, true), (true, true)] {
        {
            let mut listener = slot.as_mut().listen();
            assert!(poll(&mut listener).is_pending());
            if notify {
                assert_eq!(event.notify(1), 1);
            }
            if discard {
                assert_eq!(listener.discard(), notify);
            } else {
                assert_eq!(poll(&mut listener), Poll::Ready(()));
            }
        }
        assert!(!event.is_notified());

        let mut replacement = slot.as_mut().listen();
        assert!(poll(&mut replacement).is_pending());
        assert_eq!(event.notify(1), 1);
        assert_eq!(poll(&mut replacement), Poll::Ready(()));
        assert!(!event.is_notified());
    }
}

#[test]
fn cloned_notify_preserves_minimum_delivery_and_debug_count() {
    if support::run_in_subprocess("cloned_notify_preserves_minimum_delivery_and_debug_count") {
        return;
    }

    let notification = 1_usize.into_notification();
    let cloned = notification.clone();
    assert_eq!(format!("{notification:?}"), "Notify(1)");
    assert_eq!(format!("{cloned:?}"), "Notify(1)");

    let event = Event::new();
    let mut first = event.listen();
    let mut second = event.listen();
    assert_eq!(event.notify(cloned), 1);
    assert_eq!(event.notify(notification), 0);
    assert_eq!(poll(&mut first), Poll::Ready(()));
    assert!(poll(&mut second).is_pending());
    assert_eq!(event.notify(1), 1);
    assert_eq!(poll(&mut second), Poll::Ready(()));
    assert!(!event.is_notified());
}

#[test]
fn cloned_additional_preserves_additive_delivery_and_debug_policy() {
    if support::run_in_subprocess("cloned_additional_preserves_additive_delivery_and_debug_policy")
    {
        return;
    }

    let notification = 1_usize.additional();
    let cloned = notification.clone();
    assert_eq!(format!("{notification:?}"), "Additional(Notify(1))");
    assert_eq!(format!("{cloned:?}"), "Additional(Notify(1))");

    let event = Event::new();
    let mut first = event.listen();
    let mut second = event.listen();
    assert_eq!(event.notify(cloned), 1);
    assert_eq!(event.notify(notification), 1);
    assert_eq!(poll(&mut first), Poll::Ready(()));
    assert_eq!(poll(&mut second), Poll::Ready(()));
    assert!(!event.is_notified());
}

#[test]
fn cloned_relaxed_preserves_minimum_delivery_and_debug_policy() {
    if support::run_in_subprocess("cloned_relaxed_preserves_minimum_delivery_and_debug_policy") {
        return;
    }

    let notification = 2_usize.relaxed();
    let cloned = notification.clone();
    assert_eq!(format!("{notification:?}"), "Relaxed(Notify(2))");
    assert_eq!(format!("{cloned:?}"), "Relaxed(Notify(2))");

    let event = Event::new();
    let mut first = event.listen();
    let mut second = event.listen();
    fence(Ordering::SeqCst);
    assert_eq!(event.notify(cloned), 2);
    assert_eq!(event.notify(notification), 0);
    assert_eq!(poll(&mut first), Poll::Ready(()));
    assert_eq!(poll(&mut second), Poll::Ready(()));
    assert!(!event.is_notified());
}

#[test]
#[cfg(feature = "std")]
fn cloned_tag_preserves_owned_payload_and_debug_fields() {
    if support::run_in_subprocess("cloned_tag_preserves_owned_payload_and_debug_fields") {
        return;
    }

    let notification = 1_usize.additional().tag(String::from("payload"));
    let cloned = notification.clone();
    let expected = "Tag { tag: \"payload\", inner: Additional(Notify(1)) }";
    assert_eq!(format!("{notification:?}"), expected);
    assert_eq!(format!("{cloned:?}"), expected);

    let event = Event::<String>::with_tag();
    let mut first = event.listen();
    let mut second = event.listen();
    assert_eq!(event.notify(cloned), 1);
    assert_eq!(event.notify(notification), 1);
    let Poll::Ready(mut first_payload) = poll(&mut first) else {
        panic!("the cloned notification must deliver its payload");
    };
    assert_eq!(first_payload, "payload");
    first_payload.push_str("-changed");
    assert_eq!(poll(&mut second), Poll::Ready(String::from("payload")));
    assert_eq!(event.total_listeners(), 0);
}
