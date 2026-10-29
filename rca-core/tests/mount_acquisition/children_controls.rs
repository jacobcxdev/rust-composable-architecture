//! Public inferred child recipes preserve authority and structural-only closure boundaries.
use super::{Driver, poll};
use rca_core::{
    Feature, FeatureContext, MountIdentity, MountSite, OptionalMount, Runtime, Store, Task,
    TaskAccessReason, TaskContext, TaskProgress, TaskSendError,
};
use std::cell::Cell;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::task::Poll;

/// Counts selection, construction, callback entry and disposal at each tree level.
#[derive(Default)]
struct Counts {
    selections: AtomicUsize,
    /// Counts constructed parent Features, not configuration-only refreshes.
    parents: AtomicUsize,
    recipes: AtomicUsize,
    inner_recipes: AtomicUsize,
    inner_identities: AtomicUsize,
    leaf_identities: AtomicUsize,
    leaves: AtomicUsize,
    initials: AtomicUsize,
    /// Counts destroyed descendant Features before successor construction.
    feature_drops: AtomicUsize,
    /// Counts destroyed recipe and constructor captures separately from Features.
    callback_drops: AtomicUsize,
    reductions: AtomicUsize,
    closures: AtomicUsize,
}
/// Holds the final public Store owner until a selected parent lookup drops it.
#[derive(Default)]
struct Closing {
    armed: AtomicBool,
    /// Holds the last public owner until an armed selector takes and drops it.
    owner: Mutex<Option<Store<State, RootAction>>>,
}
struct State {
    branch: Option<BranchState>,
    configuration: u64,
    replacement: u64,
    counts: Arc<Counts>,
    closing: Arc<Closing>,
}
struct BranchState {
    inner: Option<InnerState>,
    counts: Arc<Counts>,
}
struct InnerState {
    leaf: Option<Cell<usize>>,
    counts: Arc<Counts>,
}
enum RootAction {
    Configure(u64),
    Replace,
}
struct Root;
impl Feature for Root {
    type State = State;
    type Action = RootAction;
    fn reduce(
        &mut self,
        state: &mut State,
        action: &RootAction,
        _: &mut FeatureContext<'_, State, RootAction>,
    ) {
        match action {
            RootAction::Configure(value) => state.configuration = *value,
            RootAction::Replace => state.replacement += 1,
        }
    }
}
struct Branch(Cell<usize>);
impl Feature for Branch {
    type State = BranchState;
    type Action = bool;
    fn reduce(
        &mut self,
        _: &mut BranchState,
        _: &bool,
        _: &mut FeatureContext<'_, BranchState, bool>,
    ) {
        self.0.set(self.0.get() + 1);
    }
}
struct Dropped {
    counts: Arc<Counts>,
    callback: bool,
}
impl Drop for Dropped {
    fn drop(&mut self) {
        let count = if self.callback {
            &self.counts.callback_drops
        } else {
            &self.counts.feature_drops
        };
        count.fetch_add(1, Ordering::SeqCst);
    }
}
struct Inner {
    cache: Cell<usize>,
    _dropped: Dropped,
}
impl Feature for Inner {
    type State = InnerState;
    type Action = u16;
    fn reduce(&mut self, _: &mut InnerState, _: &u16, _: &mut FeatureContext<'_, InnerState, u16>) {
        self.cache.set(self.cache.get() + 1);
    }
}
#[derive(Debug)]
enum LeafAction {
    BumpCache,
    ReportCache,
}
struct Leaf {
    cache: Cell<usize>,
    counts: Arc<Counts>,
    _dropped: Dropped,
}
impl Feature for Leaf {
    type State = Cell<usize>;
    type Action = LeafAction;
    fn reduce(
        &mut self,
        state: &mut Cell<usize>,
        action: &LeafAction,
        _: &mut FeatureContext<'_, Cell<usize>, LeafAction>,
    ) {
        self.counts.reductions.fetch_add(1, Ordering::SeqCst);
        match action {
            LeafAction::BumpCache => self.cache.set(self.cache.get() + 1),
            LeafAction::ReportCache => state.set(self.cache.get()),
        }
    }
}
type LeafSite = MountSite<State, Cell<usize>, LeafAction, RootAction>;
struct Fixture {
    store: Store<State, RootAction>,
    sites: Arc<Mutex<Vec<LeafSite>>>,
    counts: Arc<Counts>,
    closing: Arc<Closing>,
}
/// Closes on an armed parent lookup while recording selector entry.
fn parent_entry(state: &State) {
    state.counts.selections.fetch_add(1, Ordering::SeqCst);
    if state.closing.armed.swap(false, Ordering::SeqCst) {
        state.counts.closures.fetch_add(1, Ordering::SeqCst);
        drop(state.closing.owner.lock().unwrap().take());
    }
}
/// Builds a three-level tree with weak sites and counted descendant owners.
///
/// The closing mode arms after the leaf recipe, before the next parent lookup.
#[expect(
    clippy::too_many_lines,
    reason = "One nested fixture constructor preserves leaf recipe and initial-callback ordering"
)]
fn fixture(driver: &Driver, close_after_leaf_recipe: bool) -> Fixture {
    let counts = Arc::new(Counts::default());
    let closing = Arc::new(Closing::default());
    let sites = Arc::new(Mutex::new(Vec::new()));
    let made = std::sync::Arc::clone(&counts);
    let parent = OptionalMount::new(
        |state: &State| {
            parent_entry(state);
            state.branch.as_ref()
        },
        |state: &mut State| {
            parent_entry(state);
            state.branch.as_mut()
        },
        |state: &State| {
            state.counts.selections.fetch_add(1, Ordering::SeqCst);
            state.branch.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: state.replacement,
                configuration: state.configuration,
            })
        },
        move |_| {
            let previous = made.parents.fetch_add(1, Ordering::SeqCst);
            // Retained sites/contexts must not keep descendant owners alive.
            assert_eq!(
                made.feature_drops.load(Ordering::SeqCst),
                previous * 2,
                "descendant Features drain before successor parent constructor"
            );
            assert_eq!(
                made.callback_drops.load(Ordering::SeqCst),
                previous * 2,
                "descendant callbacks drain before successor parent constructor"
            );
            Branch(Cell::new(0))
        },
    );
    let saved_sites = std::sync::Arc::clone(&sites);
    let recipe_counts = std::sync::Arc::clone(&counts);
    let recipe_closing = std::sync::Arc::clone(&closing);
    let (builder, _) = Store::feature_builder(
        State {
            branch: Some(BranchState {
                inner: Some(InnerState {
                    leaf: Some(Cell::new(4)),
                    counts: std::sync::Arc::clone(&counts),
                }),
                counts: std::sync::Arc::clone(&counts),
            }),
            configuration: 0,
            replacement: 0,
            counts: std::sync::Arc::clone(&counts),
            closing: std::sync::Arc::clone(&closing),
        },
        Root,
        Runtime::new(driver.clone()),
    )
    .mount_with_children(parent, move |_, children| {
        recipe_counts.recipes.fetch_add(1, Ordering::SeqCst);
        let selections = recipe_counts.selections.load(Ordering::SeqCst);
        assert_eq!(format!("{children:?}"), "MountChildren { depth: 0, .. }");
        assert_eq!(recipe_counts.selections.load(Ordering::SeqCst), selections);
        let inner_counts = std::sync::Arc::clone(&recipe_counts);
        let inner = OptionalMount::new(
            |state: &BranchState| {
                state.counts.selections.fetch_add(1, Ordering::SeqCst);
                state.inner.as_ref()
            },
            |state: &mut BranchState| {
                state.counts.selections.fetch_add(1, Ordering::SeqCst);
                state.inner.as_mut()
            },
            |state: &BranchState| {
                state.counts.selections.fetch_add(1, Ordering::SeqCst);
                state.counts.inner_identities.fetch_add(1, Ordering::SeqCst);
                state.inner.as_ref().map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            move |_| Inner {
                cache: Cell::new(0),
                _dropped: Dropped {
                    counts: std::sync::Arc::clone(&inner_counts),
                    callback: false,
                },
            },
        );
        let counts = std::sync::Arc::clone(&recipe_counts);
        let sites = std::sync::Arc::clone(&saved_sites);
        let closing = std::sync::Arc::clone(&recipe_closing);
        let recipe_capture = Dropped {
            counts: std::sync::Arc::clone(&counts),
            callback: true,
        };
        children.optional_with_children(inner, move |_, children| {
            let _ = &recipe_capture;
            counts.inner_recipes.fetch_add(1, Ordering::SeqCst);
            let selections = counts.selections.load(Ordering::SeqCst);
            assert_eq!(format!("{children:?}"), "MountChildren { depth: 1, .. }");
            assert_eq!(counts.selections.load(Ordering::SeqCst), selections);
            let made = std::sync::Arc::clone(&counts);
            let initial = std::sync::Arc::clone(&counts);
            let constructor_capture = Dropped {
                counts: std::sync::Arc::clone(&counts),
                callback: true,
            };
            let leaf = OptionalMount::new(
                |state: &InnerState| {
                    state.counts.selections.fetch_add(1, Ordering::SeqCst);
                    state.leaf.as_ref()
                },
                |state: &mut InnerState| {
                    state.counts.selections.fetch_add(1, Ordering::SeqCst);
                    state.leaf.as_mut()
                },
                |state: &InnerState| {
                    state.counts.selections.fetch_add(1, Ordering::SeqCst);
                    state.counts.leaf_identities.fetch_add(1, Ordering::SeqCst);
                    state.leaf.as_ref().map(|_| MountIdentity {
                        key: (),
                        replacement: 0,
                        configuration: 0,
                    })
                },
                move |_| {
                    let _ = &constructor_capture;
                    made.leaves.fetch_add(1, Ordering::SeqCst);
                    Leaf {
                        cache: Cell::new(0),
                        counts: std::sync::Arc::clone(&made),
                        _dropped: Dropped {
                            counts: std::sync::Arc::clone(&made),
                            callback: false,
                        },
                    }
                },
            )
            .on_initial(Cell::get, move |_, _| {
                initial.initials.fetch_add(1, Ordering::SeqCst);
                Task::run(|_| async {})
            });
            sites.lock().unwrap().push(children.optional(leaf));
            assert_eq!(format!("{children:?}"), "MountChildren { depth: 1, .. }");
            if close_after_leaf_recipe {
                closing.armed.store(true, Ordering::SeqCst);
            }
        });
        assert_eq!(format!("{children:?}"), "MountChildren { depth: 0, .. }");
    });
    Fixture {
        store: builder.build().unwrap(),
        sites,
        counts,
        closing,
    }
}
/// Drives one public operation for at most eight manual turns.
fn finish<F: Future>(driver: &Driver, future: F) -> F::Output {
    let mut future = Box::pin(future);
    for _ in 0..8 {
        if let Poll::Ready(result) = poll(future.as_mut()) {
            return result;
        }
        driver.turn();
    }
    panic!("bounded public child operation did not finish");
}
/// Checks that retired ancestry rejects before any ancestor or leaf selector.
fn stale(driver: &Driver, context: &TaskContext<Cell<usize>, LeafAction>, counts: &Counts) {
    counts.selections.store(0, Ordering::SeqCst);
    let read = finish(driver, context.read(|_| panic!("stale read entered"))).unwrap_err();
    assert_eq!(
        (read.reason(), read.progress()),
        (TaskAccessReason::ScopeEnded, TaskProgress::NotStarted)
    );
    let update = finish(driver, context.update(|_| panic!("stale update entered"))).unwrap_err();
    assert_eq!(
        (update.reason(), update.progress()),
        (TaskAccessReason::ScopeEnded, TaskProgress::NotStarted)
    );
    let send = finish(driver, context.send(LeafAction::BumpCache)).unwrap_err();
    assert!(
        matches!(send, TaskSendError::Rejected { action: LeafAction::BumpCache, error }
        if error.reason() == TaskAccessReason::ScopeEnded && error.progress() == TaskProgress::NotStarted)
    );
    assert_eq!(
        counts.selections.load(Ordering::SeqCst),
        0,
        "authority precedes every ancestor/leaf selector"
    );
}

