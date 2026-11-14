//! Test-only exact-call control for the existing owned Snapshot workload.
//!
//! This child module wraps the private state and history, then delegates to Worker.
//! Its counters do not enter the normal measured executable or its allocation phases.
//!
//! Contract derived before the initial RED from current `TestStore`:
//! - Construction: retained first snapshot + builder baseline = 2 projections;
//!   the declaration invokes the private capture once. No history clone/eq.
//! - Each send: 1 baseline clone, 1 committed projection, 1 root comparison;
//!   preflight + committed boundary invoke the private capture twice.
//! - The workload itself compares retained first history twice per iteration.
//!   `CountedHistory` must include those calls: 3 total comparisons per send.
//! - Causal finish and explicit live-state inspection add none of these calls.
//! - `TestStore` finish adds 1 private capture, no root projection/clone/eq.
//!   Dropping an already-finished store adds none.
//!
//! The wrappers retain the actual State, manual Snapshot and Worker rather than
//! copying `TestStore`'s assertion algorithm. Assertions still check actual owned
//! history, private values and authority payload; counters do not replace work.

#![cfg(test)]

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use rca_core::{Effect, ReduceContext, Reducer, Snapshot};
use rca_test::TestStore;

use super::{Action, Authority, History, Private, Probes, State, Worker};
use crate::finite_drive::finish;

#[derive(Debug, PartialEq, Eq)]
struct Calls {
    projections: usize,
    history_clones: usize,
    history_equalities: usize,
    private_captures: usize,
}

#[derive(Debug, PartialEq, Eq)]
struct Observed {
    constructed: Calls,
    worked: Calls,
    finished_and_dropped: Calls,
    commits: usize,
    causal_finishes: usize,
    history_length: usize,
    checksum: usize,
    first_history_unchanged: bool,
    authority_drops: usize,
    authority_alive: usize,
}

#[derive(Debug, Default)]
struct Counters {
    projections: AtomicUsize,
    history_clones: AtomicUsize,
    history_equalities: AtomicUsize,
    private_captures: AtomicUsize,
}

impl Counters {
    fn read(&self) -> Calls {
        Calls {
            projections: self.projections.load(Ordering::SeqCst),
            history_clones: self.history_clones.load(Ordering::SeqCst),
            history_equalities: self.history_equalities.load(Ordering::SeqCst),
            private_captures: self.private_captures.load(Ordering::SeqCst),
        }
    }
}

struct CountedState {
    inner: State,
    counters: Arc<Counters>,
}

#[derive(Debug)]
struct CountedHistory {
    inner: History,
    counters: Arc<Counters>,
}

impl Snapshot for CountedState {
    type Value = CountedHistory;

    fn snapshot(&self) -> CountedHistory {
        self.counters.projections.fetch_add(1, Ordering::SeqCst);
        CountedHistory {
            inner: self.inner.snapshot(),
            counters: Arc::clone(&self.counters),
        }
    }
}

impl Clone for CountedHistory {
    fn clone(&self) -> Self {
        self.counters.history_clones.fetch_add(1, Ordering::SeqCst);
        Self {
            inner: self.inner.clone(),
            counters: Arc::clone(&self.counters),
        }
    }
}

impl PartialEq for CountedHistory {
    fn eq(&self, other: &Self) -> bool {
        self.counters
            .history_equalities
            .fetch_add(1, Ordering::SeqCst);
        self.inner == other.inner
    }
}

impl Eq for CountedHistory {}

struct CountedWorker(Worker);

