#![cfg(all(feature = "allocation-measurement", feature = "local"))]
//! Measures expired clock waits without charging provider construction.

use std::future::poll_fn;
use std::hint::black_box;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};
use std::thread::{self, ThreadId};
use std::time::Duration;

use rca_core::{Clock, ClockCapabilities, MonotonicInstant, RuntimeFuture};
use rca_core::{ClockDriver, local};

#[global_allocator]
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

#[derive(Debug)]
/// Rejects provider waits because both measured deadlines have elapsed.
struct SharedClock;

impl ClockDriver for SharedClock {
    fn now(&self) -> MonotonicInstant {
        MonotonicInstant::from_elapsed(Duration::from_secs(1))
    }

    fn capabilities(&self) -> ClockCapabilities {
        ClockCapabilities {
            minimum_tolerance: Some(Duration::ZERO),
            ..ClockCapabilities::default()
        }
    }

    fn sleep_until(&self, _: MonotonicInstant) -> RuntimeFuture {
        panic!("elapsed deadlines must not create provider waits")
    }
}

#[derive(Debug)]
/// Enforces owner-thread access for the Local clock lane.
struct LocalClock(ThreadId);

impl local::ClockDriver for LocalClock {
    fn now(&self) -> MonotonicInstant {
        assert_eq!(self.0, thread::current().id());
        SharedClock.now()
    }

    fn capabilities(&self) -> ClockCapabilities {
        assert_eq!(self.0, thread::current().id());
        SharedClock.capabilities()
    }

    fn sleep_until(&self, _: MonotonicInstant) -> local::RuntimeFuture {
        panic!("elapsed deadlines must not create local provider waits")
    }
}

/// Separates construction-only cost from the first-poll cost.
fn complete_or_drop<F: Future<Output = ()> + ?Sized>(mut future: Pin<Box<F>>, poll: bool) {
    if poll {
        assert_eq!(
            future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop())),
            Poll::Ready(())
        );
    }
    drop(black_box(future));
}

#[cfg(feature = "test-util")]
/// Warms diagnostic reporting before the per-wait windows.
fn warmed_forbidden_clock(
    baseline: MonotonicInstant,
    reports: &std::sync::Arc<std::sync::atomic::AtomicUsize>,
) -> rca_core::ForbiddenClock {
    let reported = std::sync::Arc::clone(reports);
    let forbidden = rca_core::ForbiddenClock::new(
        "allocation",
        baseline,
        rca_core::IssueReporter::new(move |issue| {
            assert_eq!(issue.code, "clock.forbidden.sleep_until");
            assert_eq!(
                issue.message,
                "Forbidden clock 'allocation': clock.forbidden.sleep_until",
            );
            reported.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }),
    );
    // Initialise the enabled reporter path outside the per-wait measurement.
    drop(forbidden.sleep_until(baseline.saturating_add(Duration::from_secs(1))));
    reports.store(0, std::sync::atomic::Ordering::Relaxed);
    forbidden
}

#[cfg(feature = "test-util")]
/// Measures elapsed and pending forbidden waits with identical report counts.
fn forbidden_wait_costs(
    forbidden: &rca_core::ForbiddenClock,
    deadlines: [MonotonicInstant; 2],
) -> [[(u64, u64); 2]; 2] {
    let mut costs = [[(0, 0); 2]; 2];
    for (index, poll) in [false, true].into_iter().enumerate() {
        for (lane, counts) in costs.iter_mut().enumerate() {
            let before = dhat::HeapStats::get();
            for deadline in deadlines {
                if lane == 0 {
                    complete_or_drop(forbidden.sleep_until(deadline), poll);
                } else {
                    // The pending branch has the same diagnostic but a ZST wait.
                    let mut wait =
                        forbidden.sleep_until(deadlines[1].saturating_add(Duration::from_secs(1)));
                    if poll {
                        assert_eq!(
                            wait.as_mut().poll(&mut Context::from_waker(Waker::noop())),
                            Poll::Pending,
                        );
                    }
                    drop(black_box(wait));
                }
            }
            let after = dhat::HeapStats::get();
            counts[index] = (
                after.total_blocks - before.total_blocks,
                after.total_bytes - before.total_bytes,
            );
        }
    }
    costs
}

