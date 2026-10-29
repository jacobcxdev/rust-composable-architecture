//! Bounded checkpoint, captured-graph, and structural-diagnostic profiling seed.

use std::cell::Cell;

use rca_core::diagnostics::{
    CaptureLimits, CapturedGraph, CapturedNode, Checkpoint, CheckpointOutcome, DiagnosticLimits,
    NodeCapture, ReferenceKind, StructuralView, ViewChange, capture_graph, checkpoint,
    checkpoint_disabled, diff_graph_views, write_graph_view,
};

use crate::measurement::Measurement;

const OUTPUT: &str = "[v, &1([v, *0]), *1]";

/// Selects explicit disabling separately from release-build checkpoint elision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    Observed,
    Disabled,
}

/// Admits only the fixed graph and a bounded number of root increments.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Config {
    mode: Mode,
    iterations: usize,
}

impl Config {
    pub(crate) fn parse(mut args: impl Iterator<Item = String>) -> Result<Self, &'static str> {
        let mode = match args.next().as_deref() {
            Some("diagnostic-observed") => Mode::Observed,
            Some("diagnostic-disabled") => Mode::Disabled,
            _ => return Err("unknown diagnostic workload"),
        };
        if args.next().as_deref() != Some("2") {
            return Err("diagnostic size must be 2");
        }
        let text = args.next().ok_or("missing diagnostic iterations")?;
        if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err("invalid diagnostic iterations");
        }
        let iterations = text.parse().map_err(|_| "diagnostic iterations overflow")?;
        if !(1..=1024).contains(&iterations) || args.next().is_some() {
            return Err("diagnostic iterations outside 1..=1024 or extra arguments");
        }
        Ok(Self { mode, iterations })
    }
}

/// Copies measured hook counts for validation after the work boundary.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Facts {
    operation_calls: usize,
    capture_calls: usize,
    node_capture_calls: usize,
    sink_calls: usize,
    checkpoint_changes: usize,
    checkpoint_successes: usize,
    equality_calls: usize,
    project_calls: usize,
    redaction_calls: usize,
    semantic_changes: usize,
    view_changes: usize,
    value_changes: usize,
    total_changes: usize,
    leaf_constructions: usize,
    leaf_drops: usize,
    written_bytes: usize,
}

/// Counts harness and diagnostic work without allocating an event history.
#[derive(Default)]
struct Counters {
    operation_calls: Cell<usize>,
    capture_calls: Cell<usize>,
    node_capture_calls: Cell<usize>,
    sink_calls: Cell<usize>,
    checkpoint_changes: Cell<usize>,
    checkpoint_successes: Cell<usize>,
    equality_calls: Cell<usize>,
    project_calls: Cell<usize>,
    redaction_calls: Cell<usize>,
    semantic_changes: Cell<usize>,
    view_changes: Cell<usize>,
    value_changes: Cell<usize>,
    total_changes: Cell<usize>,
    leaf_constructions: Cell<usize>,
    leaf_drops: Cell<usize>,
    written_bytes: Cell<usize>,
}

fn add(counter: &Cell<usize>, amount: usize) {
    counter.set(counter.get() + amount);
}

impl Counters {
    fn snapshot(&self) -> Facts {
        Facts {
            operation_calls: self.operation_calls.get(),
            capture_calls: self.capture_calls.get(),
            node_capture_calls: self.node_capture_calls.get(),
            sink_calls: self.sink_calls.get(),
            checkpoint_changes: self.checkpoint_changes.get(),
            checkpoint_successes: self.checkpoint_successes.get(),
            equality_calls: self.equality_calls.get(),
            project_calls: self.project_calls.get(),
            redaction_calls: self.redaction_calls.get(),
            semantic_changes: self.semantic_changes.get(),
            view_changes: self.view_changes.get(),
            value_changes: self.value_changes.get(),
            total_changes: self.total_changes.get(),
            leaf_constructions: self.leaf_constructions.get(),
            leaf_drops: self.leaf_drops.get(),
            written_bytes: self.written_bytes.get(),
        }
    }
}

