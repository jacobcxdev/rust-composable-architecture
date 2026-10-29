//! Public owning paths exercise the remaining removal boundaries without registry mutation.

use super::{Driver, DropHook, DropWatch, poll};
use rca_core::{
    EntryMount, Feature, FeatureContext, KeyedState, MountChildren, MountIdentity, MountSite,
    OptionalMount, Output, OutputFlow, RemovalSite, Runtime, Store, TaskAccessReason, TaskContext,
    TaskProgress,
};
use std::cell::Cell;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Runs one callback at a selected hash, equality or owned-key drop boundary.
#[derive(Default)]
struct KeyHooks {
    armed: AtomicUsize,
    skip: AtomicUsize,
    callback: DropHook,
    calls: AtomicUsize,
}
impl KeyHooks {
    /// Skips requested visits, disarms the hook, then calls it outside the mutex.
    fn call(&self, boundary: usize) {
        if self.armed.load(Ordering::SeqCst) == boundary
            && self
                .skip
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok()
        {
            return;
        }
        if self
            .armed
            .compare_exchange(boundary, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let hook = self.callback.lock().unwrap().take();
            if let Some(hook) = hook {
                hook();
            }
        }
    }
}
/// Stable key identity with user callbacks at actual map operations.
///
/// Callbacks do not change equality or hashing. Only the owned map key has
/// `stored`; metadata clones cannot trigger its destructor hook.
struct Key {
    id: u8,
    hooks: Arc<KeyHooks>,
    stored: bool,
}
impl Clone for Key {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            hooks: Arc::clone(&self.hooks),
            stored: false,
        }
    }
}
impl PartialEq for Key {
    fn eq(&self, other: &Self) -> bool {
        self.hooks.call(2);
        self.id == other.id
    }
}
impl Eq for Key {}
impl Hash for Key {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.hooks.call(1);
        self.id.hash(state);
    }
}
impl Drop for Key {
    fn drop(&mut self) {
        if self.stored {
            self.hooks.call(3);
        }
    }
}
/// Move-only child allocation with an independent disposal counter.
struct Value {
    value: Box<Cell<usize>>,
    drops: Arc<AtomicUsize>,
}
impl Drop for Value {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
/// Owns the keyed rows and controls faults at their mutable projection.
struct Parent {
    rows: KeyedState<Key, Value>,
    select: Arc<AtomicUsize>,
    hooks: Arc<KeyHooks>,
    disposed_rows: Option<Arc<Mutex<RowsSnapshot>>>,
}
/// Ordered key, allocation address and visible value captured before disposal.
type RowsSnapshot = Vec<(u8, usize, usize)>;
impl Parent {
    /// Copies observations only; it does not clone or retain any child value.
    fn snapshot(&self) -> RowsSnapshot {
        self.rows
            .iter()
            .map(|(key, value)| (key.id, (&raw const *value.value).addr(), value.value.get()))
            .collect()
    }
}
impl Drop for Parent {
    fn drop(&mut self) {
        if let Some(disposed_rows) = &self.disposed_rows {
            *disposed_rows.lock().unwrap() = self.snapshot();
        }
    }
}
/// Owns an optional parent and controls faults at the ancestor projection.
struct Root {
    parent: Option<Parent>,
    select: Arc<AtomicUsize>,
}
/// Leaves ancestor changes to the removal operation under test.
struct RootFeature;
/// Leaves row changes to the removal operation under test.
struct ParentFeature;
/// Records the lifetime of each mounted child feature.
struct Leaf(DropWatch);
impl Feature for RootFeature {
    type State = Root;
    type Action = ();
    fn reduce(&mut self, _: &mut Root, (): &(), _: &mut FeatureContext<'_, Root, ()>) {}
}
impl Feature for ParentFeature {
    type State = Parent;
    type Action = ();
    fn reduce(&mut self, _: &mut Parent, (): &(), _: &mut FeatureContext<'_, Parent, ()>) {}
}
impl Feature for Leaf {
    type State = Value;
    type Action = ();
    fn reduce(&mut self, _: &mut Value, (): &(), _: &mut FeatureContext<'_, Value, ()>) {
        let _ = &self.0;
    }
}
/// Removes the parent or panics once when the selected failure mode is armed.
fn select_parent(root: &mut Root) -> Option<&mut Parent> {
    match root.select.swap(0, Ordering::SeqCst) {
        1 => {
            root.parent = None;
        }
        2 => panic!("parent selector"),
        _ => {}
    }
    root.parent.as_mut()
}
/// Replaces a row, panics or arms a key callback at actual mutable selection.
fn select_rows(parent: &mut Parent) -> &mut KeyedState<Key, Value> {
    match parent.select.swap(0, Ordering::SeqCst) {
        1 => {
            let key = parent.rows.ids().next().unwrap().clone();
            let old = parent.rows.remove(&key).unwrap();
            parent.rows.insert(
                key,
                Value {
                    value: Box::new(Cell::new(100)),
                    drops: Arc::clone(&old.drops),
                },
            );
        }
        2 => panic!("owning selector"),
        mode @ 11..=13 => parent.hooks.armed.store(mode - 10, Ordering::SeqCst),
        _ => {}
    }
    &mut parent.rows
}
/// Exported key, child mount and removal site for each installed row.
type Sites = Vec<(
    u8,
    MountSite<Root, Value, (), ()>,
    RemovalSite<Parent, Value, ()>,
)>;
/// Public removal handles with independent lifetime, callback and ownership witnesses.
struct Fixture {
    driver: Driver,
    store: Store<Root, ()>,
    parent_site: MountSite<Root, Parent, (), ()>,
    parent: TaskContext<Parent, ()>,
    first: TaskContext<Value, ()>,
    second: TaskContext<Value, ()>,
    sites: Sites,
    output: Output<Value>,
    sibling_owned_output: Output<Value>,
    values: Arc<AtomicUsize>,
    features: Arc<AtomicUsize>,
    handlers: Arc<AtomicUsize>,
    handler_drops: Arc<AtomicUsize>,
    select_parent: Arc<AtomicUsize>,
    select_rows: Arc<AtomicUsize>,
    hooks: Arc<KeyHooks>,
    identity: usize,
}
/// Installs three rows and output declarations owned by the parent and a sibling.
#[expect(
    clippy::too_many_lines,
    reason = "One removal fixture constructor preserves nested mount, output and capture registration order"
)]
fn fixture() -> Fixture {
    let driver = Driver::default();
    let values = Arc::new(AtomicUsize::new(0));
    let features = Arc::new(AtomicUsize::new(0));
    let hooks = Arc::new(KeyHooks::default());
    let select_parent_flag = Arc::new(AtomicUsize::new(0));
    let select_rows_flag = Arc::new(AtomicUsize::new(0));
    let first = Box::new(Cell::new(4));
    let identity = (&raw const *first) as usize;
    let rows = [
        (1, first),
        (2, Box::new(Cell::new(20))),
        (3, Box::new(Cell::new(30))),
    ]
    .into_iter()
    .map(|(id, value)| {
        (
            Key {
                id,
                hooks: Arc::clone(&hooks),
                stored: true,
            },
            Value {
                value,
                drops: Arc::clone(&values),
            },
        )
    })
    .collect();
    let exported = Arc::new(Mutex::new(Vec::new()));
    let export = Arc::clone(&exported);
    let owner_output = Arc::new(Mutex::new(None));
    let export_output = Arc::clone(&owner_output);
    let feature_drops = Arc::clone(&features);
    let (builder, parent_site) = Store::feature_builder(
        Root {
            parent: Some(Parent {
                rows,
                select: Arc::clone(&select_rows_flag),
                hooks: Arc::clone(&hooks),
                disposed_rows: None,
            }),
            select: Arc::clone(&select_parent_flag),
        },
        RootFeature,
        Runtime::new(driver.clone()),
    )
    .mount_with_children(
        OptionalMount::new(
            |root: &Root| root.parent.as_ref(),
            select_parent,
            |root: &Root| {
                root.parent.as_ref().map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            |_| ParentFeature,
        ),
        move |_, children| {
            let parent_site = children.site().clone();
            let export = Arc::clone(&export);
            let export_output = Arc::clone(&export_output);
            let feature_drops = Arc::clone(&feature_drops);
            children.keyed_removable(
                |parent| &parent.rows,
                select_rows,
                move |key, _| {
                    let id = key.id;
                    let export = Arc::clone(&export);
                    let export_output = Arc::clone(&export_output);
                    let parent_site = parent_site.clone();
                    (
                        EntryMount::new(Leaf(DropWatch(Arc::clone(&feature_drops)))),
                        move |_: &Value,
                              children: &mut MountChildren<'_, Root, (), Value, ()>,
                              removal| {
                            export
                                .lock()
                                .unwrap()
                                .push((id, children.site().clone(), removal));
                            if id == 2 {
                                *export_output.lock().unwrap() =
                                    Some(children.output(&parent_site, |_| {}));
                            }
                        },
                    )
                },
            );
        },
    );
    let handlers = Arc::new(AtomicUsize::new(0));
    let handler_drops = Arc::new(AtomicUsize::new(0));
    let calls = Arc::clone(&handlers);
    let capture = DropWatch(Arc::clone(&handler_drops));
    let (builder, output) = builder.output(&parent_site, |handlers| {
        handlers.at_root(move |_, value, _| {
            let _ = &capture;
            calls.fetch_add(1, Ordering::SeqCst);
            drop(value);
            OutputFlow::Consumed
        });
    });
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    let sites = exported.lock().unwrap().drain(..).collect::<Vec<_>>();
    let parent = driver.finish(store.acquire(&parent_site)).unwrap().unwrap();
    let first = driver.finish(store.acquire(&sites[0].1)).unwrap().unwrap();
    let second = driver.finish(store.acquire(&sites[1].1)).unwrap().unwrap();
    let sibling_owned_output = owner_output.lock().unwrap().take().unwrap();
    Fixture {
        driver,
        store,
        parent_site,
        parent,
        first,
        second,
        sites,
        output,
        sibling_owned_output,
        values,
        features,
        handlers,
        handler_drops,
        select_parent: select_parent_flag,
        select_rows: select_rows_flag,
        hooks,
        identity,
    }
}

// Retiring the child must not cancel cleanup that the surviving parent owns.
#[test]
fn keyed_removal_receipt_awaits_parent_owned_cleanup() {
    let f = fixture();
    let (release, wait) = futures_channel::oneshot::channel::<()>();
    let replacement_drops = Arc::new(AtomicUsize::new(0));
    let replacement = Value {
        value: Box::new(Cell::new(100)),
        drops: Arc::clone(&replacement_drops),
    };
    let key = Key {
        id: 1,
        hooks: Arc::clone(&f.hooks),
        stored: true,
    };
    let identity = f.identity;
    let task = f
        .driver
        .finish(f.parent.remove_child(
            &f.sites[0].2,
            &f.first,
            |value| value.value.set(7),
            move |parent, removed, context| {
                parent.rows.insert(key, replacement);
                let original = removed.into_inner();
                context.task(rca_core::Task::run(move |_| async move {
                    wait.await.unwrap();
                    assert_eq!((&raw const *original.value) as usize, identity);
                    assert_eq!(original.value.get(), 7);
                    drop(original);
                }));
            },
        ))
        .unwrap();
    assert_eq!(
        f.driver.finish(f.first.read(|_| ())).unwrap_err().reason(),
        TaskAccessReason::ScopeEnded
    );
    let read_values = |parent: &Parent| {
        parent
            .rows
            .iter()
            .map(|(key, value)| (key.id, value.value.get()))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        f.driver.finish(f.parent.read(read_values)).unwrap(),
        [(2, 20), (3, 30), (1, 100)]
    );
    let mut completion = Box::pin(task.finish());
    f.driver.turn();
    assert!(poll(completion.as_mut()).is_pending());
    assert!(!task.is_finished());
    assert_eq!(f.values.load(Ordering::SeqCst), 0);
    assert_eq!(replacement_drops.load(Ordering::SeqCst), 0);
    release.send(()).unwrap();
    f.driver.finish(completion);
    assert!(task.is_finished());
    assert_eq!(f.values.load(Ordering::SeqCst), 1);
    assert_eq!(replacement_drops.load(Ordering::SeqCst), 0);
    assert_eq!(
        f.driver.finish(f.parent.read(read_values)).unwrap(),
        [(2, 20), (3, 30), (1, 100)]
    );
    f.driver.finish(f.store.shutdown()).unwrap();
    assert_eq!(f.values.load(Ordering::SeqCst), 3);
    assert_eq!(replacement_drops.load(Ordering::SeqCst), 1);
}

#[test]
fn keyed_reorder_preserves_target_identity_and_remaining_order() {
    let f = fixture();
    let _ = f
        .driver
        .finish(f.parent.update(|parent| parent.rows.move_index(0, 1)))
        .unwrap();
    let identity = f.identity;
    let task = f
        .driver
        .finish(f.parent.remove_child(
            &f.sites[0].2,
            &f.first,
            |child| child.value.set(7),
            move |parent, removed, _| {
                assert_eq!(
                    parent.rows.ids().map(|key| key.id).collect::<Vec<_>>(),
                    [2, 3]
                );
                let value = removed.into_inner();
                assert_eq!((&raw const *value.value) as usize, identity);
                assert_eq!(value.value.get(), 7);
            },
        ))
        .unwrap();
    f.driver.finish(task.finish());
    assert_eq!(f.values.load(Ordering::SeqCst), 1);
    assert_eq!(
        f.driver
            .finish(f.second.read(|value| value.value.get()))
            .unwrap(),
        20
    );
    f.driver.finish(f.store.shutdown()).unwrap();
}

#[test]
fn actual_entry_stamp_mismatch_and_absent_parent_are_entered_refusals() {
    for absent_parent in [false, true] {
        let f = fixture();
        if absent_parent {
            f.select_parent.store(1, Ordering::SeqCst);
        } else {
            f.select_rows.store(1, Ordering::SeqCst);
        }
        let error = f
            .driver
            .finish(f.parent.remove_child(
                &f.sites[0].2,
                &f.first,
                |_| panic!("actual path rejected before before"),
                |_, _, _| panic!("no removed value"),
            ))
            .unwrap_err();
        assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
        assert_eq!(
            error.progress(),
            TaskProgress::MayHaveRun,
            "metadata accepted before public selector changed State"
        );
        assert_eq!(f.select_parent.load(Ordering::SeqCst), 0);
        assert_eq!(f.select_rows.load(Ordering::SeqCst), 0);
        assert_eq!(
            f.driver
                .finish(f.store.read(|root| root.parent.as_ref().map(|parent| {
                    parent
                        .rows
                        .iter()
                        .map(|(key, value)| (key.id, value.value.get()))
                        .collect::<Vec<_>>()
                })))
                .unwrap(),
            (!absent_parent).then_some(vec![(2, 20), (3, 30), (1, 100)])
        );
        f.driver.finish(f.store.shutdown()).unwrap();
    }
}

#[test]
fn sibling_identity_rejects_before_any_owning_selector() {
    let f = fixture();
    f.select_rows.store(2, Ordering::SeqCst);
    let error = f
        .driver
        .finish(f.parent.remove_child(
            &f.sites[0].2,
            &f.second,
            |_| panic!("sibling before"),
            |_, _, _| panic!("sibling after"),
        ))
        .unwrap_err();
    assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
    assert_eq!(error.progress(), TaskProgress::NotStarted);
    assert_eq!(f.select_rows.swap(0, Ordering::SeqCst), 2);
    assert_eq!(
        f.driver
            .finish(f.first.read(|value| value.value.get()))
            .unwrap(),
        4
    );
    f.driver.finish(f.store.shutdown()).unwrap();
}

/// Rejects incompatible sites while the supplied parent and child contexts remain live.
mod endpoint_validation {
    use super::{
        Arc, AtomicUsize, Driver, DropWatch, EntryMount, Feature, FeatureContext, KeyedState, Leaf,
        MountChildren, MountIdentity, MountSite, Mutex, OptionalMount, Ordering, RemovalSite,
        Runtime, Store, TaskAccessReason, TaskProgress, Value,
    };
    use std::cell::Cell;

    /// Holds optional and keyed children beneath the same parent identity.
    struct Parent {
        id: u8,
        optional: Option<Value>,
        keyed: KeyedState<u8, Value>,
        selections: Arc<AtomicUsize>,
    }
    /// Separates live, fixed and keyed parent lifetimes for site mismatch checks.
    struct Root {
        live: Option<Parent>,
        fixed: Option<Parent>,
        keyed: KeyedState<u8, Parent>,
    }
    /// Retires the fixed and keyed parents without touching the live parent.
    struct RootFeature;
    /// Leaves children unchanged so validation alone determines the result.
    struct ParentFeature;
    impl Feature for RootFeature {
        type State = Root;
        type Action = ();
        fn reduce(&mut self, state: &mut Root, (): &(), _: &mut FeatureContext<'_, Root, ()>) {
            state.fixed = None;
            state.keyed = KeyedState::new();
        }
    }
    impl Feature for ParentFeature {
        type State = Parent;
        type Action = ();
        fn reduce(&mut self, _: &mut Parent, (): &(), _: &mut FeatureContext<'_, Parent, ()>) {}
    }
    /// Associates each exported mount and removal site with its actual owner.
    struct Sites {
        parent_id: u8,
        keyed: bool,
        parent: MountSite<Root, Parent, (), ()>,
        child: MountSite<Root, Value, (), ()>,
        removal: RemovalSite<Parent, Value, ()>,
    }
    /// Keeps live contexts separate from the stale or incompatible tested sites.
    struct Fixture {
        driver: Driver,
        store: Store<Root, ()>,
        sites: Vec<Sites>,
        selections: Arc<AtomicUsize>,
        values: Arc<AtomicUsize>,
        features: Arc<AtomicUsize>,
    }

    #[expect(
        clippy::too_many_lines,
        reason = "One actor provides fixed and keyed declaration lifetimes with identical public child operations"
    )]
    /// Builds six children across three independently removable parent paths.
    fn fixture() -> Fixture {
        let driver = Driver::default();
        let selections = Arc::new(AtomicUsize::new(0));
        let values = Arc::new(AtomicUsize::new(0));
        let features = Arc::new(AtomicUsize::new(0));
        let parent = |id| Parent {
            id,
            optional: Some(Value {
                value: Box::new(Cell::new(usize::from(id) * 10)),
                drops: Arc::clone(&values),
            }),
            keyed: [(
                1,
                Value {
                    value: Box::new(Cell::new(usize::from(id) * 10 + 1)),
                    drops: Arc::clone(&values),
                },
            )]
            .into_iter()
            .collect(),
            selections: Arc::clone(&selections),
        };
        let exports = Arc::new(Mutex::new(Vec::new()));
        let export = Arc::clone(&exports);
        let feature_drops = Arc::clone(&features);
        let recipe =
            move |state: &Parent, children: &mut MountChildren<'_, Root, (), Parent, ()>| {
                let parent_id = state.id;
                let parent_site = children.site().clone();
                let drops = Arc::clone(&feature_drops);
                let (child, removal) = children.optional_removable(
                    |parent| &parent.optional,
                    |parent| {
                        parent.selections.fetch_add(1, Ordering::SeqCst);
                        &mut parent.optional
                    },
                    |parent| {
                        parent.optional.as_ref().map(|_| MountIdentity {
                            key: (),
                            replacement: 0,
                            configuration: 0,
                        })
                    },
                    move |_| Leaf(DropWatch(Arc::clone(&drops))),
                );
                export.lock().unwrap().push(Sites {
                    parent_id,
                    keyed: false,
                    parent: parent_site.clone(),
                    child,
                    removal,
                });
                let export = Arc::clone(&export);
                let drops = Arc::clone(&feature_drops);
                children.keyed_removable(
                    |parent| &parent.keyed,
                    |parent| {
                        parent.selections.fetch_add(1, Ordering::SeqCst);
                        &mut parent.keyed
                    },
                    move |_, _| {
                        let export = Arc::clone(&export);
                        let parent_site = parent_site.clone();
                        (
                            EntryMount::new(Leaf(DropWatch(Arc::clone(&drops)))),
                            move |_: &Value,
                                  children: &mut MountChildren<'_, Root, (), Value, ()>,
                                  removal| {
                                export.lock().unwrap().push(Sites {
                                    parent_id,
                                    keyed: true,
                                    parent: parent_site.clone(),
                                    child: children.site().clone(),
                                    removal,
                                });
                            },
                        )
                    },
                );
            };
        let (builder, _) = Store::feature_builder(
            Root {
                live: Some(parent(1)),
                fixed: Some(parent(2)),
                keyed: [(3, parent(3))].into_iter().collect(),
            },
            RootFeature,
            Runtime::new(driver.clone()),
        )
        .mount_with_children(
            OptionalMount::new(
                |root: &Root| root.live.as_ref(),
                |root: &mut Root| root.live.as_mut(),
                |root: &Root| {
                    root.live.as_ref().map(|_| MountIdentity {
                        key: (),
                        replacement: 0,
                        configuration: 0,
                    })
                },
                |_| ParentFeature,
            ),
            recipe.clone(),
        );
        let (builder, _) = builder.mount_with_children(
            OptionalMount::new(
                |root: &Root| root.fixed.as_ref(),
                |root: &mut Root| root.fixed.as_mut(),
                |root: &Root| {
                    root.fixed.as_ref().map(|_| MountIdentity {
                        key: (),
                        replacement: 0,
                        configuration: 0,
                    })
                },
                |_| ParentFeature,
            ),
            recipe.clone(),
        );
        let store = builder
            .keyed_with_children(
                |root| &root.keyed,
                |root| &mut root.keyed,
                move |_, _| (EntryMount::new(ParentFeature), recipe.clone()),
            )
            .build()
            .unwrap();
        driver.finish(store.ready()).unwrap();
        let sites = std::mem::take(&mut *exports.lock().unwrap());
        Fixture {
            driver,
            store,
            sites,
            selections,
            values,
            features,
        }
    }