// Catches recipe reruns/cache loss on refresh, stale ancestry admission, and
// replacement retaining descendant callback/Feature owners through weak routes.
#[test]
fn configuration_retains_nested_cache_and_replacement_drains_before_successor() {
    let driver = Driver::default();
    let Fixture {
        store,
        sites,
        counts,
        ..
    } = fixture(&driver, false);
    finish(&driver, store.ready()).unwrap();
    let old_site = sites.lock().unwrap()[0].clone();
    let original = finish(&driver, store.acquire(&old_site)).unwrap().unwrap();
    let _ = finish(&driver, original.send(LeafAction::BumpCache)).unwrap();
    let _ = finish(&driver, store.send(RootAction::Configure(1))).unwrap();
    stale(&driver, &original, &counts);
    let refreshed = finish(&driver, store.acquire(&old_site)).unwrap().unwrap();
    let _ = finish(&driver, refreshed.send(LeafAction::ReportCache)).unwrap();
    assert_eq!(finish(&driver, refreshed.read(Cell::get)).unwrap(), 1);
    let _ = finish(&driver, store.send(RootAction::Configure(0))).unwrap();
    stale(&driver, &original, &counts);
    stale(&driver, &refreshed, &counts);
    assert_eq!(counts.parents.load(Ordering::SeqCst), 1);
    assert_eq!(counts.leaves.load(Ordering::SeqCst), 1);
    assert_eq!(counts.recipes.load(Ordering::SeqCst), 1);
    assert_eq!(counts.inner_recipes.load(Ordering::SeqCst), 1);
    assert_eq!(counts.initials.load(Ordering::SeqCst), 1);
    assert_eq!(counts.feature_drops.load(Ordering::SeqCst), 0);
    assert_eq!(counts.callback_drops.load(Ordering::SeqCst), 0);
    let current = finish(&driver, store.acquire(&old_site)).unwrap().unwrap();
    let _ = finish(&driver, store.send(RootAction::Replace)).unwrap();
    stale(&driver, &current, &counts);
    assert_eq!(counts.parents.load(Ordering::SeqCst), 2);
    assert_eq!(counts.recipes.load(Ordering::SeqCst), 2);
    assert_eq!(counts.inner_recipes.load(Ordering::SeqCst), 2);
    assert_eq!(counts.initials.load(Ordering::SeqCst), 2);
    assert_eq!(sites.lock().unwrap().len(), 2);
    let new_site = sites.lock().unwrap()[1].clone();
    #[cfg(feature = "test-util")]
    let gate = rca_core::testing::TransactionGate::new();
    #[cfg(feature = "test-util")]
    let error = gate.validate_acquisition(&store, &old_site).unwrap_err();
    #[cfg(feature = "test-util")]
    assert_eq!(
        (error.reason(), error.progress()),
        (TaskAccessReason::StoreClosed, TaskProgress::NotStarted)
    );
    #[cfg(feature = "test-util")]
    let error = finish(&driver, gate.acquire(&store, &old_site)).unwrap_err();
    #[cfg(feature = "test-util")]
    assert_eq!(
        (error.reason(), error.progress()),
        (TaskAccessReason::StoreClosed, TaskProgress::NotStarted)
    );
    #[cfg(feature = "test-util")]
    gate.validate_acquisition(&store, &new_site)
        .expect("the successor declaration remains live");
    let replacement = finish(&driver, store.acquire(&new_site)).unwrap().unwrap();
    let _ = finish(&driver, replacement.send(LeafAction::ReportCache)).unwrap();
    assert_eq!(finish(&driver, replacement.read(Cell::get)).unwrap(), 0);
    drop(store);
    driver.turn();
    assert_eq!(counts.feature_drops.load(Ordering::SeqCst), 4);
    assert_eq!(counts.callback_drops.load(Ordering::SeqCst), 4);
}