impl Facts {
    fn json(self) -> String {
        format!(
            concat!(
                "{{\"operation_calls\":{},\"capture_calls\":{},\"node_capture_calls\":{},",
                "\"sink_calls\":{},\"checkpoint_changes\":{},\"checkpoint_successes\":{},",
                "\"equality_calls\":{},\"project_calls\":{},\"redaction_calls\":{},",
                "\"semantic_changes\":{},\"view_changes\":{},\"value_changes\":{},",
                "\"total_changes\":{},\"leaf_constructions\":{},\"leaf_drops\":{},\"written_bytes\":{}}}"
            ),
            self.operation_calls,
            self.capture_calls,
            self.node_capture_calls,
            self.sink_calls,
            self.checkpoint_changes,
            self.checkpoint_successes,
            self.equality_calls,
            self.project_calls,
            self.redaction_calls,
            self.semantic_changes,
            self.view_changes,
            self.value_changes,
            self.total_changes,
            self.leaf_constructions,
            self.leaf_drops,
            self.written_bytes,
        )
    }
}

/// Witnesses snapshot equality and destruction without changing value comparison.
struct Leaf<'a> {
    value: usize,
    counters: &'a Counters,
}

impl PartialEq for Leaf<'_> {
    fn eq(&self, other: &Self) -> bool {
        add(&self.counters.equality_calls, 1);
        self.value == other.value
    }
}

impl Drop for Leaf<'_> {
    fn drop(&mut self) {
        add(&self.counters.leaf_drops, 1);
    }
}

/// Proves that checkpoint calls borrow, rather than consume, callback resources.
struct Owner<'a>(&'a Cell<usize>);

impl Drop for Owner<'_> {
    fn drop(&mut self) {
        add(self.0, 1);
    }
}

type Graph<'a> = CapturedGraph<usize, Leaf<'a>>;
type Capture<'a> = Result<Graph<'a>, &'static str>;

/// Owns the capture probe across every borrowed checkpoint call.
struct CaptureCallback<'a> {
    _owner: Owner<'a>,
    counters: &'a Counters,
}

impl<'a> CaptureCallback<'a> {
    fn capture(&self, state: &[usize; 2]) -> Capture<'a> {
        add(&self.counters.capture_calls, 1);
        capture_graph(0, CaptureLimits::new(2, 3), |&origin| {
            add(&self.counters.node_capture_calls, 1);
            add(&self.counters.leaf_constructions, 1);
            NodeCapture::new(
                Leaf {
                    value: state[origin],
                    counters: self.counters,
                },
                if origin == 0 {
                    vec![("a".to_owned(), 1), ("b".to_owned(), 1)]
                } else {
                    vec![("back".to_owned(), 0)]
                },
            )
        })
        .map_err(|_| "diagnostic capture failed")
    }
}

/// Exposes each captured value and edge to the structural diagnostic APIs.
fn project<'graph, 'counters>(
    node: &'graph CapturedNode<usize, Leaf<'counters>>,
) -> StructuralView<(), &'graph Leaf<'counters>> {
    add(&node.leaves().counters.project_calls, 1);
    let mut children = Vec::with_capacity(node.edges().len() + 1);
    children.push(StructuralView::Value(node.leaves()));
    children.extend(
        node.edges()
            .iter()
            .map(|edge| StructuralView::Reference(edge.target())),
    );
    StructuralView::Sequence(children)
}

/// Records the fixed graph without retaining snapshots or allocating history.
///
/// Counts and label lengths preserve evidence of values beyond the storage bounds.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Shape {
    node_count: usize,
    origins: [usize; 2],
    values: [usize; 2],
    edge_counts: [usize; 2],
    targets: [usize; 3],
    label_lengths: [usize; 3],
    labels: [[u8; 4]; 3],
    kinds: [Option<ReferenceKind>; 3],
}

