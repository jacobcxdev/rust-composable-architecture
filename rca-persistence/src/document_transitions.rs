//! Coordinates document revisions, operation attempts and persistence barriers.

use std::hash::{Hash, Hasher};
use std::sync::Arc;

use crate::document::{Active, Attempt, Barrier, Failure, LoadStatus, State};
use crate::receipt::{Reply, Resolution};
use crate::{FlushOutcome, ReceiptError};

/// Gives one document operation a stable, independently owned cancellation identity.
#[derive(Clone)]
pub(crate) struct Cancellation(
    /// Allocation identity distinguishes sibling documents even when their revisions match.
    Arc<()>,
);

impl PartialEq for Cancellation {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for Cancellation {}

impl Hash for Cancellation {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.0).hash(state);
    }
}

/// Describes work to launch only after the transition has committed its state.
pub(crate) enum Request<Value> {
    /// Encodes and saves one independently owned snapshot.
    Save {
        /// Completion identity that must still be active when delivery arrives.
        attempt: Attempt,
        /// Local revision represented by this snapshot.
        revision: u64,
        /// Owned copy that permits later edits while backend work is pending.
        value: Value,
    },
    /// Delivers terminal barrier outcomes after state publication.
    Resolve(Vec<Resolution>),
}

/// Allocates attempt identities and shares coordination between document interpreters.
pub(crate) struct Transitions {
    /// Cancels replaced or aborted loads without affecting another document.
    pub(crate) load_id: Cancellation,
    /// Cancels aborted saves without cancelling their terminal acknowledgements.
    pub(crate) save_id: Cancellation,
    /// Last issued attempt; every new load or save uses checked advancement.
    pub(crate) next_attempt: u64,
}

impl Transitions {
    /// Starts a lifetime with distinct load and save cancellation identities.
    pub(crate) fn new() -> Self {
        Self {
            load_id: Cancellation(Arc::new(())),
            save_id: Cancellation(Arc::new(())),
            next_attempt: 0,
        }
    }

    /// Retains an explicit default without treating it as loaded or persisted.
    pub(crate) fn state<Value>(value: Value) -> State<Value> {
        State {
            value,
            revision: 0,
            acknowledged_revision: None,
            edited_revision: None,
            load_status: LoadStatus::Unloaded,
            active: None,
            failure: None,
            barriers: Vec::new(),
            closing: false,
            closed: false,
            aborted: false,
        }
    }

    /// Issues a new identity, panicking rather than wrapping into stale completion IDs.
    pub(crate) fn attempt(&mut self) -> Attempt {
        self.next_attempt = self
            .next_attempt
            .checked_add(1)
            .expect("document attempt counter exhausted");
        Attempt(self.next_attempt)
    }

    /// Starts or replaces initial loading unless the document is ready or aborted.
    pub(crate) fn load<Value>(&mut self, state: &mut State<Value>) -> Option<Attempt> {
        // Closing is only entered from Ready, which never transitions back.
        if state.load_status == LoadStatus::Ready || state.aborted {
            return None;
        }
        let attempt = self.attempt();
        state.load_status = LoadStatus::Loading;
        state.failure = None;
        state.active = Some(Active::Load(attempt));
        Some(attempt)
    }

    /// Captures the latest unacknowledged revision when no other operation is active.
    fn save<Value: Clone>(&mut self, state: &mut State<Value>) -> Option<Request<Value>> {
        if state.active.is_some() || state.acknowledged_revision == Some(state.revision) {
            return None;
        }
        let revision = state.revision;
        let attempt = self.attempt();
        state.active = Some(Active::Save { attempt, revision });
        Some(Request::Save {
            attempt,
            revision,
            value: state.value.clone(),
        })
    }

