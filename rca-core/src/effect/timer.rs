//! Indexes timers by task and schedules them in deadline order.

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap, VecDeque};

use crate::MonotonicInstant;
use crate::effect::registry::TaskId;
use crate::store::scope::{ScopeToken, ScopeTransitionBatch};

/// Transfers a due payload out of both indexes before actor dispatch.
pub(crate) struct DueTimer<Payload> {
    /// Identifies the task whose deadline is due.
    pub(crate) task: TaskId,
    /// Retains the scheduled deadline for fixed-rate interval renewal.
    pub(crate) deadline: MonotonicInstant,
    /// Owns the action, authority and work leases until dispatch or destruction.
    pub(crate) payload: Payload,
}

/// Owns a payload and its finite or permanently parked deadline.
struct TimerRecord<Payload> {
    /// Reconstructs a finite ordered key; `MAX` has no ordered entry.
    deadline: MonotonicInstant,
    /// Orders equal deadlines by insertion, including interval renewals.
    sequence: u64,
    /// Remains outside ordered keys, so comparison cannot invoke user code.
    payload: Payload,
}

/// Indexes all payloads by ID and schedules only finite deadlines.
pub(crate) struct TimerQueue<Payload> {
    /// Orders finite timers by deadline, then insertion, for one-lookup removal.
    order: BTreeMap<(MonotonicInstant, u64), TaskId>,
    /// Owns each payload by task ID for direct cancellation.
    records: HashMap<TaskId, TimerRecord<Payload>>,
    /// Gives each insertion a new sequence number, including interval renewals.
    next_sequence: u64,
    /// Changes only when the earliest scheduled deadline changes.
    wake_generation: u64,
}

impl<Payload> TimerQueue<Payload> {
    /// Creates empty indexes without allocating payload storage.
    pub(crate) fn new() -> Self {
        Self {
            order: BTreeMap::new(),
            records: HashMap::new(),
            next_sequence: 0,
            wake_generation: 0,
        }
    }

    /// Schedules a task that has no live timer in the queue.
    ///
    /// Equal deadlines run in insertion order, even after an interval renews.
    ///
    /// # Panics
    ///
    /// A duplicate task or exhausted insertion sequence panics before insertion.
    /// Wake-generation exhaustion panics after insertion.
    pub(crate) fn schedule(&mut self, task: TaskId, deadline: MonotonicInstant, payload: Payload) {
        let Entry::Vacant(entry) = self.records.entry(task) else {
            panic!("task already has a scheduled timer");
        };
        let sequence = self.next_sequence;
        self.next_sequence = sequence
            .checked_add(1)
            .expect("timer insertion sequence exhausted");
        entry.insert(TimerRecord {
            deadline,
            sequence,
            payload,
        });

        if deadline != MonotonicInstant::MAX {
            let previous = self.earliest_deadline();
            let replaced_order = self.order.insert((deadline, sequence), task);
            debug_assert!(replaced_order.is_none(), "timer order key must be unique");
            self.update_wake_generation(previous);
        }
    }

    /// Schedules a test payload with a fresh task ID.
    #[cfg(test)]
    fn insert(&mut self, deadline: MonotonicInstant, payload: Payload) -> TaskId {
        let task = self.next_sequence;
        self.schedule(task, deadline, payload);
        task
    }

    /// Removes a task's timer and returns its payload without dropping it.
    pub(crate) fn remove(&mut self, task: TaskId) -> Option<Payload> {
        let record = self.records.remove(&task)?;
        if record.deadline != MonotonicInstant::MAX {
            let previous = self.earliest_deadline();
            let removed = self.order.remove(&(record.deadline, record.sequence));
            debug_assert_eq!(removed, Some(task), "timer indexes must agree");
            self.update_wake_generation(previous);
        }
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

    #[cfg(test)]
    ordinary_test_items! {
        /// Collects due timers into a fresh buffer for ordinary unit tests.
        pub(crate) fn pop_due(
            &mut self,
            now: MonotonicInstant,
            limit: usize,
        ) -> Vec<DueTimer<Payload>> {
            let mut due = VecDeque::new();
            self.pop_due_into(now, limit, &mut due);
            due.into()
        }
    }

    /// Appends at most `limit` eligible timers in deadline and insertion order.
    ///
    /// `MAX` remains parked even when `now` is saturated. Payload destruction and
    /// dispatch happen outside the queue after this method returns. Existing
    /// output entries remain owned by the caller, without being cleared or dropped.
    pub(crate) fn pop_due_into(
        &mut self,
        now: MonotonicInstant,
        limit: usize,
        due: &mut VecDeque<DueTimer<Payload>>,
    ) {
        let previous = self.earliest_deadline();
        due.reserve_exact(limit.min(self.order.len()));
        for _ in 0..limit {
            let Some(entry) = self.order.first_entry() else {
                break;
            };
            let &(deadline, _) = entry.key();
            if deadline > now {
                break;
            }
            let (_, task) = entry.remove_entry();
            let record = self
                .records
                .remove(&task)
                .expect("timer indexes must agree");
            due.push_back(DueTimer {
                task,
                deadline,
                payload: record.payload,
            });
        }
        self.update_wake_generation(previous);
    }

    /// Identifies the next runtime wake, excluding permanently parked deadlines.
    pub(crate) fn next_wake(&self) -> Option<(MonotonicInstant, u64)> {
        self.earliest_deadline()
            .map(|deadline| (deadline, self.wake_generation))
    }

    /// Checks eligibility without moving payloads or scheduling a runtime wait.
    pub(crate) fn has_due(&self, now: MonotonicInstant) -> bool {
        self.earliest_deadline()
            .is_some_and(|deadline| deadline <= now)
    }

    /// Reports whether any timer remains, including parked timers.
    pub(crate) fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Counts timers that still belong to the queue.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.records.len()
    }