impl Shape {
    fn observe(graph: &Graph<'_>) -> Self {
        let mut shape = Self {
            node_count: graph.nodes().len(),
            ..Self::default()
        };
        let mut edge_index = 0;
        for (index, node) in graph.nodes().iter().take(2).enumerate() {
            shape.origins[index] = *node.origin();
            shape.values[index] = node.leaves().value;
            shape.edge_counts[index] = node.edges().len();
            for edge in node.edges() {
                if edge_index == 3 {
                    break;
                }
                shape.targets[edge_index] = edge.target();
                shape.label_lengths[edge_index] = edge.label().len();
                for (slot, byte) in shape.labels[edge_index]
                    .iter_mut()
                    .zip(edge.label().bytes())
                {
                    *slot = byte;
                }
                shape.kinds[edge_index] = Some(edge.kind());
                edge_index += 1;
            }
        }
        shape
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Pair {
    before: Shape,
    after: Shape,
}

/// Retains final observations and errors for checking after measured work.
struct SinkCallback<'a> {
    _owner: Owner<'a>,
    counters: &'a Counters,
    output: String,
    last: Option<Pair>,
    error: Option<&'static str>,
}

impl SinkCallback<'_> {
    fn observe(&mut self, event: &Checkpoint<'_, Capture<'_>>) {
        if let Err(error) = self.try_observe(event) {
            self.error = Some(error);
        }
    }

    fn try_observe(&mut self, event: &Checkpoint<'_, Capture<'_>>) -> Result<(), &'static str> {
        add(&self.counters.sink_calls, 1);
        add(
            &self.counters.checkpoint_changes,
            usize::from(event.is_changed()),
        );
        add(
            &self.counters.checkpoint_successes,
            usize::from(event.outcome() == CheckpointOutcome::Success),
        );
        let before = event.before().as_ref().map_err(|error| *error)?;
        let after = event.after().as_ref().map_err(|error| *error)?;
        // Fixed storage records only the final pair, never an allocated history.
        self.last = Some(Pair {
            before: Shape::observe(before),
            after: Shape::observe(after),
        });
        let limits = DiagnosticLimits::new(512, 8, 20);
        let report = diff_graph_views(before, after, limits, project)
            .map_err(|_| "diagnostic diff failed")?;
        add(
            &self.counters.semantic_changes,
            usize::from(report.semantic_changed()),
        );
        add(
            &self.counters.view_changes,
            usize::from(report.view_changed()),
        );
        add(&self.counters.total_changes, report.changes().len());
        add(
            &self.counters.value_changes,
            report
                .changes()
                .iter()
                .filter(|change| **change == ViewChange::Value)
                .count(),
        );
        self.output.clear();
        write_graph_view(
            after,
            limits,
            project,
            |_| {
                add(&self.counters.redaction_calls, 1);
                "v".to_owned()
            },
            &mut self.output,
        )
        .map_err(|_| "diagnostic writer failed")?;
        // Logical accounting only. Literal comparisons belong after work_finished.
        add(&self.counters.written_bytes, self.output.len());
        Ok(())
    }
}

/// Measures operations, diagnostic callbacks and their explicit counter overhead.
#[inline(never)]
fn profile_observed(
    state: &mut [usize; 2],
    iterations: usize,
    counters: &Counters,
    capture: &CaptureCallback<'_>,
    sink: &mut SinkCallback<'_>,
) -> Result<usize, &'static str> {
    let mut checksum = 0;
    for _ in 0..iterations {
        checksum += checkpoint(
            state,
            "root increment",
            |state| capture.capture(state),
            |state| {
                add(&counters.operation_calls, 1);
                state[0] += 1;
                Ok::<_, &'static str>(state[0])
            },
            |event| sink.observe(&event),
        )?;
        if let Some(error) = sink.error {
            return Err(error);
        }
    }
    Ok(checksum)
}