    /// Accepts a new edit and starts saving unless a prior failure requires explicit retry.
    pub(crate) fn set<Value: Clone>(
        &mut self,
        state: &mut State<Value>,
        value: &Value,
    ) -> Vec<Request<Value>> {
        if state.load_status != LoadStatus::Ready || state.closing || state.aborted {
            return Vec::new();
        }
        let revision = state
            .revision
            .checked_add(1)
            .expect("document revision counter exhausted");
        state.value = value.clone();
        state.revision = revision;
        state.edited_revision = Some(state.revision);
        if state.failure.is_some() {
            Vec::new()
        } else {
            self.save(state).into_iter().collect()
        }
    }

    /// Captures a revision, optionally fences edits, and retries failed persistence.
    pub(crate) fn barrier<Value: Clone>(
        &mut self,
        state: &mut State<Value>,
        closing: bool,
        reply: &Reply,
    ) -> Vec<Request<Value>> {
        if state.aborted || state.load_status != LoadStatus::Ready {
            return vec![Request::Resolve(Self::unavailable_barrier(state, reply))];
        }
        if state.closing && !closing {
            return vec![Request::Resolve(vec![(
                reply.clone(),
                Err(ReceiptError::Closing),
            )])];
        }
        if closing {
            state.closing = true;
        }
        state.barriers.push(Barrier {
            outcome: FlushOutcome {
                revision: state.revision,
                closing,
            },
            reply: reply.clone(),
        });
        state.failure = None;
        let resolutions = satisfy(state);
        let acknowledgement = (!resolutions.is_empty()).then_some(Request::Resolve(resolutions));
        acknowledgement
            .into_iter()
            .chain(self.save(state))
            .collect()
    }

    /// Rejects a barrier without save authority, preserving abort over unloaded status.
    pub(crate) fn unavailable_barrier<Value>(
        state: &State<Value>,
        reply: &Reply,
    ) -> Vec<Resolution> {
        let error = if state.aborted {
            ReceiptError::Aborted
        } else {
            ReceiptError::NotLoaded
        };
        vec![(reply.clone(), Err(error))]
    }

    /// Ends delivery permanently and fails every pending barrier without promising I/O quiescence.
    pub(crate) fn abort<Value>(state: &mut State<Value>) -> Vec<Resolution> {
        state.aborted = true;
        state.active = None;
        if state.load_status == LoadStatus::Loading {
            state.load_status = LoadStatus::Unloaded;
        }
        fail_barriers(state, &ReceiptError::Aborted)
    }

    /// Checks all identities before a save completion may change state or resolve barriers.
    pub(crate) fn is_saving<Value>(state: &State<Value>, attempt: Attempt, revision: u64) -> bool {
        !state.aborted
            && state.load_status == LoadStatus::Ready
            && state.active == Some(Active::Save { attempt, revision })
    }

    /// Commits a matching result, then resolves barriers before launching any newer snapshot.
    pub(crate) fn saved<Value: Clone>(
        &mut self,
        state: &mut State<Value>,
        attempt: Attempt,
        revision: u64,
        result: &Result<(), Failure>,
    ) -> Vec<Request<Value>> {
        if !Self::is_saving(state, attempt, revision) {
            return Vec::new();
        }
        match result {
            Ok(()) => {
                state.active = None;
                state.acknowledged_revision = Some(revision);
                if state.edited_revision == Some(revision) {
                    state.edited_revision = None;
                }
                state.failure = None;
                let resolutions = satisfy(state);
                let acknowledgement =
                    (!resolutions.is_empty()).then_some(Request::Resolve(resolutions));
                acknowledgement
                    .into_iter()
                    .chain(self.save(state))
                    .collect()
            }
            Err(failure) => vec![Request::Resolve(Self::failed_save(state, failure))],
        }
    }

    /// Commits a matching save failure without creating another save request.
    pub(crate) fn failed_save<Value>(
        state: &mut State<Value>,
        failure: &Failure,
    ) -> Vec<Resolution> {
        state.active = None;
        state.failure = Some(failure.clone());
        fail_barriers(state, &ReceiptError::Persistence(failure.clone()))
    }

