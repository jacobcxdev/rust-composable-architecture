#![cfg(feature = "diagnostics")]

#[cfg(feature = "allocation-measurement")]
#[path = "../src/allocation_test/child.rs"]
mod allocation_test;

#[cfg(feature = "allocation-measurement")]
#[global_allocator]
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

use std::cell::Cell;
use std::fmt;

use rca_core::diagnostics::{
    CaptureLimits, DiagnosticLimits, NodeCapture, StructuralToken, StructuralView, ViewChange,
    capture_graph, diff_graph_views, write_graph_view,
};

type View = StructuralView<i32, i32>;

fn limits() -> DiagnosticLimits {
    DiagnosticLimits::new(10_000, 32, 10_000)
}

fn compare(left: View, right: View) -> Vec<ViewChange> {
    let a = capture_graph(0, CaptureLimits::new(1, 0), |_| NodeCapture::new(0, vec![])).unwrap();
    let b = capture_graph(0, CaptureLimits::new(1, 0), |_| NodeCapture::new(0, vec![])).unwrap();
    let mut views = vec![right, left];
    diff_graph_views(&a, &b, limits(), |_| {
        views.pop().expect("one projection per graph")
    })
    .unwrap()
    .changes()
    .to_vec()
}

#[test]
fn aligns_fields_by_identity_and_reports_order_insertions_and_removals() {
    let left = View::Fields(vec![
        ("a".into(), View::Value(1)),
        ("b".into(), View::Value(2)),
        ("gone".into(), View::Value(3)),
    ]);
    let right = View::Fields(vec![
        ("b".into(), View::Value(9)),
        ("a".into(), View::Value(1)),
        ("new".into(), View::Value(4)),
    ]);
    let changes = compare(left, right);
    for expected in [
        ViewChange::FieldInserted,
        ViewChange::FieldRemoved,
        ViewChange::FieldOrder,
        ViewChange::Value,
    ] {
        assert!(
            changes.contains(&expected),
            "missing {expected:?}: {changes:?}"
        );
    }
}

#[test]
fn maps_match_typed_keys_instead_of_order_or_printed_key() {
    let map =
        |pairs: &[(i32, i32)]| View::Map(pairs.iter().map(|&(k, v)| (k, View::Value(v))).collect());
    assert!(compare(map(&[(1, 10), (2, 20)]), map(&[(2, 20), (1, 10)])).is_empty());
    let changes = compare(
        map(&[(1, 10), (2, 20), (3, 30)]),
        map(&[(1, 10), (3, 31), (4, 40)]),
    );
    assert!(changes.contains(&ViewChange::MapInserted));
    assert!(changes.contains(&ViewChange::MapRemoved));
    assert!(changes.contains(&ViewChange::Value));
}

#[test]
fn omission_and_identical_redaction_do_not_hide_actual_semantic_change() {
    let a = capture_graph(0, CaptureLimits::new(1, 0), |_| NodeCapture::new(7, vec![])).unwrap();
    let b = capture_graph(0, CaptureLimits::new(1, 0), |_| NodeCapture::new(8, vec![])).unwrap();
    let diff = diff_graph_views(&a, &b, limits(), |_| View::Fields(vec![])).unwrap();
    assert!(diff.semantic_changed());
    assert!(!diff.view_changed());
    let mut out = String::new();
    write_graph_view(
        &a,
        limits(),
        |_| View::Map(vec![(1, View::Value(7)), (2, View::Value(8))]),
        |_| "[private]".into(),
        &mut out,
    )
    .unwrap();
    assert_eq!(out, "{[private]: [private], [private]: [private]}");
}

#[test]
fn zero_work_prevents_projection_and_depth_prevents_redaction() {
    let graph =
        capture_graph(0, CaptureLimits::new(1, 0), |_| NodeCapture::new(0, vec![])).unwrap();
    let calls = Cell::new(0);
    let error = write_graph_view(
        &graph,
        DiagnosticLimits::new(0, 10, 10),
        |_| {
            calls.set(calls.get() + 1);
            View::Value(1)
        },
        |_| "x".into(),
        &mut String::new(),
    )
    .unwrap_err();
    assert!(error.is_work_limit());
    assert_eq!(calls.get(), 0);
    let error = write_graph_view(
        &graph,
        DiagnosticLimits::new(100, 0, 100),
        |_| View::Sequence(vec![View::Value(1)]),
        |_| {
            calls.set(calls.get() + 1);
            "secret".into()
        },
        &mut String::new(),
    )
    .unwrap_err();
    assert!(error.is_depth_limit());
    assert_eq!(calls.get(), 0);
}

#[test]
fn unicode_output_boundary_and_writer_failure_preserve_written_prefix() {
    struct FailAfterOne {
        text: String,
    }
    impl fmt::Write for FailAfterOne {
        fn write_str(&mut self, s: &str) -> fmt::Result {
            if !self.text.is_empty() {
                return Err(fmt::Error);
            }
            self.text.push_str(s);
            Ok(())
        }
    }
    let graph =
        capture_graph(0, CaptureLimits::new(1, 0), |_| NodeCapture::new(0, vec![])).unwrap();
    let mut out = String::new();
    let error = write_graph_view(
        &graph,
        DiagnosticLimits::new(100, 0, 2),
        |_| View::Value(0),
        |_| "é🦀x".into(),
        &mut out,
    )
    .unwrap_err();
    assert!(error.is_output_limit());
    assert_eq!(out, "é🦀");
    let mut writer = FailAfterOne {
        text: String::new(),
    };
    let error = write_graph_view(
        &graph,
        limits(),
        |_| View::Value(0),
        |_| "ab".into(),
        &mut writer as &mut dyn fmt::Write,
    )
    .unwrap_err();
    assert!(error.is_writer_failure());
    assert!(std::error::Error::source(&error).is_some());
    assert_eq!(writer.text, "a");
}

