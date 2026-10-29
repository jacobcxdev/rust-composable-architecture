//! Owned Snapshot cost through an existing projected `TestStore` consumer.
//!
//! History omits the live authority and the private captured value.
//! The work phase includes expected-history allocation and live-state assertions.
//! Retained history must stay unchanged and must not keep the authority alive.
//! The separate test-only control counts projections, clones, comparisons and captures.

#[cfg(test)]
mod count_control;

use std::cell::Cell;
use std::convert::Infallible;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use rca_core::{Effect, ReduceContext, Reducer, Snapshot};
use rca_test::{CaptureKey, TestStore};

use crate::{finite_drive::finish, measurement::Measurement};

const INVALID: &str = "owned snapshot work or ownership invalid";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fault {
    None,
    #[cfg(test)]
    SkipCapture,
    #[cfg(test)]
    RetainAuthority,
}

#[derive(Default)]
struct Probes {
    commits: AtomicUsize,
    capture_checks: AtomicUsize,
    history_checks: AtomicUsize,
    source_checks: AtomicUsize,
    history_length: AtomicUsize,
    checksum: AtomicUsize,
    dropped: AtomicUsize,
}

// This is the actual live authority payload, deliberately absent from history.
struct Authority {
    probes: Arc<Probes>,
    bytes: Box<[u8; 64]>,
}

