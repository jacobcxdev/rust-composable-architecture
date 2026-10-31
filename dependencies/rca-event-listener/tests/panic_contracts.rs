//! Committed notifications survive wake and tag-producer panics.
#![cfg(all(not(loom), not(target_family = "wasm")))]

mod support;

mod wake {
    use std::future::Future;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::task::{Context, Wake, Waker};

    struct Probe {
        calls: Arc<AtomicUsize>,
        panics: bool,
    }

    impl Wake for Probe {
        fn wake(self: Arc<Self>) {
            self.calls.fetch_add(1, Ordering::SeqCst);
            assert!(!self.panics, "intentional wake panic");
        }
    }

    pub(super) fn run(waiter_count: usize, panic_index: usize) {
        let event = event_listener::Event::new();
        let mut listeners = Vec::with_capacity(waiter_count);
        let mut counters = Vec::with_capacity(waiter_count);
        for index in 0..waiter_count {
            let calls = Arc::new(AtomicUsize::new(0));
            let waker = Waker::from(Arc::new(Probe {
                calls: Arc::clone(&calls),
                panics: index == panic_index,
            }));
            let mut listener = event.listen();
            assert!(Pin::new(&mut listener)
                .poll(&mut Context::from_waker(&waker))
                .is_pending());
            listeners.push(listener);
            counters.push(calls);
        }

        let outcome = catch_unwind(AssertUnwindSafe(|| event.notify(waiter_count)));
        let observed: Vec<_> = counters
            .iter()
            .map(|calls| calls.load(Ordering::SeqCst))
            .collect();
        eprintln!("waiters={waiter_count}, panic_index={panic_index}, wake_counts={observed:?}");

        if observed.iter().any(|&count| count != 1) {
            for listener in listeners {
                std::mem::forget(listener);
            }
            panic!("a committed notification must schedule every waiter exactly once");
        }
        let panic = outcome.expect_err("the deliberate wake panic must propagate");
        let message = panic
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| panic.downcast_ref::<String>().map(String::as_str));
        assert_eq!(message, Some("intentional wake panic"));

        // Successful candidate runs must also recover valid list accounting and
        // release every listener normally; no successful-path leak is permitted.
        for mut listener in listeners {
            assert!(Pin::new(&mut listener)
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_ready());
        }
        let mut fresh = event.listen();
        assert_eq!(event.notify(1), 1);
        assert!(Pin::new(&mut fresh)
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_ready());
        assert_eq!(event.notify(1), 0);
        println!("all waiters scheduled once; panic propagated; list reused successfully");
    }
}

mod stack {
    use std::future::Future;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::task::{Context, Wake, Waker};

    struct DropReenter {
        event: Arc<event_listener::Event>,
        drops: Arc<AtomicUsize>,
    }

    #[expect(
        clippy::manual_noop_waker,
        reason = "The custom Drop callback tests reentrant cleanup; Waker::noop would erase it."
    )]
    impl Wake for DropReenter {
        fn wake(self: Arc<Self>) {}
    }

    impl Drop for DropReenter {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
            eprintln!("entered old-waker Drop; re-entering notify");
            self.event.notify(1);
            eprintln!("old-waker Drop returned");
        }
    }

    pub(super) fn run() {
        let event = Arc::new(event_listener::Event::new());
        let drops = Arc::new(AtomicUsize::new(0));
        let mut slot = Box::pin(event_listener::__private::StackSlot::new(&event));
        {
            let mut first = slot.as_mut().listen();
            let waker = Waker::from(Arc::new(DropReenter {
                event: Arc::clone(&event),
                drops: Arc::clone(&drops),
            }));
            assert!(Pin::new(&mut first)
                .poll(&mut Context::from_waker(&waker))
                .is_pending());
        }
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        eprintln!("first StackListener dropped while registration remains; reinserting");
        let result = catch_unwind(AssertUnwindSafe(|| {
            let _second = slot.as_mut().listen();
        }));
        let panic = result.expect_err("occupied StackSlot must reject reinsertion");
        let message = panic
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| panic.downcast_ref::<String>().map(String::as_str));
        assert_eq!(
            message,
            Some("cannot listen again while a previous registration remains")
        );
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(slot);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(event.notify(1), 0);
        println!("reinsertion rejected before locking; original listener cleaned up safely");
    }
}