// Catches replacing erased read_structural with ordinary read or dropping its
// inter-selector liveness checks: the leaf identity must never enter.
#[test]
fn structural_parent_projection_closure_suppresses_child_identity_constructor_and_factory() {
    let driver = Driver::default();
    let Fixture {
        store,
        counts,
        closing,
        ..
    } = fixture(&driver, true);
    let ready = store.ready();
    *closing.owner.lock().unwrap() = Some(store);
    let result = finish(&driver, ready);
    assert!(result.is_err());
    assert_eq!(counts.closures.load(Ordering::SeqCst), 1);
    assert_eq!(counts.inner_identities.load(Ordering::SeqCst), 1);
    assert_eq!(counts.inner_recipes.load(Ordering::SeqCst), 1);
    assert_eq!(counts.leaf_identities.load(Ordering::SeqCst), 0);
    assert_eq!(counts.leaves.load(Ordering::SeqCst), 0);
    assert_eq!(counts.initials.load(Ordering::SeqCst), 0);
}

// Catches extending structural closure checks into ordinary read/write paths:
// once entered, downstream selection and the operation itself still complete.
#[test]
fn ordinary_nested_read_update_and_reducer_remain_non_preemptive() {
    for operation in ["read", "update", "reduce"] {
        let driver = Driver::default();
        let Fixture {
            store,
            sites,
            counts,
            closing,
        } = fixture(&driver, false);
        finish(&driver, store.ready()).unwrap();
        let site = sites.lock().unwrap()[0].clone();
        let child = finish(&driver, store.acquire(&site)).unwrap().unwrap();
        *closing.owner.lock().unwrap() = Some(store);
        closing.armed.store(true, Ordering::SeqCst);
        counts.selections.store(0, Ordering::SeqCst);
        let entered = Arc::new(AtomicUsize::new(0));
        let called = std::sync::Arc::clone(&entered);
        match operation {
            "read" => {
                let _ = finish(
                    &driver,
                    child.read(move |state| {
                        called.fetch_add(1, Ordering::SeqCst);
                        assert_eq!(state.get(), 4);
                    }),
                );
                assert_eq!(entered.load(Ordering::SeqCst), 1);
            }
            "update" => {
                let _ = finish(
                    &driver,
                    child.update(move |state| {
                        state.set(9);
                        called.store(state.get(), Ordering::SeqCst);
                    }),
                );
                assert_eq!(entered.load(Ordering::SeqCst), 9);
            }
            "reduce" => {
                let _ = finish(&driver, child.send(LeafAction::BumpCache));
                assert_eq!(counts.reductions.load(Ordering::SeqCst), 1);
            }
            _ => unreachable!(),
        }
        assert_eq!(counts.closures.load(Ordering::SeqCst), 1);
        assert_eq!(
            counts.selections.load(Ordering::SeqCst),
            3,
            "ordinary path visits parent, inner and leaf after closure"
        );
        driver.turn();
        assert_eq!(counts.feature_drops.load(Ordering::SeqCst), 2);
        assert_eq!(counts.callback_drops.load(Ordering::SeqCst), 2);
    }
}