/// Checks that elapsed tolerant waits require no heap storage.
fn elapsed_tolerant_waits_need_no_heap_storage() {
    let shared = Clock::new(SharedClock);
    let local = local::Clock::new(LocalClock(thread::current().id()));
    let deadlines = [MonotonicInstant::ZERO, shared.now()];
    // Construct providers and initialise the local thread ID before profiling.
    assert_eq!(local.now(), shared.now());
    #[cfg(feature = "test-util")]
    let reports = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    #[cfg(feature = "test-util")]
    let forbidden = warmed_forbidden_clock(deadlines[1], &reports);
    let profiler = dhat::Profiler::builder().testing().build();
    let mut costs = [[(0, 0); 2]; 3];
    for (index, poll) in [false, true].into_iter().enumerate() {
        for (lane, counts) in costs.iter_mut().enumerate() {
            let before = dhat::HeapStats::get();
            for deadline in deadlines {
                match lane {
                    0 => complete_or_drop(
                        shared
                            .sleep_until_with_tolerance(deadline, Duration::ZERO)
                            .unwrap(),
                        poll,
                    ),
                    1 => complete_or_drop(
                        local
                            .sleep_until_with_tolerance(deadline, Duration::ZERO)
                            .unwrap(),
                        poll,
                    ),
                    _ => {
                        let candidate = poll_fn(|_| Poll::Ready(()));
                        assert_eq!(std::mem::size_of_val(&candidate), 0);
                        let candidate: RuntimeFuture = Box::pin(candidate);
                        complete_or_drop(candidate, poll);
                    }
                }
            }
            let after = dhat::HeapStats::get();
            counts[index] = (
                after.total_blocks - before.total_blocks,
                after.total_bytes - before.total_bytes,
            );
        }
    }
    #[cfg(feature = "test-util")]
    let forbidden_costs = forbidden_wait_costs(&forbidden, deadlines);
    let released = dhat::HeapStats::get();
    drop(profiler);
    for (lane, counts) in ["shared", "local", "candidate"].into_iter().zip(costs) {
        eprintln!(
            "{lane}: unpolled={:?}, first-poll={:?}",
            counts[0], counts[1]
        );
    }
    assert_eq!(
        costs[2],
        [(0, 0); 2],
        "the zero-capture comparator must not allocate"
    );
    assert_eq!(
        costs[0],
        [(0, 0); 2],
        "shared elapsed waits must not allocate"
    );
    assert_eq!(
        costs[1],
        [(0, 0); 2],
        "local elapsed waits must not allocate"
    );
    assert_eq!((released.curr_blocks, released.curr_bytes), (0, 0));
    #[cfg(feature = "test-util")]
    {
        eprintln!(
            "forbidden elapsed={:?}, pending={:?}; both include enabled reporting",
            forbidden_costs[0], forbidden_costs[1],
        );
        assert_eq!(reports.load(std::sync::atomic::Ordering::Relaxed), 8);
        assert!(forbidden.assert_unused().is_err());
        assert!(
            forbidden_costs[1]
                .iter()
                .all(|&(blocks, bytes)| blocks > 0 && bytes > 0)
        );
        assert_eq!(
            forbidden_costs[0], forbidden_costs[1],
            "elapsed forbidden waits must add no allocation beyond their report",
        );
    }
}

/// Preserves the exact names of the isolated allocation cases.
const CASES: &[(&str, fn())] = &[(
    "elapsed_tolerant_waits_need_no_heap_storage",
    elapsed_tolerant_waits_need_no_heap_storage,
)];

include!("support/allocation_main.rs");