#[test]
fn a_borrowed_trait_object_writer_preserves_redacted_unicode_output() {
    let graph =
        capture_graph(0, CaptureLimits::new(1, 0), |_| NodeCapture::new(0, vec![])).unwrap();
    let mut output = String::new();
    let writer: &mut dyn fmt::Write = &mut output;
    write_graph_view(
        &graph,
        DiagnosticLimits::new(100, 0, 2),
        |_| View::Value(0),
        |_| "é🦀".into(),
        writer,
    )
    .unwrap();
    assert_eq!(output, "é🦀");
}

#[test]
fn cases_none_payload_and_sequence_order_are_structural_not_textual() {
    let send = |value| View::Case("send".into(), Some(Box::new(value)));
    assert!(
        compare(
            send(View::Case("None".into(), None)),
            View::Case("send".into(), None)
        )
        .contains(&ViewChange::Case)
    );
    assert!(
        compare(
            View::Sequence(vec![View::Value(1), View::Value(2)]),
            View::Sequence(vec![View::Value(2), View::Value(1)])
        )
        .contains(&ViewChange::Value)
    );
}

#[test]
fn nested_case_sequence_length_and_shape_report_distinct_view_changes() {
    let left = capture_graph(0, CaptureLimits::new(1, 0), |_| NodeCapture::new(7, vec![])).unwrap();
    let right =
        capture_graph(0, CaptureLimits::new(1, 0), |_| NodeCapture::new(7, vec![])).unwrap();
    for (before, after, expected) in [
        (
            View::Case("send".into(), Some(Box::new(View::Value(1)))),
            View::Case("send".into(), Some(Box::new(View::Value(2)))),
            ViewChange::Value,
        ),
        (
            View::Sequence(vec![View::Value(1)]),
            View::Sequence(vec![View::Value(1), View::Value(2)]),
            ViewChange::SequenceLength,
        ),
        (View::Fields(vec![]), View::Map(vec![]), ViewChange::Shape),
    ] {
        let mut views = vec![after, before];
        let diff = diff_graph_views(&left, &right, limits(), |_| {
            views.pop().expect("one projection per graph")
        })
        .unwrap();
        assert!(!diff.semantic_changed());
        assert_eq!(diff.changes(), &[expected]);
        assert!(views.is_empty());
    }
}

#[test]
fn represented_value_work_limits_fail_before_redaction_or_output() {
    let graph =
        capture_graph(0, CaptureLimits::new(1, 0), |_| NodeCapture::new(0, vec![])).unwrap();
    for (work, expected_used) in [(3, 3), (5, 5)] {
        let redactions = Cell::new(0);
        let mut out = String::new();
        let error = write_graph_view(
            &graph,
            DiagnosticLimits::new(work, 1, 1),
            |_| View::Represented(Box::new(View::Value(7))),
            |_| {
                redactions.set(redactions.get() + 1);
                "é".into()
            },
            &mut out,
        )
        .unwrap_err();
        assert!(error.is_work_limit());
        assert_eq!(error.work_used(), expected_used);
        assert_eq!(error.output_written(), 0);
        assert_eq!(redactions.get(), 0);
        assert!(out.is_empty());
    }
    let redactions = Cell::new(0);
    let mut out = String::new();
    write_graph_view(
        &graph,
        DiagnosticLimits::new(10, 1, 1),
        |_| View::Represented(Box::new(View::Value(7))),
        |_| {
            redactions.set(redactions.get() + 1);
            "é".into()
        },
        &mut out,
    )
    .unwrap();
    assert_eq!(redactions.get(), 1);
    assert_eq!(out, "é");
}

#[test]
fn repeats_cycles_and_invalid_selected_references_are_finite() {
    let graph = capture_graph(10, CaptureLimits::new(2, 3), |&origin| {
        NodeCapture::new(
            origin,
            if origin == 10 {
                vec![("child".into(), 20), ("repeat".into(), 20)]
            } else {
                vec![("cycle".into(), 10)]
            },
        )
    })
    .unwrap();
    let mut out = String::new();
    write_graph_view(
        &graph,
        limits(),
        |node| {
            View::Sequence(
                node.edges()
                    .iter()
                    .map(|edge| View::Reference(edge.target()))
                    .collect(),
            )
        },
        |token| match token {
            StructuralToken::Label(_) => "label".into(),
            StructuralToken::Key(k) | StructuralToken::Value(k) => k.to_string(),
        },
        &mut out,
    )
    .unwrap();
    assert_eq!(out, "[&1([*0]), *1]");
    let diff = diff_graph_views(&graph, &graph, limits(), |node| {
        View::Sequence(
            node.edges()
                .iter()
                .map(|edge| View::Reference(edge.target()))
                .collect(),
        )
    })
    .unwrap();
    assert!(!diff.semantic_changed());
    assert!(!diff.view_changed());
    let error = diff_graph_views(&graph, &graph, limits(), |_| View::Reference(99)).unwrap_err();
    assert!(error.is_invalid_view());
}

#[test]
fn duplicate_identity_and_budget_exhaustion_never_return_equal() {
    let graph =
        capture_graph(0, CaptureLimits::new(1, 0), |_| NodeCapture::new(0, vec![])).unwrap();
    for view in [
        View::Fields(vec![
            ("a".into(), View::Value(0)),
            ("a".into(), View::Value(1)),
        ]),
        View::Map(vec![(1, View::Value(0)), (1, View::Value(1))]),
    ] {
        assert!(
            diff_graph_views(&graph, &graph, limits(), |_| view.clone())
                .unwrap_err()
                .is_invalid_view()
        );
    }
    assert!(
        diff_graph_views(&graph, &graph, DiagnosticLimits::new(1, 100, 100), |_| {
            View::Value(0)
        })
        .unwrap_err()
        .is_work_limit()
    );
}