impl Reducer for CountedWorker {
    type State = CountedState;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut CountedState,
        action: &Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        self.0.reduce(&mut state.inner, action, context)
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "Keeps count checkpoints and owner destruction in one sequential control fixture."
)]
fn observe(iterations: usize) -> Result<Observed, &'static str> {
    let counters = Arc::new(Counters::default());
    let probes = Arc::new(Probes::default());
    let authority = Arc::new(Authority {
        probes: Arc::clone(&probes),
        bytes: Box::new([7; 64]),
    });
    let weak = Arc::downgrade(&authority);
    let state = CountedState {
        inner: State {
            progress: 0,
            history: vec![0],
            private: std::cell::Cell::new(0),
            authority,
        },
        counters: Arc::clone(&counters),
    };
    let first = state.snapshot();
    let mut store = TestStore::projected_builder(state, CountedWorker(Worker), Snapshot::snapshot)
        .with_capture::<Private>(|state| {
            state
                .counters
                .private_captures
                .fetch_add(1, Ordering::SeqCst);
            Ok(state.inner.private.get())
        })
        .map_err(|_| "counted snapshot declaration failed")?
        .build();
    let constructed = counters.read();
    let mut causal_finishes = 0;
    for step in 1..=iterations {
        let task = finish(store.send_with_captures(
            Action::Advance,
            |history| {
                assert_eq!(history.inner.progress, step - 1);
                assert_eq!(history.inner.history, (0..step).collect::<Vec<_>>());
                history.inner.progress = step;
                history.inner.history.push(step);
                assert_eq!(probes.commits.load(Ordering::SeqCst), step - 1);
                assert_eq!(
                    first,
                    CountedHistory {
                        inner: History {
                            progress: 0,
                            history: vec![0]
                        },
                        counters: Arc::clone(&counters),
                    }
                );
                probes.history_checks.fetch_add(1, Ordering::SeqCst);
            },
            |captures| {
                captures.update::<Private>(|expected| {
                    assert_eq!(*expected, step - 1);
                    *expected = step;
                    assert_eq!(probes.commits.load(Ordering::SeqCst), step - 1);
                    probes.capture_checks.fetch_add(1, Ordering::SeqCst);
                })
            },
        ))?
        .map_err(|_| "counted snapshot transaction failed")?;
        finish(task.finish())?.map_err(|_| "counted snapshot causal finish failed")?;
        causal_finishes += 1;
        drop(task);
        let checked = Arc::clone(&probes);
        finish(store.assert_state(move |state| {
            let state = &state.inner;
            assert_eq!(state.progress, step);
            assert_eq!(state.history, (0..=step).collect::<Vec<_>>());
            assert_eq!(state.private.get(), step);
            assert_eq!(*state.authority.bytes, [7; 64]);
            checked.checksum.fetch_add(state.progress, Ordering::SeqCst);
            checked
                .history_length
                .store(state.history.len(), Ordering::SeqCst);
            checked.source_checks.fetch_add(1, Ordering::SeqCst);
        }))?
        .map_err(|_| "counted snapshot source inspection failed")?;
        assert_eq!(
            first,
            CountedHistory {
                inner: History {
                    progress: 0,
                    history: vec![0]
                },
                counters: Arc::clone(&counters),
            }
        );
    }
    let worked = counters.read();
    assert_eq!(probes.capture_checks.load(Ordering::SeqCst), iterations);
    assert_eq!(probes.history_checks.load(Ordering::SeqCst), iterations);
    assert_eq!(probes.source_checks.load(Ordering::SeqCst), iterations);
    let commits = probes.commits.load(Ordering::SeqCst);
    let history_length = probes.history_length.load(Ordering::SeqCst);
    let checksum = probes.checksum.load(Ordering::SeqCst);
    let first_history_unchanged = first.inner.progress == 0 && first.inner.history == [0];
    finish(store.finish())?.map_err(|_| "counted snapshot TestStore finish failed")?;
    drop(store);
    // Observe the actual authority's destruction while retained history is alive.
    let authority_drops = probes.dropped.load(Ordering::SeqCst);
    let authority_alive = weak.strong_count();
    drop(first);
    drop(weak);
    assert_eq!(Arc::strong_count(&probes), 1);
    assert_eq!(Arc::strong_count(&counters), 1);
    Ok(Observed {
        constructed,
        worked,
        finished_and_dropped: counters.read(),
        commits,
        causal_finishes,
        history_length,
        checksum,
        first_history_unchanged,
        authority_drops,
        authority_alive,
    })
}

#[test]
fn one_send_counts_projection_clone_comparison_and_private_capture_boundaries() {
    assert_eq!(
        observe(1),
        Ok(Observed {
            constructed: Calls {
                projections: 2,
                history_clones: 0,
                history_equalities: 0,
                private_captures: 1,
            },
            worked: Calls {
                projections: 3,
                history_clones: 1,
                history_equalities: 3,
                private_captures: 3,
            },
            finished_and_dropped: Calls {
                projections: 3,
                history_clones: 1,
                history_equalities: 3,
                private_captures: 4,
            },
            commits: 1,
            causal_finishes: 1,
            history_length: 2,
            checksum: 1,
            first_history_unchanged: true,
            authority_drops: 1,
            authority_alive: 0,
        }),
    );
}

#[test]
fn repeated_sends_count_each_boundary_without_extra_history_work() {
    assert_eq!(
        observe(64),
        Ok(Observed {
            constructed: Calls {
                projections: 2,
                history_clones: 0,
                history_equalities: 0,
                private_captures: 1,
            },
            worked: Calls {
                projections: 66,
                history_clones: 64,
                history_equalities: 192,
                private_captures: 129,
            },
            finished_and_dropped: Calls {
                projections: 66,
                history_clones: 64,
                history_equalities: 192,
                private_captures: 130,
            },
            commits: 64,
            causal_finishes: 64,
            history_length: 65,
            checksum: 2080,
            first_history_unchanged: true,
            authority_drops: 1,
            authority_alive: 0,
        }),
    );
}
