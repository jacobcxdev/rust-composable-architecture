//! Finite deep/wide and nested-Break controls over public actor-owned mounts.
use rca_core::{
    EntryMount, Feature, FeatureContext, Fold, FoldKey, KeyedState, MountChildren, MountIdentity,
    OptionalMount, Runtime, Store,
};
use std::cell::Cell;
use std::marker::PhantomData;
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
#[path = "fold/support.rs"]
mod support;
use support::Driver;

/// Forces each fold contribution to transfer an owned, non-`Sync` value.
struct Owned(Box<Cell<usize>>);
impl Owned {
    /// Creates one allocation whose value stays mutable without `Sync`.
    fn new(value: usize) -> Self {
        Self(Box::new(Cell::new(value)))
    }
}
/// Counts visits through their values rather than depending only on callback counters.
struct Sum;
impl FoldKey for Sum {
    type Value = Owned;
    fn identity() -> Owned {
        Owned::new(0)
    }
    fn reduce(accumulator: Owned, next: Owned) -> ControlFlow<Owned, Owned> {
        accumulator.0.set(accumulator.0.get() + next.0.get());
        ControlFlow::Continue(accumulator)
    }
}
/// Mounts arbitrary state without requiring its feature to own that state type.
struct Noop<S>(PhantomData<fn() -> S>);
impl<S: 'static> Feature for Noop<S> {
    type State = S;
    type Action = ();
    fn reduce(&mut self, _: &mut S, (): &(), _: &mut FeatureContext<'_, S, ()>) {}
}
/// Provides ordered descendants for deep, wide and transformed subtree checks.
struct Node {
    value: usize,
    children: KeyedState<u8, Node>,
}
/// Makes the complete tree optional at the root mount.
type Tree = Option<Node>;