// Catches globally sorting Initial entries by depth: each sibling subtree must
// finish before the next begins, with the actual root Initial deferred last.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One sibling sequence preserves complete subtree initialisation before the next sibling and root"
)]
fn initial_order_finishes_each_sibling_subtree_before_the_next_and_root() {
    type Read = for<'a> fn(&'a Forest) -> Option<&'a BranchState>;
    type Write = for<'a> fn(&'a mut Forest) -> Option<&'a mut BranchState>;
    struct Forest {
        left: Option<BranchState>,
        right: Option<BranchState>,
    }
    struct ForestRoot;
    impl Feature for ForestRoot {
        type State = Forest;
        type Action = ();
        fn reduce(&mut self, _: &mut Forest, (): &(), _: &mut FeatureContext<'_, Forest, ()>) {}
    }
    type ReadyCheck =
        std::pin::Pin<Box<dyn Future<Output = Result<(), rca_core::TaskAccessError>> + Send>>;
    #[derive(Default)]
    struct InitialProof {
        constructed: AtomicUsize,
        bodies: AtomicUsize,
        ready: Mutex<Vec<ReadyCheck>>,
        order: Mutex<Vec<&'static str>>,
    }
    fn factory<S: 'static, A: 'static>(proof: Arc<InitialProof>, name: &'static str) -> Task<S, A> {
        assert_eq!(
            proof.constructed.load(Ordering::SeqCst),
            6,
            "all mounted construction precedes every Initial factory"
        );
        assert_eq!(
            proof.bodies.load(Ordering::SeqCst),
            0,
            "no body runs during Initial factories"
        );
        proof.order.lock().unwrap().push(name);
        Task::run(move |_| async move {
            let mut ready = proof.ready.lock().unwrap().pop().unwrap();
            assert!(
                matches!(poll(std::pin::Pin::new(&mut ready)), Poll::Ready(Ok(()))),
                "actual Store readiness precedes every body entry"
            );
            proof.bodies.fetch_add(1, Ordering::SeqCst);
        })
    }

    let driver = Driver::default();
    let proof = Arc::new(InitialProof::default());
    let counts = Arc::new(Counts::default());
    let branch = || {
        Some(BranchState {
            inner: Some(InnerState {
                leaf: Some(Cell::new(4)),
                counts: std::sync::Arc::clone(&counts),
            }),
            counts: std::sync::Arc::clone(&counts),
        })
    };
    let root_proof = std::sync::Arc::clone(&proof);
    let mut builder = rca_core::RootMount::new(ForestRoot)
        .on_initial(
            |_| (),
            move |_, _| factory(std::sync::Arc::clone(&root_proof), "root"),
        )
        .builder(
            Forest {
                left: branch(),
                right: branch(),
            },
            Runtime::new(driver.clone()),
        );
    for (read, write, parent_name, inner_name, leaf_name) in [
        (
            (|state: &Forest| state.left.as_ref()) as Read,
            (|state: &mut Forest| state.left.as_mut()) as Write,
            "left parent",
            "left inner",
            "left grandchild",
        ),
        (
            (|state: &Forest| state.right.as_ref()) as Read,
            (|state: &mut Forest| state.right.as_mut()) as Write,
            "right parent",
            "right inner",
            "right grandchild",
        ),
    ] {
        let parent_proof = std::sync::Arc::clone(&proof);
        let parent_made = std::sync::Arc::clone(&proof);
        let parent = OptionalMount::new(
            read,
            write,
            move |state| {
                read(state).map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            move |_| {
                parent_made.constructed.fetch_add(1, Ordering::SeqCst);
                Branch(Cell::new(0))
            },
        )
        .on_initial(
            |_| (),
            move |_, _| factory(std::sync::Arc::clone(&parent_proof), parent_name),
        );
        let child_proof = std::sync::Arc::clone(&proof);
        let child_counts = std::sync::Arc::clone(&counts);
        (builder, _) = builder.mount_with_children(parent, move |_, children| {
            let made = std::sync::Arc::clone(&child_counts);
            let inner_proof = std::sync::Arc::clone(&child_proof);
            let inner_made = std::sync::Arc::clone(&child_proof);
            let inner = OptionalMount::new(
                |state: &BranchState| state.inner.as_ref(),
                |state: &mut BranchState| state.inner.as_mut(),
                |state: &BranchState| {
                    state.inner.as_ref().map(|_| MountIdentity {
                        key: (),
                        replacement: 0,
                        configuration: 0,
                    })
                },
                move |_| {
                    inner_made.constructed.fetch_add(1, Ordering::SeqCst);
                    Inner {
                        cache: Cell::new(0),
                        _dropped: Dropped {
                            counts: std::sync::Arc::clone(&made),
                            callback: false,
                        },
                    }
                },
            )
            .on_initial(
                |_| (),
                move |_, _| factory(std::sync::Arc::clone(&inner_proof), inner_name),
            );
            let leaf_proof = std::sync::Arc::clone(&child_proof);
            children.optional_with_children(inner, move |_, grandchildren| {
                let initial_proof = std::sync::Arc::clone(&leaf_proof);
                let leaf_made = std::sync::Arc::clone(&leaf_proof);
                grandchildren.optional(
                    OptionalMount::new(
                        |state: &InnerState| state.leaf.as_ref(),
                        |state: &mut InnerState| state.leaf.as_mut(),
                        |state: &InnerState| {
                            state.leaf.as_ref().map(|_| MountIdentity {
                                key: (),
                                replacement: 0,
                                configuration: 0,
                            })
                        },
                        move |_| {
                            leaf_made.constructed.fetch_add(1, Ordering::SeqCst);
                            super::ChildFeature(Cell::new(0))
                        },
                    )
                    .on_initial(Cell::get, move |_, _| {
                        factory(std::sync::Arc::clone(&initial_proof), leaf_name)
                    }),
                );
            });
        });
    }
    let store = builder.build().unwrap();
    for _ in 0..7 {
        proof.ready.lock().unwrap().push(Box::pin(store.ready()));
    }
    assert!(proof.order.lock().unwrap().is_empty());
    let mut ready = Box::pin(store.ready());
    assert!(poll(ready.as_mut()).is_pending());
    driver.turn();
    assert!(matches!(poll(ready.as_mut()), Poll::Ready(Ok(()))));
    assert_eq!(proof.bodies.load(Ordering::SeqCst), 0);
    assert_eq!(
        *proof.order.lock().unwrap(),
        [
            "left grandchild",
            "left inner",
            "left parent",
            "right grandchild",
            "right inner",
            "right parent",
            "root",
        ]
    );
    driver.turn();
    assert_eq!(proof.bodies.load(Ordering::SeqCst), 7);
    assert!(proof.ready.lock().unwrap().is_empty());
    drop(store);
    driver.turn();
}
