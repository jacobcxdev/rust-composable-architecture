//! Isolated first-use allocation ownership and TLS-teardown regressions.
#![cfg(all(not(loom), not(target_family = "wasm")))]

mod support;

use event_listener::{Event, Listener};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
#[cfg(feature = "std")]
use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
#[cfg(feature = "std")]
use std::time::Duration;

thread_local! {
    static ARMED: Cell<Option<bool>> = const { Cell::new(None) };
    #[cfg(feature = "std")]
    static ON_EXIT: RefCell<Option<DestructorWait>> = const { RefCell::new(None) };
}

static ARRIVED: AtomicUsize = AtomicUsize::new(0);
static TIMED_OUT: AtomicBool = AtomicBool::new(false);
static ALLOCATED: AtomicUsize = AtomicUsize::new(0);
static RELEASED: AtomicUsize = AtomicUsize::new(0);
static TARGETS: [AtomicPtr<u8>; 2] = [
    AtomicPtr::new(ptr::null_mut()),
    AtomicPtr::new(ptr::null_mut()),
];
// Finite even if the other worker fails. Process-level timeout is independent.
const SPIN_LIMIT: usize = 50_000_000;

struct SchedulingAllocator;

#[global_allocator]
static ALLOCATOR: SchedulingAllocator = SchedulingAllocator;

fn rendezvous() -> Option<usize> {
    // Constant non-dropping TLS is prewarmed before arming; teardown is harmless.
    // Disarm before any System call. No formatting, allocation or unwinding here.
    let slot = usize::from(ARMED.try_with(Cell::take).ok().flatten()?);
    ARRIVED.fetch_add(1, Ordering::AcqRel);
    for _ in 0..SPIN_LIMIT {
        if ARRIVED.load(Ordering::Acquire) == 2 {
            return Some(slot);
        }
        std::hint::spin_loop();
    }
    TIMED_OUT.store(true, Ordering::Release);
    Some(slot)
}

fn record(slot: Option<usize>, ptr: *mut u8) {
    if let Some(slot) = slot {
        TARGETS[slot].store(ptr, Ordering::Release);
        ALLOCATED.fetch_add(1, Ordering::Release);
    }
}

// SAFETY: All operations forward valid caller layouts and pointers unchanged to
// System. Scheduling uses only prewarmed constant TLS and static atomics. std TLS
// does not recursively call the Rust global allocator; platform System work may
// occur. There are no user initialisers, assertions or callbacks in allocation.
// Recorded pointers are only compared, never dereferenced. Clearing on dealloc
// prevents later address reuse from being counted as another owned release.
unsafe impl GlobalAlloc for SchedulingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let slot = rendezvous();
        // SAFETY: Forward the caller's valid layout to System unchanged.
        let ptr = unsafe { System.alloc(layout) };
        record(slot, ptr);
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let slot = rendezvous();
        // SAFETY: Forward the caller's valid layout to System unchanged.
        let ptr = unsafe { System.alloc_zeroed(layout) };
        record(slot, ptr);
        ptr
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // This harness only arms the source-proven Arc allocation, never realloc.
        // SAFETY: Preserve System-owned pointer, layout and new-size obligations.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        for target in &TARGETS {
            if target
                .compare_exchange(ptr, ptr::null_mut(), Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                RELEASED.fetch_add(1, Ordering::Release);
            }
        }
        // SAFETY: Every allocation came from System; original layout is retained.
        unsafe { System.dealloc(ptr, layout) }
    }
}