/// Declares each node once and records only actual contribution callbacks.
///
/// Mode zero has no transforms. Mode one adds 100 at node one and multiplies
/// node two by ten. Mode two multiplies both those nodes by ten.
fn declare<K: FoldKey<Value = Owned>>(
    node: &Node,
    children: &mut MountChildren<'_, Tree, (), Node, ()>,
    fold: &Fold<Tree, K>,
    calls: &Arc<AtomicUsize>,
    transforms: u8,
    trace: &Arc<Mutex<Vec<usize>>>,
) {
    let selected = Arc::clone(calls);
    let trace_selected = Arc::clone(trace);
    let value = move |node: &Node| {
        selected.fetch_add(1, Ordering::SeqCst);
        trace_selected.lock().unwrap().push(node.value);
        Owned::new(node.value)
    };
    if transforms == 1 && node.value == 1 {
        children.contribute_transformed(fold, value, |value| {
            value.0.set(value.0.get() + 100);
            value
        });
    } else if transforms != 0 && (node.value == 2 || node.value == 1) {
        children.contribute_transformed(fold, value, |value| {
            value.0.set(value.0.get() * 10);
            value
        });
    } else {
        children.contribute(fold, value);
    }
    let fold = fold.clone();
    let calls = Arc::clone(calls);
    let trace = Arc::clone(trace);
    children.keyed_with_children(
        |node| &node.children,
        |node| &mut node.children,
        move |_, _| {
            let fold = fold.clone();
            let calls = Arc::clone(&calls);
            let trace = Arc::clone(&trace);
            (
                EntryMount::new(Noop::<Node>(PhantomData)),
                move |node: &Node, children: &mut MountChildren<'_, Tree, (), Node, ()>| {
                    declare(node, children, &fold, &calls, transforms, &trace);
                },
            )
        },
    );
}
/// Mounts a present tree with a stable root identity.
fn descriptor() -> OptionalMount<Tree, Noop<Node>, ()> {
    OptionalMount::new(
        Option::as_ref,
        Option::as_mut,
        |tree: &Tree| {
            tree.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        |_| Noop::<Node>(PhantomData),
    )
}
#[test]
fn finite_depths_and_widths_visit_each_committed_contributor_once_per_read() {
    // Native retains scale; Miri checks the same paths with existing small shapes.
    let depths: &[usize] = if cfg!(miri) {
        &[1, 5, 20]
    } else {
        &[1, 5, 20, 256]
    };
    for &depth in depths {
        let driver = Driver::default();
        let fold = Fold::<Tree, Sum>::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let selected = Arc::clone(&calls);
        let declared = fold.clone();
        let mut root = Node {
            value: 1,
            children: KeyedState::new(),
        };
        for _ in 1..depth {
            root = Node {
                value: 1,
                children: [(0, root)].into_iter().collect(),
            };
        }
        let (builder, _) = Store::feature_builder(
            Some(root),
            Noop::<Tree>(PhantomData),
            Runtime::new(driver.clone()),
        )
        .mount_with_children(descriptor(), move |node, children| {
            declare(node, children, &declared, &selected, 0, &Arc::default());
        });
        let store = builder.build().unwrap();
        driver.finish(store.ready()).unwrap();
        for read in 1..=3 {
            assert_eq!(driver.finish(store.fold(&fold)).unwrap().0.get(), depth);
            assert_eq!(calls.load(Ordering::SeqCst), depth * read);
        }
        drop(store);
        driver.turn();
    }
    let widths: &[usize] = if cfg!(miri) {
        &[0, 1, 3, 16]
    } else {
        &[0, 1, 3, 16, 256, 1_000]
    };
    for &width in widths {
        type Rows = KeyedState<usize, usize>;
        let driver = Driver::default();
        let fold = Fold::<Rows, Sum>::new();
        let declared = fold.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let selected = Arc::clone(&calls);
        let rows: Rows = (0..width).map(|id| (id, 1)).collect();
        let store = Store::feature_builder(
            rows,
            Noop::<Rows>(PhantomData),
            Runtime::new(driver.clone()),
        )
        .keyed_with_children(
            |rows| rows,
            |rows| rows,
            move |_, _| {
                let fold = declared.clone();
                let calls = Arc::clone(&selected);
                (
                    EntryMount::new(Noop::<usize>(PhantomData)),
                    move |_: &usize, children: &mut MountChildren<'_, Rows, (), usize, ()>| {
                        let calls = Arc::clone(&calls);
                        children.contribute(&fold, move |value| {
                            calls.fetch_add(1, Ordering::SeqCst);
                            Owned::new(*value)
                        });
                    },
                )
            },
        )
        .build()
        .unwrap();
        driver.finish(store.ready()).unwrap();
        for read in 1..=3 {
            assert_eq!(driver.finish(store.fold(&fold)).unwrap().0.get(), width);
            assert_eq!(calls.load(Ordering::SeqCst), width * read);
        }
        drop(store);
        driver.turn();
    }
}
/// Stops a group at raw five or the nested subtree's transformed total of seventy.
struct Stop;
impl FoldKey for Stop {
    type Value = Owned;
    fn identity() -> Owned {
        Owned::new(0)
    }
    fn reduce(accumulator: Owned, next: Owned) -> ControlFlow<Owned, Owned> {
        let next = next.0.get();
        accumulator.0.set(accumulator.0.get() + next);
        if next == 5 || next == 70 {
            ControlFlow::Break(accumulator)
        } else {
            ControlFlow::Continue(accumulator)
        }
    }
}
/// Creates a contributor with no descendants.
fn leaf(value: usize) -> Node {
    Node {
        value,
        children: KeyedState::new(),
    }
}
#[test]
fn nested_break_finishes_each_transform_once_and_skips_only_its_group() {
    let driver = Driver::default();
    let fold = Fold::<Tree, Stop>::new();
    let declared = fold.clone();
    let trace = Arc::new(Mutex::new(Vec::new()));
    let selected = Arc::clone(&trace);
    let tree = Node {
        value: 1,
        children: [
            (
                0,
                Node {
                    value: 2,
                    children: [(0, leaf(5)), (1, leaf(9))].into_iter().collect(),
                },
            ),
            (1, leaf(3)),
        ]
        .into_iter()
        .collect(),
    };
    let (builder, _) = Store::feature_builder(
        Some(tree),
        Noop::<Tree>(PhantomData),
        Runtime::new(driver.clone()),
    )
    .mount_with_children(descriptor(), move |node, children| {
        declare(node, children, &declared, &Arc::default(), 1, &selected);
    });
    let last = declared_sibling();
    let last_fold = fold.clone();
    let last_trace = Arc::clone(&trace);
    let (builder, _) = builder.mount_with_children(last, move |_, children| {
        let trace = Arc::clone(&last_trace);
        children.contribute(&last_fold, move |_| {
            trace.lock().unwrap().push(4);
            Owned::new(4)
        });
    });
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    assert_eq!(driver.finish(store.fold(&fold)).unwrap().0.get(), 175);
    assert_eq!(*trace.lock().unwrap(), [1, 2, 5, 4]);
    drop(store);
    driver.turn();
}
/// Selects the same root state through an independent sibling declaration.
fn declared_sibling() -> OptionalMount<Tree, Noop<Node>, u8> {
    OptionalMount::new(
        Option::as_ref,
        Option::as_mut,
        |tree: &Tree| {
            tree.as_ref().map(|_| MountIdentity {
                key: 99,
                replacement: 0,
                configuration: 0,
            })
        },
        |_| Noop::<Node>(PhantomData),
    )
}

