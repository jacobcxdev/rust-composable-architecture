//! Exercises bounded diagnostic API semantics independently of profiling.

#![cfg(all(test, feature = "diagnostics"))]

use std::cell::Cell;
use std::fmt;

use rca_core::diagnostics::{
    CaptureLimits, CapturedGraph, CapturedNode, CheckpointOutcome, DiagnosticLimits, NodeCapture,
    ReferenceKind, StructuralView, ViewChange, capture_graph, checkpoint, checkpoint_disabled,
    diff_graph_views, write_graph_view,
};

/// Supplies one repeated target and one cycle for independent limit controls.
fn seed_edges(origin: usize) -> Vec<(String, usize)> {
    if origin == 0 {
        vec![("a".to_owned(), 1), ("b".to_owned(), 1)]
    } else {
        vec![("back".to_owned(), 0)]
    }
}

/// Captures the fixed topology with a variable root leaf and exact admission limits.
fn seed_graph(root: usize) -> CapturedGraph<usize, usize> {
    capture_graph(0, CaptureLimits::new(2, 3), |&origin| {
        NodeCapture::new(if origin == 0 { root } else { 7 }, seed_edges(origin))
    })
    .unwrap()
}

/// Projects captured values and references without changing their traversal order.
fn seed_view(node: &CapturedNode<usize, usize>) -> StructuralView<(), &usize> {
    let mut children = vec![StructuralView::Value(node.leaves())];
    children.extend(
        node.edges()
            .iter()
            .map(|edge| StructuralView::Reference(edge.target())),
    );
    StructuralView::Sequence(children)
}

/// Counts rejected and accepted snapshot destruction while comparing only leaf values.
struct Leaf<'a> {
    value: usize,
    drops: &'a Cell<usize>,
}

impl PartialEq for Leaf<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl Drop for Leaf<'_> {
    fn drop(&mut self) {
        self.drops.set(self.drops.get() + 1);
    }
}

#[test]
fn zero_node_budget_prevents_capture_callback() {
    let calls = Cell::new(0);
    let error = capture_graph(0, CaptureLimits::new(0, 3), |_| {
        calls.set(calls.get() + 1);
        NodeCapture::new(7, Vec::<(String, usize)>::new())
    })
    .unwrap_err();
    assert!(error.is_node_limit());
    assert_eq!(error.captured_nodes(), 0);
    assert_eq!(error.declared_edges(), 0);
    assert_eq!(calls.get(), 0);
}

#[test]
fn rejected_seed_admission_drops_partial_and_rejected_leaves() {
    // Each tuple is a hand-derived node/edge rejection for this two-node graph.
    for (nodes, edges, node_limit, calls_expected, admitted, declared) in [
        (1, 3, true, 1, 1, 2),
        (2, 1, false, 1, 0, 0),
        (2, 2, false, 2, 1, 2),
    ] {
        let calls = Cell::new(0);
        let drops = Cell::new(0);
        let error = capture_graph(0, CaptureLimits::new(nodes, edges), |&origin| {
            calls.set(calls.get() + 1);
            NodeCapture::new(
                Leaf {
                    value: origin,
                    drops: &drops,
                },
                seed_edges(origin),
            )
        })
        .unwrap_err();
        assert_eq!(error.is_node_limit(), node_limit);
        assert_eq!(error.is_edge_limit(), !node_limit);
        assert_eq!(error.captured_nodes(), admitted);
        assert_eq!(error.declared_edges(), declared);
        assert_eq!(calls.get(), calls_expected);
        assert_eq!(drops.get(), calls_expected);
    }
}

#[test]
fn exact_seed_budget_preserves_repeat_and_backedge_topology() {
    let graph = seed_graph(3);
    let nodes = graph.nodes();
    assert_eq!(nodes.len(), 2);
    assert_eq!((*nodes[0].origin(), *nodes[0].leaves()), (0, 3));
    assert_eq!((*nodes[1].origin(), *nodes[1].leaves()), (1, 7));
    let edges = |node: &CapturedNode<usize, usize>| {
        node.edges()
            .iter()
            .map(|edge| (edge.label().to_owned(), edge.target(), edge.kind()))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        edges(&nodes[0]),
        vec![
            ("a".to_owned(), 1, ReferenceKind::First),
            ("b".to_owned(), 1, ReferenceKind::Repeat),
        ]
    );
    assert_eq!(
        edges(&nodes[1]),
        vec![("back".to_owned(), 0, ReferenceKind::Cycle)]
    );
}