/// Measures the same operation with no callback owners or diagnostic hooks.
#[inline(never)]
fn profile_disabled(state: &mut [usize; 2], iterations: usize, counters: &Counters) -> usize {
    let mut checksum = 0;
    for _ in 0..iterations {
        let result = checkpoint_disabled(state, |state| {
            add(&counters.operation_calls, 1);
            state[0] += 1;
            Ok::<_, std::convert::Infallible>(state[0])
        });
        checksum += match result {
            Ok(value) => value,
            Err(never) => match never {},
        };
    }
    checksum
}

/// Holds copied facts while callback owners remain alive for validation.
struct WorkRecord {
    state: [usize; 2],
    checksum: usize,
    facts: Facts,
    pair: Option<Pair>,
    live: [bool; 2],
}

/// Checks literal topology and arithmetic counts independently of capture and projection.
fn validate_work(config: Config, record: &WorkRecord, output: &str) -> Result<(), &'static str> {
    let iterations = config.iterations;
    if record.state != [iterations, 7]
        || record.checksum != iterations * (iterations + 1) / 2
        || record.live != [config.mode == Mode::Observed; 2]
    {
        return Err("diagnostic state, checksum, or callback lifetime mismatch");
    }
    let observed = config.mode == Mode::Observed && cfg!(debug_assertions);
    let expected = if observed {
        Facts {
            operation_calls: iterations,
            capture_calls: 2 * iterations,
            node_capture_calls: 4 * iterations,
            sink_calls: iterations,
            checkpoint_changes: iterations,
            checkpoint_successes: iterations,
            equality_calls: 5 * iterations,
            project_calls: 6 * iterations,
            redaction_calls: 2 * iterations,
            semantic_changes: iterations,
            view_changes: iterations,
            value_changes: iterations,
            total_changes: iterations,
            leaf_constructions: 4 * iterations,
            leaf_drops: 4 * iterations,
            written_bytes: 20 * iterations,
        }
    } else {
        Facts {
            operation_calls: iterations,
            ..Facts::default()
        }
    };
    if record.facts != expected || output != if observed { OUTPUT } else { "" } {
        return Err("diagnostic hook counts or final output mismatch");
    }
    let expected_pair = if observed {
        let before = Shape {
            node_count: 2,
            origins: [0, 1],
            values: [iterations - 1, 7],
            edge_counts: [2, 1],
            targets: [1, 1, 0],
            label_lengths: [1, 1, 4],
            labels: [[b'a', 0, 0, 0], [b'b', 0, 0, 0], *b"back"],
            kinds: [
                Some(ReferenceKind::First),
                Some(ReferenceKind::Repeat),
                Some(ReferenceKind::Cycle),
            ],
        };
        Some(Pair {
            before,
            after: Shape {
                values: [iterations, 7],
                ..before
            },
        })
    } else {
        None
    };
    if record.pair != expected_pair {
        return Err("diagnostic final capture pair mismatch");
    }
    Ok(())
}

fn validate_drops(mode: Mode, drops: [usize; 2]) -> Result<(), &'static str> {
    if drops != [usize::from(mode == Mode::Observed); 2] {
        return Err("diagnostic callback teardown mismatch");
    }
    Ok(())
}

