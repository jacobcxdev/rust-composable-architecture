//! Public-API qualification probes for event-listener callback sequencing.
//!
//! Each native case runs in a bounded child process; Miri runs the same body directly.
//! Panic recovery cases require an unwinding panic strategy.
//! The hidden-public StackSlot reinsertion case lives in panic_contracts.
//! Without the library's std feature, query actions instead create and drop a
//! temporary listener: this still exercises list-lock re-entry without notifying.
//!
//! RawWaker is necessary here: the safe Wake trait cannot customise cloning or
//! the RawWaker drop callback. Every raw pointer below owns one Arc reference.
//! All shared data is immutable or atomic, making every vtable operation safe
//! to invoke from any thread. Re-entry actions are consumed before execution.

#![cfg(all(not(loom), not(target_family = "wasm")))]

mod support;

use event_listener::{Event, EventListener, IntoNotification, Listener};
use std::future::Future;
use std::mem::ManuallyDrop;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::pin::Pin;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

const QUERY: u8 = 1;
const NOTIFY_ONE: u8 = 2;
const PANIC: u8 = 4;

#[cfg(feature = "std")]
#[test]
fn cancelled_tags_drop_after_unlink_and_unlock() {
    if support::run_in_subprocess("cancelled_tags_drop_after_unlink_and_unlock") {
        return;
    }

    #[derive(Clone)]
    struct Tag {
        event: std::sync::Weak<Event<Tag>>,
        drops: Arc<AtomicUsize>,
        remaining: Arc<AtomicUsize>,
    }

    impl Drop for Tag {
        fn drop(&mut self) {
            let event = self.event.upgrade().unwrap();
            assert_eq!(
                event.total_listeners(),
                self.remaining.load(Ordering::SeqCst)
            );
            // Re-enter the list through a public operation on every backend.
            // This temporary listener is unnotified, so its drop creates no tag.
            drop(event.listen());
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    for additional in [false, true] {
        let event = Arc::new(Event::<Tag>::with_tag());
        let drops = Arc::new(AtomicUsize::new(0));
        let remaining = Arc::new(AtomicUsize::new(3));
        let first = event.listen();
        let second = event.listen();
        let third = event.listen();
        let notification = 2usize.tag(Tag {
            event: Arc::downgrade(&event),
            drops: Arc::clone(&drops),
            remaining: Arc::clone(&remaining),
        });
        assert_eq!(
            if additional {
                event.notify(notification.additional())
            } else {
                event.notify(notification)
            },
            2
        );
        // The notification wrapper owns the original; listeners own its clones.
        assert_eq!(drops.load(Ordering::SeqCst), 1);

        remaining.store(2, Ordering::SeqCst);
        drop(first);
        // Minimum notifications discard the first tag while another remains.
        // Additional notifications transfer it to the third listener instead.
        assert_eq!(drops.load(Ordering::SeqCst), 1 + usize::from(!additional));
        remaining.store(1, Ordering::SeqCst);
        drop(second);
        assert_eq!(drops.load(Ordering::SeqCst), 2);
        remaining.store(0, Ordering::SeqCst);
        drop(third);
        assert_eq!(drops.load(Ordering::SeqCst), 3);
        assert!(!event.is_notified());
        assert_eq!(event.total_listeners(), 0);
    }
}

struct Probe {
    event: Arc<Event>,
    clones: AtomicUsize,
    wakes: AtomicUsize,
    drops: AtomicUsize,
    on_clone: AtomicU8,
    on_wake: AtomicU8,
    on_drop: AtomicU8,
    queries: AtomicUsize,
    #[cfg(feature = "std")]
    last_listener_count: AtomicUsize,
    nested_notified: AtomicUsize,
}

impl Probe {
    fn new(event: &Arc<Event>) -> Arc<Self> {
        Arc::new(Self {
            event: Arc::clone(event),
            clones: AtomicUsize::new(0),
            wakes: AtomicUsize::new(0),
            drops: AtomicUsize::new(0),
            on_clone: AtomicU8::new(0),
            on_wake: AtomicU8::new(0),
            on_drop: AtomicU8::new(0),
            queries: AtomicUsize::new(0),
            #[cfg(feature = "std")]
            last_listener_count: AtomicUsize::new(usize::MAX),
            nested_notified: AtomicUsize::new(0),
        })
    }

    fn callback(&self, action: &AtomicU8, label: &str) {
        let action = action.swap(0, Ordering::SeqCst);
        if action == 0 {
            return;
        }
        eprintln!("entered {label} callback, action={action}");
        if action & QUERY != 0 {
            #[cfg(feature = "std")]
            {
                // This public query acquires the list lock.
                let count = self.event.total_listeners();
                self.last_listener_count.store(count, Ordering::SeqCst);
            }
            #[cfg(not(feature = "std"))]
            {
                // total_listeners is std-only. A temporary, unnotified listener
                // exercises the same lock without consuming or adding a wake.
                drop(self.event.listen());
            }
            self.queries.fetch_add(1, Ordering::SeqCst);
        }
        if action & NOTIFY_ONE != 0 {
            let count = self.event.notify(1);
            self.nested_notified.fetch_add(count, Ordering::SeqCst);
        }
        assert!(action & PANIC == 0, "intentional {label} callback panic");
        eprintln!("returned from {label} callback");
    }
}

static VTABLE: RawWakerVTable = RawWakerVTable::new(raw_clone, raw_wake, raw_wake_by_ref, raw_drop);

fn waker(probe: &Arc<Probe>) -> Waker {
    let data = Arc::into_raw(Arc::clone(probe)).cast::<()>();
    // SAFETY: data transfers one strong reference to this Waker. VTABLE only
    // reconstructs the original Probe pointer and obeys its ownership rules.
    unsafe { Waker::from_raw(RawWaker::new(data, &VTABLE)) }
}

unsafe fn raw_clone(data: *const ()) -> RawWaker {
    // SAFETY: a live Waker supplies its live Arc<Probe> pointer. ManuallyDrop
    // borrows that strong reference: neither success nor panic consumes it.
    let probe = ManuallyDrop::new(unsafe { Arc::<Probe>::from_raw(data.cast()) });
    probe.clones.fetch_add(1, Ordering::SeqCst);
    probe.callback(&probe.on_clone, "clone");
    // Only after the fallible callback do we create the reference transferred
    // to the returned RawWaker. A clone panic cannot leak a new raw reference.
    let cloned = Arc::clone(&probe);
    RawWaker::new(Arc::into_raw(cloned).cast(), &VTABLE)
}

unsafe fn raw_wake(data: *const ()) {
    // SAFETY: wake consumes this Waker's one strong reference. The local Arc
    // releases it on both normal return and unwinding from the callback.
    let probe = unsafe { Arc::<Probe>::from_raw(data.cast()) };
    probe.wakes.fetch_add(1, Ordering::SeqCst);
    probe.callback(&probe.on_wake, "wake");
}

unsafe fn raw_wake_by_ref(data: *const ()) {
    // SAFETY: this callback borrows a live Waker's reference, as raw_clone does,
    // and must not consume it even when the callback unwinds.
    let probe = ManuallyDrop::new(unsafe { Arc::<Probe>::from_raw(data.cast()) });
    probe.wakes.fetch_add(1, Ordering::SeqCst);
    probe.callback(&probe.on_wake, "wake_by_ref");
}

unsafe fn raw_drop(data: *const ()) {
    // SAFETY: drop consumes exactly this Waker's one strong reference. Callback
    // panic still unwinds through and releases the reconstructed local Arc.
    let probe = unsafe { Arc::<Probe>::from_raw(data.cast()) };
    probe.drops.fetch_add(1, Ordering::SeqCst);
    probe.callback(&probe.on_drop, "drop");
}

fn poll(listener: &mut EventListener, waker: &Waker) -> Poll<()> {
    Pin::new(listener).poll(&mut Context::from_waker(waker))
}

fn count(value: &AtomicUsize) -> usize {
    value.load(Ordering::SeqCst)
}

#[test]
fn first_clone_can_reenter_event_query() {
    if support::run_in_subprocess("first_clone_can_reenter_event_query") {
        return;
    }

    // Catches cloning a first registration under the event's lock.
    let event = Arc::new(Event::new());
    let mut listener = event.listen();
    let probe = Probe::new(&event);
    probe.on_clone.store(QUERY, Ordering::SeqCst);
    let waker = waker(&probe);
    assert!(poll(&mut listener, &waker).is_pending());
    assert_eq!(count(&probe.queries), 1);
    #[cfg(feature = "std")]
    assert_eq!(count(&probe.last_listener_count), 1);
    assert_eq!(event.notify(1), 1);
    assert_eq!(count(&probe.wakes), 1);
    assert!(poll(&mut listener, &waker).is_ready());
    drop(listener);
    drop(waker);
    assert_eq!(Arc::strong_count(&probe), 1);
}

#[test]
fn first_clone_notification_is_not_lost() {
    if support::run_in_subprocess("first_clone_notification_is_not_lost") {
        return;
    }

    // Catches an unlocked clone followed by a stale, unconditional install.
    let event = Arc::new(Event::new());
    let mut listener = event.listen();
    let probe = Probe::new(&event);
    probe.on_clone.store(NOTIFY_ONE, Ordering::SeqCst);
    let waker = waker(&probe);
    assert!(poll(&mut listener, &waker).is_ready());
    assert_eq!(count(&probe.nested_notified), 1);
    assert_eq!(count(&probe.wakes), 0);
    #[cfg(feature = "std")]
    assert_eq!(event.total_listeners(), 0);
    drop(listener);
    drop(waker);
    assert_eq!(Arc::strong_count(&probe), 1);
}

#[test]
fn replacement_clone_can_reenter_event_query() {
    if support::run_in_subprocess("replacement_clone_can_reenter_event_query") {
        return;
    }

    // Catches cloning a changed registration under the event's lock.
    let event = Arc::new(Event::new());
    let mut listener = event.listen();
    let old = Probe::new(&event);
    let new = Probe::new(&event);
    let old_waker = waker(&old);
    let new_waker = waker(&new);
    assert!(poll(&mut listener, &old_waker).is_pending());
    new.on_clone.store(QUERY, Ordering::SeqCst);
    assert!(poll(&mut listener, &new_waker).is_pending());
    assert_eq!(count(&new.queries), 1);
    assert_eq!(count(&old.drops), 1);
    assert_eq!(event.notify(1), 1);
    assert_eq!(count(&old.wakes), 0);
    assert_eq!(count(&new.wakes), 1);
    assert!(poll(&mut listener, &new_waker).is_ready());
    drop(listener);
    drop(old_waker);
    drop(new_waker);
    assert_eq!(Arc::strong_count(&old), 1);
    assert_eq!(Arc::strong_count(&new), 1);
}

#[test]
fn replacement_clone_notification_uses_existing_registration() {
    if support::run_in_subprocess("replacement_clone_notification_uses_existing_registration") {
        return;
    }

    // Catches temporarily erasing the old task while cloning a replacement.
    let event = Arc::new(Event::new());
    let mut listener = event.listen();
    let old = Probe::new(&event);
    let new = Probe::new(&event);
    let old_waker = waker(&old);
    let new_waker = waker(&new);
    assert!(poll(&mut listener, &old_waker).is_pending());
    new.on_clone.store(NOTIFY_ONE, Ordering::SeqCst);
    assert!(poll(&mut listener, &new_waker).is_ready());
    assert_eq!(count(&new.nested_notified), 1);
    assert_eq!(count(&old.wakes), 1);
    assert_eq!(count(&new.wakes), 0);
    #[cfg(feature = "std")]
    assert_eq!(event.total_listeners(), 0);
    drop(listener);
    drop(old_waker);
    drop(new_waker);
    assert_eq!(Arc::strong_count(&old), 1);
    assert_eq!(Arc::strong_count(&new), 1);
}

#[test]
fn unused_replacement_clone_drop_can_reenter_after_notification() {
    if support::run_in_subprocess("unused_replacement_clone_drop_can_reenter_after_notification") {
        return;
    }

    // Catches an unused cloned task being dropped inside the install-phase lock.
    let event = Arc::new(Event::new());
    let mut listener = event.listen();
    let old = Probe::new(&event);
    let new = Probe::new(&event);
    let old_waker = waker(&old);
    let new_waker = waker(&new);
    assert!(poll(&mut listener, &old_waker).is_pending());
    new.on_clone.store(NOTIFY_ONE, Ordering::SeqCst);
    new.on_drop.store(QUERY, Ordering::SeqCst);
    assert!(poll(&mut listener, &new_waker).is_ready());
    assert_eq!(count(&new.clones), 1);
    assert_eq!(count(&new.drops), 1);
    assert_eq!(count(&new.queries), 1);
    #[cfg(feature = "std")]
    assert_eq!(count(&new.last_listener_count), 0);
    assert_eq!(count(&new.nested_notified), 1);
    assert_eq!(count(&old.wakes), 1);
    assert_eq!(count(&new.wakes), 0);
    #[cfg(feature = "std")]
    assert_eq!(event.total_listeners(), 0);
    drop(listener);
    drop(old_waker);
    drop(new_waker);
    assert_eq!(Arc::strong_count(&old), 1);
    assert_eq!(Arc::strong_count(&new), 1);
}

#[test]
fn unchanged_waker_repoll_does_not_clone() {
    if support::run_in_subprocess("unchanged_waker_repoll_does_not_clone") {
        return;
    }

    // Catches unconditional cloning introduced by two-phase registration.
    let event = Arc::new(Event::new());
    let mut listener = event.listen();
    let probe = Probe::new(&event);
    let waker = waker(&probe);
    assert!(poll(&mut listener, &waker).is_pending());
    assert_eq!(count(&probe.clones), 1);
    probe.on_clone.store(PANIC, Ordering::SeqCst);
    for _ in 0..8 {
        assert!(poll(&mut listener, &waker).is_pending());
    }
    assert_eq!(count(&probe.clones), 1);
    assert_eq!(event.notify(1), 1);
    assert!(poll(&mut listener, &waker).is_ready());

    // A terminal repoll must report completion without invoking callbacks,
    // even when cloning this waker would itself panic.
    assert_eq!(
        (
            count(&probe.clones),
            count(&probe.wakes),
            count(&probe.drops)
        ),
        (1, 1, 0)
    );
    let panic = catch_unwind(AssertUnwindSafe(|| poll(&mut listener, &waker)))
        .expect_err("a completed listener cannot be polled again");
    let message = panic
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("");
    assert!(message.contains("after it has finished"));
    assert_eq!(
        (
            count(&probe.clones),
            count(&probe.wakes),
            count(&probe.drops)
        ),
        (1, 1, 0)
    );
    probe.on_clone.store(0, Ordering::SeqCst);
}

#[test]
fn clone_panic_preserves_old_registration() {
    if support::run_in_subprocess("clone_panic_preserves_old_registration") {
        return;
    }

    // Catches replacing the state before the changed waker clone succeeds.
    let event = Arc::new(Event::new());
    let mut listener = event.listen();
    let old = Probe::new(&event);
    let new = Probe::new(&event);
    let old_waker = waker(&old);
    let new_waker = waker(&new);
    assert!(poll(&mut listener, &old_waker).is_pending());
    new.on_clone.store(PANIC, Ordering::SeqCst);
    let panic = catch_unwind(AssertUnwindSafe(|| poll(&mut listener, &new_waker)))
        .expect_err("the replacement clone's panic must propagate");
    let message = panic
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| panic.downcast_ref::<String>().map(String::as_str));
    assert_eq!(message, Some("intentional clone callback panic"));
    assert_eq!(count(&new.clones), 1);
    assert_eq!(
        Arc::strong_count(&new),
        2,
        "failed clone owns no new reference"
    );
    assert_eq!(count(&old.drops), 0, "old task remains registered");
    assert_eq!(event.notify(1), 1);
    assert_eq!(count(&old.wakes), 1);
    assert_eq!(count(&new.wakes), 0);
    assert!(poll(&mut listener, &old_waker).is_ready());
    drop(listener);
    drop(old_waker);
    drop(new_waker);
    assert_eq!(Arc::strong_count(&old), 1);
    assert_eq!(Arc::strong_count(&new), 1);
}

#[test]
fn old_waker_drop_can_reenter_after_replacement() {
    if support::run_in_subprocess("old_waker_drop_can_reenter_after_replacement") {
        return;
    }

    // Catches dropping the replaced task while the list remains locked.
    let event = Arc::new(Event::new());
    let mut listener = event.listen();
    let old = Probe::new(&event);
    let new = Probe::new(&event);
    let old_waker = waker(&old);
    let new_waker = waker(&new);
    assert!(poll(&mut listener, &old_waker).is_pending());
    old.on_drop.store(QUERY | NOTIFY_ONE, Ordering::SeqCst);
    // The callback notifies the newly installed registration. This poll may
    // return Pending, but its new task must have been scheduled exactly once.
    let replacement_poll = poll(&mut listener, &new_waker);
    assert_eq!(count(&old.queries), 1);
    assert_eq!(count(&old.nested_notified), 1);
    assert_eq!(count(&new.wakes), 1);
    if replacement_poll.is_pending() {
        assert!(poll(&mut listener, &new_waker).is_ready());
    }
    drop(listener);
    drop(old_waker);
    drop(new_waker);
    assert_eq!(Arc::strong_count(&old), 1);
    assert_eq!(Arc::strong_count(&new), 1);
}

#[test]
fn old_waker_drop_panic_keeps_new_registration_usable() {
    if support::run_in_subprocess("old_waker_drop_panic_keeps_new_registration_usable") {
        return;
    }

    // Catches exposing a placeholder state or poisoning the list while the
    // replaced task's destructor unwinds.
    let event = Arc::new(Event::new());
    let mut listener = event.listen();
    let old = Probe::new(&event);
    let new = Probe::new(&event);
    let old_waker = waker(&old);
    let new_waker = waker(&new);
    assert!(poll(&mut listener, &old_waker).is_pending());
    old.on_drop.store(QUERY | PANIC, Ordering::SeqCst);
    let panic = catch_unwind(AssertUnwindSafe(|| poll(&mut listener, &new_waker)))
        .expect_err("the old waker's drop panic must propagate");
    let message = panic
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| panic.downcast_ref::<String>().map(String::as_str));
    assert_eq!(message, Some("intentional drop callback panic"));
    assert_eq!(count(&old.queries), 1);
    assert_eq!(
        Arc::strong_count(&old),
        2,
        "panicking drop consumed its reference"
    );
    assert_eq!(event.notify(1), 1);
    assert_eq!(count(&old.wakes), 0);
    assert_eq!(count(&new.wakes), 1);
    assert!(poll(&mut listener, &new_waker).is_ready());
    drop(listener);
    drop(old_waker);
    drop(new_waker);
    assert_eq!(Arc::strong_count(&old), 1);
    assert_eq!(Arc::strong_count(&new), 1);
}

