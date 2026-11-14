//! Bounds actor-future ownership in the shutdown memory workloads.
//!
//! Each benchmark process runs one scenario. Only its bounded shutdown driver
//! uses these counters. A final zero-sized field reports destruction after the
//! owned future. It does not report release of Tokio's task allocation.

use std::sync::{Condvar, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use rca_core::RuntimeFuture;

/// Records all driver transfers and completed future destruction in one process.
#[derive(Clone, Copy, Default)]
struct Counts {
    /// Counts futures transferred to the shutdown driver.
    spawned: usize,
    /// Counts futures after their owned payloads are destroyed.
    dropped: usize,
    /// Counts transferred futures not yet destroyed.
    unfinished: usize,
    /// Records the largest unfinished population.
    maximum_unfinished: usize,
    /// Counts completed batches, including warm-up batches.
    batches: usize,
    /// Records unfinished work at each completed batch boundary.
    maximum_batch_end_unfinished: usize,
}

/// Keeps accounting outside the actor, its state and its future allocation.
static COUNTS: Mutex<Counts> = Mutex::new(Counts {
    spawned: 0,
    dropped: 0,
    unfinished: 0,
    maximum_unfinished: 0,
    batches: 0,
    maximum_batch_end_unfinished: 0,
});

/// Wakes the benchmark caller after an owned future is destroyed.
static DESTROYED: Condvar = Condvar::new();

/// Carries the bounded population proof into each measurement record.
pub(super) struct Lifetime {
    /// Declares the exact number of simultaneously live Stores.
    pub(super) live_limit: usize,
    /// Records the largest unfinished actor-future population.
    pub(super) maximum_unfinished: usize,
    /// Counts actor futures, including warm-up.
    pub(super) spawned: usize,
    /// Counts destroyed actor futures, including warm-up.
    pub(super) dropped: usize,
    /// Must be zero after the final batch.
    pub(super) unfinished: usize,
    /// Must be zero at every completed batch boundary.
    pub(super) maximum_batch_end_unfinished: usize,
    /// Counts all measured and warm-up batches.
    pub(super) batches: usize,
}

/// Reports completion only after the preceding owned future field drops.
struct FinalDrop;

impl Drop for FinalDrop {
    fn drop(&mut self) {
        let mut counts = COUNTS.lock().expect("shutdown counter lock");
        counts.dropped += 1;
        counts.unfinished -= 1;
        DESTROYED.notify_one();
    }
}

/// Retains the original future representation with a final zero-sized report.
#[repr(transparent)]
pub(super) struct TrackedFuture {
    /// Drops the actor and all its owned captures before the marker.
    inner: RuntimeFuture,
    /// Reports completed destruction after `inner`, including unpolled drops.
    _final_drop: FinalDrop,
}

impl TrackedFuture {
    /// Counts one transfer without allocating another future box.
    pub(super) fn new(inner: RuntimeFuture) -> Self {
        let mut counts = COUNTS.lock().expect("shutdown counter lock");
        counts.spawned += 1;
        counts.unfinished += 1;
        counts.maximum_unfinished = counts.maximum_unfinished.max(counts.unfinished);
        drop(counts);
        Self {
            inner,
            _final_drop: FinalDrop,
        }
    }
}

impl Future for TrackedFuture {
    type Output = ();

    fn poll(self: std::pin::Pin<&mut Self>, context: &mut Context<'_>) -> Poll<()> {
        self.get_mut().inner.as_mut().poll(context)
    }
}

/// Reads actor-future ownership before a bounded batch starts destruction.
pub(super) fn unfinished() -> usize {
    COUNTS.lock().expect("shutdown counter lock").unfinished
}

/// Waits for a fixed unfinished population, not an elapsed settling delay.
///
/// The caller runs outside the runtime workers. The five-second overall bound
/// fails a stuck cleanup instead of changing samples or waiting indefinitely.
pub(super) fn wait_until(unfinished: usize) {
    let counts = COUNTS.lock().expect("shutdown counter lock");
    let (counts, _) = DESTROYED
        .wait_timeout_while(counts, Duration::from_secs(5), |counts| {
            counts.unfinished > unfinished
        })
        .expect("shutdown counter lock");
    assert_eq!(
        counts.unfinished, unfinished,
        "actor future destruction did not reach the fixed population"
    );
}

/// Prevents the next sample from inheriting unfinished actor destruction.
pub(super) fn finish_batch() {
    wait_until(0);
    let mut counts = COUNTS.lock().expect("shutdown counter lock");
    counts.maximum_batch_end_unfinished =
        counts.maximum_batch_end_unfinished.max(counts.unfinished);
    counts.batches += 1;
}

/// Checks exact work and population before publishing lifetime evidence.
pub(super) fn lifetime(live_limit: usize, batches: usize) -> Lifetime {
    let counts = *COUNTS.lock().expect("shutdown counter lock");
    let expected = live_limit * batches;
    assert_eq!(counts.spawned, expected, "every Store has one actor future");
    assert_eq!(counts.dropped, expected, "every actor future is destroyed");
    assert_eq!(counts.unfinished, 0);
    assert_eq!(counts.maximum_unfinished, live_limit);
    assert_eq!(counts.maximum_batch_end_unfinished, 0);
    assert_eq!(counts.batches, batches);
    Lifetime {
        live_limit,
        maximum_unfinished: counts.maximum_unfinished,
        spawned: counts.spawned,
        dropped: counts.dropped,
        unfinished: counts.unfinished,
        maximum_batch_end_unfinished: counts.maximum_batch_end_unfinished,
        batches: counts.batches,
    }
}