    /// Accepts only the current initial load, retaining the default when storage is absent.
    pub(crate) fn loaded<Value: Clone>(
        state: &mut State<Value>,
        attempt: Attempt,
        result: &Result<Option<Value>, Failure>,
    ) {
        if state.aborted
            || state.load_status != LoadStatus::Loading
            || state.active != Some(Active::Load(attempt))
        {
            return;
        }
        state.active = None;
        match result {
            Ok(value) => {
                if let Some(value) = value {
                    state.value = value.clone();
                    state.acknowledged_revision = Some(0);
                }
                state.load_status = LoadStatus::Ready;
                state.failure = None;
            }
            Err(failure) => {
                state.load_status = LoadStatus::Unloaded;
                state.failure = Some(failure.clone());
            }
        }
    }

    /// Identifies local work that prevents an external snapshot from replacing the value.
    pub(crate) fn has_pending<Value>(state: &State<Value>) -> bool {
        state.edited_revision.is_some() || state.active.is_some() || !state.barriers.is_empty()
    }

    /// Swaps an admitted external snapshot without dropping user values under caller commit locks.
    ///
    /// The caller must exclude pending local work. The displaced value remains in
    /// `value` so the caller can release it after unlocking its commit fences.
    pub(crate) fn accept_external<Value>(state: &mut State<Value>, value: &mut Option<Value>) {
        debug_assert!(!Self::has_pending(state));
        if let Some(value) = value {
            std::mem::swap(&mut state.value, value);
            state.acknowledged_revision = Some(state.revision);
        } else {
            // The retained in-memory value is not materialised in absent storage.
            state.acknowledged_revision = None;
        }
        state.load_status = LoadStatus::Ready;
    }
}

/// Moves acknowledged barriers into delivery order and marks a satisfied close complete.
fn satisfy<Value>(state: &mut State<Value>) -> Vec<Resolution> {
    let Some(acknowledged) = state.acknowledged_revision else {
        return Vec::new();
    };
    if state.closing && acknowledged >= state.revision {
        state.closed = true;
    }
    // Barrier admission appends the current monotonic revision. If the first
    // waiter is not ready, no later waiter can be ready either.
    if state
        .barriers
        .first()
        .is_none_or(|barrier| acknowledged < barrier.outcome.revision)
    {
        return Vec::new();
    }
    let ready = state
        .barriers
        .partition_point(|barrier| acknowledged >= barrier.outcome.revision);
    if ready == state.barriers.len() {
        // A completed burst must not retain its peak waiter buffer while idle.
        return std::mem::take(&mut state.barriers)
            .into_iter()
            .map(|barrier| (barrier.reply, Ok(barrier.outcome)))
            .collect();
    }
    state
        .barriers
        .drain(..ready)
        .map(|barrier| (barrier.reply, Ok(barrier.outcome)))
        .collect()
}