/// Reports only after state, diagnostics, callback teardown and retention checks pass.
pub(crate) fn run(config: Config) -> Result<String, &'static str> {
    let mut measurement = Measurement::new(None);
    let counters = Counters::default();
    let capture_drops = Cell::new(0);
    let sink_drops = Cell::new(0);
    let mut callbacks = (config.mode == Mode::Observed).then(|| {
        (
            CaptureCallback {
                _owner: Owner(&capture_drops),
                counters: &counters,
            },
            SinkCallback {
                _owner: Owner(&sink_drops),
                counters: &counters,
                output: String::with_capacity(20),
                last: None,
                error: None,
            },
        )
    });
    let mut state = [0, 7];
    measurement.construction_finished();
    let work = if let Some((capture, sink)) = &mut callbacks {
        profile_observed(&mut state, config.iterations, &counters, capture, sink)
    } else {
        Ok(profile_disabled(&mut state, config.iterations, &counters))
    };
    measurement.work_finished();
    let record = WorkRecord {
        state,
        checksum: work.unwrap_or_default(),
        facts: counters.snapshot(),
        pair: callbacks.as_ref().and_then(|(_, sink)| sink.last),
        live: [
            callbacks.is_some() && capture_drops.get() == 0,
            callbacks.is_some() && sink_drops.get() == 0,
        ],
    };
    let validation = work.and_then(|_| {
        validate_work(
            config,
            &record,
            callbacks
                .as_ref()
                .map_or("", |(_, sink)| sink.output.as_str()),
        )
    });
    measurement.validation_finished();
    drop(callbacks);
    let drops = [capture_drops.get(), sink_drops.get()];
    let teardown = validate_drops(config.mode, drops);
    let allocations = measurement.finish()?;
    validation?;
    teardown?;
    Ok(render(config, &record, drops, allocations.as_ref()))
}

/// Serialises already-validated facts after the allocation session has ended.
fn render(
    config: Config,
    record: &WorkRecord,
    drops: [usize; 2],
    allocations: Option<&crate::measurement::Record>,
) -> String {
    let (name, mode) = match config.mode {
        Mode::Observed => (
            "diagnostic-observed",
            if cfg!(debug_assertions) {
                "debug-observed"
            } else {
                "release-elided"
            },
        ),
        Mode::Disabled => ("diagnostic-disabled", "explicit-disabled"),
    };
    let pair = record.pair.map_or_else(|| "null".to_owned(), |pair| format!(
        "{{\"before_values\":{:?},\"after_values\":{:?},\"node_count\":2,\"origins\":[0,1],\"edge_counts\":[2,1],\"targets\":[1,1,0],\"labels\":[\"a\",\"b\",\"back\"],\"kinds\":[\"first\",\"repeat\",\"cycle\"]}}",
        pair.before.values, pair.after.values,
    ));
    format!(
        concat!(
            "{{\"schema\":\"rca-diagnostic-profile-v1\",\"workload\":\"{}\",\"mode\":\"{}\",",
            "\"size\":2,\"iterations\":{},\"final_state\":{:?},\"checksum\":{},",
            "\"capture_owner_live_at_validation\":{},\"sink_owner_live_at_validation\":{},",
            "\"capture_owner_drops\":{},\"sink_owner_drops\":{},\"final_output\":\"{}\",",
            "\"final_capture_pair\":{},\"work_facts\":{},\"allocations\":{}}}"
        ),
        name,
        mode,
        config.iterations,
        record.state,
        record.checksum,
        record.live[0],
        record.live[1],
        drops[0],
        drops[1],
        if record.pair.is_some() { OUTPUT } else { "" },
        pair,
        record.facts.json(),
        allocations.map_or_else(|| "null".to_owned(), crate::measurement::Record::json),
    )
}

#[cfg(test)]
#[path = "diagnostic_controls.rs"]
mod controls;

#[cfg(test)]
mod tests {
    use super::{
        Config, Facts, Mode, OUTPUT, Pair, Shape, WorkRecord, run, validate_drops, validate_work,
    };
    use rca_core::diagnostics::ReferenceKind;
    use serde_json::{Value, json};

    #[test]
    fn accepts_exact_modes_and_bounded_decimal_iterations() {
        for (name, iterations, expected_mode, expected_iterations) in [
            ("diagnostic-observed", "1", Mode::Observed, 1),
            ("diagnostic-observed", "1024", Mode::Observed, 1024),
            ("diagnostic-disabled", "3", Mode::Disabled, 3),
        ] {
            let actual = Config::parse([name, "2", iterations].into_iter().map(str::to_owned));
            assert_eq!(
                actual,
                Ok(Config {
                    mode: expected_mode,
                    iterations: expected_iterations,
                })
            );
        }
    }