#[test]
fn charges_graph_equality_before_dispatch_and_zero_output_before_redaction() {
    struct Counted<'a> {
        value: i32,
        calls: &'a Cell<usize>,
    }
    impl PartialEq for Counted<'_> {
        fn eq(&self, other: &Self) -> bool {
            self.calls.set(self.calls.get() + 1);
            self.value == other.value
        }
    }
    impl Eq for Counted<'_> {}
    let origin_calls = Cell::new(0);
    let leaf_calls = Cell::new(0);
    let graph = capture_graph(
        Counted {
            value: 0,
            calls: &origin_calls,
        },
        CaptureLimits::new(1, 0),
        |_| {
            NodeCapture::new(
                Counted {
                    value: 1,
                    calls: &leaf_calls,
                },
                vec![],
            )
        },
    )
    .unwrap();
    let project_calls = Cell::new(0);
    let error = diff_graph_views(&graph, &graph, DiagnosticLimits::new(2, 0, 0), |_| {
        project_calls.set(project_calls.get() + 1);
        View::Value(1)
    })
    .unwrap_err();
    assert!(error.is_work_limit());
    assert_eq!(error.work_used(), 2);
    assert_eq!(origin_calls.get(), 1);
    assert_eq!(leaf_calls.get(), 0);
    assert_eq!(project_calls.get(), 0);
    let redactions = Cell::new(0);
    let error = write_graph_view(
        &graph,
        DiagnosticLimits::new(100, 0, 0),
        |_| View::Value(1),
        |_| {
            redactions.set(redactions.get() + 1);
            String::new()
        },
        &mut String::new(),
    )
    .unwrap_err();
    assert!(error.is_output_limit());
    assert_eq!(redactions.get(), 0);
}

#[test]
fn local_borrowed_non_debug_values_and_labels_are_never_implicitly_printed() {
    struct Private(Cell<i32>);
    let graph = capture_graph((), CaptureLimits::new(1, 0), |()| {
        NodeCapture::new(Private(Cell::new(7)), vec![])
    })
    .unwrap();
    let view: StructuralView<(), &Private> = StructuralView::Case(
        "secret-case".into(),
        Some(Box::new(StructuralView::Fields(vec![(
            "secret-field".into(),
            StructuralView::Value(graph.root().leaves()),
        )]))),
    );
    assert!(!format!("{view:?}").contains("secret"));
    assert!(!format!("{:?}", StructuralToken::<(), Private>::Label("secret")).contains("secret"));
    let mut out = String::new();
    write_graph_view(
        &graph,
        limits(),
        |node| {
            StructuralView::<(), _>::Case(
                "secret-case".into(),
                Some(Box::new(StructuralView::Fields(vec![(
                    "secret-field".into(),
                    StructuralView::Value(node.leaves()),
                )]))),
            )
        },
        |token| match token {
            StructuralToken::Value(value) => {
                assert_eq!(value.0.get(), 7);
                "value".into()
            }
            StructuralToken::Label(_) => "label".into(),
            StructuralToken::Key(()) => unreachable!(),
        },
        &mut out,
    )
    .unwrap();
    assert_eq!(out, "label({label: value})");
}

// Headless, caller-owned selections of the locked SwiftNavigation D0 contracts.
// No native widget, controller, animation or localisation runtime is reproduced.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Text {
    Verbatim(String),
    Localised(String, String),
    Concat(Box<Self>, Box<Self>),
    Modified(Box<Self>, Vec<Modifier>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Modifier {
    Bold(bool),
    Italic,
    DefaultFont,
}

fn selected_text(text: &Text, localise: &impl Fn(&str, &str) -> String) -> String {
    match text {
        Text::Verbatim(text) => text.clone(),
        Text::Localised(key, argument) => localise(key, argument),
        Text::Concat(left, right) => {
            selected_text(left, localise) + &selected_text(right, localise)
        }
        Text::Modified(text, modifiers) => modifiers.iter().fold(
            selected_text(text, localise),
            |text, modifier| match modifier {
                Modifier::Bold(true) => format!("**{text}**"),
                Modifier::Italic => format!("_{text}_"),
                Modifier::Bold(false) | Modifier::DefaultFont => text,
            },
        ),
    }
}

#[derive(Clone, Debug)]
struct Navigation {
    id: u64,
    title: Text,
    actions: Vec<String>,
    message: Option<String>,
    visibility: Option<String>,
}

// Native dialog equality omits UUID and titleVisibility, unlike its dump view.
impl PartialEq for Navigation {
    fn eq(&self, other: &Self) -> bool {
        self.title == other.title && self.actions == other.actions && self.message == other.message
    }
}

type TextView = StructuralView<String, String>;

fn navigation_view(node: &rca_core::diagnostics::CapturedNode<(), Navigation>) -> TextView {
    let state = node.leaves();
    let localise = |key: &str, argument: &str| match key {
        "greeting" => format!("Hello {argument}"),
        _ => panic!("fixture has no localisation for {key}"),
    };
    let mut fields = Vec::new();
    if let Some(visibility) = &state.visibility {
        fields.push((
            "titleVisibility".into(),
            TextView::Value(visibility.clone()),
        ));
    }
    fields.push((
        "title".into(),
        TextView::Represented(Box::new(TextView::Value(selected_text(
            &state.title,
            &localise,
        )))),
    ));
    if !state.actions.is_empty() {
        fields.push((
            "actions".into(),
            TextView::Sequence(state.actions.iter().cloned().map(TextView::Value).collect()),
        ));
    }
    if let Some(message) = &state.message {
        fields.push(("message".into(), TextView::Value(message.clone())));
    }
    TextView::Fields(fields)
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "matches the redaction callback signature"
)]
fn text_token(token: StructuralToken<'_, String, String>) -> String {
    match token {
        StructuralToken::Label(text) => text.into(),
        StructuralToken::Key(text) | StructuralToken::Value(text) => text.clone(),
    }
}

