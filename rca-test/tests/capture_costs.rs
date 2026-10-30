//! Same-thread, warmed allocation accounting; not a throughput benchmark.
//!
//! Each measurement counts only its calling thread. Request totals include
//! allocation attempts; signed live deltas include storage freed during the
//! window even when allocated beforehand. A returned value can outlive that
//! window, so only the complete-lifetime cleanup cases assert zero live deltas.
//! Printed phase and scaling totals are observations, not fixed API budgets.

#[cfg(feature = "local")]
#[path = "capture_costs/host_phases.rs"]
mod host_phases;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::convert::Infallible;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use futures_executor::block_on;
use rca_core::{Effect, ReduceContext, Reducer};
use rca_test::{CaptureKey, TestStore};

#[derive(Clone, Copy, Debug, Default)]
/// Request totals and successful storage changes within one measurement window.
struct Allocations {
    /// Allocation, zeroed-allocation and reallocation attempts, including failures.
    calls: usize,
    /// Sum of requested sizes, not peak or currently retained storage.
    bytes: usize,
    /// Reallocation attempts already included in `calls`.
    reallocations: usize,
    /// Successful allocations minus deallocations; reallocation preserves this count.
    live_blocks: i128,
    /// Successful storage-size changes, including destruction of older allocations.
    live_bytes: i128,
}

thread_local! {
    static ACTIVE: Cell<bool> = const { Cell::new(false) };
    static ALLOCATIONS: Cell<Allocations> = const {
        Cell::new(Allocations {
            calls: 0, bytes: 0, reallocations: 0, live_blocks: 0, live_bytes: 0,
        })
    };
}

// Signed deltas also describe storage released after allocation before measurement.
// Only successful allocations change these counters; request metrics stay unchanged.
fn count_live(blocks: i128, bytes: i128) {
    if ACTIVE.try_with(Cell::get).unwrap_or(false) {
        let _ = ALLOCATIONS.try_with(|cell| {
            let mut counts = cell.get();
            counts.live_blocks = counts.live_blocks.saturating_add(blocks);
            counts.live_bytes = counts.live_bytes.saturating_add(bytes);
            cell.set(counts);
        });
    }
}

/// Forwards unchanged allocation requests while recording non-dropping TLS counters.
struct CountingAllocator;
#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn count(bytes: usize, reallocating: bool) {
    if ACTIVE.try_with(Cell::get).unwrap_or(false) {
        let _ = ALLOCATIONS.try_with(|cell| {
            let mut counts = cell.get();
            counts.calls = counts.calls.saturating_add(1);
            counts.bytes = counts.bytes.saturating_add(bytes);
            counts.reallocations = counts
                .reallocations
                .saturating_add(usize::from(reallocating));
            cell.set(counts);
        });
    }
}

// SAFETY: Delegates each operation and its original arguments to System. Counting
// touches only constant, non-dropping TLS Cells; it allocates nothing, invokes no
// user code and cannot panic. Failed allocations remain failed and are never used.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count(layout.size(), false);
        // SAFETY: The caller's valid layout passes unchanged to System.
        let result = unsafe { System.alloc(layout) };
        if !result.is_null() {
            count_live(1, layout.size() as i128);
        }
        result
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count(layout.size(), false);
        // SAFETY: The caller's valid layout passes unchanged to System.
        let result = unsafe { System.alloc_zeroed(layout) };
        if !result.is_null() {
            count_live(1, layout.size() as i128);
        }
        result
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count(new_size, true);
        // SAFETY: Original System-owned pointer, layout and new size pass unchanged.
        let result = unsafe { System.realloc(ptr, layout, new_size) };
        if !result.is_null() {
            count_live(0, new_size as i128 - layout.size() as i128);
        }
        result
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: All pointers came from System; original layout passes unchanged.
        unsafe { System.dealloc(ptr, layout) }
        count_live(-1, -(layout.size() as i128));
    }
}

/// Counts one operation without counting its caller's later result destruction.
///
/// Nested measurements are rejected. Unwinding disarms counting; a caller that
/// needs panic-path totals must catch the intended panic inside `operation`.
fn measured<T>(operation: impl FnOnce() -> T) -> (T, Allocations) {
    struct Disarm;
    impl Drop for Disarm {
        fn drop(&mut self) {
            ACTIVE.with(|active| active.set(false));
        }
    }
    // Warm both TLS slots before arming. Panic unwinding always disarms counting.
    ACTIVE.with(|active| assert!(!active.get()));
    ALLOCATIONS.with(|counts| counts.set(Allocations::default()));
    ACTIVE.with(|active| active.set(true));
    let disarm = Disarm;
    let result = operation();
    drop(disarm);
    (result, ALLOCATIONS.with(Cell::get))
}

#[test]
fn deadline_cohorts_have_explicit_costs_and_order() {
    use std::sync::Mutex;
    use std::task::{Context, Wake, Waker};
    use std::time::Duration;

    use rca_core::MonotonicInstant;
    use rca_test::DeterministicRuntime;

    struct RecordWake {
        id: usize,
        events: Arc<Mutex<Vec<usize>>>,
    }

    impl Wake for RecordWake {
        fn wake(self: Arc<Self>) {
            self.events.lock().unwrap().push(self.id);
        }
    }

    for cohorts in [1, 4] {
        for count in [1, 16, 256] {
            let host = DeterministicRuntime::new();
            let events = Arc::new(Mutex::new(Vec::with_capacity(cohorts * count)));
            // Some platforms allocate mutex storage on first lock. Keep the
            // recording fixture's lazy initialisation outside the host window.
            drop(events.lock().unwrap());
            let mut waits = Vec::with_capacity(cohorts * count);
            for cohort in 0..cohorts {
                let deadline = MonotonicInstant::from_elapsed(Duration::from_secs(
                    u64::try_from(cohort + 1).unwrap(),
                ));
                for index in 0..count {
                    let waker = Waker::from(Arc::new(RecordWake {
                        id: cohort * count + index,
                        events: Arc::clone(&events),
                    }));
                    let mut wait = host.sleep_until(deadline);
                    assert!(
                        wait.as_mut()
                            .poll(&mut Context::from_waker(&waker))
                            .is_pending()
                    );
                    waits.push(wait);
                }
            }
            assert_eq!(host.pending_sleeps().len(), cohorts * count);
            let target = Duration::from_secs(u64::try_from(cohorts).unwrap());

            let (report, cost) = measured(|| host.advance(target).unwrap());

            assert_eq!(report.polls(), 0);
            assert_eq!(report.deadline_batches(), cohorts);
            assert_eq!(
                report.current_time(),
                MonotonicInstant::from_elapsed(target)
            );
            assert_eq!(
                *events.lock().unwrap(),
                (0..cohorts * count).collect::<Vec<_>>()
            );
            assert!(host.pending_sleeps().is_empty());
            for mut wait in waits {
                assert!(
                    wait.as_mut()
                        .poll(&mut Context::from_waker(Waker::noop()))
                        .is_ready()
                );
            }
            assert_eq!(Arc::strong_count(&events), 1);
            eprintln!(
                "deadline cohorts={cohorts} count={count} allocations={} bytes={} reallocations={} live_blocks={} live_bytes={}",
                cost.calls, cost.bytes, cost.reallocations, cost.live_blocks, cost.live_bytes
            );
        }
    }
}

