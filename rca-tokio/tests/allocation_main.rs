//! Runs adapter allocation checks without a concurrent test-runner thread.

#[cfg(all(not(loom), not(miri)))]
#[expect(
    dead_code,
    unused_imports,
    reason = "The allocation host uses the private driver without running ordinary adapter tests."
)]
#[path = "../src/lib.rs"]
mod adapter;

#[cfg(all(not(loom), not(miri)))]
use adapter::allocation_tests::allocation_test;

#[cfg(all(not(loom), not(miri)))]
#[path = "allocations/drain.rs"]
mod drain;

#[cfg(all(not(loom), not(miri)))]
#[global_allocator]
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

#[cfg(all(not(loom), not(miri)))]
const CASES: &[(&str, fn())] = &[
    (
        "tests::allocations::driver_origin_needs_no_separate_allocation",
        adapter::allocation_tests::CASES[0].1,
    ),
    (
        "owned_deadline_draining_keeps_allocation_work_linear",
        drain::owned_deadline_draining_keeps_allocation_work_linear,
    ),
];

#[cfg(all(not(loom), not(miri)))]
include!("support/allocation_runner.rs");

#[cfg(all(not(loom), not(miri)))]
fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let child = std::env::var_os("RCA_ALLOCATION_TEST_CHILD");
    match run(&args, false, child.as_deref(), false) {
        Ok(true) => std::process::ExitCode::SUCCESS,
        Ok(false) => std::process::ExitCode::FAILURE,
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(any(loom, miri))]
fn main() {}