    #[derive(Clone, Copy, Debug)]
    /// Independent ways a live caller can supply an invalid removal relationship.
    enum Mismatch {
        DirectParent,
        LiveEndpointParent,
        AbsentFixedParent,
        RetiredKeyedParent,
        AbsentFixedChild,
        RetiredKeyedChild,
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "Six exact endpoint validation boundaries share one public same-actor schedule and conservation assertions"
    )]
    fn stale_or_mismatched_sites_reject_live_contexts_before_owning_selection() {
        for mismatch in [
            Mismatch::DirectParent,
            Mismatch::LiveEndpointParent,
            Mismatch::AbsentFixedParent,
            Mismatch::RetiredKeyedParent,
            Mismatch::AbsentFixedChild,
            Mismatch::RetiredKeyedChild,
        ] {
            let f = fixture();
            let live = f
                .sites
                .iter()
                .find(|site| site.parent_id == 1 && !site.keyed)
                .unwrap();
            let keyed = f
                .sites
                .iter()
                .find(|site| site.parent_id == 1 && site.keyed)
                .unwrap();
            let fixed = f
                .sites
                .iter()
                .find(|site| site.parent_id == 2 && !site.keyed)
                .unwrap();
            let dynamic = f
                .sites
                .iter()
                .find(|site| site.parent_id == 3 && !site.keyed)
                .unwrap();
            let parent = f
                .driver
                .finish(f.store.acquire(&live.parent))
                .unwrap()
                .unwrap();
            let sibling_parent = f
                .driver
                .finish(f.store.acquire(&fixed.parent))
                .unwrap()
                .unwrap();
            let optional_child = f
                .driver
                .finish(f.store.acquire(&live.child))
                .unwrap()
                .unwrap();
            let keyed_child = f
                .driver
                .finish(f.store.acquire(&keyed.child))
                .unwrap()
                .unwrap();
            match mismatch {
                Mismatch::AbsentFixedParent | Mismatch::RetiredKeyedParent => {
                    let task = f.driver.finish(f.store.send(())).unwrap();
                    f.driver.finish(task.finish());
                    assert!(
                        f.driver
                            .finish(f.store.acquire(&fixed.parent))
                            .unwrap()
                            .is_none()
                    );
                    let error = f
                        .driver
                        .finish(f.store.acquire(&dynamic.parent))
                        .unwrap_err();
                    assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
                }
                Mismatch::AbsentFixedChild => {
                    drop(
                        f.driver
                            .finish(parent.update(|parent| parent.optional.take()))
                            .unwrap(),
                    );
                    assert!(
                        f.driver
                            .finish(f.store.acquire(&live.child))
                            .unwrap()
                            .is_none()
                    );
                }
                Mismatch::RetiredKeyedChild => {
                    drop(
                        f.driver
                            .finish(parent.update(|parent| parent.keyed.remove(&1)))
                            .unwrap(),
                    );
                    let error = f.driver.finish(f.store.acquire(&keyed.child)).unwrap_err();
                    assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
                }
                Mismatch::DirectParent | Mismatch::LiveEndpointParent => {}
            }
            let supplied_parent = if matches!(mismatch, Mismatch::DirectParent) {
                &sibling_parent
            } else {
                &parent
            };
            let supplied_child = if matches!(mismatch, Mismatch::AbsentFixedChild) {
                &keyed_child
            } else {
                &optional_child
            };
            // Both supplied authorities are still usable; rejection must inspect
            // their relationship/site rather than an already retired context.
            f.driver.finish(supplied_parent.read(|_| ())).unwrap();
            let identity = f
                .driver
                .finish(
                    supplied_child
                        .read(|value| ((&raw const *value.value).addr(), value.value.get())),
                )
                .unwrap();
            let site = match mismatch {
                Mismatch::DirectParent | Mismatch::AbsentFixedChild => &live.removal,
                Mismatch::LiveEndpointParent | Mismatch::AbsentFixedParent => &fixed.removal,
                Mismatch::RetiredKeyedParent => &dynamic.removal,
                Mismatch::RetiredKeyedChild => &keyed.removal,
            };
            let captures = Arc::new(AtomicUsize::new(0));
            let before = DropWatch(Arc::clone(&captures));
            let after = DropWatch(Arc::clone(&captures));
            let selections = f.selections.load(Ordering::SeqCst);
            let values = f.values.load(Ordering::SeqCst);
            let features = f.features.load(Ordering::SeqCst);
            let error = f
                .driver
                .finish(supplied_parent.remove_child(
                    site,
                    supplied_child,
                    move |_| {
                        let _ = &before;
                        panic!("invalid site must not enter before");
                    },
                    move |_, _, _| {
                        let _ = &after;
                        panic!("invalid site must not enter after");
                    },
                ))
                .unwrap_err();
            assert_eq!(error.reason(), TaskAccessReason::ScopeEnded, "{mismatch:?}");
            assert_eq!(error.progress(), TaskProgress::NotStarted, "{mismatch:?}");
            assert_eq!(captures.load(Ordering::SeqCst), 2);
            assert_eq!(f.selections.load(Ordering::SeqCst), selections);
            assert_eq!(f.values.load(Ordering::SeqCst), values);
            assert_eq!(f.features.load(Ordering::SeqCst), features);
            assert_eq!(
                f.driver
                    .finish(
                        supplied_child
                            .read(|value| ((&raw const *value.value).addr(), value.value.get()))
                    )
                    .unwrap(),
                identity
            );
            f.driver.finish(f.store.shutdown()).unwrap();
            drop(f.store);
            f.driver.turn();
            assert_eq!(f.values.load(Ordering::SeqCst), 6);
            assert_eq!(f.features.load(Ordering::SeqCst), 6);
            assert!(f.driver.0.lock().unwrap().is_empty());
        }
    }
}