#[test]
fn pending_listener_drop_can_reenter_after_unlink() {
    if support::run_in_subprocess("pending_listener_drop_can_reenter_after_unlink") {
        return;
    }

    // Catches dropping an unnotified task before its entry is fully removed.
    let event = Arc::new(Event::new());
    let mut listener = event.listen();
    let probe = Probe::new(&event);
    let waker = waker(&probe);
    assert!(poll(&mut listener, &waker).is_pending());
    probe.on_drop.store(QUERY | NOTIFY_ONE, Ordering::SeqCst);
    drop(listener);
    assert_eq!(count(&probe.queries), 1);
    #[cfg(feature = "std")]
    assert_eq!(count(&probe.last_listener_count), 0);
    assert_eq!(count(&probe.nested_notified), 0);
    #[cfg(feature = "std")]
    assert_eq!(event.total_listeners(), 0);
    drop(waker);
    assert_eq!(Arc::strong_count(&probe), 1);
}

struct ThreeListeners {
    event: Arc<Event>,
    listeners: [Option<EventListener>; 3],
    probes: [Arc<Probe>; 3],
    wakers: [Waker; 3],
}

impl ThreeListeners {
    fn new() -> Self {
        let event = Arc::new(Event::new());
        let probes = [Probe::new(&event), Probe::new(&event), Probe::new(&event)];
        let wakers = [waker(&probes[0]), waker(&probes[1]), waker(&probes[2])];
        let mut listeners = [
            Some(event.listen()),
            Some(event.listen()),
            Some(event.listen()),
        ];
        for index in 0..3 {
            assert!(poll(listeners[index].as_mut().unwrap(), &wakers[index]).is_pending());
        }
        Self {
            event,
            listeners,
            probes,
            wakers,
        }
    }

