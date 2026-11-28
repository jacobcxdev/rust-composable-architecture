//! Public diagnostics and delivery while a broadcast contends with registration.

#![cfg(all(not(loom), not(target_family = "wasm")))]

mod support;

use event_listener::Event;
#[cfg(not(feature = "critical-section"))]
use event_listener::IntoNotification;
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
#[cfg(not(feature = "critical-section"))]
use std::time::{Duration, Instant};

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
    #[cfg(feature = "critical-section")]
    static DEBUG_EVENT: Cell<*const Event> = const { Cell::new(std::ptr::null()) };
    #[cfg(feature = "critical-section")]
    static FORMATTING: Cell<bool> = const { Cell::new(false) };
}

static INTERCEPTED: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "critical-section")]
static DEBUG_LOCKED: AtomicBool = AtomicBool::new(false);
#[cfg(not(feature = "critical-section"))]
static ALLOCATION_WAITING: AtomicBool = AtomicBool::new(false);
#[cfg(not(feature = "critical-section"))]
static RELEASE_ALLOCATION: AtomicBool = AtomicBool::new(false);
#[cfg(not(feature = "critical-section"))]
static ALLOCATION_TIMED_OUT: AtomicBool = AtomicBool::new(false);
#[cfg(not(feature = "critical-section"))]
static START_CONTENDER: AtomicBool = AtomicBool::new(false);
#[cfg(not(feature = "critical-section"))]
static CONTENDER_ENTERED: AtomicBool = AtomicBool::new(false);
#[cfg(not(feature = "critical-section"))]
static CONTENDER_DONE: AtomicBool = AtomicBool::new(false);

// Exceed the production inline capacity to exercise a real broadcast allocation.
const LISTENERS: usize = 33;
// Finite even if the coordinator fails; the subprocess has a separate watchdog.
#[cfg(not(feature = "critical-section"))]
const ALLOCATOR_SPIN_LIMIT: usize = 500_000_000;

struct SchedulingAllocator;

#[global_allocator]
static ALLOCATOR: SchedulingAllocator = SchedulingAllocator;

fn allocation_rendezvous() {
    #[cfg(feature = "critical-section")]
    if FORMATTING.try_with(Cell::get).unwrap_or(false) {
        // A diagnostic must not recursively enter the allocator, even disarmed.
        std::process::abort();
    }
    // Prewarmed, constant, non-dropping TLS. Disarm before either mode; neither
    // may allocate, assert or unwind through GlobalAlloc.
    if !ARMED.try_with(Cell::take).unwrap_or(false) {
        return;
    }
    INTERCEPTED.fetch_add(1, Ordering::AcqRel);
    #[cfg(feature = "critical-section")]
    allocation_diagnostic();
    #[cfg(not(feature = "critical-section"))]
    {
        ALLOCATION_WAITING.store(true, Ordering::Release);
        for _ in 0..ALLOCATOR_SPIN_LIMIT {
            if RELEASE_ALLOCATION.load(Ordering::Acquire) {
                ALLOCATION_WAITING.store(false, Ordering::Release);
                return;
            }
            std::hint::spin_loop();
        }
        ALLOCATION_TIMED_OUT.store(true, Ordering::Release);
        ALLOCATION_WAITING.store(false, Ordering::Release);
    }
}

#[cfg(feature = "critical-section")]
fn allocation_diagnostic() {
    struct AbortOnUnwind;
    impl Drop for AbortOnUnwind {
        fn drop(&mut self) {
            std::process::abort();
        }
    }

    // No panic may escape an allocator method, including a future Debug change.
    let abort_on_unwind = AbortOnUnwind;
    let event = DEBUG_EVENT
        .try_with(|slot| slot.replace(std::ptr::null()))
        .unwrap_or(std::ptr::null());
    if !event.is_null() {
        if FORMATTING.try_with(|flag| flag.set(true)).is_err() {
            std::process::abort();
        }
        let mut sink = DebugSink {
            bytes: [0; 15],
            len: 0,
        };
        // SAFETY: The test installs Arc::as_ptr only for its synchronous notify
        // call, retains that Arc, and clears the pointer before it can be dropped.
        // This same-thread hook takes the pointer before use and borrows only the
        // public Event. Debug's nested critical section uses try_borrow, so it
        // never aliases the list's live mutable borrow or reads private state.
        let result = std::fmt::write(&mut sink, format_args!("{:?}", unsafe { &*event }));
        DEBUG_LOCKED.store(
            result.is_ok() && sink.len == 15 && &sink.bytes == b"Event(<locked>)",
            Ordering::Release,
        );
        if FORMATTING.try_with(|flag| flag.set(false)).is_err() {
            std::process::abort();
        }
    }
    std::mem::forget(abort_on_unwind);
}