#[test]
fn real_key_hash_equality_and_owned_key_drop_obey_close_and_panic_boundaries() {
    for boundary in 1..=3 {
        for panics in [false, true] {
            let f = fixture();
            let before = Arc::new(AtomicUsize::new(0));
            let called = Arc::clone(&before);
            let closes = Arc::new(AtomicUsize::new(0));
            let close_count = Arc::clone(&closes);
            let retained_store = if panics {
                Some(f.store)
            } else {
                *f.hooks.callback.lock().unwrap() = Some(Box::new(move || {
                    close_count.fetch_add(1, Ordering::SeqCst);
                    drop(f.store);
                }));
                None
            };
            if panics {
                *f.hooks.callback.lock().unwrap() =
                    Some(Box::new(|| panic!("real keyed user callback")));
            }
            f.select_rows.store(boundary + 10, Ordering::SeqCst);
            let mut operation = Box::pin(f.parent.remove_child(
                &f.sites[0].2,
                &f.first,
                move |_| {
                    called.fetch_add(1, Ordering::SeqCst);
                },
                |_, _, _| panic!("closed or panicked key never produces removal"),
            ));
            assert!(poll(operation.as_mut()).is_pending());
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f.driver.turn()));
            if panics {
                let panic = result.expect_err("the selected key callback must panic");
                assert_eq!(
                    panic.downcast_ref::<&str>(),
                    Some(&"real keyed user callback")
                );
            } else {
                result.expect("closing the store from a key callback must not panic");
            }
            let error = f.driver.finish(operation).unwrap_err();
            assert_eq!(
                error.reason(),
                if panics {
                    TaskAccessReason::ActorFailed
                } else {
                    TaskAccessReason::StoreClosed
                }
            );
            assert_eq!(error.progress(), TaskProgress::MayHaveRun);
            assert_eq!(before.load(Ordering::SeqCst), usize::from(boundary == 3));
            assert_eq!(f.hooks.calls.load(Ordering::SeqCst), 1);
            assert_eq!(closes.load(Ordering::SeqCst), usize::from(!panics));
            drop(retained_store);
            f.driver.turn();
            assert_eq!(
                f.values.load(Ordering::SeqCst),
                3,
                "all original owned values drop once, including key-drop unwind"
            );
        }
    }
}