    fn wake_counts(&self) -> [usize; 3] {
        [
            count(&self.probes[0].wakes),
            count(&self.probes[1].wakes),
            count(&self.probes[2].wakes),
        ]
    }

    fn complete(&mut self, index: usize) {
        let mut listener = self.listeners[index].take().unwrap();
        assert!(poll(&mut listener, &self.wakers[index]).is_ready());
    }
}

#[test]
fn normal_cancellation_transfers_to_next_with_reentrant_wake() {
    if support::run_in_subprocess("normal_cancellation_transfers_to_next_with_reentrant_wake") {
        return;
    }

    // Catches FIFO errors, callback-under-lock, or incomplete unlink accounting
    // when a single ordinary notification is handed off after cancellation.
    let mut fixture = ThreeListeners::new();
    assert_eq!(fixture.event.notify(1), 1);
    assert_eq!(fixture.wake_counts(), [1, 0, 0]);
    fixture.probes[1].on_wake.store(QUERY, Ordering::SeqCst);
    drop(fixture.listeners[0].take());
    assert_eq!(fixture.wake_counts(), [1, 1, 0]);
    assert_eq!(count(&fixture.probes[1].queries), 1);
    #[cfg(feature = "std")]
    assert_eq!(count(&fixture.probes[1].last_listener_count), 2);
    fixture.complete(1);
    assert_eq!(fixture.event.notify(1), 1);
    assert_eq!(fixture.wake_counts(), [1, 1, 1]);

    // Terminal cancellation must not retain notification credit or re-wake
    // removed listeners when the same event is reused.
    drop(fixture.listeners[2].take());
    assert_eq!(fixture.wake_counts(), [1, 1, 1]);
    assert!(!fixture.event.is_notified());
    let mut next = fixture.event.listen();
    assert!(poll(&mut next, &fixture.wakers[2]).is_pending());
    assert_eq!(fixture.wake_counts(), [1, 1, 1]);
    assert_eq!(fixture.event.notify(1), 1);
    assert_eq!(fixture.wake_counts(), [1, 1, 2]);
    assert_eq!(poll(&mut next, &fixture.wakers[2]), Poll::Ready(()));
    assert!(!fixture.event.is_notified());
    #[cfg(feature = "std")]
    assert_eq!(fixture.event.total_listeners(), 0);
}