#[cfg(feature = "critical-section")]
struct DebugSink {
    // Exactly the expected diagnostic; longer output returns fmt::Error.
    bytes: [u8; 15],
    len: usize,
}

#[cfg(feature = "critical-section")]
impl std::fmt::Write for DebugSink {
    fn write_str(&mut self, text: &str) -> std::fmt::Result {
        let end = self.len.checked_add(text.len()).ok_or(std::fmt::Error)?;
        let destination = self.bytes.get_mut(self.len..end).ok_or(std::fmt::Error)?;
        destination.copy_from_slice(text.as_bytes());
        self.len = end;
        Ok(())
    }
}

// SAFETY: Every pointer, layout and size is forwarded unchanged to System.
// Scheduling uses atomics and prewarmed constant TLS. The diagnostic mode uses
// a fixed stack sink, aborts on recursive allocation or unwinding, and borrows
// only a live public Event. Neither mode dereferences allocation pointers or
// fabricates event state. All assertion/heap-formatting work stays outside.
unsafe impl GlobalAlloc for SchedulingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        allocation_rendezvous();
        // SAFETY: Preserve the caller's valid layout.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        allocation_rendezvous();
        // SAFETY: Preserve the caller's valid layout and zeroing request.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        allocation_rendezvous();
        // SAFETY: Preserve the System-owned pointer and reallocation obligations.
        unsafe { System.realloc(ptr, layout, size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: All allocations came from System; retain the original layout.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[cfg(not(feature = "critical-section"))]
struct ReleaseOnDrop;

#[cfg(not(feature = "critical-section"))]
impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        RELEASE_ALLOCATION.store(true, Ordering::Release);
    }
}

struct WakeCounter(AtomicUsize);

impl Wake for WakeCounter {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::AcqRel);
    }
}

#[cfg(not(feature = "critical-section"))]
fn wait_for(flag: &AtomicBool, reason: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !flag.load(Ordering::Acquire) {
        assert!(Instant::now() < deadline, "{reason}");
        std::thread::yield_now();
    }
}

#[test]
#[cfg(not(feature = "critical-section"))]
fn contended_broadcast_keeps_debug_responsive_and_delivers_every_listener() {
    if support::run_in_subprocess(
        "contended_broadcast_keeps_debug_responsive_and_delivers_every_listener",
    ) {
        return;
    }

    let event = Arc::new(Event::new());
    assert_eq!(format!("{event:?}"), "Event(<uninitialized>)");
    let wake_count = Arc::new(WakeCounter(AtomicUsize::new(0)));
    let waker = Waker::from(Arc::clone(&wake_count));
    let mut context = Context::from_waker(&waker);
    let mut listeners: Vec<_> = (0..LISTENERS).map(|_| event.listen()).collect();
    for listener in &mut listeners {
        assert!(Pin::new(listener).poll(&mut context).is_pending());
    }

    // Create both threads before arming, avoiding thread-startup allocations in
    // the interval that proves the notifier's first allocation is overflow.
    let contender_event = Arc::clone(&event);
    let contender = std::thread::spawn(move || {
        wait_for(&START_CONTENDER, "contender was never started");
        CONTENDER_ENTERED.store(true, Ordering::Release);
        let mut listener = contender_event.listen();
        assert_eq!(contender_event.notify(1usize.additional()), 1);
        assert_eq!(
            Pin::new(&mut listener).poll(&mut Context::from_waker(Waker::noop())),
            Poll::Ready(())
        );
        CONTENDER_DONE.store(true, Ordering::Release);
    });

    let notifier_event = Arc::clone(&event);
    let notifier = std::thread::spawn(move || {
        ARMED.with(|armed| assert!(!armed.get()));
        ARMED.with(|armed| armed.set(true));
        // Event state and all Wakers already exist. The first allocation is the
        // 33rd deferred wake overflowing its inline buffer under the list lock.
        let notified = notifier_event.notify(usize::MAX);
        assert!(!ARMED.with(Cell::take), "overflow was not intercepted");
        notified
    });

    let release = ReleaseOnDrop;
    wait_for(&ALLOCATION_WAITING, "broadcast never reached overflow");
    assert_eq!(INTERCEPTED.load(Ordering::Acquire), 1);
    assert_eq!(wake_count.0.load(Ordering::Acquire), 0);
    let diagnostic = format!("{event:?}");
    assert!(diagnostic.contains("<locked>"), "{diagnostic}");
    assert!(ALLOCATION_WAITING.load(Ordering::Acquire));

    START_CONTENDER.store(true, Ordering::Release);
    wait_for(&CONTENDER_ENTERED, "contender never entered its operation");
    // This is a genuine contention window, not proof that the thread reached
    // any private instruction. Native coverage must establish CAS/spin hits.
    std::thread::sleep(Duration::from_millis(75));
    assert!(ALLOCATION_WAITING.load(Ordering::Acquire));
    assert!(!ALLOCATION_TIMED_OUT.load(Ordering::Acquire));
    assert!(!CONTENDER_DONE.load(Ordering::Acquire));
    assert_eq!(wake_count.0.load(Ordering::Acquire), 0);
    drop(release);

    assert_eq!(notifier.join().expect("notifier panicked"), LISTENERS);
    contender.join().expect("contender panicked");
    assert!(CONTENDER_DONE.load(Ordering::Acquire));
    assert!(!ALLOCATION_TIMED_OUT.load(Ordering::Acquire));
    assert_eq!(wake_count.0.load(Ordering::Acquire), LISTENERS);
    assert_eq!(Arc::strong_count(&wake_count), 2);
    for listener in &mut listeners {
        assert_eq!(Pin::new(listener).poll(&mut context), Poll::Ready(()));
    }
    assert!(!event.is_notified());
    assert!(format!("{event:?}").contains("listeners_total: 0"));
}