#[test]
fn allocator_forwards_aligned_zeroed_and_reallocated_storage() {
    for alignment in [1, 8, 64, 4096] {
        for size in [1, 17, 256] {
            let layout = Layout::from_size_align(size, alignment).unwrap();
            let grown_size = size * 2 + 1;
            let shrunk_size = 1;
            let ((), counts) = measured(|| {
                // SAFETY: All layouts have nonzero sizes and valid alignments.
                // Successful allocations are checked before access. Each reallocation
                // receives its live System pointer and exact current layout; only
                // initialised, in-bounds bytes are read. The final live pointers are
                // deallocated once with their exact layouts.
                unsafe {
                    let ordinary = ALLOCATOR.alloc(layout);
                    assert!(!ordinary.is_null());
                    assert_eq!(ordinary.addr() % alignment, 0);
                    ordinary.write_bytes(0x3c, size);
                    assert!(
                        std::slice::from_raw_parts(ordinary, size)
                            .iter()
                            .all(|byte| *byte == 0x3c)
                    );
                    ALLOCATOR.dealloc(ordinary, layout);

                    let zeroed = ALLOCATOR.alloc_zeroed(layout);
                    assert!(!zeroed.is_null());
                    assert_eq!(zeroed.addr() % alignment, 0);
                    assert!(
                        std::slice::from_raw_parts(zeroed, size)
                            .iter()
                            .all(|byte| *byte == 0)
                    );
                    zeroed.write_bytes(0xa5, size);
                    let grown = ALLOCATOR.realloc(zeroed, layout, grown_size);
                    assert!(!grown.is_null());
                    let live = ALLOCATIONS.with(Cell::get);
                    assert_eq!((live.live_blocks, live.live_bytes), (1, grown_size as i128));
                    assert_eq!(grown.addr() % alignment, 0);
                    assert!(
                        std::slice::from_raw_parts(grown, size)
                            .iter()
                            .all(|byte| *byte == 0xa5)
                    );
                    grown.write_bytes(0x5a, grown_size);
                    let grown_layout = Layout::from_size_align(grown_size, alignment).unwrap();
                    let shrunk = ALLOCATOR.realloc(grown, grown_layout, shrunk_size);
                    assert!(!shrunk.is_null());
                    let live = ALLOCATIONS.with(Cell::get);
                    assert_eq!(
                        (live.live_blocks, live.live_bytes),
                        (1, shrunk_size as i128)
                    );
                    assert_eq!(shrunk.addr() % alignment, 0);
                    assert_eq!(shrunk.read(), 0x5a);
                    let shrunk_layout = Layout::from_size_align(shrunk_size, alignment).unwrap();
                    ALLOCATOR.dealloc(shrunk, shrunk_layout);
                }
            });
            assert_eq!(counts.calls, 4);
            assert_eq!(counts.bytes, size * 2 + grown_size + shrunk_size);
            assert_eq!(counts.reallocations, 2);
            assert_eq!((counts.live_blocks, counts.live_bytes), (0, 0));
        }
    }
}

#[test]
fn allocator_counting_saturates_and_stays_disabled_outside_measurement() {
    let ((), counts) = measured(|| {
        ALLOCATIONS.with(|cell| {
            cell.set(Allocations {
                calls: usize::MAX,
                bytes: usize::MAX,
                reallocations: usize::MAX,
                live_blocks: i128::MAX,
                live_bytes: i128::MAX,
            });
        });
        count(usize::MAX, true);
        count_live(1, 1);
    });
    assert_eq!(counts.calls, usize::MAX);
    assert_eq!(counts.bytes, usize::MAX);
    assert_eq!(counts.reallocations, usize::MAX);
    assert_eq!(
        (counts.live_blocks, counts.live_bytes),
        (i128::MAX, i128::MAX)
    );
    let ((), counts) = measured(|| {
        ALLOCATIONS.with(|cell| {
            cell.set(Allocations {
                live_blocks: i128::MIN,
                live_bytes: i128::MIN,
                ..Allocations::default()
            });
        });
        count_live(-1, -1);
    });
    assert_eq!(
        (counts.live_blocks, counts.live_bytes),
        (i128::MIN, i128::MIN)
    );
    ALLOCATIONS.with(|cell| cell.set(Allocations::default()));
    count(usize::MAX, true);
    count_live(1, 1);
    let disabled = ALLOCATIONS.with(Cell::get);
    assert_eq!(disabled.calls, 0);
    assert_eq!(disabled.bytes, 0);
    assert_eq!(disabled.reallocations, 0);
    assert_eq!((disabled.live_blocks, disabled.live_bytes), (0, 0));
}

#[test]
fn allocator_measurement_disarms_after_user_panic_and_rejected_nesting() {
    let panic = std::panic::catch_unwind(|| measured(|| panic!("measurement panic")));
    panic.unwrap_err();
    assert!(!ACTIVE.with(Cell::get));
    let nested = std::panic::catch_unwind(|| measured(|| measured(|| ())));
    nested.unwrap_err();
    assert!(!ACTIVE.with(Cell::get));
    let ((), counts) = measured(|| count(7, false));
    assert_eq!(counts.calls, 1);
    assert_eq!(counts.bytes, 7);
    assert_eq!(counts.reallocations, 0);
}

#[test]
fn allocator_measurement_is_thread_local() {
    let ((), parent) = measured(|| count(7, false));
    std::thread::spawn(|| {
        assert!(!ACTIVE.with(Cell::get));
        let ((), child) = measured(|| count(5, true));
        assert_eq!(child.calls, 1);
        assert_eq!(child.bytes, 5);
        assert_eq!(child.reallocations, 1);
        assert!(!ACTIVE.with(Cell::get));
    })
    .join()
    .unwrap();
    let after = ALLOCATIONS.with(Cell::get);
    assert_eq!(after.calls, parent.calls);
    assert_eq!(after.bytes, parent.bytes);
    assert_eq!(after.reallocations, parent.reallocations);
    assert!(!ACTIVE.with(Cell::get));
}