#[test]
fn zero_work_prevents_projection_and_redaction_dispatch() {
    let graph = seed_graph(0);
    let projections = Cell::new(0);
    let redactions = Cell::new(0);
    let mut output = String::new();
    let error = write_graph_view(
        &graph,
        DiagnosticLimits::new(0, 3, 20),
        |node| {
            projections.set(projections.get() + 1);
            seed_view(node)
        },
        |_| {
            redactions.set(redactions.get() + 1);
            "v".to_owned()
        },
        &mut output,
    )
    .unwrap_err();
    assert!(error.is_work_limit());
    assert_eq!((projections.get(), redactions.get()), (0, 0));
    assert_eq!(output, "");
    let error = diff_graph_views(&graph, &graph, DiagnosticLimits::new(0, 3, 0), |node| {
        projections.set(projections.get() + 1);
        seed_view(node)
    })
    .unwrap_err();
    assert!(error.is_work_limit());
    assert_eq!(projections.get(), 0);
}

#[test]
fn seed_depth_three_succeeds_and_depth_two_is_incomplete() {
    let before = seed_graph(0);
    let after = seed_graph(1);
    let report =
        diff_graph_views(&before, &after, DiagnosticLimits::new(512, 3, 0), seed_view).unwrap();
    assert!(report.semantic_changed());
    assert!(report.view_changed());
    assert_eq!(report.changes(), &[ViewChange::Value]);
    let error =
        diff_graph_views(&before, &after, DiagnosticLimits::new(512, 2, 0), seed_view).unwrap_err();
    assert!(error.is_depth_limit());

    let mut output = String::new();
    write_graph_view(
        &after,
        DiagnosticLimits::new(512, 3, 20),
        seed_view,
        |_| "v".to_owned(),
        &mut output,
    )
    .unwrap();
    assert_eq!(output, "[v, &1([v, *0]), *1]");
    output.clear();
    let error = write_graph_view(
        &after,
        DiagnosticLimits::new(512, 2, 20),
        seed_view,
        |_| "v".to_owned(),
        &mut output,
    )
    .unwrap_err();
    assert!(error.is_depth_limit());
}

#[test]
fn seed_output_limit_preserves_the_exact_nineteen_scalar_prefix() {
    let graph = seed_graph(0);
    let mut output = String::new();
    let error = write_graph_view(
        &graph,
        DiagnosticLimits::new(512, 3, 19),
        seed_view,
        |_| "v".to_owned(),
        &mut output,
    )
    .unwrap_err();
    assert!(error.is_output_limit());
    assert_eq!(error.output_written(), 19);
    assert_eq!(output, "[v, &1([v, *0]), *1");
}

#[test]
fn unicode_output_limits_count_scalars_and_zero_skips_redaction() {
    let graph = capture_graph(0, CaptureLimits::new(1, 0), |_| {
        NodeCapture::new(7, Vec::<(String, usize)>::new())
    })
    .unwrap();
    for (limit, expected, calls_expected) in [(0, "", 0), (1, "é", 1), (2, "é🦀", 1)] {
        let redactions = Cell::new(0);
        let mut output = String::new();
        let result = write_graph_view(
            &graph,
            DiagnosticLimits::new(512, 0, limit),
            |node| StructuralView::<(), _>::Value(node.leaves()),
            |_| {
                redactions.set(redactions.get() + 1);
                "é🦀".to_owned()
            },
            &mut output,
        );
        if limit == 2 {
            result.unwrap();
            assert_eq!(output.len(), 6);
        } else {
            let error = result.unwrap_err();
            assert!(error.is_output_limit());
            assert_eq!(error.output_written(), limit);
        }
        assert_eq!(output, expected);
        assert_eq!(redactions.get(), calls_expected);
    }
}

#[test]
fn invalid_projection_never_renders_or_returns_an_equal_report() {
    type View = StructuralView<usize, usize>;
    let graph = seed_graph(0);
    for view in [
        View::Reference(2),
        View::Fields(vec![
            ("x".to_owned(), View::Value(0)),
            ("x".to_owned(), View::Value(1)),
        ]),
        View::Map(vec![(0, View::Value(0)), (0, View::Value(1))]),
    ] {
        let redactions = Cell::new(0);
        let mut output = String::new();
        let error = write_graph_view(
            &graph,
            DiagnosticLimits::new(512, 3, 20),
            |_| view.clone(),
            |_| {
                redactions.set(redactions.get() + 1);
                "v".to_owned()
            },
            &mut output,
        )
        .unwrap_err();
        assert!(error.is_invalid_view());
        assert_eq!(redactions.get(), 0);
        assert_eq!(output, "");
        let error = diff_graph_views(&graph, &graph, DiagnosticLimits::new(512, 3, 0), |_| {
            view.clone()
        })
        .unwrap_err();
        assert!(error.is_invalid_view());
    }
}