#[test]
fn headless_navigation_omissions_and_localised_modifier_order_remain_explicit() {
    let state = Navigation {
        id: 42,
        title: Text::Modified(
            Box::new(Text::Localised("greeting".into(), "Ada".into())),
            vec![
                Modifier::Bold(false),
                Modifier::DefaultFont,
                Modifier::Bold(true),
                Modifier::Italic,
            ],
        ),
        actions: vec![],
        message: None,
        visibility: None,
    };
    assert_eq!(state.id, 42); // This caller field is intentionally not selected.
    let graph = capture_graph((), CaptureLimits::new(1, 0), |()| {
        NodeCapture::new(state.clone(), vec![])
    })
    .unwrap();
    let mut out = String::new();
    write_graph_view(&graph, limits(), navigation_view, text_token, &mut out).unwrap();
    assert_eq!(out, "{title: _**Hello Ada**_}");
    let mut changed = state;
    changed.id = 99;
    changed.title = Text::Modified(
        Box::new(Text::Localised("greeting".into(), "Ada".into())),
        vec![Modifier::Italic, Modifier::Bold(true)],
    );
    changed.actions = vec!["Continue".into()];
    changed.message = Some("Message".into());
    changed.visibility = Some("visible".into());
    let after = capture_graph((), CaptureLimits::new(1, 0), |()| {
        NodeCapture::new(changed.clone(), vec![])
    })
    .unwrap();
    let mut out = String::new();
    write_graph_view(&after, limits(), navigation_view, text_token, &mut out).unwrap();
    assert_eq!(
        out,
        "{titleVisibility: visible, title: **_Hello Ada_**, actions: [Continue], message: Message}"
    );
    let diff = diff_graph_views(&graph, &after, limits(), navigation_view).unwrap();
    assert!(diff.semantic_changed());
    assert!(diff.changes().contains(&ViewChange::FieldInserted));
    assert!(diff.changes().contains(&ViewChange::Value));
}

#[test]
fn represented_text_equality_and_dialog_visibility_are_distinct_axes() {
    let state = Navigation {
        id: 1,
        title: Text::Concat(
            Box::new(Text::Verbatim("Hello ".into())),
            Box::new(Text::Verbatim("Ada".into())),
        ),
        actions: vec![],
        message: None,
        visibility: None,
    };
    let graph = capture_graph((), CaptureLimits::new(1, 0), |()| {
        NodeCapture::new(state.clone(), vec![])
    })
    .unwrap();
    let mut represented_equal = state.clone();
    represented_equal.title = Text::Verbatim("Hello Ada".into());
    let after = capture_graph((), CaptureLimits::new(1, 0), |()| {
        NodeCapture::new(represented_equal.clone(), vec![])
    })
    .unwrap();
    let diff = diff_graph_views(&graph, &after, limits(), navigation_view).unwrap();
    assert!(diff.semantic_changed());
    assert!(!diff.view_changed());
    let mut visibility_only = state;
    visibility_only.id = 2;
    visibility_only.visibility = Some("visible".into());
    let after = capture_graph((), CaptureLimits::new(1, 0), |()| {
        NodeCapture::new(visibility_only.clone(), vec![])
    })
    .unwrap();
    let diff = diff_graph_views(&graph, &after, limits(), navigation_view).unwrap();
    assert!(!diff.semantic_changed());
    assert_eq!(diff.changes(), &[ViewChange::FieldInserted]);
}

#[test]
fn headless_button_omits_id_and_default_role_but_keeps_send_none() {
    let graph = capture_graph((), CaptureLimits::new(1, 0), |()| {
        NodeCapture::new(0, vec![])
    })
    .unwrap();
    let button = |role: Option<&str>| {
        let mut fields = Vec::new();
        if let Some(role) = role {
            fields.push(("role".into(), TextView::Value(role.into())));
        }
        fields.push((
            "action".into(),
            TextView::Case(
                "send".into(),
                Some(Box::new(TextView::Case("None".into(), None))),
            ),
        ));
        fields.push(("label".into(), TextView::Value("Continue".into())));
        TextView::Fields(fields)
    };
    let mut out = String::new();
    write_graph_view(&graph, limits(), |_| button(None), text_token, &mut out).unwrap();
    assert_eq!(out, "{action: send(None), label: Continue}");
    let mut out = String::new();
    write_graph_view(
        &graph,
        limits(),
        |_| button(Some("cancel")),
        text_token,
        &mut out,
    )
    .unwrap();
    assert_eq!(out, "{role: cancel, action: send(None), label: Continue}");
}

#[test]
fn exact_work_and_scalar_boundaries_allow_success_without_one_extra_dispatch() {
    let graph = capture_graph((), CaptureLimits::new(1, 0), |()| {
        NodeCapture::new(0, vec![])
    })
    .unwrap();
    let mut out = String::new();
    write_graph_view(
        &graph,
        DiagnosticLimits::new(6, 0, 1),
        |_| View::Value(0),
        |_| "é".into(),
        &mut out,
    )
    .unwrap();
    assert_eq!(out, "é");
    let mut out = String::new();
    let error = write_graph_view(
        &graph,
        DiagnosticLimits::new(5, 0, 1),
        |_| View::Value(0),
        |_| "é".into(),
        &mut out,
    )
    .unwrap_err();
    assert!(error.is_work_limit());
    assert_eq!(error.work_used(), 5);
    assert_eq!(error.output_written(), 0);
    assert!(out.is_empty());
    let diff = diff_graph_views(&graph, &graph, DiagnosticLimits::new(9, 0, 0), |_| {
        View::Value(0)
    })
    .unwrap();
    assert!(!diff.semantic_changed());
    assert!(!diff.view_changed());
    assert!(
        diff_graph_views(&graph, &graph, DiagnosticLimits::new(8, 0, 0), |_| {
            View::Value(0)
        })
        .unwrap_err()
        .is_work_limit()
    );
}