fn concurrent_first_use() {
    // Allocating Arc<Event> does not initialise the Event's private shared state.
    let event = Arc::new(Event::new());
    let workers = [false, true].map(|slot| {
        let event = Arc::clone(&event);
        std::thread::spawn(move || {
            ARMED.with(|armed| assert!(armed.get().is_none()));
            ARMED.with(|armed| armed.set(Some(slot)));
            // The first allocation is Arc::new(Inner::new()), after loading null.
            let notified = event.notify(0);
            let still_armed = ARMED.with(Cell::take);
            assert!(
                still_armed.is_none(),
                "the source-proven allocation was not intercepted"
            );
            assert_eq!(notified, 0);
        })
    });
    for worker in workers {
        worker.join().expect("first-use worker panicked");
    }
    assert!(
        !TIMED_OUT.load(Ordering::Acquire),
        "allocation rendezvous timed out"
    );
    assert_eq!(ARRIVED.load(Ordering::Acquire), 2);
    assert_eq!(ALLOCATED.load(Ordering::Acquire), 2);
    assert_eq!(
        RELEASED.load(Ordering::Acquire),
        1,
        "the losing shared state must be released"
    );
    #[cfg(feature = "std")]
    assert_eq!(event.total_listeners(), 0);

    let first = event.listen();
    let second = event.listen();
    assert_eq!(event.notify(1), 1);
    assert!(first.discard(), "the first listener owns the notification");
    assert!(
        !second.discard(),
        "ordinary notification must not be duplicated"
    );
    let mut reused = event.listen();
    assert_eq!(event.notify(1), 1);
    assert_eq!(
        Pin::new(&mut reused).poll(&mut Context::from_waker(Waker::noop())),
        Poll::Ready(())
    );
    drop(reused);
    #[cfg(feature = "std")]
    assert_eq!(event.total_listeners(), 0);
    assert_eq!(RELEASED.load(Ordering::Acquire), 1);
    drop(event);
    assert_eq!(
        RELEASED.load(Ordering::Acquire),
        2,
        "the winning shared state must be released exactly once"
    );
    assert!(TARGETS
        .iter()
        .all(|target| target.load(Ordering::Acquire).is_null()));
    println!(
        "concurrent_first_use: two allocations, one losing release, FIFO reuse, final release verified"
    );
}

#[cfg(feature = "std")]
struct DestructorWait {
    event: Arc<Event>,
    observations: Arc<AtomicUsize>,
}

#[cfg(feature = "std")]
impl Drop for DestructorWait {
    fn drop(&mut self) {
        // PARKER was initialised after this TLS object. Its destruction order
        // is platform-dependent; waits must still work if its cache is unavailable.
        let expired = self.event.listen().wait_timeout(Duration::ZERO);
        let listener = self.event.listen();
        let notified = self.event.notify(1);
        let delivered = listener.wait_timeout(Duration::ZERO);
        let good = expired.is_none()
            && notified == 1
            && delivered == Some(())
            && self.event.total_listeners() == 0;
        self.observations
            .store(if good { 2 } else { 1 }, Ordering::Release);
    }
}

#[cfg(feature = "std")]
fn wait_after_parker_tls_destruction() {
    let event = Arc::new(Event::new());
    let observations = Arc::new(AtomicUsize::new(0));
    let worker_event = Arc::clone(&event);
    let worker_observations = Arc::clone(&observations);
    std::thread::spawn(move || {
        ON_EXIT.with(|slot| {
            *slot.borrow_mut() = Some(DestructorWait {
                event: Arc::clone(&worker_event),
                observations: worker_observations,
            });
        });
        assert_eq!(worker_event.listen().wait_timeout(Duration::ZERO), None);
        assert_eq!(worker_event.total_listeners(), 0);
    })
    .join()
    .expect("TLS worker panicked");
    assert_eq!(
        observations.load(Ordering::Acquire),
        2,
        "destructor waits must expire or deliver normally"
    );
    assert_eq!(Arc::strong_count(&event), 1);
    assert_eq!(event.total_listeners(), 0);
    let listener = event.listen();
    assert_eq!(event.notify(1), 1);
    assert_eq!(listener.wait_timeout(Duration::ZERO), Some(()));
    println!(
        "tls_destructor: expired and notified waits succeeded, captures released, Event reused"
    );
}

#[test]
fn concurrent_first_use_releases_losing_allocation() {
    if support::run_in_subprocess("concurrent_first_use_releases_losing_allocation") {
        return;
    }
    concurrent_first_use();
}

#[test]
#[cfg(feature = "std")]
fn waits_succeed_during_tls_teardown() {
    if support::run_in_subprocess("waits_succeed_during_tls_teardown") {
        return;
    }
    wait_after_parker_tls_destruction();
}
