//! Measures allocation costs without the ordinary test harness.

use super::TimerQueue;
use crate::MonotonicInstant;
use std::time::Duration;

/// Measures timer-index storage across insertion, removal and expiry.
pub(crate) fn timer_index_storage_costs() {
    if crate::allocation_test::run_in_child("effect::timer::tests::timer_index_storage_costs") {
        return;
    }
    let mut samples = Vec::with_capacity(2);
    let profiler = dhat::Profiler::builder().testing().build();

    for count in [16_usize, 1_024] {
        let mut ids = Vec::with_capacity(count);
        let mut queue = TimerQueue::new();
        let before = dhat::HeapStats::get();
        for value in 0..count {
            let deadline =
                MonotonicInstant::from_elapsed(Duration::from_millis((value % 4) as u64));
            ids.push(queue.insert(deadline, value));
        }
        let inserted = dhat::HeapStats::get();
        for value in (0..count).step_by(2) {
            assert_eq!(queue.remove(ids[value]), Some(value));
        }
        let removed = dhat::HeapStats::get();
        let due = queue.pop_due(
            MonotonicInstant::from_elapsed(Duration::from_secs(1)),
            count,
        );
        let popped = dhat::HeapStats::get();
        assert_eq!(due.len(), count / 2);
        assert!(queue.is_empty());
        assert!(due.iter().all(|timer| timer.payload % 2 == 1));
        assert!(due.windows(2).all(|pair| {
            (pair[0].deadline, pair[0].payload) < (pair[1].deadline, pair[1].payload)
        }));
        samples.push((
            count,
            inserted.total_blocks - before.total_blocks,
            inserted.total_bytes - before.total_bytes,
            inserted.curr_bytes - before.curr_bytes,
            removed.total_blocks - inserted.total_blocks,
            popped.total_blocks - removed.total_blocks,
            popped.total_bytes - removed.total_bytes,
        ));
    }
    drop(profiler);
    println!(
        "timer index sizes queue={} record={}; samples(count,insert_blocks,insert_bytes,live_bytes,remove_blocks,pop_blocks,pop_bytes)={samples:?}",
        std::mem::size_of::<TimerQueue<usize>>(),
        std::mem::size_of::<super::TimerRecord<usize>>(),
    );
}