#[test]
fn assertion_cleanup_panics_release_owned_failure_diagnostics() {
    use std::backtrace::BacktraceStatus;
    use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};

    struct PanicOnDrop(bool);
    impl Drop for PanicOnDrop {
        fn drop(&mut self) {
            if self.0 {
                // Avoid global panic-hook state and its unrelated formatting work.
                resume_unwind(Box::new("assertion cleanup"));
            }
        }
    }
    struct Snapshot {
        value: usize,
        _cleanup: PanicOnDrop,
    }
    impl PartialEq for Snapshot {
        fn eq(&self, other: &Self) -> bool {
            self.value == other.value
        }
    }

    fn exercise(case: usize) {
        if case == 0 {
            let cleanup = PanicOnDrop(true);
            drop(rca_test::check_case_with(
                &None::<usize>,
                Option::as_ref,
                move |_| {
                    std::hint::black_box(&cleanup);
                    Ok::<_, ()>(())
                },
            ));
            return;
        }
        let render_cleanup = PanicOnDrop(case == 3);
        let render = move |snapshot: &Snapshot| {
            std::hint::black_box(&render_cleanup);
            snapshot.value.to_string()
        };
        let message = || Some(String::from("caller-owned comparison message"));
        let expected = || Snapshot {
            value: 0,
            _cleanup: PanicOnDrop(matches!(case, 2 | 5)),
        };
        let actual = || Snapshot {
            value: 1,
            _cleanup: PanicOnDrop(matches!(case, 1 | 4)),
        };
        match case {
            1..=3 => drop(rca_test::check_equal(expected, actual, render, message)),
            4 | 5 => drop(rca_test::check_expected_changes(
                expected,
                actual,
                |_| (),
                render,
                message,
            )),
            6 => {
                let mut captures = 0;
                drop(rca_test::check_change(
                    &mut (),
                    |()| {
                        captures += 1;
                        Snapshot {
                            value: captures,
                            _cleanup: PanicOnDrop(captures == 1),
                        }
                    },
                    |()| Ok::<_, ()>(()),
                    |_| (),
                    render,
                    message,
                ));
            }
            _ => unreachable!("fixed test cases"),
        }
    }

    // The ordinary run proves owned message/excerpt cleanup without backtraces.
    // The explicit environment-qualified run also proves the opaque case error.
    let failure = rca_test::check_case(&None::<usize>, Option::as_ref).unwrap_err();
    let status = failure.backtrace().status();
    if std::env::var_os("RCA_REQUIRE_CAPTURED_BACKTRACE").is_some() {
        assert_eq!(status, BacktraceStatus::Captured);
    }
    println!("assertion_cleanup_backtrace={status:?}");
    drop(failure);

    let mut retained = [(0, 0); 7];
    for (case, result) in retained.iter_mut().enumerate() {
        for _ in 0..2 {
            drop(catch_unwind(AssertUnwindSafe(|| exercise(case))).unwrap_err());
        }
        let (correct_panic, counts) = measured(|| {
            let panic = catch_unwind(AssertUnwindSafe(|| exercise(case))).unwrap_err();
            let correct = panic.downcast_ref::<&str>() == Some(&"assertion cleanup");
            drop(panic);
            correct
        });
        assert!(correct_panic);
        *result = (counts.live_blocks, counts.live_bytes);
    }
    assert_eq!(retained, [(0, 0); 7]);
}

/// Full-lifetime diagnostic retention across rejected capture expectations.
mod capture_validation_cleanup {
    use super::*;
    use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};

    struct PanicOnDrop(bool);

    impl Drop for PanicOnDrop {
        fn drop(&mut self) {
            if self.0 {
                resume_unwind(Box::new("capture validation cleanup"));
            }
        }
    }

    struct Declared;
    struct Unknown;

    impl CaptureKey for Declared {
        type Value = usize;
        type Error = Infallible;
    }

    impl CaptureKey for Unknown {
        type Value = usize;
        type Error = Infallible;
    }

    #[derive(Debug)]
    struct Snapshot {
        value: usize,
        panic_on_drop: bool,
    }

    impl Clone for Snapshot {
        fn clone(&self) -> Self {
            Self {
                value: self.value,
                panic_on_drop: false,
            }
        }
    }

    impl PartialEq for Snapshot {
        fn eq(&self, other: &Self) -> bool {
            self.value == other.value
        }
    }

    impl Drop for Snapshot {
        fn drop(&mut self) {
            if self.panic_on_drop {
                resume_unwind(Box::new("capture validation cleanup"));
            }
        }
    }

    struct Mismatch;

    impl CaptureKey for Mismatch {
        type Value = Snapshot;
        type Error = Infallible;
    }

    struct Increment;

    impl Reducer for Increment {
        type State = usize;
        type Action = ();

        fn reduce(&mut self, state: &mut usize, (): &(), _: &mut ReduceContext<'_>) -> Effect<()> {
            *state += 1;
            Effect::none()
        }
    }

    fn drop_after_checked_failure(value: impl Sized) {
        // Debt has already been checked. Drop during a resumed unwind so the
        // test's second, deliberate teardown failure does not grow libtest's
        // retained output buffer inside the allocation measurement.
        let panic = catch_unwind(AssertUnwindSafe(move || {
            let _value = value;
            resume_unwind(Box::new("checked debt teardown"));
        }))
        .unwrap_err();
        assert_eq!(panic.downcast_ref::<&str>(), Some(&"checked debt teardown"));
    }

    fn exercise(case: usize) {
        if matches!(case, 3 | 4) {
            let calls = AtomicUsize::new(0);
            let mut store = TestStore::builder((), Unchanged)
                .with_capture::<Mismatch>(move |()| {
                    let value = calls.fetch_add(1, Ordering::SeqCst);
                    Ok(Snapshot {
                        value,
                        panic_on_drop: value != 0,
                    })
                })
                .unwrap()
                .build();
            let panic = catch_unwind(AssertUnwindSafe(|| {
                if case == 3 {
                    drop(block_on(store.check_captures(|_| Ok(()))));
                } else {
                    drop(block_on(store.finish()));
                }
            }))
            .unwrap_err();
            assert_eq!(
                panic.downcast_ref::<&str>(),
                Some(&"capture validation cleanup")
            );
            drop(panic);
            assert!(
                block_on(store.finish())
                    .unwrap_err()
                    .is_incomplete_assertion()
            );
            drop_after_checked_failure(store);
            return;
        }
        if case == 5 {
            let builder = TestStore::builder((), Unchanged)
                .with_capture::<Declared>(|()| Ok(0))
                .unwrap();
            let cleanup = PanicOnDrop(true);
            let panic = catch_unwind(AssertUnwindSafe(|| {
                drop(builder.with_capture::<Declared>(move |()| {
                    std::hint::black_box(&cleanup);
                    Ok(0)
                }));
            }))
            .unwrap_err();
            assert_eq!(
                panic.downcast_ref::<&str>(),
                Some(&"capture validation cleanup")
            );
            return;
        }
        let mut store = TestStore::projected_builder(0, Increment, |state| Snapshot {
            value: *state,
            panic_on_drop: *state == 1,
        })
        .with_capture::<Declared>(|state| Ok(*state))
        .unwrap()
        .build();
        let panic = catch_unwind(AssertUnwindSafe(|| {
            if case == 2 {
                // The declared value mismatches before root equality. Its
                // diagnostic must remain owned while the actual root drops.
                drop(block_on(store.send_with((), |value| value.value = 1)));
            } else {
                drop(block_on(store.check_captures(|values| {
                    let cleanup = PanicOnDrop(true);
                    if case == 0 {
                        values.update::<Unknown>(move |_| {
                            std::hint::black_box(&cleanup);
                        })
                    } else {
                        values.set::<Declared>(0)?;
                        values.update::<Declared>(move |_| {
                            std::hint::black_box(&cleanup);
                        })
                    }
                })));
            }
        }))
        .unwrap_err();
        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"capture validation cleanup")
        );
        drop(panic);
        if case == 2 {
            assert!(
                block_on(store.finish())
                    .unwrap_err()
                    .is_incomplete_assertion()
            );
            drop_after_checked_failure(store);
        } else {
            block_on(store.finish()).unwrap();
        }
    }

    #[test]
    fn capture_validation_cleanup_panics_release_owned_failure_diagnostics() {
        let mut retained = [(0, 0); 6];
        for (case, result) in retained.iter_mut().enumerate() {
            exercise(case);
            exercise(case);
            let ((), counts) = measured(|| exercise(case));
            *result = (counts.live_blocks, counts.live_bytes);
        }
        eprintln!("capture_validation_cleanup_retained={retained:?}");
        assert_eq!(retained, [(0, 0); 6]);
    }
}