#[test]
fn second_key_lookup_close_rejects_before_callbacks_and_disposes_captures() {
    let f = fixture();
    let closed = f.store.closed();
    let captures = Arc::new(AtomicUsize::new(0));
    let before_capture = DropWatch(Arc::clone(&captures));
    let after_capture = DropWatch(Arc::clone(&captures));
    let disposed_rows = Arc::new(Mutex::new(Vec::new()));
    let snapshot = Arc::clone(&disposed_rows);
    let expected_rows = f
        .driver
        .finish(f.parent.update(move |parent| {
            parent.disposed_rows = Some(snapshot);
            parent.snapshot()
        }))
        .unwrap()
        .into_parts()
        .0;
    assert_eq!(expected_rows[0], (1, f.identity, 4));
    *f.hooks.callback.lock().unwrap() = Some(Box::new(move || drop(f.store)));
    // The owning selector arms the hook after ingress/route validation. Allow
    // entry_stamp's hash, then close during the actual get_mut lookup.
    f.hooks.skip.store(1, Ordering::SeqCst);
    f.select_rows.store(11, Ordering::SeqCst);
    let error = f
        .driver
        .finish(f.parent.remove_child(
            &f.sites[0].2,
            &f.first,
            move |_| {
                let _ = &before_capture;
                panic!("closure during get_mut must prevent before");
            },
            move |_, _, _| {
                let _ = &after_capture;
                panic!("no removed value may escape the closed Store");
            },
        ))
        .unwrap_err();
    assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), TaskProgress::MayHaveRun);
    assert_eq!(f.hooks.skip.load(Ordering::SeqCst), 0);
    assert_eq!(f.hooks.calls.load(Ordering::SeqCst), 1);
    assert_eq!(f.hooks.armed.load(Ordering::SeqCst), 0);
    assert!(f.hooks.callback.lock().unwrap().is_none());
    assert_eq!(captures.load(Ordering::SeqCst), 2);
    assert_eq!(f.handlers.load(Ordering::SeqCst), 0);
    f.driver.finish(closed);
    f.driver.turn();
    assert_eq!(
        *disposed_rows.lock().unwrap(),
        expected_rows,
        "closure during lookup retains every original entry until State disposal"
    );
    assert_eq!(f.values.load(Ordering::SeqCst), 3);
    assert_eq!(f.features.load(Ordering::SeqCst), 3);
    assert!(f.driver.0.lock().unwrap().is_empty());
}