#[test]
fn selected_references_compare_logical_origins_not_local_numbering() {
    let make = |reverse: bool| {
        capture_graph(0, CaptureLimits::new(3, 2), |&origin| {
            NodeCapture::new(
                origin,
                if origin == 0 {
                    if reverse {
                        vec![("b".into(), 2), ("a".into(), 1)]
                    } else {
                        vec![("a".into(), 1), ("b".into(), 2)]
                    }
                } else {
                    vec![]
                },
            )
        })
        .unwrap()
    };
    let left = make(false);
    let right = make(true);
    assert_eq!(left.nodes()[1].origin(), &1);
    assert_eq!(right.nodes()[2].origin(), &1);
    let diff = diff_graph_views(&left, &right, limits(), |node| {
        if *node.origin() == 0 {
            View::Reference(
                node.edges()
                    .iter()
                    .find(|edge| edge.label() == "a")
                    .unwrap()
                    .target(),
            )
        } else {
            View::Value(*node.leaves())
        }
    })
    .unwrap();
    assert!(diff.semantic_changed()); // Actual ordered graph edges changed.
    assert!(!diff.view_changed()); // Selected target origin and value did not.
    let mut calls = 0;
    let diff = diff_graph_views(&left, &right, limits(), |node| {
        if *node.origin() == 0 {
            calls += 1;
            View::Reference(
                node.edges()
                    .iter()
                    .find(|edge| edge.label() == if calls == 1 { "a" } else { "b" })
                    .unwrap()
                    .target(),
            )
        } else {
            View::Value(*node.leaves())
        }
    })
    .unwrap();
    assert!(diff.changes().contains(&ViewChange::Reference));
}

#[test]
fn edge_and_selected_reference_depths_fail_instead_of_claiming_equal() {
    let graph = capture_graph(0, CaptureLimits::new(2, 1), |&origin| {
        NodeCapture::new(
            origin,
            if origin == 0 {
                vec![("next".into(), 1)]
            } else {
                vec![]
            },
        )
    })
    .unwrap();
    let error = diff_graph_views(&graph, &graph, DiagnosticLimits::new(100, 0, 0), |_| {
        View::Value(0)
    })
    .unwrap_err();
    assert!(error.is_depth_limit());
    let mut out = String::new();
    let error = write_graph_view(
        &graph,
        DiagnosticLimits::new(100, 0, 100),
        |node| {
            if *node.origin() == 0 {
                View::Reference(1)
            } else {
                View::Value(1)
            }
        },
        |_| "value".into(),
        &mut out,
    )
    .unwrap_err();
    assert!(error.is_depth_limit());
    assert!(!out.contains("value"));
}

#[test]
fn every_view_and_token_debug_keeps_private_domain_data_opaque() {
    struct Private;
    let private = Private;
    for (view, shape) in [
        (StructuralView::Value(&private), "Value"),
        (
            StructuralView::Fields(vec![(
                "private-field".into(),
                StructuralView::Value(&private),
            )]),
            "Fields",
        ),
        (
            StructuralView::Case(
                "private-case".into(),
                Some(Box::new(StructuralView::Value(&private))),
            ),
            "Case",
        ),
        (
            StructuralView::Sequence(vec![StructuralView::Value(&private)]),
            "Sequence",
        ),
        (
            StructuralView::Map(vec![(&private, StructuralView::Value(&private))]),
            "Map",
        ),
        (
            StructuralView::Represented(Box::new(StructuralView::Value(&private))),
            "Represented",
        ),
        (StructuralView::Reference(987_654), "Reference"),
    ] {
        assert_eq!(format!("{view:?}"), format!("{shape} {{ .. }}"));
    }
    for token in [
        StructuralToken::Label("private-label"),
        StructuralToken::Key(&private),
        StructuralToken::Value(&private),
    ] {
        assert_eq!(format!("{token:?}"), "StructuralToken { .. }");
    }
}

#[test]
fn diagnostic_failures_expose_only_their_class_counters_and_backtrace() {
    struct FailingWriter;
    impl fmt::Write for FailingWriter {
        fn write_str(&mut self, _: &str) -> fmt::Result {
            Err(fmt::Error)
        }
    }
    let graph = capture_graph("private-origin", CaptureLimits::new(1, 0), |_| {
        NodeCapture::new("private-leaf", vec![])
    })
    .unwrap();
    let work = write_graph_view(
        &graph,
        DiagnosticLimits::new(0, 0, 0),
        |_| View::Value(0),
        |_| "redacted".into(),
        &mut String::new(),
    )
    .unwrap_err();
    let depth = write_graph_view(
        &graph,
        DiagnosticLimits::new(100, 0, 100),
        |_| View::Sequence(vec![View::Value(0)]),
        |_| "redacted".into(),
        &mut String::new(),
    )
    .unwrap_err();
    let output = write_graph_view(
        &graph,
        DiagnosticLimits::new(100, 0, 0),
        |_| View::Value(0),
        |_| "redacted".into(),
        &mut String::new(),
    )
    .unwrap_err();
    let invalid = write_graph_view(
        &graph,
        limits(),
        |_| View::Fields(vec![("private-field".into(), View::Reference(1))]),
        |_| "redacted".into(),
        &mut String::new(),
    )
    .unwrap_err();
    let writer = write_graph_view(
        &graph,
        limits(),
        |_| View::Value(0),
        |_| "redacted".into(),
        &mut FailingWriter,
    )
    .unwrap_err();
    for (index, error) in [work, depth, output, invalid, writer]
        .into_iter()
        .enumerate()
    {
        assert_eq!(
            [
                error.is_work_limit(),
                error.is_depth_limit(),
                error.is_output_limit(),
                error.is_invalid_view(),
                error.is_writer_failure(),
            ],
            [index == 0, index == 1, index == 2, index == 3, index == 4]
        );
        assert_eq!(error.output_written(), 0);
        let display = error.to_string();
        assert!(display.contains(&format!("work {}, output 0", error.work_used())));
        assert!(display.ends_with(&error.backtrace().to_string()));
        assert!(!display.contains("private-"));
        assert!(!format!("{error:?}").contains("private-"));
        if index == 4 {
            assert!(
                std::error::Error::source(&error)
                    .unwrap()
                    .is::<fmt::Error>()
            );
        } else {
            assert!(std::error::Error::source(&error).is_none());
        }
    }
}