/// Real public error exits with one owned-input destructor armed per scenario.
mod public_error_cleanup {
    use super::*;
    use futures_util::FutureExt;
    use rca_core::{Feature, FeatureContext, MountIdentity, OptionalMount, Task};
    use rca_test::Exhaustivity;
    use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
    use std::task::Poll;

    const CLEANUP: &str = "public error input cleanup";

    struct PanicOnDrop;

    impl Drop for PanicOnDrop {
        fn drop(&mut self) {
            resume_unwind(Box::new(CLEANUP));
        }
    }

    fn expect_cleanup(operation: impl FnOnce()) {
        let panic = catch_unwind(AssertUnwindSafe(operation)).unwrap_err();
        assert_eq!(panic.downcast_ref::<&str>(), Some(&CLEANUP));
    }

    fn drop_failed_store(value: impl Sized) {
        // Keep all storage in the measurement without adding a second libtest
        // diagnostic buffer allocation for the already-checked Store debt.
        let panic = catch_unwind(AssertUnwindSafe(move || {
            let _value = value;
            resume_unwind(Box::new("checked public error teardown"));
        }))
        .unwrap_err();
        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"checked public error teardown")
        );
    }

    #[derive(Debug, Default)]
    /// Arms one exact historical clone, leaving capture and baseline teardown inert.
    struct CloneControl {
        calls: AtomicUsize,
        panic_at: AtomicUsize,
    }

    #[derive(Debug)]
    struct Snapshot {
        value: usize,
        panic_on_drop: bool,
        clones: Arc<CloneControl>,
    }

    impl Snapshot {
        fn new(value: usize, clones: &Arc<CloneControl>) -> Self {
            Self {
                value,
                panic_on_drop: false,
                clones: Arc::clone(clones),
            }
        }
    }

    impl Clone for Snapshot {
        fn clone(&self) -> Self {
            let call = self.clones.calls.fetch_add(1, Ordering::SeqCst) + 1;
            Self {
                value: self.value,
                panic_on_drop: self.clones.panic_at.load(Ordering::SeqCst) == call,
                clones: Arc::clone(&self.clones),
            }
        }
    }

    impl PartialEq for Snapshot {
        fn eq(&self, other: &Self) -> bool {
            self.value == other.value
        }
    }

    impl Drop for Snapshot {
        fn drop(&mut self) {
            if self.panic_on_drop {
                resume_unwind(Box::new(CLEANUP));
            }
        }
    }

    struct Declared;

    impl CaptureKey for Declared {
        type Value = Snapshot;
        type Error = Infallible;
    }

    struct Transactions {
        root_action: bool,
    }

    impl Feature for Transactions {
        type State = usize;
        type Action = bool;

        fn update(
            &mut self,
            _: &mut usize,
            action: &bool,
            context: &mut FeatureContext<'_, usize, bool>,
        ) {
            if *action {
                let root_action = self.root_action;
                context.task(Task::run(move |context| async move {
                    if root_action {
                        let _ = context.send(false).await;
                    } else {
                        let _ = context.update(|_| ()).await;
                    }
                }));
            }
        }
    }

    fn cancelled_release(root_action: bool, declared_drop: bool) {
        let root_clones = Arc::new(CloneControl::default());
        let capture_clones = Arc::new(CloneControl::default());
        let mut store =
            TestStore::projected_builder(0, Transactions { root_action }, move |state| {
                Snapshot::new(*state, &root_clones)
            })
            .with_capture::<Declared>(move |state| Ok(Snapshot::new(*state, &capture_clones)))
            .unwrap()
            .build();
        let task = block_on(store.send(true)).unwrap();
        let captures = AtomicUsize::new(0);
        expect_cleanup(|| {
            let update = |expected: &mut Snapshot| {
                expected.panic_on_drop = !declared_drop;
                task.cancel().now_or_never().unwrap().unwrap();
            };
            let capture = |values: &mut rca_test::CaptureExpectations<'_>| {
                captures.fetch_add(1, Ordering::SeqCst);
                values.update::<Declared>(|expected| expected.panic_on_drop = declared_drop)
            };
            if root_action {
                drop(block_on(store.receive_with_captures(
                    |_| true,
                    update,
                    capture,
                )));
            } else {
                drop(block_on(
                    store.receive_update_with_captures(update, capture),
                ));
            }
        });
        assert_eq!(captures.load(Ordering::SeqCst), 1);
        let boundary = store.pending_boundary().unwrap();
        block_on(store.receive_terminal(boundary, |error| {
            error.reason() == rca_core::TaskAccessReason::Cancelled
        }))
        .unwrap();
        block_on(store.finish()).unwrap();
    }

    fn empty_skip(declared_drop: bool) {
        let root_clones = Arc::new(CloneControl::default());
        let capture_clones = Arc::new(CloneControl::default());
        let selected = Arc::clone(if declared_drop {
            &capture_clones
        } else {
            &root_clones
        });
        let mut store =
            TestStore::projected_builder(0, Transactions { root_action: false }, move |state| {
                Snapshot::new(*state, &root_clones)
            })
            .with_capture::<Declared>(move |state| Ok(Snapshot::new(*state, &capture_clones)))
            .unwrap()
            .with_exhaustivity(Exhaustivity::NonExhaustive)
            .build();
        // Declared preflight consumes one valid clone before the speculative
        // skip clone. Root history is cloned only for that speculative skip.
        selected
            .panic_at
            .store(if declared_drop { 2 } else { 1 }, Ordering::SeqCst);
        expect_cleanup(|| drop(block_on(store.skip_received())));
        assert_eq!(
            selected.calls.load(Ordering::SeqCst),
            if declared_drop { 2 } else { 1 }
        );
        selected.panic_at.store(0, Ordering::SeqCst);
        block_on(store.finish()).unwrap();
    }

    struct Count;

    impl CaptureKey for Count {
        type Value = usize;
        type Error = Infallible;
    }

    fn incomplete_capture() {
        let mut store = TestStore::builder((), Unchanged)
            .with_capture::<Count>(|()| Ok(0))
            .unwrap()
            .build();
        assert!(
            block_on(store.check_captures(|values| values.set::<Count>(1)))
                .unwrap_err()
                .is_capture_mismatch()
        );
        let calls = AtomicUsize::new(0);
        let observed = &calls;
        let cleanup = PanicOnDrop;
        expect_cleanup(|| {
            drop(block_on(store.check_captures(move |_| {
                std::hint::black_box(&cleanup);
                observed.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })));
        });
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(
            block_on(store.finish())
                .unwrap_err()
                .is_incomplete_assertion()
        );
        drop_failed_store(store);
    }

    fn missing_issue() {
        let mut store = TestStore::new((), Unchanged);
        let calls = AtomicUsize::new(0);
        let observed = &calls;
        let cleanup = PanicOnDrop;
        expect_cleanup(|| {
            drop(block_on(store.expect_issue(move |_| {
                std::hint::black_box(&cleanup);
                observed.fetch_add(1, Ordering::SeqCst);
                true
            })));
        });
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        block_on(store.finish()).unwrap();
    }

    struct OptionalRoot;
    struct Child;

    impl Feature for OptionalRoot {
        type State = Option<()>;
        type Action = ();
        fn update(
            &mut self,
            _: &mut Option<()>,
            (): &(),
            _: &mut FeatureContext<'_, Option<()>, ()>,
        ) {
        }
    }

    impl Feature for Child {
        type State = ();
        type Action = ();
        fn update(&mut self, (): &mut (), (): &(), _: &mut FeatureContext<'_, (), ()>) {}
    }

    fn missing_output_terminal() {
        let mount = OptionalMount::new(
            Option::as_ref,
            Option::as_mut,
            |state: &Option<()>| {
                state.map(|()| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            |()| Child,
        );
        let (builder, site) = TestStore::builder(None, OptionalRoot).mount_with_site(mount);
        let (builder, output) = builder.output::<()>(&site, |_| {});
        let mut store = builder.build();
        // Installing the mount itself is a structural transaction even while
        // its optional child is absent. Acknowledge it before testing no output.
        drop(block_on(store.receive_structural(|_| {})).unwrap());
        assert!(store.pending_boundary().is_none());
        let calls = AtomicUsize::new(0);
        let observed = &calls;
        let cleanup = PanicOnDrop;
        expect_cleanup(|| {
            drop(block_on(store.receive_output_terminal(
                &output,
                move |_| {
                    std::hint::black_box(&cleanup);
                    observed.fetch_add(1, Ordering::SeqCst);
                    true
                },
            )));
        });
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        block_on(store.finish()).unwrap();
    }

    struct BudgetWork {
        gate: Option<rca_test::ControlledGate>,
        polls: Arc<AtomicUsize>,
        with_boundary: bool,
    }

    impl Feature for BudgetWork {
        type State = ();
        type Action = ();

        fn update(&mut self, (): &mut (), (): &(), context: &mut FeatureContext<'_, (), ()>) {
            let gate = self.gate.take().unwrap();
            let polls = Arc::clone(&self.polls);
            context.task(Task::run(move |_| async move {
                gate.await.unwrap();
                std::future::poll_fn(move |context| {
                    if polls.fetch_add(1, Ordering::SeqCst) + 1 == 8 {
                        Poll::Ready(())
                    } else {
                        context.waker().wake_by_ref();
                        Poll::Pending
                    }
                })
                .await;
            }));
            if self.with_boundary {
                context.task(Task::run(|context| async move {
                    let _ = context.update(|()| ()).await;
                }));
            }
        }
    }

    #[derive(Clone, Copy)]
    enum BudgetTarget {
        State,
        Terminal,
        Receive,
    }

    fn budget_failure(target: BudgetTarget) {
        let terminal = matches!(target, BudgetTarget::Terminal);
        let (control, gate) = rca_test::controlled_gate();
        let polls = Arc::new(AtomicUsize::new(0));
        let mut store = TestStore::builder(
            (),
            BudgetWork {
                gate: Some(gate),
                polls: Arc::clone(&polls),
                with_boundary: terminal,
            },
        )
        .with_step_budget(7)
        .build();
        let task = block_on(store.send(())).unwrap();
        block_on(control.wait_until_waiting()).unwrap();
        let boundary = store.pending_boundary();
        assert_eq!(boundary.is_some(), terminal);
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&calls);
        let cleanup = PanicOnDrop;
        control.release().unwrap();
        expect_cleanup(|| {
            if matches!(target, BudgetTarget::Receive) {
                drop(block_on(store.receive_with_captures(
                    move |()| {
                        std::hint::black_box(&cleanup);
                        observed.fetch_add(1, Ordering::SeqCst);
                        true
                    },
                    |()| {},
                    |_| Ok(()),
                )));
            } else if terminal {
                // This is a real selected update token. Readiness fails before
                // terminal matching can inspect or reject its current kind.
                drop(block_on(store.receive_terminal(
                    boundary.unwrap(),
                    move |_| {
                        std::hint::black_box(&cleanup);
                        observed.fetch_add(1, Ordering::SeqCst);
                        true
                    },
                )));
            } else {
                drop(block_on(store.assert_state(move |()| {
                    std::hint::black_box(&cleanup);
                    observed.fetch_add(1, Ordering::SeqCst);
                })));
            }
        });
        assert_eq!(polls.load(Ordering::SeqCst), 7);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(store.pending_boundary().is_some(), terminal);
        drop_failed_store(store);
        drop(task);
    }

    fn absent_receive(root_action: bool, captures_drop: bool) {
        let mut store = TestStore::new((), Unchanged);
        let calls = AtomicUsize::new(0);
        let update_calls = &calls;
        let capture_calls = &calls;
        let matcher_calls = &calls;
        let update_cleanup = (!captures_drop).then(|| PanicOnDrop);
        let capture_cleanup = captures_drop.then(|| PanicOnDrop);
        expect_cleanup(|| {
            let update = move |(): &mut ()| {
                std::hint::black_box(&update_cleanup);
                update_calls.fetch_add(1, Ordering::SeqCst);
            };
            let captures = move |_: &mut rca_test::CaptureExpectations<'_>| {
                std::hint::black_box(&capture_cleanup);
                capture_calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            };
            if root_action {
                drop(block_on(store.receive_with_captures(
                    |()| {
                        matcher_calls.fetch_add(1, Ordering::SeqCst);
                        true
                    },
                    update,
                    captures,
                )));
            } else {
                drop(block_on(
                    store.receive_update_with_captures(update, captures),
                ));
            }
        });
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        block_on(store.finish()).unwrap();
    }

    #[test]
    fn public_error_exits_release_diagnostics_before_owned_input_cleanup() {
        let cases: [(&str, fn()); 16] = [
            ("root cancellation/root snapshot", || {
                cancelled_release(true, false);
            }),
            ("root cancellation/declared value", || {
                cancelled_release(true, true);
            }),
            ("typed cancellation/root snapshot", || {
                cancelled_release(false, false);
            }),
            ("typed cancellation/declared value", || {
                cancelled_release(false, true);
            }),
            ("empty skip/root snapshot", || empty_skip(false)),
            ("empty skip/declared value", || empty_skip(true)),
            ("incomplete capture callback", incomplete_capture),
            ("empty issue matcher", missing_issue),
            ("absent output terminal matcher", missing_output_terminal),
            ("state readiness callback", || {
                budget_failure(BudgetTarget::State);
            }),
            ("terminal readiness matcher", || {
                budget_failure(BudgetTarget::Terminal);
            }),
            ("root receive readiness matcher", || {
                budget_failure(BudgetTarget::Receive);
            }),
            ("root absent receive/update", || absent_receive(true, false)),
            ("root absent receive/captures", || {
                absent_receive(true, true);
            }),
            ("typed absent receive/update", || {
                absent_receive(false, false);
            }),
            ("typed absent receive/captures", || {
                absent_receive(false, true);
            }),
        ];
        let mut retained = [(0, 0); 16];
        for ((name, exercise), result) in cases.into_iter().zip(&mut retained) {
            exercise();
            exercise();
            let ((), counts) = measured(exercise);
            *result = (counts.live_blocks, counts.live_bytes);
            eprintln!("public_error_cleanup {name}: {result:?}");
        }
        assert_eq!(retained, [(0, 0); 16]);
    }
}

