//! Maintains indexed timers in deadline and insertion order.

use std::collections::{BTreeMap, HashMap};

use crate::MonotonicInstant;
use crate::store::scope::{ScopeToken, ScopeTransitionBatch};

/// Identifies one insertion and orders timers with equal deadlines.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct TimerId(u64);

/// Transfers a due payload out of both indexes before actor dispatch.
pub(crate) struct DueTimer<Payload> {
    /// Identifies the removed insertion for the actor's task-to-timer index.
    pub(crate) id: TimerId,
    /// Retains the scheduled deadline for fixed-rate interval renewal.
    pub(crate) deadline: MonotonicInstant,
    /// Owns the action, authority and work leases until dispatch or destruction.
    pub(crate) payload: Payload,
}

/// Owns a payload and the deadline needed for cancellation by ID.
struct TimerRecord<Payload> {
    /// Reconstructs the ordered index key together with this record's map key.
    deadline: MonotonicInstant,
    /// Remains outside ordered keys, so comparison cannot invoke user code.
    payload: Payload,
}

/// Indexes live payloads by ID and schedules them by deadline, then insertion.
pub(crate) struct TimerQueue<Payload> {
    /// A unit-valued map permits checking and removing one minimum entry together.
    #[expect(
        clippy::zero_sized_map_values,
        reason = "BTreeSet lacks an occupied entry API for one-lookup conditional removal"
    )]
    order: BTreeMap<(MonotonicInstant, TimerId), ()>,
    /// Owns each payload once and supports cancellation without scanning deadlines.
    records: HashMap<TimerId, TimerRecord<Payload>>,
    /// Never reuses a timer ID, including after cancellation or interval renewal.
    next_id: u64,
    /// Changes only when the earliest deadline changes, not its equal-deadline ID.
    wake_generation: u64,
}

impl<Payload> TimerQueue<Payload> {
    /// Creates empty indexes without allocating payload storage.
    pub(crate) fn new() -> Self {
        Self {
            #[expect(
                clippy::zero_sized_map_values,
                reason = "the unit-valued map provides conditional occupied-entry removal"
            )]
            order: BTreeMap::new(),
            records: HashMap::new(),
            next_id: 0,
            wake_generation: 0,
        }
    }

    /// Owns one insertion; equal deadlines retain their insertion order.
    ///
    /// ID exhaustion panics before insertion. Wake-generation exhaustion occurs
    /// after both indexes change; this method does not promise panic rollback.
    pub(crate) fn insert(&mut self, deadline: MonotonicInstant, payload: Payload) -> TimerId {
        let previous = self.earliest_deadline();
        let id = TimerId(self.next_id);
        self.next_id = self
            .next_id
            .checked_add(1)
            .expect("timer ID space exhausted");
        let replaced = self.records.insert(id, TimerRecord { deadline, payload });
        debug_assert!(replaced.is_none(), "new timer ID must be unique");
        let replaced_order = self.order.insert((deadline, id), ());
        debug_assert!(replaced_order.is_none(), "timer order key must be unique");
        self.update_wake_generation(previous);
        id
    }

    /// Removes a live insertion and returns its payload without dropping it.
    pub(crate) fn remove(&mut self, id: TimerId) -> Option<Payload> {
        let previous = self.earliest_deadline();
        let record = self.records.remove(&id)?;
        let removed = self.order.remove(&(record.deadline, id));
        debug_assert!(removed.is_some(), "timer indexes must agree");
        self.update_wake_generation(previous);
        Some(record.payload)
    }

    /// Collects affected IDs before removing their payloads from either index.
    ///
    /// A scope callback panic leaves the indexes unchanged. Returned payload
    /// order is unspecified; callers destroy them after updating their own indexes.
    pub(crate) fn cancel_scopes(
        &mut self,
        transitions: &ScopeTransitionBatch,
        mut scope: impl FnMut(&Payload) -> ScopeToken,
    ) -> Vec<Payload> {
        if transitions.is_empty() {
            return Vec::new();
        }

        let ids: Vec<_> = self
            .records
            .iter()
            .filter_map(|(id, record)| transitions.contains(scope(&record.payload)).then_some(*id))
            .collect();
        ids.into_iter().filter_map(|id| self.remove(id)).collect()
    }

    /// Transfers at most `limit` eligible timers in deadline and insertion order.
    ///
    /// `MAX` remains parked even when `now` is saturated. Payload destruction and
    /// dispatch happen outside the queue after this method returns.
    pub(crate) fn pop_due(
        &mut self,
        now: MonotonicInstant,
        limit: usize,
    ) -> Vec<DueTimer<Payload>> {
        let previous = self.earliest_deadline();
        let mut due = Vec::with_capacity(limit.min(self.records.len()));
        while due.len() < limit {
            let Some(entry) = self.order.first_entry() else {
                break;
            };
            let &(deadline, _) = entry.key();
            if deadline == MonotonicInstant::MAX || deadline > now {
                break;
            }
            let ((_, id), ()) = entry.remove_entry();
            let record = self.records.remove(&id).expect("timer indexes must agree");
            due.push(DueTimer {
                id,
                deadline,
                payload: record.payload,
            });
        }
        self.update_wake_generation(previous);
        due
    }

    /// Identifies the next runtime wake, excluding permanently parked deadlines.
    pub(crate) fn next_wake(&self) -> Option<(MonotonicInstant, u64)> {
        self.earliest_deadline()
            .filter(|deadline| *deadline != MonotonicInstant::MAX)
            .map(|deadline| (deadline, self.wake_generation))
    }

    /// Checks eligibility without moving payloads or scheduling a runtime wait.
    pub(crate) fn has_due(&self, now: MonotonicInstant) -> bool {
        self.earliest_deadline()
            .is_some_and(|deadline| deadline != MonotonicInstant::MAX && deadline <= now)
    }

    /// Reports whether any timer remains, including parked timers.
    pub(crate) fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    #[cfg(feature = "test-util")]
    /// Copies ordered deadlines for deterministic inspection without exposing IDs.
    pub(crate) fn deadlines(&self) -> Vec<MonotonicInstant> {
        self.order.keys().map(|(deadline, _)| *deadline).collect()
    }

    /// Reads the earliest key, including `MAX` for wake-change detection.
    fn earliest_deadline(&self) -> Option<MonotonicInstant> {
        self.order
            .first_key_value()
            .map(|(&(deadline, _), ())| deadline)
    }

    /// Invalidates a runtime wait only when its deadline changes.
    fn update_wake_generation(&mut self, previous: Option<MonotonicInstant>) {
        if previous != self.earliest_deadline() {
            self.wake_generation = self
                .wake_generation
                .checked_add(1)
                .expect("timer wake generation space exhausted");
        }
    }
}