    #[test]
    fn rejects_unknown_modes_shapes_and_unbounded_iterations() {
        for args in [
            vec![],
            vec!["diagnostic-observed"],
            vec!["diagnostic-observed", "2"],
            vec!["diagnostic-unknown", "2", "1"],
            vec!["diagnostic-observed", "1", "1"],
            vec!["diagnostic-observed", "3", "1"],
            vec!["diagnostic-observed", "+2", "1"],
            vec!["diagnostic-observed", "02", "1"],
            vec!["diagnostic-observed", "2", "0"],
            vec!["diagnostic-observed", "2", "1025"],
            vec!["diagnostic-observed", "2", "18446744073709551616"],
            vec!["diagnostic-observed", "2", "-1"],
            vec!["diagnostic-observed", "2", "+1"],
            vec!["diagnostic-observed", "2", " 1"],
            vec!["diagnostic-observed", "2", "1.0"],
            vec!["diagnostic-observed", "2", "١"],
            vec!["diagnostic-disabled", "2", "1", "extra"],
        ] {
            assert!(Config::parse(args.into_iter().map(str::to_owned)).is_err());
        }
    }

    fn literal_record(mode: Mode) -> WorkRecord {
        let shape = Shape {
            node_count: 2,
            origins: [0, 1],
            values: [2, 7],
            edge_counts: [2, 1],
            targets: [1, 1, 0],
            label_lengths: [1, 1, 4],
            labels: [[b'a', 0, 0, 0], [b'b', 0, 0, 0], *b"back"],
            kinds: [
                Some(ReferenceKind::First),
                Some(ReferenceKind::Repeat),
                Some(ReferenceKind::Cycle),
            ],
        };
        let observed = mode == Mode::Observed && cfg!(debug_assertions);
        WorkRecord {
            state: [3, 7],
            checksum: 6,
            facts: if observed {
                Facts {
                    operation_calls: 3,
                    capture_calls: 6,
                    node_capture_calls: 12,
                    sink_calls: 3,
                    checkpoint_changes: 3,
                    checkpoint_successes: 3,
                    equality_calls: 15,
                    project_calls: 18,
                    redaction_calls: 6,
                    semantic_changes: 3,
                    view_changes: 3,
                    value_changes: 3,
                    total_changes: 3,
                    leaf_constructions: 12,
                    leaf_drops: 12,
                    written_bytes: 60,
                }
            } else {
                Facts {
                    operation_calls: 3,
                    ..Facts::default()
                }
            },
            pair: observed.then_some(Pair {
                before: shape,
                after: Shape {
                    values: [3, 7],
                    ..shape
                },
            }),
            live: [mode == Mode::Observed; 2],
        }
    }

