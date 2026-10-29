#![cfg(feature = "diagnostics")]

use std::cell::{Cell, RefCell};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;

use rca_core::diagnostics::{CaptureLimits, NodeCapture, ReferenceKind, capture_graph};

#[derive(Eq, PartialEq)]
struct Origin(u8); // Deliberately neither Clone, Debug nor Hash.

struct Model {
    private: Cell<u32>,
    shared: Rc<RefCell<String>>,
}

impl Model {
    fn capture(&self) -> rca_core::diagnostics::CapturedGraph<Origin, (u32, String)> {
        capture_graph(Origin(0), CaptureLimits::new(1, 0), |_| {
            NodeCapture::new(
                (self.private.get(), self.shared.borrow().to_owned()),
                vec![],
            )
        })
        .unwrap()
    }
}

#[test]
fn independently_captured_private_and_aliased_values_keep_history() {
    let shared = Rc::new(RefCell::new(String::from("before")));
    let model = Model {
        private: Cell::new(1),
        shared: Rc::clone(&shared),
    };
    let before = model.capture();
    model.private.set(2);
    shared.borrow_mut().push_str(" changed");
    let after = model.capture();
    assert_eq!(before.root().leaves(), &(1, String::from("before")));
    assert_eq!(after.root().leaves(), &(2, String::from("before changed")));
    assert_ne!(before, after);
    // Public Debug must not require Origin/Leaves Debug or expose private values.
    let rendered = format!("{before:?}");
    assert!(!rendered.contains("before"));
}

fn graph(reversed: bool) -> rca_core::diagnostics::CapturedGraph<u8, Cell<u32>> {
    capture_graph(0, CaptureLimits::new(3, 5), |id: &u8| {
        let edges = match id {
            0 if reversed => vec![("second".into(), 2), ("first".into(), 1)],
            0 => vec![("first".into(), 1), ("second".into(), 2)],
            1 => vec![("shared".into(), 2), ("root".into(), 0)],
            2 => vec![("self".into(), 2)],
            _ => unreachable!(),
        };
        NodeCapture::new(Cell::new(u32::from(*id)), edges)
    })
    .unwrap()
}

#[test]
fn repeats_and_cycles_are_finite_and_graph_clones_do_not_share_selected_leaves() {
    let before = graph(false);
    assert_eq!(before.nodes().len(), 3);
    assert_eq!(before.root().edges()[0].kind(), ReferenceKind::First);
    assert_eq!(before.root().edges()[1].kind(), ReferenceKind::Repeat);
    assert_eq!(before.nodes()[1].edges()[1].kind(), ReferenceKind::Cycle);
    assert_eq!(before.nodes()[2].edges()[0].kind(), ReferenceKind::Cycle);
    assert_eq!(before.nodes()[2].origin(), &2);
    assert_eq!(before.root().edges()[1].target(), 2);
    let mut expected = before.clone();
    expected.leaves_mut(&2).unwrap().set(99);
    assert_eq!(before.nodes()[2].leaves().get(), 2);
    assert_ne!(before, expected);
    assert!(expected.leaves_mut(&99).is_none());
    assert_eq!(before, graph(false));
    assert_ne!(before, graph(true)); // Selected edge order is semantic.
}

#[test]
fn equal_origins_compare_leaves_not_allocations_or_origin_identity_alone() {
    let left = Model {
        private: Cell::new(7),
        shared: Rc::new(RefCell::new("equal".into())),
    };
    let right = Model {
        private: Cell::new(7),
        shared: Rc::new(RefCell::new("equal".into())),
    };
    assert!(!Rc::ptr_eq(&left.shared, &right.shared));
    assert_eq!(left.capture(), right.capture());
    right.private.set(8);
    assert_ne!(left.capture(), right.capture());
}

#[test]
fn reference_storage_and_callback_captures_are_reclaimed() {
    struct Leaf(Rc<()>);
    let probe = Rc::new(());
    let weak = Rc::downgrade(&probe);
    let captured = capture_graph(0, CaptureLimits::new(1, 1), |_| {
        NodeCapture::new(Leaf(Rc::clone(&probe)), vec![("self".into(), 0)])
    })
    .unwrap();
    assert_eq!(Rc::strong_count(&captured.root().leaves().0), 2);
    drop(probe);
    assert!(weak.upgrade().is_some());
    drop(captured);
    assert!(weak.upgrade().is_none());
}