#[test]
fn normal_cancellation_does_not_oversubscribe_existing_notification() {
    if support::run_in_subprocess(
        "normal_cancellation_does_not_oversubscribe_existing_notification",
    ) {
        return;
    }

    // An ordinary notification is a minimum, not an additional notification.
    let mut fixture = ThreeListeners::new();
    assert_eq!(fixture.event.notify(2), 2);
    assert_eq!(fixture.wake_counts(), [1, 1, 0]);
    drop(fixture.listeners[0].take());
    assert_eq!(fixture.wake_counts(), [1, 1, 0]);
    assert!(poll(fixture.listeners[2].as_mut().unwrap(), &fixture.wakers[2]).is_pending());
    fixture.complete(1);
    assert_eq!(fixture.event.notify(1), 1);
    fixture.complete(2);
    #[cfg(feature = "std")]
    assert_eq!(fixture.event.total_listeners(), 0);
}

#[test]
fn additional_cancellation_transfers_to_next_with_reentrant_wake() {
    if support::run_in_subprocess("additional_cancellation_transfers_to_next_with_reentrant_wake") {
        return;
    }

    // Additional notification handoff must survive another notified listener.
    let mut fixture = ThreeListeners::new();
    assert_eq!(fixture.event.notify(2.additional()), 2);
    assert_eq!(fixture.wake_counts(), [1, 1, 0]);
    fixture.probes[2].on_wake.store(QUERY, Ordering::SeqCst);
    drop(fixture.listeners[0].take());
    assert_eq!(fixture.wake_counts(), [1, 1, 1]);
    assert_eq!(count(&fixture.probes[2].queries), 1);
    #[cfg(feature = "std")]
    assert_eq!(count(&fixture.probes[2].last_listener_count), 2);
    fixture.complete(1);
    fixture.complete(2);
    #[cfg(feature = "std")]
    assert_eq!(fixture.event.total_listeners(), 0);
}

#[test]
fn discarding_notification_does_not_propagate() {
    if support::run_in_subprocess("discarding_notification_does_not_propagate") {
        return;
    }

    // A removal rewrite must retain discard's deliberate no-handoff contract.
    let mut fixture = ThreeListeners::new();
    assert_eq!(fixture.event.notify(1), 1);
    assert!(fixture.listeners[0].take().unwrap().discard());
    assert_eq!(fixture.wake_counts(), [1, 0, 0]);
    assert_eq!(fixture.event.notify(2), 2);
    fixture.complete(1);
    fixture.complete(2);
    #[cfg(feature = "std")]
    assert_eq!(fixture.event.total_listeners(), 0);
}