#[cfg(feature = "std")]
mod tag {
    use event_listener::{Event, IntoNotification};
    use std::future::Future;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake, Waker};

    struct Count {
        event: Arc<Event<usize>>,
        wakes: AtomicUsize,
    }

    impl Wake for Count {
        fn wake(self: Arc<Self>) {
            // The notification lock must already be released during unwind.
            assert!(self.event.total_listeners() > 0);
            self.wakes.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(super) fn run(waiter_count: usize, panic_index: usize) {
        let event = Arc::new(Event::<usize>::with_tag());
        let counts: Vec<_> = (0..waiter_count)
            .map(|_| {
                Arc::new(Count {
                    event: Arc::clone(&event),
                    wakes: AtomicUsize::new(0),
                })
            })
            .collect();
        let wakers: Vec<_> = counts
            .iter()
            .map(|count| Waker::from(Arc::clone(count)))
            .collect();
        let mut listeners: Vec<_> = (0..waiter_count).map(|_| event.listen()).collect();
        for (listener, waker) in listeners.iter_mut().zip(&wakers) {
            assert!(Pin::new(listener)
                .poll(&mut Context::from_waker(waker))
                .is_pending());
        }
        let mut generated = 0;
        let result = catch_unwind(AssertUnwindSafe(|| {
            event.notify(waiter_count.tag_with(|| {
                let index = generated;
                generated += 1;
                assert_ne!(index, panic_index, "intentional tag-production panic");
                7
            }));
        }));
        let panic = result.expect_err("the selected tag producer must panic");
        let message = panic
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
            .expect("tag assertion carries a string diagnostic");
        assert!(message.contains("intentional tag-production panic"));
        assert_eq!(generated, panic_index + 1);
        for (index, ((listener, waker), count)) in
            listeners.iter_mut().zip(&wakers).zip(&counts).enumerate()
        {
            let outcome = Pin::new(listener).poll(&mut Context::from_waker(waker));
            if index < panic_index {
                assert_eq!(outcome, Poll::Ready(7));
                assert_eq!(count.wakes.load(Ordering::Relaxed), 1);
            } else {
                assert!(outcome.is_pending());
                assert_eq!(count.wakes.load(Ordering::Relaxed), 0);
            }
        }
        assert_eq!(event.total_listeners(), waiter_count - panic_index);
        assert_eq!(event.notify(usize::MAX.tag(9)), waiter_count - panic_index);
        for (listener, waker) in listeners.iter_mut().zip(&wakers).skip(panic_index) {
            assert_eq!(
                Pin::new(listener).poll(&mut Context::from_waker(waker)),
                Poll::Ready(9)
            );
        }
        assert_eq!(event.total_listeners(), 0);
        for count in &counts {
            assert_eq!(count.wakes.load(Ordering::Relaxed), 1);
        }
        println!("tag-production panic contract passed: {waiter_count} waiters, panic index {panic_index}");
    }
}

#[test]
fn wake_panics_preserve_every_committed_notification() {
    if support::run_in_subprocess("wake_panics_preserve_every_committed_notification") {
        return;
    }
    for (waiters, index) in [
        (1, 0),
        (2, 0),
        (32, 0),
        (33, 0),
        (40, 0),
        (40, 20),
        (40, 32),
        (40, 39),
    ] {
        wake::run(waiters, index);
    }
}

#[test]
fn occupied_stack_slot_rejects_reinsertion_before_dropping_registration() {
    if support::run_in_subprocess(
        "occupied_stack_slot_rejects_reinsertion_before_dropping_registration",
    ) {
        return;
    }
    stack::run();
}

#[cfg(feature = "std")]
#[test]
fn tag_panics_preserve_committed_notifications_and_remaining_listeners() {
    if support::run_in_subprocess(
        "tag_panics_preserve_committed_notifications_and_remaining_listeners",
    ) {
        return;
    }
    for (waiters, index) in [
        (2, 1),
        (40, 0),
        (40, 1),
        (40, 31),
        (40, 32),
        (40, 33),
        (40, 39),
    ] {
        tag::run(waiters, index);
    }
}