#[test]
fn successful_issue_expectation_adds_no_allocations_beyond_collection() {
    let prefilled = || {
        let collector = rca_core::IssueCollector::new();
        let reporter = collector.reporter();
        for code in ["first", "second", "third"] {
            reporter
                .report(rca_core::Issue::new(code, format!("owned {code}")))
                .unwrap();
        }
        collector
    };
    let collector = prefilled();
    let (issues, collection) = measured(|| collector.finish());
    assert_eq!(issues.len(), 3);
    drop(issues);
    let collector = prefilled();
    let (outcome, counts) = measured(|| {
        rca_test::expect_issues(
            collector,
            rca_test::IssueExpectationMode::Strict,
            |_| true,
            |_| Ok::<_, ()>(()),
        )
    });
    assert_eq!(
        (counts.calls, counts.bytes, counts.reallocations),
        (collection.calls, collection.bytes, collection.reallocations),
    );
    assert_eq!(outcome.expectation.unwrap().len(), 3);
}

#[derive(Default)]
/// Independent semantic counters; allocation totals alone cannot prove callback counts.
struct Calls {
    clones: AtomicUsize,
    equalities: AtomicUsize,
    captures: AtomicUsize,
}

/// Uses the same comparison contract with either empty or independently owned storage.
struct Value {
    scalar: u64,
    owned: Vec<u64>,
    calls: Arc<Calls>,
}
impl Clone for Value {
    fn clone(&self) -> Self {
        self.calls.clones.fetch_add(1, Ordering::SeqCst);
        Self {
            scalar: self.scalar,
            owned: self.owned.clone(),
            calls: Arc::clone(&self.calls),
        }
    }
}
impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        self.calls.equalities.fetch_add(1, Ordering::SeqCst);
        self.scalar == other.scalar && self.owned == other.owned
    }
}
struct Key;
impl CaptureKey for Key {
    type Value = Value;
    type Error = Infallible;
}
struct Unchanged;
impl Reducer for Unchanged {
    type State = ();
    type Action = ();
    fn reduce(&mut self, (): &mut (), (): &(), _: &mut ReduceContext<'_>) -> Effect<()> {
        Effect::none()
    }
}