#[test]
fn a_partially_writing_error_preserves_bytes_but_not_successful_scalar_credit() {
    struct PartialWriter(String);
    impl fmt::Write for PartialWriter {
        fn write_str(&mut self, text: &str) -> fmt::Result {
            self.0.push_str(text);
            Err(fmt::Error)
        }
    }
    let graph = capture_graph(0, CaptureLimits::new(1, 0), |_| {
        NodeCapture::new(7, Vec::<(String, usize)>::new())
    })
    .unwrap();
    let mut writer = PartialWriter(String::new());
    let error = write_graph_view(
        &graph,
        DiagnosticLimits::new(512, 0, 1),
        |node| StructuralView::<(), _>::Value(node.leaves()),
        |_| "é".to_owned(),
        &mut writer,
    )
    .unwrap_err();
    assert!(error.is_writer_failure());
    assert!(std::error::Error::source(&error).is_some());
    assert_eq!(error.output_written(), 0);
    assert_eq!(writer.0, "é");
}

#[test]
fn checkpoint_modes_preserve_non_clone_typed_result_identity() {
    struct Token(Box<usize>);
    for disabled in [false, true] {
        for is_error in [false, true] {
            let token = Token(Box::new(17));
            let identity = std::ptr::from_ref(token.0.as_ref());
            let operations = Cell::new(0);
            let captures = Cell::new(0);
            let sinks = Cell::new(0);
            let mut state = 0;
            let operation = |state: &mut usize| {
                operations.set(operations.get() + 1);
                *state += 1;
                if is_error { Err(token) } else { Ok(token) }
            };
            let result = if disabled {
                checkpoint_disabled(&mut state, operation)
            } else {
                checkpoint(
                    &mut state,
                    "result identity",
                    |state| {
                        captures.set(captures.get() + 1);
                        *state
                    },
                    operation,
                    |event| {
                        sinks.set(sinks.get() + 1);
                        assert_eq!((*event.before(), *event.after()), (0, 1));
                        assert!(event.is_changed());
                        assert_eq!(
                            event.outcome(),
                            if is_error {
                                CheckpointOutcome::Error
                            } else {
                                CheckpointOutcome::Success
                            }
                        );
                    },
                )
            };
            let ((Ok(returned), false) | (Err(returned), true)) = (result, is_error) else {
                panic!("checkpoint changed the typed result branch");
            };
            assert_eq!(std::ptr::from_ref(returned.0.as_ref()), identity);
            assert_eq!(*returned.0, 17);
            assert_eq!((operations.get(), state), (1, 1));
            assert_eq!(
                (captures.get(), sinks.get()),
                if !disabled && cfg!(debug_assertions) {
                    (2, 1)
                } else {
                    (0, 0)
                }
            );
        }
    }
}

#[cfg(panic = "unwind")]
#[test]
fn panic_cleanup_drops_snapshots_but_preserves_borrowed_callback_owners() {
    use rca_core::diagnostics::Checkpoint;
    use std::panic::{AssertUnwindSafe, catch_unwind};

    struct Owner<'a>(&'a Cell<usize>);
    impl Drop for Owner<'_> {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }
    for stage in ["operation", "sink"] {
        let captures = Cell::new(0);
        let operations = Cell::new(0);
        let sinks = Cell::new(0);
        let leaf_drops = Cell::new(0);
        let capture_drops = Cell::new(0);
        let sink_drops = Cell::new(0);
        let capture = {
            let calls = &captures;
            let drops = &leaf_drops;
            let owner = Owner(&capture_drops);
            move |state: &usize| {
                std::hint::black_box(&owner);
                calls.set(calls.get() + 1);
                Leaf {
                    value: *state,
                    drops,
                }
            }
        };
        let mut sink = {
            let calls = &sinks;
            let owner = Owner(&sink_drops);
            move |_: Checkpoint<'_, Leaf<'_>>| {
                std::hint::black_box(&owner);
                calls.set(calls.get() + 1);
                panic!("sink");
            }
        };
        let mut state = 0;
        let result = catch_unwind(AssertUnwindSafe(|| {
            checkpoint(
                &mut state,
                "panic cleanup",
                &capture,
                |state| {
                    operations.set(operations.get() + 1);
                    *state += 1;
                    assert!(stage != "operation", "operation");
                    Ok::<_, ()>(())
                },
                &mut sink,
            )
        }));
        if cfg!(debug_assertions) || stage == "operation" {
            let failure = result.expect_err("the selected callback must panic");
            assert_eq!(failure.downcast_ref::<&str>().copied(), Some(stage));
        } else {
            assert!(matches!(result, Ok(Ok(()))));
        }
        assert_eq!((operations.get(), state), (1, 1));
        assert_eq!(
            (captures.get(), sinks.get(), leaf_drops.get()),
            if !cfg!(debug_assertions) {
                (0, 0, 0)
            } else if stage == "operation" {
                (1, 0, 1)
            } else {
                (2, 1, 2)
            }
        );
        assert_eq!((capture_drops.get(), sink_drops.get()), (0, 0));
        drop(capture);
        drop(sink);
        assert_eq!((capture_drops.get(), sink_drops.get()), (1, 1));
    }
}