#[test]
fn every_write_work_budget_preserves_a_valid_prefix_without_extra_redaction() {
    let graph = capture_graph(0, CaptureLimits::new(2, 1), |&origin| {
        NodeCapture::new(
            origin,
            if origin == 0 {
                vec![("child".into(), 1)]
            } else {
                vec![]
            },
        )
    })
    .unwrap();
    for (view, expected, expected_redactions) in [
        (View::Value(1), "1", 1),
        (
            View::Fields(vec![
                ("left".into(), View::Value(1)),
                ("right".into(), View::Value(2)),
            ]),
            "{left: 1, right: 2}",
            4,
        ),
        (
            View::Map(vec![(1, View::Value(3)), (2, View::Value(4))]),
            "{1: 3, 2: 4}",
            4,
        ),
        (
            View::Sequence(vec![View::Value(1), View::Value(2)]),
            "[1, 2]",
            2,
        ),
        (
            View::Case("some".into(), Some(Box::new(View::Value(1)))),
            "some(1)",
            2,
        ),
        (View::Case("none".into(), None), "none", 1),
        (View::Represented(Box::new(View::Value(1))), "1", 1),
        (View::Reference(0), "*0", 0),
        (View::Reference(1), "&1(7)", 1),
        (
            View::Sequence(vec![
                View::Reference(1),
                View::Reference(1),
                View::Reference(0),
            ]),
            "[&1(7), *1, *0]",
            1,
        ),
    ] {
        let mut succeeded = false;
        for work in 0..=128 {
            let projections = Cell::new(0);
            let redactions = Cell::new(0);
            let mut out = String::new();
            let result = write_graph_view(
                &graph,
                DiagnosticLimits::new(work, 4, 100),
                |node| {
                    projections.set(projections.get() + 1);
                    if *node.origin() == 0 {
                        view.clone()
                    } else {
                        View::Value(7)
                    }
                },
                |token| {
                    redactions.set(redactions.get() + 1);
                    match token {
                        StructuralToken::Label(label) => label.into(),
                        StructuralToken::Key(value) | StructuralToken::Value(value) => {
                            value.to_string()
                        }
                    }
                },
                &mut out,
            );
            match result {
                Ok(()) => {
                    succeeded = true;
                    assert_eq!(out, expected);
                    assert_eq!(projections.get(), 2);
                    assert_eq!(redactions.get(), expected_redactions);
                }
                Err(error) => {
                    assert!(!succeeded, "increasing work must not undo success");
                    assert!(error.is_work_limit(), "{expected}, work {work}: {error}");
                    assert_eq!(error.work_used(), work);
                    assert_eq!(error.output_written(), out.chars().count());
                    assert!(expected.starts_with(&out), "unexpected prefix: {out:?}");
                    assert!(projections.get() <= work.min(2));
                    assert!(redactions.get() <= expected_redactions);
                }
            }
        }
        assert!(succeeded, "fixture must finish within its work budget");
    }
}

#[test]
fn every_diff_work_budget_returns_complete_metadata_or_exhaustion() {
    let graph = capture_graph(0, CaptureLimits::new(2, 1), |&origin| {
        NodeCapture::new(
            origin,
            if origin == 0 {
                vec![("child".into(), 1)]
            } else {
                vec![]
            },
        )
    })
    .unwrap();
    for (before, after, expected) in [
        (View::Value(1), View::Value(2), vec![ViewChange::Value]),
        (
            View::Fields(vec![
                ("a".into(), View::Value(1)),
                ("b".into(), View::Value(2)),
            ]),
            View::Fields(vec![
                ("b".into(), View::Value(3)),
                ("a".into(), View::Value(1)),
            ]),
            vec![ViewChange::FieldOrder, ViewChange::Value],
        ),
        (
            View::Map(vec![(1, View::Value(10)), (2, View::Value(20))]),
            View::Map(vec![(2, View::Value(21)), (3, View::Value(30))]),
            vec![
                ViewChange::MapRemoved,
                ViewChange::MapInserted,
                ViewChange::Value,
            ],
        ),
        (
            View::Sequence(vec![View::Value(1)]),
            View::Sequence(vec![View::Value(2), View::Value(3)]),
            vec![ViewChange::SequenceLength, ViewChange::Value],
        ),
        (
            View::Case("before".into(), Some(Box::new(View::Value(1)))),
            View::Case("after".into(), Some(Box::new(View::Value(2)))),
            vec![ViewChange::Case, ViewChange::Value],
        ),
        (
            View::Represented(Box::new(View::Value(1))),
            View::Represented(Box::new(View::Value(2))),
            vec![ViewChange::Value],
        ),
        (View::Reference(1), View::Reference(1), vec![]),
        (
            View::Reference(1),
            View::Reference(0),
            vec![ViewChange::Reference, ViewChange::Shape],
        ),
        (
            View::Sequence(vec![View::Reference(1), View::Reference(1)]),
            View::Sequence(vec![View::Reference(1), View::Reference(1)]),
            vec![],
        ),
    ] {
        let mut succeeded = false;
        for work in 0..=128 {
            let mut projections = vec![
                View::Value(7),
                after.clone(),
                View::Value(7),
                before.clone(),
            ];
            match diff_graph_views(&graph, &graph, DiagnosticLimits::new(work, 4, 0), |_| {
                projections
                    .pop()
                    .expect("each capture node is projected once")
            }) {
                Ok(diff) => {
                    succeeded = true;
                    assert!(!diff.semantic_changed());
                    assert_eq!(diff.changes(), expected);
                    assert_eq!(diff.view_changed(), !expected.is_empty());
                    assert!(projections.is_empty());
                }
                Err(error) => {
                    assert!(!succeeded, "increasing work must not undo success");
                    assert!(error.is_work_limit(), "work {work}: {error}");
                    assert_eq!(error.work_used(), work);
                    assert_eq!(error.output_written(), 0);
                }
            }
        }
        assert!(succeeded, "fixture must finish within its work budget");
    }
}