#[test]
fn exact_budgets_accept_cycles_and_fail_without_partial_success() {
    let calls = Cell::new(0);
    let zero = capture_graph(0, CaptureLimits::new(0, 1), |_| {
        calls.set(calls.get() + 1);
        NodeCapture::new((), vec![])
    })
    .unwrap_err();
    assert!(zero.is_node_limit());
    assert!(!zero.is_edge_limit());
    assert_eq!(calls.get(), 0);
    assert_eq!(zero.captured_nodes(), 0);
    assert_eq!(zero.limits(), CaptureLimits::new(0, 1));
    let edges = capture_graph(0, CaptureLimits::new(1, 0), |_| {
        NodeCapture::new((), vec![("self".into(), 0)])
    })
    .unwrap_err();
    assert!(edges.is_edge_limit());
    assert!(!edges.is_node_limit());
    assert_eq!(edges.declared_edges(), 0);
    assert!(format!("{edges}").contains("edge limit"));
    let nodes = capture_graph(0, CaptureLimits::new(1, 1), |_| {
        NodeCapture::new((), vec![("next".into(), 1)])
    })
    .unwrap_err();
    assert!(nodes.is_node_limit());
    assert!(!nodes.is_edge_limit());
    assert_eq!(
        nodes.to_string().lines().next(),
        Some("capture node limit exhausted: 1 nodes and 1 declared edges admitted, limits 1/1")
    );
    assert_eq!(nodes.captured_nodes(), 1);
    assert_eq!(nodes.declared_edges(), 1);
    assert!(std::error::Error::source(&nodes).is_none());
    let exact = capture_graph(0, CaptureLimits::new(1, 1), |_| {
        NodeCapture::new((), vec![("self".into(), 0)])
    })
    .unwrap();
    assert_eq!(exact.root().edges()[0].kind(), ReferenceKind::Cycle);
}

#[test]
fn callback_panics_propagate_and_drop_partial_storage() {
    let resource = Rc::new(());
    let weak = Rc::downgrade(&resource);
    let result = catch_unwind(AssertUnwindSafe(move || {
        let _ = capture_graph(0, CaptureLimits::new(2, 1), |id| {
            assert_eq!(*id, 0, "capture panic");
            NodeCapture::new(Rc::clone(&resource), vec![("child".into(), 1)])
        });
    }));
    assert!(result.is_err());
    assert!(weak.upgrade().is_none());
}

#[test]
fn origin_comparison_panic_releases_pending_origins_and_captured_leaves() {
    struct PanickingOrigin(Rc<()>);

    impl PartialEq for PanickingOrigin {
        fn eq(&self, other: &Self) -> bool {
            assert!(Rc::ptr_eq(&self.0, &other.0));
            panic!("origin comparison");
        }
    }

    impl Eq for PanickingOrigin {}

    let resource = Rc::new(());
    let weak = Rc::downgrade(&resource);
    let root = PanickingOrigin(Rc::clone(&resource));
    let calls = Cell::new(0);
    let capture_calls = &calls;
    let panic = catch_unwind(AssertUnwindSafe(move || {
        capture_graph(root, CaptureLimits::new(3, 2), |_| {
            capture_calls.set(capture_calls.get() + 1);
            NodeCapture::new(
                Rc::clone(&resource),
                vec![
                    ("first".into(), PanickingOrigin(Rc::clone(&resource))),
                    ("pending".into(), PanickingOrigin(Rc::clone(&resource))),
                ],
            )
        })
    }))
    .unwrap_err();

    assert_eq!(panic.downcast_ref::<&str>(), Some(&"origin comparison"));
    assert_eq!(calls.get(), 1);
    assert!(weak.upgrade().is_none());
}