impl<Payload> Default for TimerQueue<Payload> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
ordinary_test_items! {
mod tests {
    use std::time::Duration;

    use proptest::prelude::*;

    use super::TimerQueue;
    use crate::MonotonicInstant;
    use crate::store::scope::{ScopeIdentity, ScopeRegistry, ScopeSlot, ScopeTransitionBatch};

    #[test]
    fn equal_deadlines_pop_in_insertion_order() {
        let mut queue = TimerQueue::default();
        let deadline = MonotonicInstant::from_elapsed(Duration::from_secs(1));
        queue.insert(deadline, 1);
        queue.insert(deadline, 2);

        let values: Vec<_> = queue
            .pop_due(deadline, 8)
            .into_iter()
            .map(|timer| timer.payload)
            .collect();

        assert_eq!(values, [1, 2]);
    }

    #[test]
    fn removing_far_future_timer_reclaims_record() {
        let mut queue = TimerQueue::new();
        let id = queue.insert(MonotonicInstant::MAX, 1);

        assert_eq!(queue.remove(id), Some(1));
        assert!(queue.is_empty());
    }

    #[test]
    fn removing_retired_timer_preserves_next_payload_and_wake() {
        let mut queue = TimerQueue::new();
        let first = queue.insert(MonotonicInstant::ZERO, 1);
        let deadline = MonotonicInstant::from_elapsed(Duration::from_secs(1));
        let second = queue.insert(deadline, 2);
        assert_eq!(queue.remove(first), Some(1));
        let wake = queue.next_wake();

        assert_eq!(queue.remove(first), None);
        assert_eq!(queue.next_wake(), wake);
        assert!(!queue.has_due(MonotonicInstant::ZERO));
        let due = queue.pop_due(deadline, 2);
        assert_eq!(due.len(), 1);
        assert_eq!(
            (due[0].id, due[0].deadline, due[0].payload),
            (second, deadline, 2)
        );
        assert_eq!(queue.remove(second), None);
        assert!(queue.is_empty());
        assert_eq!(queue.next_wake(), None);
    }

    #[test]
    fn due_batches_preserve_remaining_order() {
        let mut queue = TimerQueue::new();
        let deadline = MonotonicInstant::from_elapsed(Duration::from_secs(1));
        for value in 0..5 {
            queue.insert(deadline, value);
        }

        let first: Vec<_> = queue
            .pop_due(deadline, 2)
            .into_iter()
            .map(|timer| timer.payload)
            .collect();
        let second: Vec<_> = queue
            .pop_due(deadline, 8)
            .into_iter()
            .map(|timer| timer.payload)
            .collect();

        assert_eq!(first, [0, 1]);
        assert_eq!(second, [2, 3, 4]);
        assert!(queue.is_empty());
    }

    #[test]
    fn saturated_deadline_has_no_runtime_wake() {
        let mut queue = TimerQueue::new();
        queue.insert(MonotonicInstant::MAX, 1);

        assert_eq!(queue.next_wake(), None);
        assert!(queue.pop_due(MonotonicInstant::MAX, 1).is_empty());
        assert!(!queue.is_empty());
    }

    #[test]
    fn reinserted_equal_deadlines_follow_remaining_older_timers() {
        let mut queue = TimerQueue::new();
        let removed = queue.insert(MonotonicInstant::ZERO, 1);
        queue.insert(MonotonicInstant::ZERO, 2);
        let wake = queue.next_wake();
        assert_eq!(queue.remove(removed), Some(1));
        assert_eq!(queue.next_wake(), wake);
        queue.insert(MonotonicInstant::ZERO, 3);
        assert_eq!(queue.pop_due(MonotonicInstant::ZERO, 1)[0].payload, 2);
        assert_eq!(queue.next_wake(), wake);
        queue.insert(MonotonicInstant::ZERO, 4);
        let remaining: Vec<_> = queue
            .pop_due(MonotonicInstant::ZERO, 2)
            .into_iter()
            .map(|timer| timer.payload)
            .collect();
        assert_eq!(remaining, [3, 4]);
        assert_eq!(queue.remove(removed), None);
        assert_eq!(queue.next_wake(), None);

        queue.insert(MonotonicInstant::ZERO, 5);
        assert_ne!(queue.next_wake(), wake);
    }

    #[cfg(feature = "allocation-measurement")]
    #[test]
    fn timer_index_storage_costs() {
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
                let deadline = MonotonicInstant::from_elapsed(Duration::from_millis(
                    (value % 4) as u64,
                ));
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

    #[test]
    fn empty_scope_cancellation_does_not_visit_pending_timers() {
        let mut visits = Vec::new();
        for count in [0_usize, 1, 1_024] {
            let scope = ScopeRegistry::for_test().1.root();
            let mut queue = TimerQueue::new();
            let deadline = MonotonicInstant::from_elapsed(Duration::from_secs(1));
            for value in 0..count {
                queue.insert(deadline, (scope, value));
            }
            let wake = queue.next_wake();
            let mut inspected = 0;

            let cancelled = queue.cancel_scopes(&ScopeTransitionBatch::default(), |payload| {
                inspected += 1;
                payload.0
            });

            visits.push(inspected);
            assert!(cancelled.is_empty());
            assert_eq!(queue.next_wake(), wake);
            let due = queue.pop_due(deadline, count);
            assert_eq!(due.len(), count);
            for (value, timer) in due.into_iter().enumerate() {
                assert_eq!(timer.deadline, deadline);
                assert_eq!(timer.payload, (scope, value));
            }
            assert!(queue.is_empty());
        }
        assert_eq!(visits, [0, 0, 0]);
    }

    #[test]
    fn cancelling_scopes_removes_descendant_timers_only() {
        let (_scopes_lifecycle, mut scopes) = ScopeRegistry::for_test();
        let root = scopes.root();
        let parent_node = ScopeRegistry::node(&mut ScopeSlot::new());
        let child_node = ScopeRegistry::node(&mut ScopeSlot::new());
        let sibling_node = ScopeRegistry::node(&mut ScopeSlot::new());
        let parent = scopes.mount(root, parent_node, ScopeIdentity::unit());
        let child = scopes.mount(parent, child_node, ScopeIdentity::unit());
        let sibling = scopes.mount(root, sibling_node, ScopeIdentity::unit());
        let mut queue = TimerQueue::new();
        queue.insert(MonotonicInstant::ZERO, (parent, 1));
        queue.insert(MonotonicInstant::ZERO, (child, 2));
        queue.insert(MonotonicInstant::ZERO, (sibling, 3));
        let mut transitions = ScopeTransitionBatch::default();
        transitions.extend(scopes.leave(parent));

        let mut cancelled: Vec<_> = queue
            .cancel_scopes(&transitions, |payload| payload.0)
            .into_iter()
            .map(|(_, value)| value)
            .collect();
        cancelled.sort_unstable();

        assert_eq!(cancelled, [1, 2]);
        assert_eq!(
            queue
                .pop_due(MonotonicInstant::ZERO, 8)
                .into_iter()
                .map(|timer| timer.payload.1)
                .collect::<Vec<_>>(),
            [3]
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            // Miri interprets each BTreeMap operation; normal tests retain the full case budget.
            cases: if cfg!(miri) { 4 } else { ProptestConfig::default().cases },
            failure_persistence: None,
            ..ProptestConfig::default()
        })]

        #[test]
        fn generated_deadlines_match_stable_reference_order(
            entries in prop::collection::vec((0_u16..1_000, any::<u8>()), 0..129)
        ) {
            let mut queue = TimerQueue::new();
            for (millis, value) in &entries {
                queue.insert(
                    MonotonicInstant::from_elapsed(Duration::from_millis(u64::from(*millis))),
                    *value,
                );
            }

            let actual: Vec<_> = queue
                .pop_due(
                    MonotonicInstant::from_elapsed(Duration::from_secs(1)),
                    entries.len(),
                )
                .into_iter()
                .map(|timer| timer.payload)
                .collect();
            let mut expected: Vec<_> = entries.iter().enumerate().collect();
            expected.sort_by_key(|(sequence, (millis, _))| (*millis, *sequence));
            let expected: Vec<_> = expected
                .into_iter()
                .map(|(_, (_, value))| *value)
                .collect();

            prop_assert_eq!(actual, expected);
        }
    }
}
}