#[test]
fn selected_child_depth_accumulates_across_references_before_redaction() {
    let graph = capture_graph(0, CaptureLimits::new(2, 1), |&origin| {
        NodeCapture::new(
            origin,
            if origin == 0 {
                vec![("child".into(), 1)]
            } else {
                vec![]
            },
        )
    })
    .unwrap();
    for child in [
        View::Fields(vec![("private-field".into(), View::Value(7))]),
        View::Map(vec![(1, View::Value(7))]),
        View::Sequence(vec![View::Value(7)]),
        View::Case("private-case".into(), Some(Box::new(View::Value(7)))),
        View::Represented(Box::new(View::Value(7))),
    ] {
        let project = |node: &rca_core::diagnostics::CapturedNode<i32, i32>| {
            if *node.origin() == 0 {
                View::Reference(1)
            } else {
                child.clone()
            }
        };
        let error = diff_graph_views(&graph, &graph, DiagnosticLimits::new(100, 1, 0), project)
            .unwrap_err();
        assert!(error.is_depth_limit());
        assert_eq!(error.output_written(), 0);
        let redactions = Cell::new(0);
        let mut out = String::new();
        let error = write_graph_view(
            &graph,
            DiagnosticLimits::new(100, 1, 100),
            project,
            |_| {
                redactions.set(redactions.get() + 1);
                "redacted".into()
            },
            &mut out,
        )
        .unwrap_err();
        assert!(error.is_depth_limit());
        assert_eq!(error.output_written(), 3);
        assert_eq!(out, "&1(");
        assert_eq!(redactions.get(), 0);
    }
}

#[test]
fn selected_reference_chain_depth_is_independent_of_captured_edge_depth() {
    // The captured star has depth one; selected references form a depth-two chain.
    let graph = capture_graph(0, CaptureLimits::new(3, 2), |&origin| {
        NodeCapture::new(
            origin,
            if origin == 0 {
                vec![("first".into(), 1), ("second".into(), 2)]
            } else {
                vec![]
            },
        )
    })
    .unwrap();
    let projections = Cell::new(0);
    let project = |node: &rca_core::diagnostics::CapturedNode<i32, i32>| {
        projections.set(projections.get() + 1);
        match *node.origin() {
            0 => View::Reference(1),
            1 => View::Reference(2),
            _ => View::Value(7),
        }
    };
    let error =
        diff_graph_views(&graph, &graph, DiagnosticLimits::new(100, 1, 0), project).unwrap_err();
    assert!(error.is_depth_limit());
    assert_eq!(error.output_written(), 0);
    assert_eq!(projections.get(), 6);

    projections.set(0);
    let diff = diff_graph_views(&graph, &graph, DiagnosticLimits::new(100, 2, 0), project).unwrap();
    assert!(!diff.semantic_changed());
    assert!(!diff.view_changed());
    assert!(diff.changes().is_empty());
    assert_eq!(projections.get(), 6);
}

#[cfg(feature = "allocation-measurement")]
/// Checks scratch reuse in the dedicated allocation host.
///
/// # Panics
/// Panics if structural validation or allocation counts change.
pub fn structural_validation_reuses_scratch_across_captured_nodes() {
    struct CountOutput(usize);

    impl fmt::Write for CountOutput {
        fn write_str(&mut self, s: &str) -> fmt::Result {
            self.0 += s.len();
            Ok(())
        }
    }

    if allocation_test::run_in_child("structural_validation_reuses_scratch_across_captured_nodes") {
        return;
    }

    let _profiler = dhat::Profiler::builder().testing().build();
    let mut costs = [[(0, 0); 2]; 2];
    for (shape, wide_root) in [false, true].into_iter().enumerate() {
        for (size, nodes) in [1, 16].into_iter().enumerate() {
            let graph = capture_graph(0, CaptureLimits::new(nodes, nodes - 1), |&origin| {
                NodeCapture::new(
                    (),
                    if origin == 0 {
                        (1..nodes).map(|child| (String::new(), child)).collect()
                    } else {
                        Vec::new()
                    },
                )
            })
            .unwrap();
            // Graph capture and caller-owned view construction are not library
            // validation costs. The wide root is followed by scalar nodes.
            let mut views = (0..nodes)
                .map(|node| {
                    if wide_root && node == 0 {
                        StructuralView::Sequence(
                            (0..32).map(|_| StructuralView::Value(())).collect(),
                        )
                    } else {
                        StructuralView::<(), ()>::Value(())
                    }
                })
                .collect::<Vec<_>>()
                .into_iter();
            let projections = Cell::new(0);
            let redactions = Cell::new(0);
            let mut output = CountOutput(0);
            let before = dhat::HeapStats::get();
            write_graph_view(
                &graph,
                limits(),
                |_| {
                    projections.set(projections.get() + 1);
                    views.next().expect("one view per captured node")
                },
                |_| {
                    redactions.set(redactions.get() + 1);
                    String::new()
                },
                &mut output,
            )
            .unwrap();
            let after = dhat::HeapStats::get();
            costs[shape][size] = (
                after.total_blocks - before.total_blocks,
                after.total_bytes - before.total_bytes,
            );
            assert_eq!(projections.get(), nodes);
            assert!(views.next().is_none());
            assert_eq!(redactions.get(), if wide_root { 32 } else { 1 });
            assert_eq!(output.0, if wide_root { 64 } else { 0 });
        }
    }
    eprintln!(
        "structural validation allocations (blocks, bytes): scalar={:?}, wide-then-scalar={:?}",
        costs[0], costs[1]
    );
    for [one, sixteen] in costs {
        assert!(
            sixteen.0 <= one.0 + 3,
            "extra nodes may grow the view vector, not allocate one validation buffer each: {one:?} -> {sixteen:?}",
        );
    }
}

