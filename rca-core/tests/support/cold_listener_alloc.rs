// Intentional package-local copy: keep core and test crate archives self-contained.
// Keep both cold_listener_alloc.rs copies identical; never used in production.
//! Pauses one armed allocation until a producer completes a cold-listener race.
//!
//! One scenario owns this process-wide rendezvous. Other threads keep allocating
//! normally. The spin limit bounds failed coordination, not elapsed wall time;
//! the compiler/test job still owns the external process watchdog.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

thread_local! {
    /// Arms only the consumer thread's next allocation.
    static ARMED: Cell<bool> = const { Cell::new(false) };
}

/// No scenario has started; this helper is intentionally not reset for reuse.
const IDLE: u8 = 0;
/// The consumer is armed but has not reached an allocation.
const WAITING: u8 = 1;
/// The allocator is waiting for the producer's operation.
const INTERCEPTED: u8 = 2;
/// The producer completed its operation and released the allocator.
const PRODUCED: u8 = 3;
/// Either side exhausted its finite coordination budget.
const TIMED_OUT: u8 = 4;
/// Bounds work even if the producer panics or the consumer never allocates.
const SPIN_LIMIT: usize = 50_000_000;
/// Publishes the rendezvous phase between the consumer and producer.
static PHASE: AtomicU8 = AtomicU8::new(IDLE);
/// Counts interceptions so an unrelated or repeated allocation cannot pass.
static INTERCEPTIONS: AtomicUsize = AtomicUsize::new(0);

/// Forwards allocation unchanged to `System`, with a one-shot scheduling hook.
struct SchedulingAllocator;

/// Installs the hook for this test binary, never a production library build.
#[global_allocator]
static ALLOCATOR: SchedulingAllocator = SchedulingAllocator;

// SAFETY: Every allocation operation delegates unchanged to System. The rendezvous
// accesses static atomics and constant, non-dropping TLS; consumer TLS is prewarmed.
// std guarantees TLS access does not invoke the Rust global allocator (platform
// System allocation/locking may occur). There is no user initializer, callback,
// pointer dereference, assertion or unwinding in the rendezvous.
unsafe impl GlobalAlloc for SchedulingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        intercept();
        // SAFETY: Forward the caller's valid allocation layout unchanged.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        intercept();
        // SAFETY: Forward the caller's valid allocation layout unchanged.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        intercept();
        // SAFETY: All System-owned pointer/layout/new-size obligations pass unchanged.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: All allocations came from System, with the original layout.
        unsafe { System.dealloc(ptr, layout) }
    }
}

/// Consumes the current thread's arm and waits without allocating or unwinding.
fn intercept() {
    // try_with also makes unrelated allocation during TLS teardown harmless.
    if !ARMED
        .try_with(|armed| armed.replace(false))
        .unwrap_or(false)
    {
        return;
    }
    INTERCEPTIONS.fetch_add(1, Ordering::SeqCst);
    PHASE.store(INTERCEPTED, Ordering::Release);
    for _ in 0..SPIN_LIMIT {
        if PHASE.load(Ordering::Acquire) != INTERCEPTED {
            return;
        }
        std::hint::spin_loop();
    }
    let _ = PHASE.compare_exchange(INTERCEPTED, TIMED_OUT, Ordering::AcqRel, Ordering::Acquire);
}

/// Initialises consumer TLS before arming and checks the one-scenario contract.
///
/// # Panics
///
/// Panics if this thread is already armed or a scenario has already started.
pub(crate) fn prepare() {
    ARMED.with(|armed| assert!(!armed.replace(false)));
    assert_eq!(PHASE.load(Ordering::Acquire), IDLE);
}

/// Marks the consumer thread's next allocation as the scheduling boundary.
pub(crate) fn arm() {
    PHASE.store(WAITING, Ordering::Release);
    ARMED.with(|armed| armed.set(true));
}

/// Prevents a later allocation from intercepting when the expected poll did not.
pub(crate) fn disarm() {
    ARMED.with(|armed| armed.set(false));
}

/// Waits for the consumer's allocation, returning false when coordination times out.
pub(crate) fn await_interception() -> bool {
    for _ in 0..SPIN_LIMIT {
        match PHASE.load(Ordering::Acquire) {
            INTERCEPTED => return true,
            TIMED_OUT => return false,
            _ => std::hint::spin_loop(),
        }
    }
    PHASE.store(TIMED_OUT, Ordering::Release);
    false
}

/// Releases the allocator after the producer's operation without hiding a timeout.
pub(crate) fn produced() {
    let _ = PHASE.compare_exchange(INTERCEPTED, PRODUCED, Ordering::Release, Ordering::Relaxed);
}

/// Checks the full handshake after the caller has joined the producer thread.
///
/// # Panics
///
/// Panics unless exactly one allocation was intercepted and released by the producer.
pub(crate) fn assert_completed() {
    assert_eq!(INTERCEPTIONS.load(Ordering::Acquire), 1);
    assert_eq!(
        PHASE.load(Ordering::Acquire),
        PRODUCED,
        "rendezvous timed out"
    );
}
