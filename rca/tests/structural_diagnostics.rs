//! Separates omitted semantic changes from rendered output and Unicode output limits.

#![cfg(feature = "diagnostics")]

use rca::diagnostics::{
    CaptureLimits, DiagnosticLimits, NodeCapture, StructuralView, capture_graph, diff_graph_views,
    write_graph_view,
};

/// Compares private borrowed values without exposing them through `Debug`.
#[derive(PartialEq)]
struct Secret<'a>(&'a str);

#[test]
fn facade_keeps_omitted_semantics_and_bounded_redaction_independent() {
    let before = capture_graph(0, CaptureLimits::new(1, 0), |_| {
        NodeCapture::new(Secret("old private value"), vec![])
    })
    .unwrap();
    let after = capture_graph(0, CaptureLimits::new(1, 0), |_| {
        NodeCapture::new(Secret("new private value"), vec![])
    })
    .unwrap();
    let report = diff_graph_views(&before, &after, DiagnosticLimits::new(100, 0, 0), |_| {
        StructuralView::<u8, &Secret<'_>>::Fields(vec![])
    })
    .unwrap();
    assert!(report.semantic_changed());
    assert!(!report.view_changed());

    let mut output = String::new();
    let error = write_graph_view(
        &before,
        DiagnosticLimits::new(100, 0, 2),
        |node| StructuralView::<u8, _>::Value(node.leaves()),
        |_| "é🦀x".into(),
        &mut output,
    )
    .unwrap_err();
    assert!(error.is_output_limit());
    assert_eq!(output, "é🦀");
}