struct OpaqueCause(Cell<u64>);
struct CauseKey;
impl CaptureKey for CauseKey {
    type Value = ();
    type Error = OpaqueCause;
}
struct OtherCauseKey;
impl CaptureKey for OtherCauseKey {
    type Value = ();
    type Error = OpaqueCause;
}

#[test]
fn repeated_wrong_key_recovery_preserves_cause_without_allocating() {
    let builder = TestStore::builder((), Unchanged);
    let (result, construction) =
        measured(|| builder.with_capture::<CauseKey>(|()| Err(OpaqueCause(Cell::new(7)))));
    let Err(failure) = result else {
        panic!("capture failure must reject declaration");
    };
    let origin = failure.origin();
    let (failure, misses) = measured(|| {
        let mut failure = failure;
        for _ in 0..128 {
            failure = match failure.into_capture_error::<OtherCauseKey>() {
                Ok(_) => panic!("same cause type is not the declared key"),
                Err(error) => error,
            };
        }
        failure
    });
    assert_eq!(failure.origin(), origin);
    assert_eq!(
        (misses.calls, misses.bytes, misses.reallocations),
        (0, 0, 0)
    );
    let (cause, recovery) = measured(|| failure.into_capture_error::<CauseKey>().unwrap());
    assert_eq!(cause.0.get(), 7);
    assert_eq!(
        (recovery.calls, recovery.bytes, recovery.reallocations),
        (0, 0, 0)
    );
    println!(
        "capture_error_cost construction={construction:?} misses={misses:?} recovery={recovery:?}"
    );
}

#[test]
fn boundaries_report_allocations_and_keep_one_clone_capture_and_equality() {
    for owned in [false, true] {
        for mode in ["omitted", "set", "update"] {
            let calls = Arc::new(Calls::default());
            let capture_calls = Arc::clone(&calls);
            let mut store = TestStore::builder((), Unchanged)
                .with_capture::<Key>(move |()| {
                    capture_calls.captures.fetch_add(1, Ordering::SeqCst);
                    Ok(Value {
                        scalar: 7,
                        owned: if owned { vec![3; 64] } else { Vec::new() },
                        calls: Arc::clone(&capture_calls),
                    })
                })
                .unwrap()
                .build();
            // Settle actor/executor startup and one full boundary before measurement.
            block_on(store.check_captures(|_| Ok(()))).unwrap();
            let replacement = Value {
                scalar: 7,
                owned: if owned { vec![3; 64] } else { Vec::new() },
                calls: Arc::clone(&calls),
            };
            calls.clones.store(0, Ordering::SeqCst);
            calls.equalities.store(0, Ordering::SeqCst);
            calls.captures.store(0, Ordering::SeqCst);
            let (result, allocations) = measured(|| {
                block_on(store.check_captures(|values| match mode {
                    "set" => values.set::<Key>(replacement),
                    "update" => values.update::<Key>(|value| {
                        value.scalar = 7;
                    }),
                    _ => Ok(()),
                }))
            });
            result.unwrap();
            assert_eq!(calls.clones.load(Ordering::SeqCst), 1);
            assert_eq!(calls.equalities.load(Ordering::SeqCst), 1);
            assert_eq!(calls.captures.load(Ordering::SeqCst), 1);
            println!(
                "capture_cost owned={owned} mode={mode} {allocations:?}; clone=1 capture=1 equality=1"
            );
            block_on(store.finish()).unwrap();
        }
    }
}