    #[test]
    fn production_oracle_rejects_corrupt_work_and_ownership() {
        let mutations: &[fn(&mut WorkRecord)] = &[
            |r| r.state[0] += 1,
            |r| r.state[1] += 1,
            |r| r.checksum += 1,
            |r| r.facts.operation_calls -= 1,
            |r| r.facts.operation_calls += 1,
            |r| r.facts.capture_calls += 1,
            |r| r.facts.node_capture_calls += 1,
            |r| r.facts.sink_calls += 1,
            |r| r.facts.checkpoint_changes += 1,
            |r| r.facts.checkpoint_successes += 1,
            |r| r.facts.equality_calls += 1,
            |r| r.facts.project_calls += 1,
            |r| r.facts.redaction_calls += 1,
            |r| r.facts.semantic_changes += 1,
            |r| r.facts.view_changes += 1,
            |r| r.facts.value_changes += 1,
            |r| r.facts.total_changes += 1,
            |r| r.facts.leaf_constructions += 1,
            |r| r.facts.leaf_drops += 1,
            |r| r.facts.written_bytes += 1,
            |r| r.live[0] = !r.live[0],
            |r| r.live[1] = !r.live[1],
        ];
        let mut accepted_corruption = 0;
        for mode in [Mode::Observed, Mode::Disabled] {
            let config = Config {
                mode,
                iterations: 3,
            };
            let output = if mode == Mode::Observed && cfg!(debug_assertions) {
                OUTPUT
            } else {
                ""
            };
            assert!(validate_work(config, &literal_record(mode), output).is_ok());
            for mutate in mutations {
                let mut record = literal_record(mode);
                mutate(&mut record);
                accepted_corruption += usize::from(validate_work(config, &record, output).is_ok());
            }
            accepted_corruption +=
                usize::from(validate_work(config, &literal_record(mode), "wrong output").is_ok());
            let expected = if mode == Mode::Observed {
                [1, 1]
            } else {
                [0, 0]
            };
            assert!(validate_drops(mode, expected).is_ok());
            for index in 0..2 {
                for value in [0, 1, 2] {
                    if value == expected[index] {
                        continue;
                    }
                    let mut drops = expected;
                    drops[index] = value;
                    accepted_corruption += usize::from(validate_drops(mode, drops).is_ok());
                }
            }
        }
        assert_eq!(
            accepted_corruption, 0,
            "production validator accepted corrupted facts"
        );
    }

    #[test]
    fn production_oracle_rejects_corrupt_final_capture_topology() {
        if !cfg!(debug_assertions) {
            return;
        }
        let mutations: &[fn(&mut Shape)] = &[
            |s| s.node_count += 1,
            |s| s.origins[0] += 1,
            |s| s.origins[1] += 1,
            |s| s.values[0] += 1,
            |s| s.values[1] += 1,
            |s| s.edge_counts[0] += 1,
            |s| s.edge_counts[1] += 1,
            |s| s.targets[0] += 1,
            |s| s.targets[1] += 1,
            |s| s.targets[2] += 1,
            |s| s.label_lengths[0] += 1,
            |s| s.labels[0][0] = b'z',
            |s| s.labels[1][0] = b'z',
            |s| s.labels[2][0] = b'z',
            |s| s.kinds[0] = None,
            |s| s.kinds[1] = None,
            |s| s.kinds[2] = None,
        ];
        let config = Config {
            mode: Mode::Observed,
            iterations: 3,
        };
        let mut accepted_corruption = 0;
        for mutate in mutations {
            for before in [true, false] {
                let mut record = literal_record(Mode::Observed);
                let pair = record.pair.as_mut().unwrap();
                mutate(if before {
                    &mut pair.before
                } else {
                    &mut pair.after
                });
                accepted_corruption += usize::from(validate_work(config, &record, OUTPUT).is_ok());
            }
        }
        let mut record = literal_record(Mode::Observed);
        record.pair = None;
        accepted_corruption += usize::from(validate_work(config, &record, OUTPUT).is_ok());
        assert_eq!(
            accepted_corruption, 0,
            "production validator accepted corrupted topology"
        );
    }