/// Makes traversal order and transform grouping visible in the final numeric result.
struct Digits;
impl FoldKey for Digits {
    type Value = Owned;
    fn identity() -> Owned {
        Owned::new(0)
    }
    fn reduce(accumulator: Owned, next: Owned) -> ControlFlow<Owned, Owned> {
        accumulator.0.set(accumulator.0.get() * 10 + next.0.get());
        ControlFlow::Continue(accumulator)
    }
}
/// Reads the fixed 1–2–3 tree, then drains its actor before returning the result.
fn three_node_read<K: FoldKey<Value = Owned>>(transforms: u8) -> usize {
    let driver = Driver::default();
    let fold = Fold::<Tree, K>::new();
    let declared = fold.clone();
    let tree = Node {
        value: 1,
        children: [(0, leaf(2)), (1, leaf(3))].into_iter().collect(),
    };
    let (builder, _) = Store::feature_builder(
        Some(tree),
        Noop::<Tree>(PhantomData),
        Runtime::new(driver.clone()),
    )
    .mount_with_children(descriptor(), move |node, children| {
        declare(
            node,
            children,
            &declared,
            &Arc::default(),
            transforms,
            &Arc::default(),
        );
    });
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    let result = driver.finish(store.fold(&fold)).unwrap().0.get();
    drop(store);
    driver.turn();
    result
}
#[test]
fn non_associative_preorder_and_nested_transforms_have_exact_public_oracles() {
    assert_eq!(three_node_read::<Digits>(0), 123);
    assert_eq!(three_node_read::<Sum>(2), 240);
    assert_eq!(three_node_read::<Digits>(2), 3_030);
}

#[test]
fn root_break_skips_later_root_sibling() {
    let driver = Driver::default();
    let fold = Fold::<Tree, Stop>::new();
    let declared = fold.clone();
    let trace = Arc::new(Mutex::new(Vec::new()));
    let selected = Arc::clone(&trace);
    let (builder, _) = Store::feature_builder(
        Some(leaf(5)),
        Noop::<Tree>(PhantomData),
        Runtime::new(driver.clone()),
    )
    .mount_with_children(descriptor(), move |node, children| {
        declare(node, children, &declared, &Arc::default(), 0, &selected);
    });
    let last_fold = fold.clone();
    let last_trace = Arc::clone(&trace);
    let last_calls = Arc::new(AtomicUsize::new(0));
    let selected_calls = Arc::clone(&last_calls);
    let (builder, _) = builder.mount_with_children(declared_sibling(), move |_, children| {
        let trace = Arc::clone(&last_trace);
        let calls = Arc::clone(&selected_calls);
        children.contribute(&last_fold, move |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            trace.lock().unwrap().push(4);
            Owned::new(4)
        });
    });
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    assert_eq!(driver.finish(store.fold(&fold)).unwrap().0.get(), 5);
    assert_eq!(*trace.lock().unwrap(), [5]);
    assert_eq!(last_calls.load(Ordering::SeqCst), 0);
    drop(store);
    driver.turn();
}

#[test]
fn missing_fold_registration_skips_live_mount() {
    let driver = Driver::default();
    let registered = Fold::<Tree, Sum>::new();
    let unregistered = Fold::<Tree, Digits>::new();
    let declared = registered.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let selected_calls = Arc::clone(&calls);
    let trace = Arc::new(Mutex::new(Vec::new()));
    let selected_trace = Arc::clone(&trace);
    let (builder, _) = Store::feature_builder(
        Some(leaf(5)),
        Noop::<Tree>(PhantomData),
        Runtime::new(driver.clone()),
    )
    .mount_with_children(descriptor(), move |node, children| {
        declare(
            node,
            children,
            &declared,
            &selected_calls,
            0,
            &selected_trace,
        );
    });
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    assert_eq!(driver.finish(store.fold(&unregistered)).unwrap().0.get(), 0);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(trace.lock().unwrap().is_empty());
    assert_eq!(driver.finish(store.fold(&registered)).unwrap().0.get(), 5);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(*trace.lock().unwrap(), [5]);
    drop(store);
    driver.turn();
}