#[cfg(feature = "local")]
/// Mirrors the real public boundary and typed-cause checks with non-Send Rc values.
mod local_boundaries {
    use super::{Calls, measured};
    use futures_executor::block_on;
    use rca_core::ReduceContext;
    use rca_core::local::{Effect, Reducer};
    use rca_test::local::{CaptureKey, TestStore};
    use std::cell::Cell;
    use std::rc::Rc;
    use std::sync::atomic::Ordering;

    struct Value {
        scalar: u64,
        owned: Vec<u64>,
        calls: Rc<Calls>,
    }
    impl Clone for Value {
        fn clone(&self) -> Self {
            self.calls.clones.fetch_add(1, Ordering::SeqCst);
            Self {
                scalar: self.scalar,
                owned: self.owned.clone(),
                calls: Rc::clone(&self.calls),
            }
        }
    }
    impl PartialEq for Value {
        fn eq(&self, other: &Self) -> bool {
            self.calls.equalities.fetch_add(1, Ordering::SeqCst);
            self.scalar == other.scalar && self.owned == other.owned
        }
    }
    struct Key;
    impl CaptureKey for Key {
        type Value = Value;
        type Error = Rc<Cell<usize>>;
    }
    struct Other;
    impl CaptureKey for Other {
        type Value = Value;
        type Error = Rc<Cell<usize>>;
    }
    struct Unchanged;
    impl Reducer for Unchanged {
        type State = ();
        type Action = ();
        fn reduce(&mut self, (): &mut (), (): &(), _: &mut ReduceContext<'_>) -> Effect<()> {
            Effect::none()
        }
    }

    #[test]
    fn public_rc_boundaries_keep_one_clone_capture_equality_and_report_costs() {
        for owned in [false, true] {
            for mode in ["omitted", "set", "update"] {
                let calls = Rc::new(Calls::default());
                let capture_calls = Rc::clone(&calls);
                let mut store = TestStore::builder((), Unchanged)
                    .with_capture::<Key>(move |()| {
                        capture_calls.captures.fetch_add(1, Ordering::SeqCst);
                        Ok(Value {
                            scalar: 7,
                            owned: if owned { vec![3; 64] } else { Vec::new() },
                            calls: Rc::clone(&capture_calls),
                        })
                    })
                    .unwrap()
                    .build();
                block_on(store.check_captures(|_| Ok(()))).unwrap();
                let replacement = Value {
                    scalar: 7,
                    owned: if owned { vec![3; 64] } else { Vec::new() },
                    calls: Rc::clone(&calls),
                };
                calls.clones.store(0, Ordering::SeqCst);
                calls.equalities.store(0, Ordering::SeqCst);
                calls.captures.store(0, Ordering::SeqCst);
                let (result, allocations) = measured(|| {
                    block_on(store.check_captures(|values| match mode {
                        "set" => values.set::<Key>(replacement),
                        "update" => values.update::<Key>(|value| value.scalar = 7),
                        _ => Ok(()),
                    }))
                });
                result.unwrap();
                assert_eq!(calls.clones.load(Ordering::SeqCst), 1);
                assert_eq!(calls.equalities.load(Ordering::SeqCst), 1);
                assert_eq!(calls.captures.load(Ordering::SeqCst), 1);
                println!(
                    "local_capture_cost owned={owned} mode={mode} {allocations:?}; clone=1 capture=1 equality=1"
                );
                block_on(store.finish()).unwrap();
            }
        }
    }

    #[test]
    fn public_rc_error_misses_and_recovery_allocate_nothing() {
        let cause = Rc::new(Cell::new(7));
        let captured = Rc::clone(&cause);
        let builder = TestStore::builder((), Unchanged);
        let (result, construction) =
            measured(|| builder.with_capture::<Key>(move |()| Err(Rc::clone(&captured))));
        let error = result.expect_err("initial capture failure");
        let (error, misses) = measured(|| {
            let mut error = error;
            for _ in 0..128 {
                error = error
                    .into_capture_error::<Other>()
                    .expect_err("wrong nominal key");
            }
            error
        });
        let (recovered, recovery) = measured(|| error.into_capture_error::<Key>().unwrap());
        assert!(Rc::ptr_eq(&cause, &recovered));
        assert_eq!(
            (misses.calls, misses.bytes, misses.reallocations),
            (0, 0, 0)
        );
        assert_eq!(
            (recovery.calls, recovery.bytes, recovery.reallocations),
            (0, 0, 0)
        );
        println!(
            "local_capture_error_cost construction={construction:?} misses={misses:?} recovery={recovery:?}"
        );
    }
}

struct ScalingKey<const INDEX: usize>;
impl<const INDEX: usize> CaptureKey for ScalingKey<INDEX> {
    type Value = Value;
    type Error = Infallible;
}

#[derive(Clone, Copy, Debug)]
enum ScalingMode {
    Omitted,
    Set,
    Update,
}

fn scaling_value(calls: &Arc<Calls>, owned: bool, index: usize, generation: usize) -> Value {
    let scalar = (index * 100 + generation) as u64;
    Value {
        scalar,
        owned: if owned { vec![scalar; 64] } else { Vec::new() },
        calls: Arc::clone(calls),
    }
}

fn scaling_declaration<const INDEX: usize>(
    builder: rca_test::TestStoreBuilder<(), (), Unchanged>,
    calls: &Arc<Calls>,
    generation: &Arc<AtomicUsize>,
    owned: bool,
) -> rca_test::TestStoreBuilder<(), (), Unchanged> {
    let calls = Arc::clone(calls);
    let generation = Arc::clone(generation);
    builder
        .with_capture::<ScalingKey<INDEX>>(move |()| {
            calls.captures.fetch_add(1, Ordering::SeqCst);
            Ok(scaling_value(
                &calls,
                owned,
                INDEX,
                generation.load(Ordering::SeqCst),
            ))
        })
        .unwrap()
}