#[test]
fn keyed_before_close_keeps_entered_value_from_after_and_disposes_captures() {
    let f = fixture();
    let closed = f.store.closed();
    let identity = f.identity;
    let before_calls = Arc::new(AtomicUsize::new(0));
    let called = Arc::clone(&before_calls);
    let captures = Arc::new(AtomicUsize::new(0));
    let before_capture = DropWatch(Arc::clone(&captures));
    let after_capture = DropWatch(Arc::clone(&captures));
    let disposed_rows = Arc::new(Mutex::new(Vec::new()));
    let snapshot = Arc::clone(&disposed_rows);
    let mut expected_rows = f
        .driver
        .finish(f.parent.update(move |parent| {
            parent.disposed_rows = Some(snapshot);
            parent.snapshot()
        }))
        .unwrap()
        .into_parts()
        .0;
    assert_eq!(expected_rows[0], (1, identity, 4));
    expected_rows[0].2 = 7;
    let error = f
        .driver
        .finish(f.parent.remove_child(
            &f.sites[0].2,
            &f.first,
            move |value| {
                let _ = &before_capture;
                assert_eq!((&raw const *value.value).addr(), identity);
                assert_eq!(value.value.get(), 4);
                value.value.set(7);
                called.fetch_add(1, Ordering::SeqCst);
                drop(f.store);
                assert_eq!(value.value.get(), 7, "the borrowed value remains valid");
            },
            move |_, _, _| {
                let _ = &after_capture;
                panic!("closure during before must prevent owning removal and after");
            },
        ))
        .unwrap_err();
    assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), TaskProgress::MayHaveRun);
    assert_eq!(before_calls.load(Ordering::SeqCst), 1);
    assert_eq!(captures.load(Ordering::SeqCst), 2);
    assert_eq!(f.handlers.load(Ordering::SeqCst), 0);
    f.driver.finish(closed);
    f.driver.turn();
    assert_eq!(
        *disposed_rows.lock().unwrap(),
        expected_rows,
        "before's mutation survives, but owning removal never takes the target or siblings"
    );
    assert_eq!(f.values.load(Ordering::SeqCst), 3);
    assert_eq!(f.features.load(Ordering::SeqCst), 3);
    assert!(f.driver.0.lock().unwrap().is_empty());
}