/// Moves all pending replies into failed delivery without dropping their original action owners.
fn fail_barriers<Value>(state: &mut State<Value>, error: &ReceiptError) -> Vec<Resolution> {
    std::mem::take(&mut state.barriers)
        .into_iter()
        .map(|barrier| (barrier.reply, Err(error.clone())))
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn barriers_resolve_in_arrival_order_across_three_revisions() {
        let mut transitions = Transitions::new();
        let mut state = Transitions::state(7);
        let attempt = transitions.load(&mut state).unwrap();
        Transitions::loaded(&mut state, attempt, &Ok(Some(7)));
        drop(transitions.set(&mut state, &11));
        let first_save = state.active.unwrap().attempt();
        let (first, _first_receipt) = crate::receipt::channel();
        let (second, _second_receipt) = crate::receipt::channel();
        let (third, _third_receipt) = crate::receipt::channel();
        let (close, _close_receipt) = crate::receipt::channel();
        drop(transitions.barrier(&mut state, false, &first));
        drop(transitions.barrier(&mut state, false, &second));
        drop(transitions.set(&mut state, &13));
        drop(transitions.barrier(&mut state, false, &third));
        let capacity = state.barriers.capacity();

        let requests = transitions.saved(&mut state, first_save, 1, &Ok(()));
        let [
            Request::Resolve(resolutions),
            Request::Save {
                revision: 2,
                value: 13,
                ..
            },
        ] = requests.as_slice()
        else {
            panic!("acknowledgement must precede the next snapshot");
        };
        assert_eq!(resolutions.len(), 2);
        for ((reply, result), expected_reply) in resolutions.iter().zip([&first, &second]) {
            assert!(reply.same_channel(expected_reply));
            assert_eq!(
                result.as_ref().unwrap(),
                &FlushOutcome {
                    revision: 1,
                    closing: false
                }
            );
        }
        assert_eq!(state.barriers.len(), 1);
        assert!(state.barriers[0].reply.same_channel(&third));
        assert_eq!(state.barriers.capacity(), capacity);
        assert!(!state.closed);
        let second_save = state.active.unwrap().attempt();
        drop(transitions.set(&mut state, &17));
        drop(transitions.barrier(&mut state, true, &close));

        let requests = transitions.saved(&mut state, second_save, 2, &Ok(()));
        let [
            Request::Resolve(resolutions),
            Request::Save {
                revision: 3,
                value: 17,
                ..
            },
        ] = requests.as_slice()
        else {
            panic!("later close must not erase the earlier barrier's acknowledgement");
        };
        assert_eq!(resolutions.len(), 1);
        assert!(resolutions[0].0.same_channel(&third));
        assert_eq!(
            resolutions[0].1.as_ref().unwrap(),
            &FlushOutcome {
                revision: 2,
                closing: false
            }
        );
        assert_eq!(state.barriers.len(), 1);
        assert!(state.barriers[0].reply.same_channel(&close));
        assert!(!state.closed);

        let third_save = state.active.unwrap().attempt();
        let requests = transitions.saved(&mut state, third_save, 3, &Ok(()));
        let [Request::Resolve(resolutions)] = requests.as_slice() else {
            panic!("final acknowledgement must not launch another save");
        };
        assert_eq!(resolutions.len(), 1);
        assert!(resolutions[0].0.same_channel(&close));
        assert_eq!(
            resolutions[0].1.as_ref().unwrap(),
            &FlushOutcome {
                revision: 3,
                closing: true
            }
        );
        assert!(state.barriers.is_empty());
        assert!(state.closed);
        assert!(state.active.is_none());
        assert_eq!(state.barriers.capacity(), 0);
    }

    #[test]
    fn failed_save_releases_waiter_buffer_and_retains_original_cause() {
        let mut transitions = Transitions::new();
        let mut state = Transitions::state(7);
        let attempt = transitions.load(&mut state).unwrap();
        Transitions::loaded(&mut state, attempt, &Ok(Some(7)));
        drop(transitions.set(&mut state, &11));
        let (first, _first_receipt) = crate::receipt::channel();
        let (second, _second_receipt) = crate::receipt::channel();
        drop(transitions.barrier(&mut state, false, &first));
        drop(transitions.barrier(&mut state, true, &second));
        let failure = Failure::new(crate::document::Operation::Save, Some(1), std::fmt::Error);
        let attempt = state.active.unwrap().attempt();
        let requests = transitions.saved(&mut state, attempt, 1, &Err(failure.clone()));

        assert!(state.barriers.is_empty());
        assert_eq!(state.barriers.capacity(), 0);
        assert!(state.closing);
        assert!(!state.closed);
        assert!(state.active.is_none());
        assert!(std::ptr::eq(
            state.failure.as_ref().unwrap().error(),
            failure.error()
        ));
        let [Request::Resolve(resolutions)] = requests.as_slice() else {
            panic!("failed save must resolve all waiters without automatically retrying");
        };
        assert_eq!(resolutions.len(), 2);
        for ((reply, result), expected_reply) in resolutions.iter().zip([&first, &second]) {
            assert!(reply.same_channel(expected_reply));
            let Err(ReceiptError::Persistence(cause)) = result else {
                panic!("failed save must retain its persistence failure");
            };
            assert!(std::ptr::eq(cause.error(), failure.error()));
        }
    }

    /// Runs inside the existing isolated allocation test with its profiler active.
    #[cfg(feature = "file")]
    pub(crate) fn measure_transition_allocations() {
        println!(
            "document layout active={} optional_active={} state_i64={}",
            std::mem::size_of::<Active>(),
            std::mem::size_of::<Option<Active>>(),
            std::mem::size_of::<State<i64>>(),
        );
        for waiters in [0, 1, 16, 256] {
            let initial = dhat::HeapStats::get();
            let mut transitions = Transitions::new();
            let mut state = Transitions::state(7);
            let attempt = transitions.load(&mut state).unwrap();
            Transitions::loaded(&mut state, attempt, &Ok(Some(7)));
            drop(transitions.set(&mut state, &11));
            let active = state.active.unwrap();
            drop(transitions.set(&mut state, &13));
            let (reply, receipt) = crate::receipt::channel();
            let mut receipts = Vec::new();
            for _ in 0..waiters {
                let (reply, receipt) = crate::receipt::channel();
                drop(transitions.barrier(&mut state, false, &reply));
                receipts.push(receipt);
            }

            // Revision one cannot resolve barriers that captured revision two.
            let before = dhat::HeapStats::get();
            let requests = transitions.saved(&mut state, active.attempt(), 1, &Ok(()));
            let after = dhat::HeapStats::get();
            assert!(matches!(
                requests.as_slice(),
                [Request::Save {
                    revision: 2,
                    value: 13,
                    ..
                }]
            ));
            assert_eq!(state.barriers.len(), waiters);
            let save_allocations = after.total_blocks - before.total_blocks;
            assert_eq!(
                save_allocations, 1,
                "only the outward save request allocates"
            );
            drop(requests);

            // This flush joins an active save; it must not emit another save.
            state.barriers.reserve(1);
            let before = dhat::HeapStats::get();
            let requests = transitions.barrier(&mut state, false, &reply);
            let after = dhat::HeapStats::get();
            assert!(
                requests.is_empty(),
                "joining a save needs no immediate work"
            );
            assert_eq!(state.barriers.len(), waiters + 1);
            let barrier_allocations = after.total_blocks - before.total_blocks;
            assert_eq!(
                barrier_allocations, 0,
                "spare waiter capacity must be reused"
            );
            drop(requests);

            let waiting_bytes = state.barriers.capacity() * std::mem::size_of::<Barrier>();
            let active = state.active.unwrap();
            drop(transitions.saved(&mut state, active.attempt(), 2, &Ok(())));
            assert!(state.barriers.is_empty());
            let idle_bytes = state.barriers.capacity() * std::mem::size_of::<Barrier>();
            assert_eq!(
                idle_bytes, 0,
                "completed bursts must not retain waiter capacity"
            );
            let before_release = dhat::HeapStats::get();
            drop(std::mem::take(&mut state.barriers));
            let released_bytes = before_release.curr_bytes - dhat::HeapStats::get().curr_bytes;
            assert_eq!(released_bytes, idle_bytes);
            drop(state);
            drop(transitions);
            drop(reply);
            drop(receipt);
            drop(receipts);
            let released = dhat::HeapStats::get();
            assert_eq!(released.curr_blocks, initial.curr_blocks);
            assert_eq!(released.curr_bytes, initial.curr_bytes);
            println!(
                "document transitions waiters={waiters} coalesced_save_allocations={save_allocations} waiting_barrier_allocations={barrier_allocations} waiting_buffer_bytes={waiting_bytes} idle_buffer_bytes={idle_bytes} completion_released_bytes={released_bytes}"
            );
        }
    }

    #[test]
    fn active_initial_load_counts_as_pending_without_local_edits() {
        let mut transitions = Transitions::new();
        let mut state = Transitions::state(7);
        assert!(!Transitions::has_pending(&state));

        let attempt = transitions.load(&mut state).unwrap();
        assert_eq!(state.value, 7);
        assert!(state.edited_revision.is_none());
        assert!(Transitions::has_pending(&state));

        Transitions::loaded(&mut state, attempt, &Ok(Some(11)));
        assert_eq!(state.value, 11);
        assert!(!Transitions::has_pending(&state));
    }
}