fn scaling_expectation<const INDEX: usize>(
    values: &mut rca_test::CaptureExpectations<'_>,
    mode: ScalingMode,
    replacement: Value,
    updates: &mut usize,
) -> Result<(), rca_test::TestStoreError> {
    match mode {
        ScalingMode::Omitted => Ok(()),
        ScalingMode::Set => values.set::<ScalingKey<INDEX>>(replacement),
        ScalingMode::Update => values.update::<ScalingKey<INDEX>>(|value| {
            *updates += 1;
            value.scalar += 1;
            for element in &mut value.owned {
                *element += 1;
            }
        }),
    }
}

/// Reports fixed test inputs while asserting per-key work, not allocator-specific totals.
fn scaling_case(declarations: usize, boundaries: usize, mode: ScalingMode, owned: bool) {
    let calls: [Arc<Calls>; 4] = std::array::from_fn(|_| Arc::new(Calls::default()));
    let generation = Arc::new(AtomicUsize::new(0));
    let mut builder = TestStore::builder((), Unchanged);
    if declarations >= 1 {
        builder = scaling_declaration::<0>(builder, &calls[0], &generation, owned);
    }
    if declarations == 4 {
        builder = scaling_declaration::<1>(builder, &calls[1], &generation, owned);
        builder = scaling_declaration::<2>(builder, &calls[2], &generation, owned);
        builder = scaling_declaration::<3>(builder, &calls[3], &generation, owned);
    }
    let mut store = builder.build();
    block_on(store.check_captures(|_| Ok(()))).unwrap();
    for key in &calls {
        key.clones.store(0, Ordering::SeqCst);
        key.captures.store(0, Ordering::SeqCst);
        key.equalities.store(0, Ordering::SeqCst);
    }
    let mut expectations = 0;
    let mut updates = 0;
    let mut total = Allocations::default();
    let mut first = None;
    let mut uniform = true;
    for boundary in 1..=boundaries {
        let next = if matches!(mode, ScalingMode::Omitted) {
            0
        } else {
            boundary
        };
        generation.store(next, Ordering::SeqCst);
        // Independently prepare replacements outside counting. Updates edit the
        // cloned prior boundary in place; capture reads the external generation.
        let [a, b, c, d] =
            std::array::from_fn(|index| scaling_value(&calls[index], owned, index, next));
        let (result, allocations) = measured(|| {
            block_on(store.check_captures(|values| {
                expectations += 1;
                if declarations >= 1 {
                    scaling_expectation::<0>(values, mode, a, &mut updates)?;
                }
                if declarations == 4 {
                    scaling_expectation::<1>(values, mode, b, &mut updates)?;
                    scaling_expectation::<2>(values, mode, c, &mut updates)?;
                    scaling_expectation::<3>(values, mode, d, &mut updates)?;
                }
                Ok(())
            }))
        });
        result.unwrap();
        let unit = first.get_or_insert(allocations);
        uniform &= (unit.calls, unit.bytes, unit.reallocations)
            == (
                allocations.calls,
                allocations.bytes,
                allocations.reallocations,
            );
        total.calls += allocations.calls;
        total.bytes += allocations.bytes;
        total.reallocations += allocations.reallocations;
    }
    // Independent witnesses distinguish each key, including inactive controls.
    // Declaration capture/setup is excluded: successful check_captures prepares
    // one historical clone, captures once, and compares once for every key.
    for (index, key) in calls.iter().enumerate() {
        let expected = if index < declarations { boundaries } else { 0 };
        assert_eq!(key.clones.load(Ordering::SeqCst), expected, "key {index}");
        assert_eq!(key.captures.load(Ordering::SeqCst), expected, "key {index}");
        assert_eq!(
            key.equalities.load(Ordering::SeqCst),
            expected,
            "key {index}"
        );
    }
    assert_eq!(expectations, boundaries);
    let expected_updates = if matches!(mode, ScalingMode::Update) {
        declarations * boundaries
    } else {
        0
    };
    assert_eq!(updates, expected_updates);
    println!(
        "capture_scaling declarations={declarations} boundaries={boundaries} mode={mode:?} owned={owned} total={total:?} first={first:?} uniform={uniform} expectation={expectations} update={updates} clone_capture_equality_each={}",
        declarations * boundaries
    );
    block_on(store.finish()).unwrap();
}

#[test]
fn declared_captures_scale_over_repeated_boundaries() {
    for declarations in [0, 1, 4] {
        for boundaries in [1, 32] {
            for mode in [ScalingMode::Omitted, ScalingMode::Set, ScalingMode::Update] {
                for owned in [false, true] {
                    scaling_case(declarations, boundaries, mode, owned);
                }
            }
        }
    }
}

#[test]
fn snapshot_unused_redactor_panics_release_owned_failure_paths() {
    use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};

    use rca_test::snapshots::{
        ArtifactKind, RecordMode, ReferenceWriter, SnapshotNamespace, SnapshotPath, SnapshotRequest,
    };

    struct PanicOnDrop;
    impl Drop for PanicOnDrop {
        fn drop(&mut self) {
            resume_unwind(Box::new("snapshot redactor cleanup"));
        }
    }

    struct Writer;
    impl ReferenceWriter for Writer {
        type Error = Infallible;

        fn write(
            &mut self,
            _: &SnapshotPath,
            _: ArtifactKind,
            _: Option<&[u8]>,
            _: &[u8],
        ) -> Result<(), Self::Error> {
            panic!("these comparisons must not invoke a writer");
        }
    }

    let exercise = |(collision, mode, reference)| {
        let mut namespace = SnapshotNamespace::new("cleanup_suite").unwrap();
        if collision {
            namespace.compare("case", Some(b"old"), b"old").unwrap();
        }
        let cleanup = PanicOnDrop;
        drop(namespace.check(
            SnapshotRequest {
                name: "case",
                reference,
                actual: b"new",
                mode,
            },
            move |_| -> Result<Vec<u8>, ()> {
                std::hint::black_box(&cleanup);
                panic!("these comparisons must not invoke redaction");
            },
            None::<&mut Writer>,
        ));
    };

    let cases = [
        (false, RecordMode::CompareOnly, Some(b"old".as_slice())),
        (true, RecordMode::All, Some(b"old".as_slice())),
        (false, RecordMode::Missing, None),
    ];
    let mut retained = [(0, 0); 3];
    for (scenario, result) in cases.into_iter().zip(&mut retained) {
        for _ in 0..2 {
            drop(catch_unwind(AssertUnwindSafe(|| exercise(scenario))).unwrap_err());
        }
        let (correct_panic, counts) = measured(|| {
            let panic = catch_unwind(AssertUnwindSafe(|| exercise(scenario))).unwrap_err();
            let correct = panic.downcast_ref::<&str>() == Some(&"snapshot redactor cleanup");
            drop(panic);
            correct
        });
        assert!(correct_panic);
        *result = (counts.live_blocks, counts.live_bytes);
    }
    assert_eq!(retained, [(0, 0); 3]);
}
