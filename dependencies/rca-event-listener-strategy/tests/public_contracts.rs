//! Public wrapper, pinning and listener-consumption contracts.
//!
//! Manual polls use ready notifications, not a real executor wakeup. Blocking
//! cases require native `std`; host tests of minimal builds still use host `std`.

#[cfg(feature = "std")]
use event_listener_strategy::event_listener::IntoNotification;
use event_listener_strategy::{
    easy_wrapper, event_listener::Event, EventListenerFuture, FutureWrapper, NonBlocking, Strategy,
};
use std::{
    cell::Cell,
    future::Future,
    marker::PhantomPinned,
    pin::Pin,
    task::{Context, Poll, Waker},
};

/// A ready future whose value can be changed through an unpinned wrapper.
struct Value(usize);

#[cfg(all(feature = "std", not(target_family = "wasm")))]
#[test]
fn wait_repolls_an_unusual_safe_implementor_after_pending() {
    /// Counts the safe trait's fallback polls without creating a listener.
    struct PendingOnce<'a>(&'a Cell<usize>);

    impl EventListenerFuture for PendingOnce<'_> {
        type Output = usize;

        fn poll_with_strategy<'a, S: Strategy<'a>>(
            self: Pin<&mut Self>,
            _strategy: &mut S,
            _context: &mut S::Context,
        ) -> Poll<Self::Output> {
            let polls = self.0.get();
            self.0.set(polls + 1);
            match polls {
                0 => Poll::Pending,
                1 => Poll::Ready(43),
                _ => panic!("completed future was polled again"),
            }
        }
    }

    // The docs discourage this hot-loop behaviour, but the safe trait permits
    // it. This checks the fallback loop, not listener blocking or wakeups.
    let polls = Cell::new(0);
    assert_eq!(PendingOnce(&polls).wait(), 43);
    assert_eq!(polls.get(), 2);
}

impl EventListenerFuture for Value {
    type Output = usize;

    fn poll_with_strategy<'a, S: Strategy<'a>>(
        self: Pin<&mut Self>,
        _strategy: &mut S,
        _context: &mut S::Context,
    ) -> Poll<Self::Output> {
        Poll::Ready(self.0)
    }
}

#[test]
fn wrapper_mutation_survives_recovery_and_execution() {
    // Losing or copying the wrapped state would return the original value.
    let mut wrapper = FutureWrapper::from(Value(7));
    assert_eq!(wrapper.get_ref().0, 7);
    wrapper.get_mut().0 = 11;
    let recovered = wrapper.into_inner();
    assert_eq!(
        futures_lite::future::block_on(FutureWrapper::new(recovered)),
        11
    );
}

/// A future that must stay at the same address through wrapper projections.
struct PinnedValue {
    /// Allows mutation through a pinned shared borrow without moving the future.
    value: Cell<usize>,
    /// Prevents the fixture from satisfying `Unpin` accidentally.
    _pin: PhantomPinned,
}

impl EventListenerFuture for PinnedValue {
    type Output = usize;

    fn poll_with_strategy<'a, S: Strategy<'a>>(
        self: Pin<&mut Self>,
        _strategy: &mut S,
        _context: &mut S::Context,
    ) -> Poll<Self::Output> {
        Poll::Ready(self.as_ref().get_ref().value.get())
    }
}

#[test]
fn pinned_projections_keep_the_same_non_unpin_future() {
    // Both projections must refer to the future that is eventually polled.
    let mut wrapper = Box::pin(FutureWrapper::new(PinnedValue {
        value: Cell::new(13),
        _pin: PhantomPinned,
    }));
    let address = wrapper.as_ref().get_pin_ref().get_ref() as *const PinnedValue;
    wrapper
        .as_mut()
        .get_pin_mut()
        .as_ref()
        .get_ref()
        .value
        .set(29);
    assert_eq!(
        wrapper.as_ref().get_pin_ref().get_ref() as *const PinnedValue,
        address
    );
    assert_eq!(futures_lite::future::block_on(wrapper), 29);
}

#[test]
fn nonblocking_poll_preserves_pending_listener_until_ready() {
    let event = Event::new();
    let mut listener = Some(event.listen());
    let mut strategy = NonBlocking::default();
    let mut context = Context::from_waker(Waker::noop());
    assert_eq!(strategy.poll(&mut listener, &mut context), Poll::Pending);
    assert!(listener.is_some());
    assert_eq!(event.notify(1), 1);
    assert_eq!(strategy.poll(&mut listener, &mut context), Poll::Ready(()));
    assert!(listener.is_none());
}