#[test]
fn selector_after_and_converter_panics_report_entered_failure_and_drop_captures() {
    for boundary in 0..4 {
        let f = fixture();
        if boundary == 0 {
            f.select_parent.store(2, Ordering::SeqCst);
        }
        if boundary == 1 {
            f.select_rows.store(2, Ordering::SeqCst);
        }
        let after_drops = Arc::new(AtomicUsize::new(0));
        let capture = DropWatch(Arc::clone(&after_drops));
        let output = f.output.clone();
        let mut operation = Box::pin(f.parent.remove_child(
            &f.sites[0].2,
            &f.first,
            |_| {},
            move |_, removed, context| {
                if boundary == 2 {
                    let _ = &capture;
                    panic!("after callback");
                }
                removed
                    .into_output(
                        context,
                        &output,
                        |_| panic!("converter callback"),
                        move |_| {
                            drop(capture);
                            panic!("unentered receive");
                        },
                    )
                    .unwrap();
            },
        ));
        assert!(poll(operation.as_mut()).is_pending());
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f.driver.turn()))
            .expect_err("the selected removal callback must panic");
        let expected = [
            "parent selector",
            "owning selector",
            "after callback",
            "converter callback",
        ][boundary];
        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&expected),
            "an unrelated panic must not satisfy the selected failure boundary"
        );
        let error = f.driver.finish(operation).unwrap_err();
        assert_eq!(error.reason(), TaskAccessReason::ActorFailed);
        assert_eq!(error.progress(), TaskProgress::MayHaveRun);
        assert_eq!(after_drops.load(Ordering::SeqCst), 1);
        assert_eq!(f.handlers.load(Ordering::SeqCst), 0);
        drop(f.store);
        f.driver.turn();
        assert_eq!(f.values.load(Ordering::SeqCst), 3);
    }
}

#[test]
fn live_parent_source_with_unbuilt_declaration_returns_owned_removal() {
    uninstalled_output_rejection(false);
}

#[test]
fn live_parent_source_with_dropped_declaration_returns_owned_removal() {
    uninstalled_output_rejection(true);
}

/// Proves rejected output conversion returns the original removed value to its caller.
fn uninstalled_output_rejection(drop_declaration: bool) {
    let f = fixture();
    let handler_drops = Arc::new(AtomicUsize::new(0));
    let handler_capture = DropWatch(Arc::clone(&handler_drops));
    let (builder, output) = Store::feature_builder(
        Root {
            parent: None,
            select: Arc::new(AtomicUsize::new(0)),
        },
        RootFeature,
        Runtime::new(f.driver.clone()),
    )
    .output::<Value>(&f.parent_site, |handlers| {
        handlers.at_root(move |_, _, _| {
            let _ = &handler_capture;
            panic!("an unbuilt declaration cannot receive");
        });
    });
    let mut builder = Some(builder);
    if drop_declaration {
        drop(builder.take());
    }
    assert_eq!(
        handler_drops.load(Ordering::SeqCst),
        usize::from(drop_declaration)
    );
    let reason = if drop_declaration {
        TaskAccessReason::StoreClosed
    } else {
        TaskAccessReason::ScopeEnded
    };
    let receive_drops = Arc::new(AtomicUsize::new(0));
    let receive_capture = DropWatch(Arc::clone(&receive_drops));
    let recovered = Arc::new(Mutex::new(None));
    let recovery = Arc::clone(&recovered);
    let task = f
        .driver
        .finish(f.parent.remove_child(
            &f.sites[0].2,
            &f.first,
            |_| {},
            move |_, removed, context| {
                let (removed, error) = removed
                    .into_output(
                        context,
                        &output,
                        |_| panic!("an unbuilt declaration cannot convert"),
                        move |_| {
                            drop(receive_capture);
                            panic!("an unbuilt declaration cannot receive a reply");
                        },
                    )
                    .unwrap_err()
                    .into_parts();
                assert_eq!(error.reason(), reason);
                assert_eq!(error.progress(), TaskProgress::NotStarted);
                *recovery.lock().unwrap() = Some(removed.into_inner());
            },
        ))
        .unwrap();
    f.driver.finish(task.finish());
    assert_eq!(receive_drops.load(Ordering::SeqCst), 1);
    assert_eq!(
        handler_drops.load(Ordering::SeqCst),
        usize::from(drop_declaration)
    );
    assert_eq!(f.values.load(Ordering::SeqCst), 0);
    assert_eq!(f.handlers.load(Ordering::SeqCst), 0);
    assert_eq!(
        f.driver
            .finish(f.parent.read(|parent| parent.rows.len()))
            .unwrap(),
        2
    );
    let value = recovered.lock().unwrap().take().unwrap();
    assert_eq!((&raw const *value.value) as usize, f.identity);
    assert_eq!(value.value.get(), 4);
    f.driver.finish(f.store.shutdown()).unwrap();
    drop(f.store);
    assert_eq!(f.values.load(Ordering::SeqCst), 2);
    assert_eq!(value.value.get(), 4);
    drop(value);
    assert_eq!(f.values.load(Ordering::SeqCst), 3);
    drop(builder);
    assert_eq!(handler_drops.load(Ordering::SeqCst), 1);
}

