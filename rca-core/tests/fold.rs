//! Public folds follow the committed roster and incarnation-owned callbacks.
use rca_core::{
    Feature, FeatureContext, Fold, FoldKey, MountIdentity, OptionalMount, Runtime, Store,
};
use std::cell::Cell;
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::Poll;
#[path = "fold/support.rs"]
mod support;
use support::{Driver, poll};

/// Owns a value without requiring `Clone`, `Debug`, `Eq` or `Sync`.
struct Owned(Box<Cell<usize>>);

/// Detects traversal order without requiring the key itself to be `Send` or `Sync`.
struct Digits(std::marker::PhantomData<std::rc::Rc<()>>);
impl FoldKey for Digits {
    type Value = Owned;
    fn identity() -> Owned {
        Owned(Box::new(Cell::new(0)))
    }
    fn reduce(accumulator: Owned, next: Owned) -> ControlFlow<Owned, Owned> {
        accumulator.0.set(accumulator.0.get() * 10 + next.0.get());
        ControlFlow::Continue(accumulator)
    }
}
/// Holds one optional parent whose leaf can change identity independently.
struct Root {
    parent: Option<Parent>,
}
/// Separates the leaf's value from its replacement generation.
struct Parent {
    leaf: Option<usize>,
    replacement: u64,
}
/// Replaces the leaf while leaving the parent installation intact.
enum Action {
    Replace,
}
/// Applies the one replacement used to check callback lifetime and read ordering.
struct RootFeature;
impl Feature for RootFeature {
    type State = Root;
    type Action = Action;
    fn update(&mut self, state: &mut Root, _: &Action, _: &mut FeatureContext<'_, Root, Action>) {
        let parent = state.parent.as_mut().unwrap();
        parent.leaf = Some(8);
        parent.replacement += 1;
    }
}
/// Keeps the parent mounted without adding reducer behaviour.
struct ParentFeature;
impl Feature for ParentFeature {
    type State = Parent;
    type Action = ();
    fn update(&mut self, _: &mut Parent, (): &(), _: &mut FeatureContext<'_, Parent, ()>) {}
}
/// Keeps the leaf mounted so its fold callback owns the observed capture.
struct LeafFeature;
impl Feature for LeafFeature {
    type State = usize;
    type Action = ();
    fn update(&mut self, _: &mut usize, (): &(), _: &mut FeatureContext<'_, usize, ()>) {}
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One owned fold sequence preserves contribution order and replacement-only callback recreation"
)]
fn ordered_owned_fold_recreates_only_replaced_leaf_callbacks() {
    let driver = Driver::default();
    let fold = Fold::<Root, Digits>::new();
    let callbacks = Arc::new(AtomicUsize::new(0));
    let parent_recipes = Arc::new(AtomicUsize::new(0));
    let leaf_recipes = Arc::new(AtomicUsize::new(0));
    let captures = Arc::new(Mutex::new(Vec::<Weak<()>>::new()));
    let parent_mount = OptionalMount::new(
        |root: &Root| root.parent.as_ref(),
        |root: &mut Root| root.parent.as_mut(),
        |root: &Root| {
            root.parent.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        |_| ParentFeature,
    );
    let declared_fold = fold.clone();
    let declared_callbacks = std::sync::Arc::clone(&callbacks);
    let declared_parents = std::sync::Arc::clone(&parent_recipes);
    let declared_leaves = std::sync::Arc::clone(&leaf_recipes);
    let declared_captures = std::sync::Arc::clone(&captures);
    let (builder, parent_site) = Store::builder(
        Root {
            parent: Some(Parent {
                leaf: Some(2),
                replacement: 0,
            }),
        },
        RootFeature,
        Runtime::new(driver.clone()),
    )
    .mount_with_children(parent_mount, move |_, children| {
        declared_parents.fetch_add(1, Ordering::SeqCst);
        let count = std::sync::Arc::clone(&declared_callbacks);
        let send_not_sync = Cell::new(0);
        children.contribute(&declared_fold, move |_| {
            send_not_sync.set(send_not_sync.get() + 1);
            count.fetch_add(1, Ordering::SeqCst);
            Owned(Box::new(Cell::new(1)))
        });
        let old_captures = std::sync::Arc::clone(&declared_captures);
        let leaf = OptionalMount::new(
            |parent: &Parent| parent.leaf.as_ref(),
            |parent: &mut Parent| parent.leaf.as_mut(),
            |parent: &Parent| {
                parent.leaf.map(|_| MountIdentity {
                    key: (),
                    replacement: parent.replacement,
                    configuration: 0,
                })
            },
            move |_| {
                assert!(
                    old_captures
                        .lock()
                        .unwrap()
                        .iter()
                        .all(|old| old.upgrade().is_none()),
                    "retired leaf callbacks die before successor construction"
                );
                LeafFeature
            },
        );
        let leaf_fold = declared_fold.clone();
        let count = std::sync::Arc::clone(&declared_callbacks);
        let recipes = std::sync::Arc::clone(&declared_leaves);
        let captures = std::sync::Arc::clone(&declared_captures);
        children.optional_with_children(leaf, move |_, children| {
            recipes.fetch_add(1, Ordering::SeqCst);
            let capture = Arc::new(());
            captures.lock().unwrap().push(Arc::downgrade(&capture));
            let count = std::sync::Arc::clone(&count);
            children.contribute(&leaf_fold, move |value| {
                let _ = &capture;
                count.fetch_add(1, Ordering::SeqCst);
                Owned(Box::new(Cell::new(*value)))
            });
        });
    });
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    drop(store.fold(&fold));
    assert_eq!(
        callbacks.load(Ordering::SeqCst),
        0,
        "unpolled read is inert"
    );
    assert_eq!(driver.finish(store.fold(&fold)).unwrap().0.get(), 12);
    let parent = driver.finish(store.acquire(&parent_site)).unwrap().unwrap();
    let before = driver.finish(parent.update(|_| ())).unwrap().revision();

    let mut replace = Box::pin(store.send(Action::Replace));
    assert!(poll(replace.as_mut()).is_pending());
    let mut read = Box::pin(store.fold(&fold));
    assert!(poll(read.as_mut()).is_pending());
    driver.turn();
    assert!(matches!(poll(replace.as_mut()), Poll::Ready(Ok(_))));
    drop(replace);
    let Poll::Ready(Ok(value)) = poll(read.as_mut()) else {
        panic!("ordered fold reply")
    };
    assert_eq!(value.0.get(), 18, "fold follows the preceding mutation");
    assert_eq!(parent_recipes.load(Ordering::SeqCst), 1);
    assert_eq!(leaf_recipes.load(Ordering::SeqCst), 2);
    assert_eq!(callbacks.load(Ordering::SeqCst), 4);
    let after = driver.finish(parent.update(|_| ())).unwrap().revision();
    assert_eq!(
        after,
        before + 2,
        "only replacement and explicit update commit"
    );
    drop(store);
    driver.turn();
    assert!(
        captures
            .lock()
            .unwrap()
            .iter()
            .all(|capture| capture.upgrade().is_none())
    );
}

#[test]
fn empty_fold_requires_readiness_and_does_not_retain_store() {
    let driver = Driver::default();
    let fold = Fold::<Root, Digits>::default();
    let absent = OptionalMount::new(
        |root: &Root| root.parent.as_ref(),
        |root: &mut Root| root.parent.as_mut(),
        |root: &Root| {
            root.parent.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        |_| ParentFeature,
    );
    let (builder, _) = Store::builder(
        Root { parent: None },
        RootFeature,
        Runtime::new(driver.clone()),
    )
    .mount_with_site(absent);
    let store = builder.build().unwrap();
    let mut early = Box::pin(store.fold(&fold));
    let Poll::Ready(Err(error)) = poll(early.as_mut()) else {
        panic!("not ready")
    };
    assert_eq!(error.reason(), rca_core::TaskAccessReason::NotReady);
    assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
    driver.finish(store.ready()).unwrap();
    assert_eq!(driver.finish(store.fold(&fold)).unwrap().0.get(), 0);
    let late = store.fold(&fold);
    drop(store);
    let mut late = Box::pin(late);
    let Poll::Ready(Err(error)) = poll(late.as_mut()) else {
        panic!("closed root")
    };
    assert_eq!(error.reason(), rca_core::TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
    driver.turn();
}

/// Adds owned contributions to isolate subtree transformation from traversal order.
struct Sum;
impl FoldKey for Sum {
    type Value = Owned;
    fn identity() -> Owned {
        Owned(Box::new(Cell::new(0)))
    }
    fn reduce(accumulator: Owned, next: Owned) -> ControlFlow<Owned, Owned> {
        accumulator.0.set(accumulator.0.get() + next.0.get());
        ControlFlow::Continue(accumulator)
    }
}

#[test]
fn transform_applies_only_to_its_subtree() {
    let driver = Driver::default();
    let fold = Fold::<Root, Sum>::new();
    let declared = fold.clone();
    let mount = OptionalMount::new(
        |root: &Root| root.parent.as_ref(),
        |root: &mut Root| root.parent.as_mut(),
        |root: &Root| {
            root.parent.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        |_| ParentFeature,
    );
    let (builder, _) = Store::builder(
        Root {
            parent: Some(Parent {
                leaf: Some(2),
                replacement: 0,
            }),
        },
        RootFeature,
        Runtime::new(driver.clone()),
    )
    .mount_with_children(mount, move |_, children| {
        children.contribute(&declared, |_| Owned(Box::new(Cell::new(1))));
        let leaf = OptionalMount::new(
            |parent: &Parent| parent.leaf.as_ref(),
            |parent: &mut Parent| parent.leaf.as_mut(),
            |parent: &Parent| {
                parent.leaf.map(|_| MountIdentity {
                    key: (),
                    replacement: parent.replacement,
                    configuration: 0,
                })
            },
            |_| LeafFeature,
        );
        let declared = declared.clone();
        children.optional_with_children(leaf, move |_, children| {
            children.contribute_transformed(
                &declared,
                |value| Owned(Box::new(Cell::new(*value))),
                |value| {
                    value.0.set(value.0.get() * 10);
                    value
                },
            );
        });
    });
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    assert_eq!(driver.finish(store.fold(&fold)).unwrap().0.get(), 21);
    driver.finish(store.shutdown()).unwrap();
}

/// Stops at the first real contribution, exposing any synthetic identity contribution.
struct First;
impl FoldKey for First {
    type Value = Owned;
    fn identity() -> Owned {
        Owned(Box::new(Cell::new(0)))
    }
    fn reduce(_: Owned, next: Owned) -> ControlFlow<Owned, Owned> {
        ControlFlow::Break(next)
    }
}

#[test]
fn transform_only_does_not_synthesise_a_breaking_identity_contribution() {
    let driver = Driver::default();
    let fold = Fold::<Root, First>::new();
    let declared = fold.clone();
    let mount = OptionalMount::new(
        |root: &Root| root.parent.as_ref(),
        |root: &mut Root| root.parent.as_mut(),
        |root: &Root| {
            root.parent.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        |_| ParentFeature,
    );
    let (builder, _) = Store::builder(
        Root {
            parent: Some(Parent {
                leaf: Some(2),
                replacement: 0,
            }),
        },
        RootFeature,
        Runtime::new(driver.clone()),
    )
    .mount_with_children(mount, move |_, children| {
        children.transform_fold(&declared, |value| {
            value.0.set(value.0.get() * 10);
            value
        });
        let leaf = OptionalMount::new(
            |parent: &Parent| parent.leaf.as_ref(),
            |parent: &mut Parent| parent.leaf.as_mut(),
            |parent: &Parent| {
                parent.leaf.map(|_| MountIdentity {
                    key: (),
                    replacement: parent.replacement,
                    configuration: 0,
                })
            },
            |_| LeafFeature,
        );
        let declared = declared.clone();
        children.optional_with_children(leaf, move |_, children| {
            children.contribute(&declared, |value| Owned(Box::new(Cell::new(*value))));
        });
    });
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    assert_eq!(driver.finish(store.fold(&fold)).unwrap().0.get(), 20);
    driver.finish(store.shutdown()).unwrap();
}