#[test]
fn nonblocking_wait_retains_the_pending_subscription() {
    let event = Event::new();
    let first = event.listen();
    let mut successor = event.listen();
    let mut waiting = NonBlocking::default().wait(first);
    let mut context = Context::from_waker(Waker::noop());

    // Returning a replacement listener would lose this subscription's wakeup.
    assert_eq!(Pin::new(&mut waiting).poll(&mut context), Poll::Pending);
    assert_eq!(event.notify(1), 1);
    assert_eq!(Pin::new(&mut waiting).poll(&mut context), Poll::Ready(()));
    assert_eq!(Pin::new(&mut successor).poll(&mut context), Poll::Pending);
    assert_eq!(event.notify(1), 1);
    assert_eq!(Pin::new(&mut successor).poll(&mut context), Poll::Ready(()));
}

#[cfg(all(feature = "std", not(target_family = "wasm")))]
#[test]
fn blocking_wait_consumes_instead_of_forwarding_the_notification() {
    let event = Event::new();
    let first = event.listen();
    let mut successor = event.listen();
    assert_eq!(event.notify(1), 1);

    let mut ready = event_listener_strategy::Blocking::default().wait(first);
    let mut context = Context::from_waker(Waker::noop());
    assert_eq!(Pin::new(&mut ready).poll(&mut context), Poll::Ready(()));

    // Dropping the notified listener instead of waiting would wake its successor.
    assert_eq!(Pin::new(&mut successor).poll(&mut context), Poll::Pending);
    assert_eq!(event.notify(1), 1);
    assert_eq!(Pin::new(&mut successor).poll(&mut context), Poll::Ready(()));
}

#[cfg(feature = "std")]
#[test]
fn nonblocking_poll_preserves_pending_listener_then_returns_its_tag() {
    // Clearing a pending listener loses the notification; retaining a ready one
    // permits a second consumption of an already-completed listener.
    let event = Event::<usize>::with_tag();
    let mut listener = Some(event.listen());
    let mut strategy = NonBlocking::default();
    let mut context = Context::from_waker(Waker::noop());
    assert_eq!(strategy.poll(&mut listener, &mut context), Poll::Pending);
    assert!(listener.is_some());
    assert_eq!(event.notify(1.tag(37)), 1);
    assert_eq!(strategy.poll(&mut listener, &mut context), Poll::Ready(37));
    assert!(listener.is_none());
}

#[cfg(all(feature = "std", not(target_family = "wasm")))]
#[test]
fn blocking_poll_returns_tag_and_consumes_listener() {
    let event = Event::<usize>::with_tag();
    let mut listener = Some(event.listen());
    assert_eq!(event.notify(1.tag(41)), 1);
    assert_eq!(
        event_listener_strategy::Blocking::default().poll(&mut listener, &mut ()),
        Poll::Ready(41)
    );
    assert!(listener.is_none());
}

/// Returns the original borrow, including its lifetime and unsized target.
struct Borrowed<'a, T: ?Sized> {
    /// The caller-owned value that every generated wrapper path must retain.
    value: &'a T,
}

impl<'a, T: ?Sized> EventListenerFuture for Borrowed<'a, T> {
    type Output = &'a T;

    fn poll_with_strategy<'b, S: Strategy<'b>>(
        self: Pin<&mut Self>,
        _strategy: &mut S,
        _context: &mut S::Context,
    ) -> Poll<Self::Output> {
        Poll::Ready(self.value)
    }
}

easy_wrapper! {
    struct BorrowedWrapper<'a, T: ?Sized>(Borrowed<'a, T> => &'a T);
    #[cfg(all(feature = "std", not(target_family = "wasm")))]
    wait();
}

#[test]
fn macro_wrapper_executes_a_borrowed_unsized_output() {
    // Incorrect lifetime/generic forwarding must not lose the original borrow.
    let text = String::from("borrowed output");
    let value = text.as_str();
    let mut wrapper = Box::pin(BorrowedWrapper::_new(Borrowed { value }));
    let mut context = Context::from_waker(Waker::noop());
    assert_eq!(
        wrapper
            .as_mut()
            .poll_with_strategy(&mut NonBlocking::default(), &mut context),
        Poll::Ready("borrowed output")
    );
    let output = futures_lite::future::block_on(BorrowedWrapper::_new(Borrowed { value }));
    assert!(std::ptr::eq(output, value));
    #[cfg(all(feature = "std", not(target_family = "wasm")))]
    assert!(std::ptr::eq(
        BorrowedWrapper::_new(Borrowed { value }).wait(),
        value
    ));
}