#[test]
fn invalid_wide_view_drops_all_projected_values_without_redaction() {
    struct Value<'a> {
        index: usize,
        drops: &'a std::cell::RefCell<Vec<usize>>,
    }

    impl Drop for Value<'_> {
        fn drop(&mut self) {
            self.drops.borrow_mut().push(self.index);
        }
    }

    let graph = capture_graph(0, CaptureLimits::new(3, 2), |&origin| {
        NodeCapture::new(
            (),
            if origin == 0 {
                vec![(String::new(), 1), (String::new(), 2)]
            } else {
                Vec::new()
            },
        )
    })
    .unwrap();
    let projections = std::cell::RefCell::new(Vec::new());
    let drops = std::cell::RefCell::new(Vec::new());
    let mut output = String::new();
    let error = write_graph_view(
        &graph,
        limits(),
        |node| {
            let origin = *node.origin();
            projections.borrow_mut().push(origin);
            if origin == 0 {
                let mut children = (0..32)
                    .map(|index| {
                        StructuralView::Value(Value {
                            index,
                            drops: &drops,
                        })
                    })
                    .collect::<Vec<_>>();
                // Validation has already queued 32 borrowed values when this
                // invalid reference fails. None of those loans owns a value.
                children.push(StructuralView::Reference(3));
                StructuralView::<(), _>::Sequence(children)
            } else {
                StructuralView::Value(Value {
                    index: origin + 31,
                    drops: &drops,
                })
            }
        },
        |_| panic!("invalid views must fail before redaction"),
        &mut output,
    )
    .unwrap_err();
    assert!(error.is_invalid_view());
    assert_eq!(error.output_written(), 0);
    assert!(output.is_empty());
    assert_eq!(*projections.borrow(), [0, 1, 2]);
    assert_eq!(*drops.borrow(), (0..34).collect::<Vec<_>>());
}

#[cfg(feature = "allocation-measurement")]
/// Checks allocation cleanup when a structural report destructor panics.
///
/// # Panics
/// Panics if report cleanup leaves tracked allocations alive.
pub fn structural_diff_cleanup_panic_releases_the_completed_report() {
    struct Probe<'a> {
        tag: u8,
        panics: bool,
        events: &'a Cell<[u8; 3]>,
        count: &'a Cell<usize>,
    }

    impl Drop for Probe<'_> {
        fn drop(&mut self) {
            let index = self.count.get();
            let mut events = self.events.get();
            events[index] = self.tag;
            self.events.set(events);
            self.count.set(index + 1);
            assert!(!self.panics, "structural cleanup panic");
        }
    }

    struct Value<'a> {
        value: i32,
        _probe: Probe<'a>,
    }

    impl PartialEq for Value<'_> {
        fn eq(&self, other: &Self) -> bool {
            self.value == other.value
        }
    }

    fn run(panic_in_view: bool) {
        let left =
            capture_graph(0, CaptureLimits::new(1, 0), |_| NodeCapture::new(1, vec![])).unwrap();
        let right =
            capture_graph(0, CaptureLimits::new(1, 0), |_| NodeCapture::new(2, vec![])).unwrap();
        let events = Cell::new([0; 3]);
        let count = Cell::new(0);
        let projections = Cell::new(0);
        let capture = Probe {
            tag: 3,
            panics: !panic_in_view,
            events: &events,
            count: &count,
        };
        let project = {
            let events = &events;
            let count = &count;
            let projections = &projections;
            move |node: &rca_core::diagnostics::CapturedNode<i32, i32>| {
                // Retain the whole Drop probe in the owned callback.
                std::hint::black_box(&capture);
                let value = *node.leaves();
                projections.set(projections.get() + 1);
                StructuralView::<(), _>::Value(Value {
                    value,
                    _probe: Probe {
                        tag: u8::try_from(value).unwrap(),
                        panics: panic_in_view && value == 2,
                        events,
                        count,
                    },
                })
            }
        };
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // The values differ, so the completed report owns a non-empty Vec.
            diff_graph_views(&left, &right, limits(), project)
        }))
        .unwrap_err();
        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"structural cleanup panic")
        );
        assert_eq!(projections.get(), 2);
        assert_eq!(count.get(), 3);
        assert_eq!(events.get(), [2, 1, 3]);
    }

    if allocation_test::run_in_child("structural_diff_cleanup_panic_releases_the_completed_report")
    {
        return;
    }
    for panic_in_view in [false, true] {
        run(panic_in_view);
    }
    let profiler = dhat::Profiler::builder().testing().build();
    let mut retained = [(0, 0); 2];
    for (index, panic_in_view) in [false, true].into_iter().enumerate() {
        let before = dhat::HeapStats::get();
        run(panic_in_view);
        let after = dhat::HeapStats::get();
        retained[index] = (
            after.curr_blocks - before.curr_blocks,
            after.curr_bytes - before.curr_bytes,
        );
    }
    drop(profiler);
    eprintln!("structural cleanup retained (blocks, bytes), capture/view: {retained:?}");
    assert_eq!(retained, [(0, 0); 2]);
}