#[test]
fn live_parent_source_with_sibling_declaration_owner_rejects_before_conversion() {
    let f = fixture();
    let output = f.sibling_owned_output.clone();
    let captures = Arc::new(AtomicUsize::new(0));
    let capture = DropWatch(Arc::clone(&captures));
    let identity = f.identity;
    let task = f
        .driver
        .finish(f.parent.remove_child(
            &f.sites[0].2,
            &f.first,
            |_| {},
            move |_, removed, context| {
                let (removed, error) = removed
                    .into_output(
                        context,
                        &output,
                        |_| panic!("live source alone is insufficient"),
                        move |_| {
                            drop(capture);
                            panic!("no producer");
                        },
                    )
                    .unwrap_err()
                    .into_parts();
                assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
                assert_eq!(error.progress(), TaskProgress::NotStarted);
                assert_eq!((&raw const *removed.into_inner().value) as usize, identity);
            },
        ))
        .unwrap();
    f.driver.finish(task.finish());
    assert_eq!(captures.load(Ordering::SeqCst), 1);
    assert_eq!(
        f.driver
            .finish(f.second.read(|value| value.value.get()))
            .unwrap(),
        20,
        "output declaration owner remains live"
    );
    assert!(
        f.driver.finish(f.parent.read(|_| ())).is_ok(),
        "output source remains live"
    );
    f.driver.finish(f.store.shutdown()).unwrap();
}

#[test]
fn ordinary_overwrite_drop_and_retained_weak_removal_metadata_are_silent() {
    for overwrite in [false, true] {
        let f = fixture();
        let spawns = f.driver.1.load(Ordering::SeqCst);
        if overwrite {
            let _ = f
                .driver
                .finish(f.parent.update(|parent| parent.rows = KeyedState::new()))
                .unwrap();
        }
        assert_eq!(
            f.values.load(Ordering::SeqCst),
            if overwrite { 3 } else { 0 }
        );
        assert_eq!(
            f.features.load(Ordering::SeqCst),
            if overwrite { 3 } else { 0 }
        );
        assert_eq!(f.driver.1.load(Ordering::SeqCst), spawns);
        let callback_drops = Arc::new(AtomicUsize::new(0));
        let capture = DropWatch(Arc::clone(&callback_drops));
        let operation = f.parent.remove_child(
            &f.sites[0].2,
            &f.first,
            |_| panic!("closed before"),
            move |_, _, _| {
                drop(capture);
                panic!("closed after");
            },
        );
        drop(f.store);
        f.driver.turn();
        assert_eq!(f.values.load(Ordering::SeqCst), 3);
        assert_eq!(f.features.load(Ordering::SeqCst), 3);
        assert_eq!(
            f.handler_drops.load(Ordering::SeqCst),
            1,
            "retained sites, outputs, contexts and unpolled request do not retain handler payloads"
        );
        assert_eq!(
            callback_drops.load(Ordering::SeqCst),
            0,
            "unpolled request still owns its callback"
        );
        let error = f.driver.finish(operation).unwrap_err();
        assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
        assert_eq!(error.progress(), TaskProgress::NotStarted);
        assert_eq!(callback_drops.load(Ordering::SeqCst), 1);
        assert_eq!(f.handlers.load(Ordering::SeqCst), 0);
        assert_eq!(f.driver.1.load(Ordering::SeqCst), spawns);
    }
}

