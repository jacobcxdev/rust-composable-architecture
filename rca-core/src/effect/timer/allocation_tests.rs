//! Measures allocation costs without the ordinary test harness.

use super::TimerQueue;
use crate::MonotonicInstant;
use std::cell::Cell;
use std::collections::VecDeque;
use std::time::Duration;

/// Counts destruction independently of timer-index ownership.
struct ParkedPayload<'a> {
    /// Identifies the payload returned by explicit cancellation.
    value: usize,
    /// Counts payloads destroyed after cancellation returns them.
    drops: &'a Cell<usize>,
}

impl Drop for ParkedPayload<'_> {
    fn drop(&mut self) {
        self.drops.set(self.drops.get() + 1);
    }
}

/// Measures timer-index storage across insertion, removal and expiry.
pub(crate) fn timer_index_storage_costs() {
    if crate::allocation_test::run_in_child("effect::timer::tests::timer_index_storage_costs") {
        return;
    }
    let mut samples = Vec::with_capacity(2);
    let mut parked_samples = Vec::with_capacity(2);
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
        let mut due = VecDeque::new();
        queue.pop_due_into(
            MonotonicInstant::from_elapsed(Duration::from_secs(1)),
            count,
            &mut due,
        );
        let popped = dhat::HeapStats::get();
        assert_eq!(due.len(), count / 2);
        assert!(queue.is_empty());
        assert!(due.iter().all(|timer| timer.payload % 2 == 1));
        assert!(due.iter().zip(due.iter().skip(1)).all(|(first, second)| {
            (first.deadline, first.payload) < (second.deadline, second.payload)
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

    for count in [16_usize, 1_024] {
        parked_samples.push(measure_parked_timers(count));
    }
    drop(profiler);
    println!(
        "timer index sizes queue={} record={}; samples(count,insert_blocks,insert_bytes,live_bytes,remove_blocks,pop_blocks,pop_bytes)={samples:?}",
        std::mem::size_of::<TimerQueue<usize>>(),
        std::mem::size_of::<super::TimerRecord<usize>>(),
    );
    println!(
        "parked timer index sizes queue={} record={}; samples(count,insert_blocks,insert_bytes,live_bytes,pop_blocks,pop_bytes)={parked_samples:?}",
        std::mem::size_of::<TimerQueue<ParkedPayload<'_>>>(),
        std::mem::size_of::<super::TimerRecord<ParkedPayload<'_>>>(),
    );
    for (count, _, _, _, pop_blocks, pop_bytes) in parked_samples {
        assert_eq!(
            (pop_blocks, pop_bytes),
            (0, 0),
            "parked-only expiry must not reserve a due buffer for {count} timers"
        );
    }
}

/// Measures parked storage and checks that expiry leaves its payloads owned.
fn measure_parked_timers(count: usize) -> (usize, u64, u64, usize, u64, u64) {
    let mut ids = Vec::with_capacity(count);
    let drops = Cell::new(0);
    let mut queue = TimerQueue::new();
    let before = dhat::HeapStats::get();
    for value in 0..count {
        ids.push(queue.insert(
            MonotonicInstant::MAX,
            ParkedPayload {
                value,
                drops: &drops,
            },
        ));
    }
    let inserted = dhat::HeapStats::get();
    let mut due = VecDeque::new();
    queue.pop_due_into(MonotonicInstant::MAX, count, &mut due);
    let popped = dhat::HeapStats::get();

    assert!(due.is_empty());
    assert!(!queue.is_empty());
    assert_eq!(queue.next_wake(), None);
    assert!(!queue.has_due(MonotonicInstant::MAX));
    assert_eq!(drops.get(), 0);
    for (value, id) in ids.into_iter().enumerate() {
        let payload = queue.remove(id).expect("parked payload remains owned");
        assert_eq!(payload.value, value);
        assert_eq!(drops.get(), value);
        drop(payload);
        assert_eq!(drops.get(), value + 1);
    }
    assert!(queue.is_empty());
    assert_eq!(drops.get(), count);
    (
        count,
        inserted.total_blocks - before.total_blocks,
        inserted.total_bytes - before.total_bytes,
        inserted.curr_bytes - before.curr_bytes,
        popped.total_blocks - inserted.total_blocks,
        popped.total_bytes - inserted.total_bytes,
    )
}