#[test]
#[cfg(feature = "critical-section")]
fn same_thread_broadcast_keeps_debug_responsive_and_delivers_every_listener() {
    if support::run_in_subprocess(
        "same_thread_broadcast_keeps_debug_responsive_and_delivers_every_listener",
    ) {
        return;
    }

    let event = Arc::new(Event::new());
    assert_eq!(format!("{event:?}"), "Event(<uninitialized>)");
    let wake_count = Arc::new(WakeCounter(AtomicUsize::new(0)));
    let waker = Waker::from(Arc::clone(&wake_count));
    let mut context = Context::from_waker(&waker);
    let mut listeners: Vec<_> = (0..LISTENERS).map(|_| event.listen()).collect();
    for listener in &mut listeners {
        assert!(Pin::new(listener).poll(&mut context).is_pending());
    }

    // Registration has already initialised the host's reentrant critical-section
    // TLS. Prewarm the allocator TLS too, before installing the scoped pointer.
    ARMED.with(|armed| assert!(!armed.get()));
    DEBUG_EVENT.with(|slot| assert!(slot.get().is_null()));
    FORMATTING.with(|flag| assert!(!flag.get()));
    struct DisarmOnDrop;
    impl Drop for DisarmOnDrop {
        fn drop(&mut self) {
            let _ = ARMED.try_with(|armed| armed.set(false));
            let _ = DEBUG_EVENT.try_with(|slot| slot.set(std::ptr::null()));
        }
    }
    // Declared after event: clear the TLS pointer before its Arc can be dropped,
    // including if notify unexpectedly panics before reaching overflow.
    let disarm = DisarmOnDrop;
    DEBUG_EVENT.with(|slot| slot.set(Arc::as_ptr(&event)));
    ARMED.with(|armed| armed.set(true));
    // The 33rd deferred wake allocates under the live list RefMut. The allocator
    // formats this same Event on this same thread; nested try_borrow must fail.
    let notified = event.notify(usize::MAX);
    let still_armed = ARMED.with(Cell::take);
    let retained_pointer = DEBUG_EVENT.with(|slot| slot.replace(std::ptr::null()));
    drop(disarm);

    assert!(!still_armed, "overflow was not intercepted");
    assert!(retained_pointer.is_null());
    assert!(!FORMATTING.with(Cell::get));
    assert_eq!(INTERCEPTED.load(Ordering::Acquire), 1);
    assert!(DEBUG_LOCKED.load(Ordering::Acquire));
    assert_eq!(notified, LISTENERS);
    assert_eq!(wake_count.0.load(Ordering::Acquire), LISTENERS);
    assert_eq!(Arc::strong_count(&wake_count), 2);
    for listener in &mut listeners {
        assert_eq!(Pin::new(listener).poll(&mut context), Poll::Ready(()));
    }
    assert!(!event.is_notified());
    assert!(format!("{event:?}").contains("listeners_total: 0"));
}
