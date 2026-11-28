//! Measures the Tokio adapter's own construction and storage.

use super::{TokioDriver, TokioRuntime};
use rca_core::{MonotonicInstant, RuntimeDriver};

#[path = "../support/allocation_test.rs"]
pub(crate) mod allocation_test;

pub(crate) const CASES: &[(&str, fn())] = &[(
    "tests::allocations::driver_origin_needs_no_separate_allocation",
    driver_origin_needs_no_separate_allocation,
)];

#[test]
fn allocation_host_retains_its_exact_case() {
    assert_eq!(
        CASES.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
        ["tests::allocations::driver_origin_needs_no_separate_allocation"]
    );
}

/// Checks adapter storage without a concurrent test-runner thread.
///
/// # Panics
/// Panics if construction, cloning, timer storage or release exceeds its budget.
pub(crate) fn driver_origin_needs_no_separate_allocation() {
    if allocation_test::run_in_child(
        "tests::allocations::driver_origin_needs_no_separate_allocation",
    ) {
        return;
    }
    let host = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("build Tokio runtime");
    let _entered = host.enter();
    // Initialise Tokio's thread-local state before measuring the adapter.
    drop(TokioRuntime::current().expect("active Tokio runtime"));

    let profiler = dhat::Profiler::builder().testing().build();
    let before = dhat::HeapStats::get();
    let driver = TokioDriver::current().expect("active Tokio runtime");
    let constructed = dhat::HeapStats::get();
    let clone = driver.clone();
    let cloned = dhat::HeapStats::get();
    let sleep = driver.sleep_until(MonotonicInstant::MAX);
    let sleep_size = size_of_val(sleep.as_ref().get_ref());
    let boxed = dhat::HeapStats::get();
    // Registration and host teardown belong to the conformance tests.
    // This budget covers the adapter's own construction and release.
    drop(sleep);
    drop(clone);
    drop(driver);
    let before_runtime = dhat::HeapStats::get();
    let runtime = TokioRuntime::current().expect("active Tokio runtime");
    let wrapped = dhat::HeapStats::get();
    drop(runtime);
    let released = dhat::HeapStats::get();
    drop(profiler);

    let delta = |before: &dhat::HeapStats, after: &dhat::HeapStats| {
        (
            after.total_blocks - before.total_blocks,
            after.total_bytes - before.total_bytes,
        )
    };
    let construction = delta(&before, &constructed);
    let cloning = delta(&constructed, &cloned);
    let boxing = delta(&cloned, &boxed);
    let wrapping = delta(&before_runtime, &wrapped);
    println!(
        "(blocks, bytes): driver={construction:?}, clone={cloning:?}, \
         sleep={boxing:?}, runtime={wrapping:?}; \
         driver_size={}, sleep_size={sleep_size}",
        size_of::<TokioDriver>(),
    );
    assert_eq!(construction, (0, 0));
    assert_eq!(cloning, (0, 0));
    assert_eq!(boxing, (1, u64::try_from(sleep_size).unwrap()));
    assert_eq!(wrapping.0, 1);
    assert_eq!((released.curr_blocks, released.curr_bytes), (0, 0));
}