    #[cfg(feature = "test-util")]
    /// Copies ordered deadlines for deterministic inspection without exposing IDs.
    pub(crate) fn deadlines(&self) -> Vec<MonotonicInstant> {
        self.order
            .keys()
            .map(|(deadline, _)| *deadline)
            .chain(std::iter::repeat_n(
                MonotonicInstant::MAX,
                self.records.len() - self.order.len(),
            ))
            .collect()
    }

    /// Reads the earliest scheduled deadline without visiting parked records.
    fn earliest_deadline(&self) -> Option<MonotonicInstant> {
        self.order
            .first_key_value()
            .map(|(&(deadline, _), _)| deadline)
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

#[cfg(all(test, feature = "allocation-measurement"))]
pub(crate) mod allocation_tests;

#[cfg(test)]
ordinary_test_items! {
mod tests {
    use std::collections::VecDeque;
    use std::time::Duration;

    use proptest::prelude::*;

    use super::TimerQueue;
    use crate::MonotonicInstant;
    use crate::store::scope::{
        ScopeIdentity, ScopeRegistry, ScopeSlot, ScopeToken, ScopeTransitionBatch,
    };

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
    fn renewed_task_follows_an_existing_peer_at_the_same_deadline() {
        let mut queue = TimerQueue::new();
        let deadline = MonotonicInstant::from_elapsed(Duration::from_secs(1));
        queue.schedule(3, MonotonicInstant::ZERO, "first tick");
        queue.schedule(7, deadline, "peer");

        let due = queue.pop_due(MonotonicInstant::ZERO, 1);
        assert_eq!((due[0].task, due[0].payload), (3, "first tick"));
        queue.schedule(due[0].task, deadline, "renewed tick");

        let due = queue.pop_due(deadline, 2);
        assert_eq!((due[0].task, due[0].payload), (7, "peer"));
        assert_eq!((due[1].task, due[1].payload), (3, "renewed tick"));
        assert!(queue.is_empty());
    }

    #[test]
    fn duplicate_task_preserves_the_old_payload_and_wake() {
        let mut queue = TimerQueue::new();
        let deadline = MonotonicInstant::from_elapsed(Duration::from_secs(1));
        queue.schedule(7, deadline, "original");
        let wake = queue.next_wake();

        let duplicate = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            queue.schedule(7, MonotonicInstant::ZERO, "duplicate");
        }));

        assert!(duplicate.is_err());
        assert_eq!(queue.next_wake(), wake);
        assert!(!queue.has_due(MonotonicInstant::ZERO));
        assert_eq!(queue.remove(7), Some("original"));
        assert!(queue.is_empty());
        assert_eq!(queue.next_wake(), None);
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
            (due[0].task, due[0].deadline, due[0].payload),
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
    fn reused_due_buffer_preserves_cutoffs_and_detached_batch_order() {
        let mut queue = TimerQueue::new();
        let deadline = MonotonicInstant::from_elapsed(Duration::from_secs(1));
        let first = queue.insert(deadline, 1);
        let second = queue.insert(deadline, 2);
        queue.insert(MonotonicInstant::MAX, 4);
        let wake = queue.next_wake();
        let mut due = VecDeque::new();

        queue.pop_due_into(deadline, 0, &mut due);
        queue.pop_due_into(MonotonicInstant::ZERO, 8, &mut due);
        assert!(due.is_empty());
        assert_eq!(queue.next_wake(), wake);

        queue.pop_due_into(deadline, 2, &mut due);
        assert_eq!(queue.remove(first), None);
        assert_eq!(queue.remove(second), None);
        let mut dispatched = Vec::new();
        while let Some(timer) = due.pop_front() {
            dispatched.push(timer.payload);
            if timer.payload == 1 {
                queue.insert(MonotonicInstant::ZERO, 3);
            }
        }
        assert_eq!(dispatched, [1, 2]);
        assert!(due.is_empty());

        queue.pop_due_into(MonotonicInstant::MAX, 8, &mut due);
        assert_eq!(
            due.iter().map(|timer| timer.payload).collect::<Vec<_>>(),
            [3]
        );
        due.clear();
        assert!(!queue.is_empty());
        assert_eq!(queue.next_wake(), None);
        assert!(!queue.has_due(MonotonicInstant::MAX));
    }

    #[test]
    fn detached_due_batch_drops_remainder_after_return_or_unwind() {
        use std::cell::RefCell;
        use std::rc::Rc;

        /// Records payload destruction without invoking the timer queue.
        struct DropProbe {
            /// Distinguishes the current payload from the detached remainder.
            value: usize,
            /// Shared destruction order, checked after the dispatch boundary.
            dropped: Rc<RefCell<Vec<usize>>>,
        }

        impl Drop for DropProbe {
            fn drop(&mut self) {
                self.dropped.borrow_mut().push(self.value);
            }
        }

        for unwind in [false, true] {
            let dropped = Rc::new(RefCell::new(Vec::new()));
            let mut queue = TimerQueue::new();
            let mut due = VecDeque::with_capacity(3);
            // Advance the ring so dropping the later remainder covers both slices.
            queue.insert(
                MonotonicInstant::ZERO,
                DropProbe {
                    value: 3,
                    dropped: Rc::clone(&dropped),
                },
            );
            queue.pop_due_into(MonotonicInstant::ZERO, 1, &mut due);
            drop(due.pop_front());
            dropped.borrow_mut().clear();

            for value in 0..3 {
                queue.insert(
                    MonotonicInstant::ZERO,
                    DropProbe {
                        value,
                        dropped: Rc::clone(&dropped),
                    },
                );
            }
            queue.pop_due_into(MonotonicInstant::ZERO, 3, &mut due);
            assert!(queue.is_empty());
            assert!(dropped.borrow().is_empty());

            let mut dispatched = Vec::new();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut due = due;
                while let Some(timer) = due.pop_front() {
                    dispatched.push(timer.payload.value);
                    if timer.payload.value == 0 {
                        assert!(!unwind, "timer dispatch failed");
                        return;
                    }
                }
            }));
            assert_eq!(result.is_err(), unwind);
            assert_eq!(dispatched, [0]);
            assert_eq!(*dropped.borrow(), [0, 1, 2]);
        }
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
    fn parked_timers_leave_scheduled_wake_generations_unchanged() {
        let mut queue = TimerQueue::new();
        let early = MonotonicInstant::from_elapsed(Duration::from_secs(1));
        let later = MonotonicInstant::from_elapsed(Duration::from_secs(2));
        let parked_first = queue.insert(MonotonicInstant::MAX, 10);
        let parked_last = queue.insert(MonotonicInstant::MAX, 20);
        assert_eq!(queue.wake_generation, 0);
        assert_eq!(queue.next_wake(), None);

        let finite_first = queue.insert(later, 1);
        let finite_last = queue.insert(later, 2);
        assert_eq!(queue.next_wake(), Some((later, 1)));
        queue.insert(early, 0);
        assert_eq!(queue.next_wake(), Some((early, 2)));
        #[cfg(feature = "test-util")]
        assert_eq!(
            queue.deadlines(),
            [early, later, later, MonotonicInstant::MAX, MonotonicInstant::MAX]
        );

        assert_eq!(queue.remove(parked_first), Some(10));
        assert_eq!(queue.next_wake(), Some((early, 2)));
        assert_eq!(queue.pop_due(early, 1)[0].payload, 0);
        assert_eq!(queue.next_wake(), Some((later, 3)));
        let first = queue.pop_due(later, 1);
        assert_eq!((first[0].task, first[0].payload), (finite_first, 1));
        assert_eq!(queue.next_wake(), Some((later, 3)));
        let last = queue.pop_due(later, 1);
        assert_eq!((last[0].task, last[0].payload), (finite_last, 2));
        assert_eq!(queue.wake_generation, 4);
        assert_eq!(queue.next_wake(), None);
        assert!(!queue.has_due(MonotonicInstant::MAX));
        assert!(!queue.is_empty());
        #[cfg(feature = "test-util")]
        assert_eq!(queue.deadlines(), [MonotonicInstant::MAX]);

        assert!(queue.pop_due(MonotonicInstant::MAX, 8).is_empty());
        assert_eq!(queue.wake_generation, 4);
        assert_eq!(queue.remove(parked_last), Some(20));
        assert_eq!(queue.wake_generation, 4);
        assert!(queue.is_empty());
        #[cfg(feature = "test-util")]
        assert_eq!(queue.deadlines(), [] as [MonotonicInstant; 0]);

        queue.insert(later, 3);
        assert_eq!(queue.next_wake(), Some((later, 5)));
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
            assert_eq!(cancelled, [] as [(ScopeToken, usize); 0]);
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