impl Drop for Authority {
    fn drop(&mut self) {
        self.probes.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

struct State {
    progress: usize,
    history: Vec<usize>,
    private: Cell<usize>,
    authority: Arc<Authority>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct History {
    progress: usize,
    history: Vec<usize>,
}

impl Snapshot for State {
    type Value = History;
    fn snapshot(&self) -> History {
        History {
            progress: self.progress,
            history: self.history.clone(),
        }
    }
}

struct Private;

impl CaptureKey for Private {
    type Value = usize;
    type Error = Infallible;
}

enum Action {
    Advance,
}

struct Worker;

impl Reducer for Worker {
    type State = State;
    type Action = Action;
    fn reduce(
        &mut self,
        state: &mut State,
        _: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        state.progress += 1;
        state.history.push(state.progress);
        state.private.set(state.private.get() + 1);
        state
            .authority
            .probes
            .commits
            .fetch_add(1, Ordering::SeqCst);
        Effect::none()
    }
}

pub(crate) fn run(args: impl Iterator<Item = String>) -> Result<String, &'static str> {
    run_fault(args, Fault::None)
}

#[expect(
    clippy::too_many_lines,
    reason = "Keeps measured construction, work, validation and destruction in one profiling frame."
)]
fn run_fault(mut args: impl Iterator<Item = String>, fault: Fault) -> Result<String, &'static str> {
    if args.next().as_deref() != Some("owned-snapshot") || args.next().as_deref() != Some("1") {
        return Err("invalid owned snapshot arguments");
    }
    let iterations = match args.next().as_deref() {
        Some("1") => 1,
        Some("64") => 64,
        _ => return Err("invalid owned snapshot arguments"),
    };
    if args.next().is_some() {
        return Err("invalid owned snapshot arguments");
    }
    drop(args);

    let mut measurement = Measurement::new(None);
    let probes = Arc::new(Probes::default());
    let authority = Arc::new(Authority {
        probes: Arc::clone(&probes),
        bytes: Box::new([7; 64]),
    });
    let weak = Arc::downgrade(&authority);
    #[cfg(test)]
    let retained = (fault == Fault::RetainAuthority).then(|| Arc::clone(&authority));
    let state = State {
        progress: 0,
        history: vec![0],
        private: Cell::new(0),
        authority,
    };
    let first = state.snapshot();
    let mut store = TestStore::projected_builder(state, Worker, Snapshot::snapshot)
        .with_capture::<Private>(|state| Ok(state.private.get()))
        .map_err(|_| "owned snapshot capture declaration failed")?
        .build();
    measurement.construction_finished();

    let mut finishes = 0;
    for step in 1..=iterations {
        #[cfg(test)]
        if fault == Fault::SkipCapture {
            continue;
        }
        let task = finish(store.send_with_captures(
            Action::Advance,
            |history| {
                assert_eq!(history.progress, step - 1);
                assert_eq!(history.history, (0..step).collect::<Vec<_>>());
                history.progress = step;
                history.history.push(step);
                // The expectation callback runs before reduction, and its owned
                // history edits cannot change the live source transaction count.
                assert_eq!(probes.commits.load(Ordering::SeqCst), step - 1);
                assert_eq!(
                    first,
                    History {
                        progress: 0,
                        history: vec![0]
                    }
                );
                probes.history_checks.fetch_add(1, Ordering::SeqCst);
            },
            |captures| {
                captures.update::<Private>(|expected: &mut usize| {
                    assert_eq!(*expected, step - 1);
                    *expected = step;
                    assert_eq!(probes.commits.load(Ordering::SeqCst), step - 1);
                    probes.capture_checks.fetch_add(1, Ordering::SeqCst);
                })
            },
        ))?
        .map_err(|_| "owned snapshot transaction failed")?;
        finish(task.finish())?.map_err(|_| "owned snapshot causal finish failed")?;
        finishes += 1;
        drop(task);
        let checked = Arc::clone(&probes);
        finish(store.assert_state(move |state| {
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
        .map_err(|_| "owned snapshot source inspection failed")?;
        assert_eq!(
            first,
            History {
                progress: 0,
                history: vec![0]
            }
        );
    }
    measurement.work_finished();

    let commits = probes.commits.load(Ordering::SeqCst);
    let captures = probes.capture_checks.load(Ordering::SeqCst);
    let histories = probes.history_checks.load(Ordering::SeqCst);
    let source_checks = probes.source_checks.load(Ordering::SeqCst);
    let checksum = probes.checksum.load(Ordering::SeqCst);
    let history_length = probes.history_length.load(Ordering::SeqCst);
    let first_unchanged = first.progress == 0 && first.history == [0];
    let wanted_checksum = if iterations == 1 { 1 } else { 2080 };
    let work_valid = commits == iterations
        && finishes == iterations
        && captures == iterations
        && histories == iterations
        && source_checks == iterations
        && checksum == wanted_checksum
        && history_length == iterations + 1
        && first_unchanged;
    measurement.validation_finished();

    finish(store.finish())?.map_err(|_| "owned snapshot TestStore finish failed")?;
    drop(store);
    let dropped = probes.dropped.load(Ordering::SeqCst);
    let alive = weak.strong_count();
    let ownership_valid = dropped == 1 && alive == 0;
    #[cfg(test)]
    drop(retained);
    #[cfg(not(test))]
    let _ = fault;
    drop(first);
    drop(weak);
    let probe_owners_valid = Arc::strong_count(&probes) == 1;
    drop(probes);
    let allocations = measurement.finish()?;
    if !work_valid || !ownership_valid || !probe_owners_valid {
        return Err(INVALID);
    }
    let allocations = allocations.map_or_else(|| "null".to_owned(), |record| record.json());
    Ok(format!(
        "{{\"schema\":\"rca-performance-profile-v2\",\"workload\":\"owned-snapshot\",\"backend\":\"deterministic-shared\",\"size\":1,\"iterations\":{iterations},\"value\":{commits},\"history_length\":{history_length},\"checksum\":{checksum},\"first_history_unchanged\":{first_unchanged},\"useful_work\":{{\"commits\":{commits},\"causal_finishes\":{finishes},\"private_capture_checks\":{captures},\"independent_history_checks\":{histories}}},\"ownership\":{{\"created\":1,\"dropped\":{dropped},\"alive\":{alive}}},\"allocations\":{allocations}}}",
    ))
}

#[cfg(test)]
fn run_control(fault: &str) -> Result<String, &'static str> {
    let fault = match fault {
        "skip-capture" => Fault::SkipCapture,
        "retain-authority" => Fault::RetainAuthority,
        _ => return Err("unknown owned snapshot control"),
    };
    run_fault(
        ["owned-snapshot", "1", "1"].map(str::to_owned).into_iter(),
        fault,
    )
}

#[cfg(test)]
mod tests {
    use super::{run, run_control};

    #[test]
    fn rejects_skipped_capture_and_retained_authority() {
        #[cfg(feature = "allocations")]
        if !crate::tests::in_child(
            "owned_snapshot::tests::rejects_skipped_capture_and_retained_authority",
        ) {
            return;
        }
        for fault in ["skip-capture", "retain-authority"] {
            assert_eq!(
                run_control(fault).err(),
                Some("owned snapshot work or ownership invalid")
            );
        }
    }

    #[test]
    fn owned_history_captures_private_mutation_without_retaining_authority() {
        #[cfg(feature = "allocations")]
        if !crate::tests::in_child(
            "owned_snapshot::tests::owned_history_captures_private_mutation_without_retaining_authority",
        ) {
            return;
        }
        for (iterations, checksum) in [(1, 1), (64, 2080)] {
            let result =
                run(["owned-snapshot".into(), "1".into(), iterations.to_string()].into_iter())
                    .unwrap();
            let result: serde_json::Value = serde_json::from_str(&result).unwrap();
            assert_eq!(result["useful_work"]["commits"], iterations);
            assert_eq!(result["useful_work"]["causal_finishes"], iterations);
            assert_eq!(result["useful_work"]["private_capture_checks"], iterations);
            assert_eq!(
                result["useful_work"]["independent_history_checks"],
                iterations
            );
            assert_eq!(result["value"], iterations);
            assert_eq!(result["history_length"], iterations + 1);
            assert_eq!(result["checksum"], checksum);
            assert_eq!(result["first_history_unchanged"], true);
            assert_eq!(result["ownership"]["created"], 1);
            assert_eq!(result["ownership"]["dropped"], 1);
            assert_eq!(result["ownership"]["alive"], 0);
        }
    }

    #[test]
    fn rejects_unqualified_work_sizes() {
        for args in [
            ["owned-snapshot", "0", "1"],
            ["owned-snapshot", "2", "1"],
            ["owned-snapshot", "1", "0"],
            ["owned-snapshot", "1", "2"],
        ] {
            assert_eq!(
                run(args.map(str::to_owned).into_iter()),
                Err("invalid owned snapshot arguments")
            );
        }
    }
}