#[cfg(feature = "test-util")]
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one borrowed removal output retains cancellation ownership, terminal debt and successor order"
)]
fn held_removal_output_cancel_waits_for_borrow_and_explicit_terminal_acknowledgement() {
    let gate = rca_core::testing::TransactionGate::<super::Root, ()>::new();
    let super::Fixture {
        driver,
        store,
        parent,
        child,
        removal,
        tracked,
        handlers,
        ..
    } = super::fixture_with_gate(Some(&gate));
    let values = Arc::new(AtomicUsize::new(0));
    let captures = Arc::new(AtomicUsize::new(0));
    let receives = Arc::new(AtomicUsize::new(0));
    let value = DropWatch(Arc::clone(&values));
    let capture = DropWatch(Arc::clone(&captures));
    let delivery_count = Arc::clone(&receives);
    let output = tracked.clone();
    let mut remove = Box::pin(parent.remove_child(
        &removal,
        &child,
        |_| {},
        move |_, removed, context| {
            removed
                .into_output(
                    context,
                    &output,
                    move |_| value,
                    move |_| {
                        delivery_count.fetch_add(1, Ordering::SeqCst);
                        drop(capture);
                    },
                )
                .unwrap();
        },
    ));
    assert!(poll(remove.as_mut()).is_pending());
    driver.turn();
    assert!(gate.release_pending());
    driver.turn();
    let std::task::Poll::Ready(Ok(task)) = poll(remove.as_mut()) else {
        panic!("removal committed");
    };
    assert!(gate.acknowledge(gate.boundary().unwrap()));
    for _ in 0..4 {
        driver.turn();
    }
    let boundary = gate.boundary().unwrap();
    let terminal = gate.pending_boundary().unwrap();
    assert_eq!(
        gate.pending_kind(),
        Some(rca_core::testing::TransactionKind::Output)
    );
    let origin = gate.output_origin(&tracked).unwrap();
    assert_eq!(gate.output_origin(&tracked), Some(origin));
    gate.inspect_output(&tracked, |_| {
        task.cancel();
        for _ in 0..4 {
            driver.turn();
        }
        assert_eq!(
            gate.boundary(),
            Some(boundary),
            "borrow pins the pending command during retirement"
        );
        assert_eq!(values.load(Ordering::SeqCst), 0);
        assert_eq!(handlers.load(Ordering::SeqCst), 0);
    })
    .unwrap();
    for _ in 0..4 {
        driver.turn();
    }
    assert_eq!(
        gate.boundary(),
        Some(boundary),
        "returning the borrow retains the cancelled output's terminal debt"
    );
    driver.finish(task.finish());
    driver.turn();
    assert_eq!(values.load(Ordering::SeqCst), 1);
    assert_eq!(captures.load(Ordering::SeqCst), 1);
    assert_eq!(receives.load(Ordering::SeqCst), 0);
    assert_eq!(handlers.load(Ordering::SeqCst), 0);

    let mut later = Box::pin(parent.output(&tracked, DropWatch(Arc::clone(&values))));
    assert!(poll(later.as_mut()).is_pending());
    for _ in 0..4 {
        driver.turn();
    }
    assert_eq!(gate.boundary(), Some(boundary));
    assert_eq!(handlers.load(Ordering::SeqCst), 0);
    assert!(
        !gate.acknowledge(boundary),
        "no committed-state debt exists"
    );
    assert_eq!(
        gate.match_terminal(&terminal, |error| {
            error.reason() == TaskAccessReason::Cancelled
                && error.progress() == TaskProgress::NotStarted
        }),
        Some(true)
    );
    assert_eq!(
        gate.boundary(),
        Some(boundary),
        "matching is not acknowledgement"
    );
    assert!(gate.acknowledge_terminal(&terminal));
    assert_eq!(gate.boundary(), None);
    assert!(gate.output_origin(&tracked).is_none());
    for _ in 0..4 {
        driver.turn();
    }
    let later_boundary = gate.boundary().unwrap();
    assert_ne!(later_boundary, boundary);
    assert!(gate.release_pending());
    assert!(matches!(
        driver.finish(later),
        Ok(rca_core::OutputDelivery::Consumed { .. })
    ));
    assert!(gate.acknowledge(later_boundary));
    assert_eq!(handlers.load(Ordering::SeqCst), 1);
    assert_eq!(values.load(Ordering::SeqCst), 2);
    drop(store);
}

#[cfg(feature = "test-util")]
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One committed removal sequence preserves cancellation debt until exact acknowledgement"
)]
fn committed_removal_output_cancellation_keeps_debt_until_exact_acknowledgement() {
    let gate = rca_core::testing::TransactionGate::<super::Root, ()>::new();
    let super::Fixture {
        driver,
        store,
        parent,
        child,
        removal,
        tracked,
        handlers,
        ..
    } = super::fixture_with_gate(Some(&gate));
    let values = Arc::new(AtomicUsize::new(0));
    let captures = Arc::new(AtomicUsize::new(0));
    let receives = Arc::new(AtomicUsize::new(0));
    let value = DropWatch(Arc::clone(&values));
    let capture = DropWatch(Arc::clone(&captures));
    let delivery_count = Arc::clone(&receives);
    let output = tracked.clone();
    let mut remove = Box::pin(parent.remove_child(
        &removal,
        &child,
        |_| {},
        move |_, removed, context| {
            removed
                .into_output(
                    context,
                    &output,
                    move |_| value,
                    move |result| {
                        assert!(matches!(
                            result,
                            Ok(rca_core::OutputDelivery::Consumed { .. })
                        ));
                        delivery_count.fetch_add(1, Ordering::SeqCst);
                        drop(capture);
                    },
                )
                .unwrap();
        },
    ));
    assert!(poll(remove.as_mut()).is_pending());
    driver.turn();
    assert!(gate.release_pending());
    driver.turn();
    let std::task::Poll::Ready(Ok(task)) = poll(remove.as_mut()) else {
        panic!("removal committed");
    };
    assert!(gate.acknowledge(gate.boundary().unwrap()));
    for _ in 0..4 {
        driver.turn();
    }
    let boundary = gate.boundary().unwrap();
    assert!(gate.output_origin(&tracked).is_some());
    assert!(gate.release_pending());
    driver.turn();
    assert_eq!(
        handlers.load(Ordering::SeqCst),
        1,
        "output has committed before cancellation"
    );
    assert_eq!(
        receives.load(Ordering::SeqCst),
        0,
        "producer has not resumed with the committed reply"
    );
    assert_eq!(gate.boundary(), Some(boundary));
    assert!(
        !gate.has_pending(),
        "committed debt is not a pending output command"
    );
    let later_drops = Arc::new(AtomicUsize::new(0));
    let mut later = Box::pin(parent.output(&tracked, DropWatch(Arc::clone(&later_drops))));
    assert!(poll(later.as_mut()).is_pending());
    task.cancel();
    for _ in 0..4 {
        driver.turn();
    }
    assert_eq!(
        gate.boundary(),
        Some(boundary),
        "causal cancellation cannot acknowledge committed output debt"
    );
    assert!(!gate.has_pending());
    assert!(
        poll(later.as_mut()).is_pending(),
        "later output cannot overtake the debt"
    );
    assert_eq!(handlers.load(Ordering::SeqCst), 1);
    assert_eq!(values.load(Ordering::SeqCst), 1);
    assert_eq!(captures.load(Ordering::SeqCst), 1);
    assert_eq!(
        receives.load(Ordering::SeqCst),
        1,
        "this cooperative schedule resumes the already-committed reply before actor abort"
    );
    assert!(!gate.acknowledge(boundary + 1));
    assert_eq!(gate.boundary(), Some(boundary));
    assert!(gate.acknowledge(boundary));
    driver.turn();
    let later_boundary = gate.boundary().unwrap();
    assert_ne!(later_boundary, boundary);
    assert_eq!(
        gate.pending_kind(),
        Some(rca_core::testing::TransactionKind::Output)
    );
    assert!(
        gate.output_origin(&tracked).is_none(),
        "ordinary later output gains no removal origin"
    );
    assert!(gate.release_pending());
    driver.turn();
    assert!(gate.acknowledge(later_boundary));
    assert!(matches!(
        driver.finish(later),
        Ok(rca_core::OutputDelivery::Consumed { .. })
    ));
    driver.finish(task.finish());
    assert_eq!(handlers.load(Ordering::SeqCst), 2);
    assert_eq!(later_drops.load(Ordering::SeqCst), 1);
    drop(store);
}