    #[test]
    fn observed_seed_emits_complete_exact_work_record() {
        #[cfg(feature = "allocations")]
        if !crate::tests::in_child(
            "diagnostic_workloads::tests::observed_seed_emits_complete_exact_work_record",
        ) {
            return;
        }
        let output = run(Config {
            mode: Mode::Observed,
            iterations: 3,
        })
        .expect("the observed diagnostic seed must perform its real work");
        let value: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(value["schema"], "rca-diagnostic-profile-v1");
        assert_eq!(value["workload"], "diagnostic-observed");
        assert_eq!(value["size"], 2);
        assert_eq!(value["iterations"], 3);
        assert_eq!(value["final_state"], json!([3, 7]));
        assert_eq!(value["checksum"], 6);
        assert_eq!(value["capture_owner_live_at_validation"], true);
        assert_eq!(value["sink_owner_live_at_validation"], true);
        assert_eq!(value["capture_owner_drops"], 1);
        assert_eq!(value["sink_owner_drops"], 1);
        if cfg!(debug_assertions) {
            assert_eq!(value["mode"], "debug-observed");
            assert_eq!(value["final_output"], "[v, &1([v, *0]), *1]");
            assert_eq!(
                value["final_capture_pair"],
                json!({
                    "before_values": [2, 7],
                    "after_values": [3, 7],
                    "node_count": 2,
                    "origins": [0, 1],
                    "edge_counts": [2, 1],
                    "targets": [1, 1, 0],
                    "labels": ["a", "b", "back"],
                    "kinds": ["first", "repeat", "cycle"],
                })
            );
            assert_eq!(
                value["work_facts"],
                json!({
                    "operation_calls": 3,
                    "capture_calls": 6,
                    "node_capture_calls": 12,
                    "sink_calls": 3,
                    "checkpoint_changes": 3,
                    "checkpoint_successes": 3,
                    "equality_calls": 15,
                    "project_calls": 18,
                    "redaction_calls": 6,
                    "semantic_changes": 3,
                    "view_changes": 3,
                    "value_changes": 3,
                    "total_changes": 3,
                    "leaf_constructions": 12,
                    "leaf_drops": 12,
                    "written_bytes": 60,
                })
            );
        } else {
            assert_eq!(value["mode"], "release-elided");
            assert_eq!(value["final_output"], "");
            assert!(value["final_capture_pair"].is_null());
            assert_eq!(value["work_facts"], operation_only_facts());
        }
        assert_retention_record(&value);
    }

    #[test]
    fn explicit_disabled_seed_has_no_diagnostic_work_or_callback_owners() {
        #[cfg(feature = "allocations")]
        if !crate::tests::in_child(
            "diagnostic_workloads::tests::explicit_disabled_seed_has_no_diagnostic_work_or_callback_owners",
        ) {
            return;
        }
        let output = run(Config {
            mode: Mode::Disabled,
            iterations: 3,
        })
        .expect("the explicitly disabled seed must still perform its operation");
        let value: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(value["schema"], "rca-diagnostic-profile-v1");
        assert_eq!(value["workload"], "diagnostic-disabled");
        assert_eq!(value["mode"], "explicit-disabled");
        assert_eq!(value["size"], 2);
        assert_eq!(value["iterations"], 3);
        assert_eq!(value["final_state"], json!([3, 7]));
        assert_eq!(value["checksum"], 6);
        assert_eq!(value["final_output"], "");
        assert!(value["final_capture_pair"].is_null());
        assert_eq!(value["capture_owner_live_at_validation"], false);
        assert_eq!(value["sink_owner_live_at_validation"], false);
        assert_eq!(value["capture_owner_drops"], 0);
        assert_eq!(value["sink_owner_drops"], 0);
        assert_eq!(value["work_facts"], operation_only_facts());
        assert_retention_record(&value);
    }

    fn operation_only_facts() -> Value {
        json!({
            "operation_calls": 3,
            "capture_calls": 0,
            "node_capture_calls": 0,
            "sink_calls": 0,
            "checkpoint_changes": 0,
            "checkpoint_successes": 0,
            "equality_calls": 0,
            "project_calls": 0,
            "redaction_calls": 0,
            "semantic_changes": 0,
            "view_changes": 0,
            "value_changes": 0,
            "total_changes": 0,
            "leaf_constructions": 0,
            "leaf_drops": 0,
            "written_bytes": 0,
        })
    }

    fn assert_retention_record(value: &Value) {
        #[cfg(feature = "allocations")]
        {
            assert_eq!(
                value["allocations"]["executor_thread_local"]["source"],
                "none"
            );
            assert_eq!(
                value["allocations"]["executor_thread_local"]["retained_blocks"],
                0
            );
            assert_eq!(
                value["allocations"]["executor_thread_local"]["retained_bytes"],
                0
            );
        }
        #[cfg(not(feature = "allocations"))]
        assert!(value["allocations"].is_null());
    }
}