#[test]
fn capture_destructor_panic_releases_the_unreturned_graph() {
    struct PanicOnDrop;

    impl Drop for PanicOnDrop {
        fn drop(&mut self) {
            panic!("capture destructor");
        }
    }

    let leaves = Rc::new(());
    let weak = Rc::downgrade(&leaves);
    let callback_probe = PanicOnDrop;
    let panic = catch_unwind(AssertUnwindSafe(move || {
        capture_graph(0, CaptureLimits::new(1, 0), move |_| {
            std::hint::black_box(&callback_probe);
            NodeCapture::new(Rc::clone(&leaves), vec![])
        })
    }))
    .unwrap_err();

    assert_eq!(panic.downcast_ref::<&str>(), Some(&"capture destructor"));
    assert!(
        weak.upgrade().is_none(),
        "unreturned graph retained its leaves"
    );
}

#[test]
fn rejected_capture_drops_admitted_and_rejected_leaf_resources() {
    let resource = Rc::new(());
    let weak = Rc::downgrade(&resource);
    let calls = Cell::new(0);
    let error = capture_graph(0, CaptureLimits::new(2, 1), |id| {
        calls.set(calls.get() + 1);
        NodeCapture::new(Rc::clone(&resource), vec![("next".into(), id + 1)])
    })
    .unwrap_err();
    assert!(error.is_edge_limit());
    assert_eq!(error.captured_nodes(), 1);
    assert_eq!(error.declared_edges(), 1);
    assert_eq!(calls.get(), 2);
    drop(resource);
    assert!(weak.upgrade().is_none());
}

#[test]
fn caller_borrowed_origins_and_move_only_leaves_need_no_static_or_clone_bounds() {
    struct MoveOnly(u8);
    let name = String::from("local private origin");
    let graph = capture_graph(name.as_str(), CaptureLimits::new(1, 1), |_| {
        let node = NodeCapture::new(MoveOnly(5), vec![("private label".into(), name.as_str())]);
        assert_eq!(format!("{node:?}"), "NodeCapture { edge_count: 1, .. }");
        node
    })
    .unwrap();
    assert_eq!(graph.root().leaves().0, 5);
    assert_eq!(graph.root().origin(), &name.as_str());
    assert_eq!(graph.root().edges()[0].label(), "private label");
    let output = format!("{graph:?} {:?} {:?}", graph.root(), graph.root().edges());
    assert!(!output.contains("private"));
}

#[test]
fn selected_alias_topology_and_root_origin_are_not_erased() {
    let shared = capture_graph(0, CaptureLimits::new(2, 2), |id| {
        NodeCapture::new(
            7,
            if *id == 0 {
                vec![("a".into(), 1), ("b".into(), 1)]
            } else {
                vec![]
            },
        )
    })
    .unwrap();
    let duplicated = capture_graph(0, CaptureLimits::new(3, 2), |id| {
        NodeCapture::new(
            7,
            if *id == 0 {
                vec![("a".into(), 1), ("b".into(), 2)]
            } else {
                vec![]
            },
        )
    })
    .unwrap();
    assert_ne!(shared, duplicated);
    let different_root = capture_graph(9, CaptureLimits::new(2, 2), |id| {
        NodeCapture::new(
            7,
            if *id == 9 {
                vec![("a".into(), 1), ("b".into(), 1)]
            } else {
                vec![]
            },
        )
    })
    .unwrap();
    assert_ne!(shared, different_root);
}

#[test]
fn an_additional_reference_to_an_existing_child_changes_graph_equality() {
    let capture = |extra_reference| {
        capture_graph(0, CaptureLimits::new(2, 2), |id| {
            let edges = if *id == 0 {
                let mut edges = vec![("first".into(), 1)];
                if extra_reference {
                    edges.push(("second".into(), 1));
                }
                edges
            } else {
                vec![]
            };
            NodeCapture::new(*id, edges)
        })
        .unwrap()
    };
    let single_reference = capture(false);
    let repeated_reference = capture(true);

    assert_eq!(single_reference.nodes().len(), 2);
    assert_eq!(repeated_reference.nodes().len(), 2);
    assert_eq!(single_reference.root().edges().len(), 1);
    assert_eq!(repeated_reference.root().edges().len(), 2);
    assert_eq!(single_reference, capture(false));
    assert_eq!(repeated_reference, capture(true));
    assert_ne!(single_reference, repeated_reference);
    assert_ne!(repeated_reference, single_reference);
}
