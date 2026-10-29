//! Checks drain allocation budgets and complete release in isolated child processes.

#![cfg(all(feature = "qualification-dhat", not(loom), not(miri)))]

#[path = "support/allocation_test.rs"]
mod allocation_test;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

use rca_core::MonotonicInstant;
use rca_test::{DeterministicRuntime, PendingSleep};

/// Counts allocations only inside the explicitly owned profiling intervals.
#[global_allocator]
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

/// Checks real wakeups and re-entry while keeping the deterministic host alive.
struct WakeProbe {
    host: DeterministicRuntime,
    wakes: AtomicUsize,
}

impl Wake for WakeProbe {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        // Real user-waker re-entry must remain outside the scheduler lock.
        let _ = self.host.now();
        self.wakes.fetch_add(1, Ordering::SeqCst);
    }
}

/// Measures draining registered timers, using shared or distinct deadlines.
///
/// Construction is outside the measured delta but inside the final release check.
fn measured_drain(count: usize, distinct: bool) -> (u64, u64) {
    // Track construction too, so final live totals prove release. Only the
    // before/after delta below measures draining; registration is excluded.
    let profiler = dhat::Profiler::builder().testing().build();
    let host = DeterministicRuntime::with_step_budget(1);
    let probe = Arc::new(WakeProbe {
        host: host.clone(),
        wakes: AtomicUsize::new(0),
    });
    let waker = Waker::from(Arc::clone(&probe));
    let mut context = Context::from_waker(&waker);
    let mut waits = Vec::with_capacity(count);
    for index in 0..count {
        let tick = if distinct { index + 1 } else { 1 };
        let deadline =
            MonotonicInstant::from_elapsed(Duration::from_nanos(u64::try_from(tick).unwrap()));
        let mut wait = host.sleep_until(deadline);
        assert!(wait.as_mut().poll(&mut context).is_pending());
        waits.push(wait);
    }
    assert_eq!(host.pending_sleeps().len(), count);
    let expected_batches = if distinct { count } else { 1 };
    let expected_time = MonotonicInstant::from_elapsed(Duration::from_nanos(
        u64::try_from(expected_batches).unwrap(),
    ));
    // Box before measurement to permit explicit destruction before the release oracle.
    let mut drain = Box::pin(host.drain_deadlines(expected_batches));
    let before = dhat::HeapStats::get();
    let output = drain.as_mut().poll(&mut Context::from_waker(Waker::noop()));
    let after = dhat::HeapStats::get();
    let Poll::Ready(Ok(report)) = output else {
        panic!("finite owned sleeps must drain in one poll");
    };
    assert_eq!(report.polls(), 0);
    assert_eq!(report.deadline_batches(), expected_batches);
    assert_eq!(report.current_time(), expected_time);
    assert_eq!(host.now(), expected_time);
    assert_eq!(probe.wakes.load(Ordering::SeqCst), count);
    assert!(host.pending_sleeps().is_empty());
    for wait in &mut waits {
        assert!(wait.as_mut().poll(&mut context).is_ready());
    }
    // Every registered waker clone has been released, not merely removed from diagnostics.
    assert_eq!(Arc::strong_count(&probe), 2);
    drop(waits);
    drop(drain);
    drop(waker);
    assert_eq!(Arc::strong_count(&probe), 1);
    drop(probe);
    drop(host);
    let released = dhat::HeapStats::get();
    drop(profiler);
    assert_eq!(released.curr_blocks, 0);
    assert_eq!(released.curr_bytes, 0);
    (
        after.total_blocks - before.total_blocks,
        after.total_bytes - before.total_bytes,
    )
}

#[test]
fn owned_deadline_draining_keeps_allocation_work_linear() {
    if allocation_test::run_in_child("owned_deadline_draining_keeps_allocation_work_linear") {
        return;
    }
    let cases = [
        (1, false),
        (8, false),
        (32, false),
        (1, true),
        (8, true),
        (32, true),
    ];
    let mut observed = [(0, 0); 6];
    for (index, (count, distinct)) in cases.into_iter().enumerate() {
        observed[index] = measured_drain(count, distinct);
    }
    // Print all six samples before budget checks, outside every profiling window.
    println!("drain allocation (blocks, bytes), equal then distinct N=1/8/32: {observed:?}");
    for ((count, distinct), (_, bytes)) in cases.into_iter().zip(observed) {
        // Allow two ordinary extraction buffers with up to four slots per timer.
        // This deliberately does not require a particular Vec reuse optimisation.
        // A fresh full diagnostic list at every distinct deadline exceeds this
        // linear bound at N=32; the extraction/wake work remains necessary.
        let slot_bytes = size_of::<PendingSleep>().max(size_of::<Waker>());
        let byte_budget = u64::try_from(8 * count * slot_bytes).unwrap();
        assert!(
            bytes <= byte_budget,
            "count={count} distinct={distinct}: {bytes} bytes exceeds linear budget {byte_budget}",
        );
    }
}
