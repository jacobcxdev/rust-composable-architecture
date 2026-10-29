//! Non-Send collection ownership through real actors and private reconciliation calls.

use super::*;
use crate::runtime::local as runtime;
use crate::store::local as profile;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicU8, Ordering};

/// Holds local actor tasks for explicit polling on the owning test thread.
/// Time stays at zero; yields suspend once and request another poll.
#[derive(Clone, Default)]
struct LocalDriver(Rc<RefCell<Vec<runtime::RuntimeFuture>>>);
impl fmt::Debug for LocalDriver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("KeyedLocalDriver")
    }
}
impl runtime::RuntimeDriver for LocalDriver {
    fn spawn(&self, task: runtime::RuntimeFuture) {
        self.0.borrow_mut().push(task);
    }
    fn now(&self) -> crate::MonotonicInstant {
        crate::MonotonicInstant::from_elapsed(std::time::Duration::ZERO)
    }
    fn sleep_until(&self, deadline: crate::MonotonicInstant) -> runtime::RuntimeFuture {
        if deadline <= self.now() {
            Box::pin(std::future::ready(()))
        } else {
            Box::pin(std::future::pending())
        }
    }
    fn yield_now(&self) -> runtime::RuntimeFuture {
        let mut yielded = false;
        Box::pin(std::future::poll_fn(move |context| {
            if yielded {
                Poll::Ready(())
            } else {
                yielded = true;
                context.waker().wake_by_ref();
                Poll::Pending
            }
        }))
    }
}
impl LocalDriver {
    /// Polls outside the queue borrow; tasks spawned during polling wait a turn.
    fn turn(&self) {
        let tasks = std::mem::take(&mut *self.0.borrow_mut());
        for mut task in tasks {
            if poll(task.as_mut()).is_pending() {
                self.0.borrow_mut().push(task);
            }
        }
    }
    /// Allows at most 32 turns, without treating no-op-waker polling as wake proof.
    fn run<T>(&self, future: impl Future<Output = T>) -> T {
        let mut future = Box::pin(future);
        for _ in 0..32 {
            if let Poll::Ready(value) = poll(future.as_mut()) {
                return value;
            }
            self.turn();
        }
        panic!("bounded local actor operation")
    }
}
#[test]
fn local_driver_clock_waits_respect_the_fixed_origin() {
    let driver = LocalDriver::default();
    let zero = crate::MonotonicInstant::from_elapsed(std::time::Duration::ZERO);
    assert_eq!(runtime::RuntimeDriver::now(&driver), zero);

    let mut elapsed = runtime::RuntimeDriver::sleep_until(&driver, zero);
    assert!(poll(elapsed.as_mut()).is_ready());
    for deadline in [
        crate::MonotonicInstant::from_elapsed(std::time::Duration::from_nanos(1)),
        crate::MonotonicInstant::MAX,
    ] {
        let mut future = runtime::RuntimeDriver::sleep_until(&driver, deadline);
        assert!(poll(future.as_mut()).is_pending());
    }
    assert!(driver.0.borrow().is_empty());
}

#[test]
fn local_driver_yield_suspends_and_wakes_before_completion() {
    /// Counts scheduling requests without polling any local payload off-thread.
    struct WakeCount(std::sync::atomic::AtomicUsize);
    impl std::task::Wake for WakeCount {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    let driver = LocalDriver::default();
    let notifications = Arc::new(WakeCount(std::sync::atomic::AtomicUsize::new(0)));
    let waker = Waker::from(Arc::clone(&notifications));
    let mut context = Context::from_waker(&waker);
    let mut future = runtime::RuntimeDriver::yield_now(&driver);
    assert_eq!(notifications.0.load(std::sync::atomic::Ordering::SeqCst), 0);

    assert!(future.as_mut().poll(&mut context).is_pending());
    assert_eq!(notifications.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(future.as_mut().poll(&mut context).is_ready());
    assert_eq!(notifications.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(driver.0.borrow().is_empty());
}

struct State {
    rows: KeyedState<u8, Row>,
    other: KeyedState<u8, Rc<Cell<usize>>>,
}
struct Row {
    values: KeyedState<u8, Rc<Cell<usize>>>,
    rc: Rc<()>,
}
enum RootAction {
    Reverse(Rc<()>),
    ReplaceDetails(Rc<()>),
}
struct RowAction(Rc<()>);
struct LeafAction(Rc<usize>);
/// Counts callback disposal without making the local payload Send or Sync.
struct LocalCapture(Rc<Cell<usize>>);
impl Drop for LocalCapture {
    fn drop(&mut self) {
        self.0.set(self.0.get() + 1);
    }
}
struct Root;
impl profile::Feature for Root {
    type State = State;
    type Action = RootAction;
    fn reduce(
        &mut self,
        state: &mut State,
        action: &RootAction,
        _: &mut profile::FeatureContext<'_, State, RootAction>,
    ) {
        match action {
            RootAction::Reverse(rc) => {
                assert_eq!(Rc::strong_count(rc), 1);
                state.rows.reverse();
                for (_, row) in state.rows.iter_mut() {
                    row.values.reverse();
                }
            }
            RootAction::ReplaceDetails(rc) => {
                assert_eq!(Rc::strong_count(rc), 1);
                for (_, row) in state.rows.iter_mut() {
                    row.values.remove(&1);
                    row.values.insert(1, Rc::new(Cell::new(10)));
                }
            }
        }
    }
}
/// Keeps parent reducer state outside the projected row to detect remounts.
struct Parent(Rc<Cell<usize>>);
impl profile::Feature for Parent {
    type State = Row;
    type Action = RowAction;
    fn reduce(
        &mut self,
        state: &mut Row,
        action: &RowAction,
        _: &mut profile::FeatureContext<'_, Row, RowAction>,
    ) {
        assert_eq!(Rc::strong_count(&action.0), 1);
        assert_eq!(Rc::strong_count(&state.rc), 1);
        self.0.set(self.0.get() + 1);
    }
}
/// Tracks child reducer continuity independently from its Rc-owned state value.
struct Leaf(Rc<Cell<usize>>);
impl profile::Feature for Leaf {
    type State = Rc<Cell<usize>>;
    type Action = LeafAction;
    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &LeafAction,
        _: &mut profile::FeatureContext<'_, Self::State, LeafAction>,
    ) {
        self.0.set(self.0.get() + 1);
        state.set(state.get() + *action.0);
    }
}
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one local collection schedule keeps recipe discovery, replacement and callback ordering together"
)]
fn local_rc_tuple_recipes_and_nested_changed_collections() {
    let driver = LocalDriver::default();
    let counts = Rc::new(RefCell::new(Vec::new()));
    let parents = Rc::new(RefCell::new(Vec::new()));
    let leaves = Rc::new(RefCell::new(Vec::new()));
    let make_counts = std::rc::Rc::clone(&counts);
    let make_parents = std::rc::Rc::clone(&parents);
    let make_leaves = std::rc::Rc::clone(&leaves);
    let changes = Rc::new(RefCell::new(Vec::new()));
    let make_changes = Rc::clone(&changes);
    let drops = Rc::new(Cell::new(0));
    let factory_capture = LocalCapture(std::rc::Rc::clone(&drops));
    let make_drops = std::rc::Rc::clone(&drops);
    let store = profile::Store::feature_builder(
        State {
            rows: (1..=2)
                .map(|id| {
                    (
                        id,
                        Row {
                            values: [(1, Rc::new(Cell::new(1))), (2, Rc::new(Cell::new(2)))]
                                .into_iter()
                                .collect(),
                            rc: Rc::new(()),
                        },
                    )
                })
                .collect(),
            other: [(3, Rc::new(Cell::new(3)))].into_iter().collect(),
        },
        Root,
        runtime::Runtime::new(driver.clone()),
    )
    .keyed(
        |state| &state.other,
        |state| &mut state.other,
        move |_, _| {
            let changes = Rc::clone(&make_changes);
            crate::store::mount::local::EntryMount::new(Leaf(Rc::new(Cell::new(0))))
                .on_initial(
                    |_| (),
                    |_, _| {
                        profile::Task::run(|context| async move {
                            drop(
                                context
                                    .update(|value: &mut Rc<Cell<usize>>| {
                                        value.set(value.get() + 10);
                                    })
                                    .await
                                    .unwrap(),
                            );
                        })
                    },
                )
                .on_change(
                    |value| value.get(),
                    crate::InitialPolicy::Run,
                    move |change, _| {
                        changes.borrow_mut().push(change);
                        profile::Task::run(|_| async {})
                    },
                )
        },
    )
    .keyed_with_children(
        |state| &state.rows,
        |state| &mut state.rows,
        move |id, _| {
            let _ = &factory_capture;
            let id = *id;
            make_counts.borrow_mut().push(("parent make", id, 0));
            let counts = std::rc::Rc::clone(&make_counts);
            let parents = std::rc::Rc::clone(&make_parents);
            let leaves = std::rc::Rc::clone(&make_leaves);
            let config_capture = LocalCapture(std::rc::Rc::clone(&make_drops));
            let initial_capture = LocalCapture(std::rc::Rc::clone(&make_drops));
            let recipe_capture = LocalCapture(std::rc::Rc::clone(&make_drops));
            let leaf_drops = std::rc::Rc::clone(&make_drops);
            let entry = crate::store::mount::local::EntryMount::new(Parent(Rc::new(Cell::new(0))))
                .configuration(move |row| {
                    let _ = &config_capture;
                    Rc::strong_count(&row.rc) as u64
                })
                .on_initial(
                    move |row| {
                        let _ = &initial_capture;
                        std::rc::Rc::clone(&row.rc)
                    },
                    |change, _| {
                        let ReactionChange::Initial { current } = change else {
                            panic!("initial")
                        };
                        profile::Task::run(move |_| async move {
                            drop(current);
                        })
                    },
                );
            (entry, move |_, children| {
                let _ = &recipe_capture;
                parents.borrow_mut().push((id, children.site().clone()));
                let counts = std::rc::Rc::clone(&counts);
                let leaves = std::rc::Rc::clone(&leaves);
                let leaf_drops = std::rc::Rc::clone(&leaf_drops);
                children.keyed_with_children(
                    |row| &row.values,
                    |row| &mut row.values,
                    move |key, _| {
                        let key = *key;
                        counts.borrow_mut().push(("leaf make", id, key));
                        let leaves = std::rc::Rc::clone(&leaves);
                        let leaf_capture = LocalCapture(std::rc::Rc::clone(&leaf_drops));
                        (
                            crate::store::mount::local::EntryMount::new(Leaf(Rc::new(Cell::new(
                                0,
                            )))),
                            move |_, children| {
                                let _ = &leaf_capture;
                                leaves.borrow_mut().push((id, key, children.site().clone()));
                            },
                        )
                    },
                );
            })
        },
    )
    .build()
    .unwrap();
    driver.turn();
    driver.turn();
    assert_eq!(
        driver
            .run(store.read(|state| state.other.get(&3).unwrap().get()))
            .unwrap(),
        13
    );
    assert!(matches!(
        changes.borrow().as_slice(),
        [
            ReactionChange::Initial { current: 3 },
            ReactionChange::Changed {
                previous: 3,
                current: 13,
            },
        ]
    ));
    assert_eq!(parents.borrow().len(), 2);
    assert_eq!(leaves.borrow().len(), 4);
    let parent_site = parents.borrow()[0].1.clone();
    let leaf_site = leaves.borrow()[0].2.clone();
    let parent = driver.run(store.acquire(&parent_site)).unwrap().unwrap();
    let leaf = driver.run(store.acquire(&leaf_site)).unwrap().unwrap();
    drop(driver.run(parent.send(RowAction(Rc::new(())))).unwrap());
    drop(driver.run(leaf.send(LeafAction(Rc::new(8)))).unwrap());
    assert_eq!(
        *driver.run(leaf.read(|value| Rc::new(value.get()))).unwrap(),
        9
    );
    counts.borrow_mut().clear();
    drop(
        driver
            .run(store.send(RootAction::Reverse(Rc::new(()))))
            .unwrap(),
    );
    assert!(counts.borrow().is_empty());
    assert_eq!(
        changes.borrow().len(),
        2,
        "reorder does not repeat reactions"
    );
    assert_eq!(
        driver
            .run(store.read(|state| state.other.get(&3).unwrap().get()))
            .unwrap(),
        13
    );
    assert_eq!(
        drops.get(),
        0,
        "reorder retains all move-only callback owners"
    );
    assert_eq!(
        *driver.run(leaf.read(|value| Rc::new(value.get()))).unwrap(),
        9
    );
    drop(
        driver
            .run(store.send(RootAction::ReplaceDetails(Rc::new(()))))
            .unwrap(),
    );
    assert_eq!(*counts.borrow(), [("leaf make", 2, 1), ("leaf make", 1, 1)]);
    assert_eq!(parents.borrow().len(), 2);
    assert_eq!(leaves.borrow().len(), 6);
    driver
        .run(store.acquire(&leaf_site))
        .as_ref()
        .expect_err("replaced leaf acquisition must fail");
    assert!(driver.run(store.acquire(&parent_site)).unwrap().is_some());
    assert_eq!(
        drops.get(),
        2,
        "only the two replaced leaf recipe captures retire"
    );
    drop(store);
    driver.turn();
    assert_eq!(
        drops.get(),
        13,
        "factory, six parent callbacks and six leaf callbacks drop once"
    );
}

#[test]
fn local_keyed_make_and_configuration_shutdown_dispose_uninstalled_entries() {
    for close_at in ["make", "config"] {
        let driver = LocalDriver::default();
        let owner = Rc::new(RefCell::new(None::<profile::Store<State, RootAction>>));
        let trace = Rc::new(RefCell::new(Vec::new()));
        let drops = Rc::new(Cell::new(0));
        let make_owner = Rc::clone(&owner);
        let make_trace = Rc::clone(&trace);
        let make_drops = Rc::clone(&drops);
        let store = profile::Store::feature_builder(
            State {
                rows: (1..=2)
                    .map(|id| {
                        (
                            id,
                            Row {
                                values: [].into_iter().collect(),
                                rc: Rc::new(()),
                            },
                        )
                    })
                    .collect(),
                other: [].into_iter().collect(),
            },
            Root,
            runtime::Runtime::new(driver.clone()),
        )
        .keyed_with_children(
            |state| &state.rows,
            |state| &mut state.rows,
            move |id, _| {
                let id = *id;
                make_trace.borrow_mut().push(("make", id));
                if close_at == "make" && id == 1 {
                    let store = make_owner.borrow_mut().take();
                    drop(store);
                }
                let config_owner = Rc::clone(&make_owner);
                let config_trace = Rc::clone(&make_trace);
                let recipe_trace = Rc::clone(&make_trace);
                let initial_trace = Rc::clone(&make_trace);
                let config_capture = LocalCapture(Rc::clone(&make_drops));
                let initial_capture = LocalCapture(Rc::clone(&make_drops));
                let recipe_capture = LocalCapture(Rc::clone(&make_drops));
                let entry =
                    crate::store::mount::local::EntryMount::new(Parent(Rc::new(Cell::new(0))))
                        .configuration(move |_| {
                            let _ = &config_capture;
                            config_trace.borrow_mut().push(("config", id));
                            if close_at == "config" && id == 1 {
                                let store = config_owner.borrow_mut().take();
                                drop(store);
                            }
                            0
                        })
                        .on_initial(
                            move |_| {
                                let _ = &initial_capture;
                                initial_trace.borrow_mut().push(("initial", id));
                            },
                            |_, _| profile::Task::run(|_| async {}),
                        );
                (entry, move |_, _| {
                    let _ = &recipe_capture;
                    recipe_trace.borrow_mut().push(("recipe", id));
                })
            },
        )
        .build()
        .unwrap();
        let ready = store.ready();
        assert!(owner.borrow_mut().replace(store).is_none());
        assert_eq!(
            driver.run(ready).unwrap_err().reason(),
            crate::TaskAccessReason::StoreClosed
        );
        assert!(owner.borrow().is_none());
        let expected: &[(&str, u8)] = if close_at == "make" {
            &[("make", 1)]
        } else {
            &[("make", 1), ("make", 2), ("config", 1)]
        };
        assert_eq!(&*trace.borrow(), expected);
        assert_eq!(drops.get(), if close_at == "make" { 3 } else { 6 });
        assert!(driver.0.borrow().is_empty());
    }
}

/// Closes the real lifecycle from key operations while retaining ID-based equality.
struct LookupKey {
    id: u8,
    lifecycle: Arc<Lifecycle>,
    /// Zero is inert; one closes on clone and two closes on hash.
    close_on: Arc<AtomicU8>,
}
impl PartialEq for LookupKey {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}
impl Eq for LookupKey {}
impl std::hash::Hash for LookupKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        if self.close_on.load(Ordering::SeqCst) == 2 {
            self.lifecycle
                .close(crate::store::lifecycle::CloseState::Abortive);
        }
        self.id.hash(state);
    }
}
impl Clone for LookupKey {
    fn clone(&self) -> Self {
        if self.close_on.load(Ordering::SeqCst) == 1 {
            self.lifecycle
                .close(crate::store::lifecycle::CloseState::Abortive);
        }
        Self {
            id: self.id,
            lifecycle: Arc::clone(&self.lifecycle),
            close_on: Arc::clone(&self.close_on),
        }
    }
}
struct LookupState {
    rows: KeyedState<LookupKey, usize>,
    visible: Rc<Cell<bool>>,
}
struct LookupFeature(LocalCapture);
impl profile::Feature for LookupFeature {
    type State = usize;
    type Action = ();
    fn reduce(&mut self, _: &mut usize, (): &(), _: &mut profile::FeatureContext<'_, usize, ()>) {
        let _ = &self.0;
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "keep callback shutdown, hidden selection and exact disposal in one production reconciliation schedule"
)]
fn local_key_clone_lookup_and_hidden_path_stop_entry_installation() {
    for phase in ["clone", "hash", "hidden"] {
        let (lifecycle, mut scopes) = ScopeRegistry::for_test();
        let close_on = Arc::new(AtomicU8::new(0));
        let visible = Rc::new(Cell::new(true));
        let state = LookupState {
            rows: (1..=2)
                .map(|id| {
                    (
                        LookupKey {
                            id,
                            lifecycle: Arc::clone(&lifecycle),
                            close_on: Arc::clone(&close_on),
                        },
                        0,
                    )
                })
                .collect(),
            visible: Rc::clone(&visible),
        };
        let trace = Rc::new(RefCell::new(Vec::new()));
        let drops = Rc::new(Cell::new(0));
        let make_trace = Rc::clone(&trace);
        let make_drops = Rc::clone(&drops);
        close_on.store(u8::from(phase == "clone"), Ordering::SeqCst);
        let declaration = crate::store::mount::local::children::keyed(
            OptionalPath::new(
                |root: &LookupState| root.visible.get().then_some(&root.rows),
                |root| root.visible.get().then_some(&mut root.rows),
            ),
            |rows| rows,
            |rows| rows,
            0,
            move |key, _, _| {
                let id = key.id;
                make_trace.borrow_mut().push(("make", id));
                if id == 2 {
                    close_on.store(if phase == "hash" { 2 } else { 0 }, Ordering::SeqCst);
                    visible.set(phase != "hidden");
                }
                let config_trace = Rc::clone(&make_trace);
                let recipe_trace = Rc::clone(&make_trace);
                let initial_trace = Rc::clone(&make_trace);
                let config_capture = LocalCapture(Rc::clone(&make_drops));
                let recipe_capture = LocalCapture(Rc::clone(&make_drops));
                let initial_capture = LocalCapture(Rc::clone(&make_drops));
                let entry = crate::store::mount::local::EntryMount::new(LookupFeature(
                    LocalCapture(Rc::clone(&make_drops)),
                ))
                .configuration(move |_| {
                    let _ = &config_capture;
                    config_trace.borrow_mut().push(("config", id));
                    0
                })
                .on_initial(
                    move |_| {
                        let _ = &initial_capture;
                        initial_trace.borrow_mut().push(("initial", id));
                    },
                    |_, _| profile::Task::run(|_| async {}),
                );
                (
                    entry,
                    Some(
                        move |_: &usize,
                              _: &mut crate::store::mount::local::children::MountChildren<
                            '_,
                            LookupState,
                            (),
                            usize,
                            (),
                        >| {
                            let _ = &recipe_capture;
                            recipe_trace.borrow_mut().push(("recipe", id));
                        },
                    ),
                )
            },
        );
        let (sender, _receiver) =
            crate::store::mailbox::bounded(std::num::NonZeroUsize::new(1).unwrap());
        #[expect(
            clippy::arc_with_non_send_sync,
            reason = "Local mount binding requires Arc ingress but remains on its owning thread"
        )]
        let ingress = Arc::new(crate::store::ingress::CommandIngress::new(
            sender,
            crate::effect::lineage::ControlWake::new(),
        ));
        declaration.bind(&ingress, &lifecycle);
        let mut mounts: Vec<Box<dyn MountDeclaration<LookupState, (), crate::store::Local>>> =
            vec![Box::new(declaration)];
        let mut nodes = Vec::new();
        crate::store::mount::nested::reconcile_roster::<_, (), crate::store::Local>(
            &mut mounts,
            true,
            &state,
            &mut scopes,
            &mut ScopeTransitionBatch::default(),
            &mut nodes,
            #[cfg(feature = "diagnostics")]
            None,
        );
        assert!(nodes.is_empty(), "{phase}");
        assert_eq!(lifecycle.is_closed(), phase != "hidden");
        drop((mounts, nodes));
        let expected: &[(&str, u8)] = if phase == "clone" {
            &[]
        } else {
            &[("make", 1), ("make", 2)]
        };
        assert_eq!(&*trace.borrow(), expected, "{phase}");
        assert_eq!(drops.get(), if phase == "clone" { 0 } else { 8 }, "{phase}");
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Selects whether a later parent read succeeds, closes the lifecycle or hides state.
enum ParentLookup {
    Live,
    Close,
    Hidden,
}
struct NestedLookupRow {
    child: Option<usize>,
    lookup: Cell<ParentLookup>,
}
struct NestedLookupState {
    row: NestedLookupRow,
    lifecycle: Arc<Lifecycle>,
}
/// Applies the row's armed lookup action before returning its actual projection.
fn nested_parent(state: &NestedLookupState) -> Option<&NestedLookupRow> {
    match state.row.lookup.get() {
        ParentLookup::Live => Some(&state.row),
        ParentLookup::Close => {
            state
                .lifecycle
                .close(crate::store::lifecycle::CloseState::Abortive);
            Some(&state.row)
        }
        ParentLookup::Hidden => None,
    }
}
struct NestedLookupFeature(LocalCapture);
impl profile::Feature for NestedLookupFeature {
    type State = NestedLookupRow;
    type Action = ();
    fn reduce(
        &mut self,
        _: &mut NestedLookupRow,
        (): &(),
        _: &mut profile::FeatureContext<'_, NestedLookupRow, ()>,
    ) {
        let _ = &self.0;
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "keep optional/removable controls, parent invalidation and exact disposal in one reconciliation schedule"
)]
fn local_nested_parent_lookup_stops_optional_and_removable_child_callbacks() {
    for (removable, lookup) in [false, true].into_iter().flat_map(|removable| {
        [
            ParentLookup::Live,
            ParentLookup::Close,
            ParentLookup::Hidden,
        ]
        .map(|lookup| (removable, lookup))
    }) {
        let (lifecycle, mut scopes) = ScopeRegistry::for_test();
        let state = NestedLookupState {
            row: NestedLookupRow {
                child: Some(1),
                lookup: Cell::new(ParentLookup::Live),
            },
            lifecycle: Arc::clone(&lifecycle),
        };
        let trace = Rc::new(RefCell::new(Vec::new()));
        let drops = Rc::new(Cell::new(0));
        let parent_drops = Rc::clone(&drops);
        let recipe_drops = Rc::clone(&drops);
        let recipe_trace = Rc::clone(&trace);
        let (owner, _) = crate::store::mount::local::children::attach::<_, (), _, _>(
            profile::OptionalMount::new(
                nested_parent,
                |state: &mut NestedLookupState| Some(&mut state.row),
                |_| {
                    Some(MountIdentity {
                        key: (),
                        replacement: 0,
                        configuration: 0,
                    })
                },
                move |_| NestedLookupFeature(LocalCapture(Rc::clone(&parent_drops))),
            ),
            move |state, children| {
                let identity_trace = Rc::clone(&recipe_trace);
                let make_trace = Rc::clone(&recipe_trace);
                let make_drops = Rc::clone(&recipe_drops);
                let identity_capture = LocalCapture(Rc::clone(&recipe_drops));
                let make_capture = LocalCapture(Rc::clone(&recipe_drops));
                let identity = move |_: &NestedLookupRow| {
                    let _ = &identity_capture;
                    identity_trace.borrow_mut().push("identity");
                    Some(MountIdentity {
                        key: (),
                        replacement: 0,
                        configuration: 0,
                    })
                };
                let make = move |_: &usize| {
                    let _ = &make_capture;
                    make_trace.borrow_mut().push("make");
                    LookupFeature(LocalCapture(Rc::clone(&make_drops)))
                };
                if removable {
                    children.optional_removable(
                        |row| &row.child,
                        |row| &mut row.child,
                        identity,
                        make,
                    );
                } else {
                    children.optional(profile::OptionalMount::new(
                        |row: &NestedLookupRow| row.child.as_ref(),
                        |row| row.child.as_mut(),
                        identity,
                        make,
                    ));
                }
                recipe_trace.borrow_mut().push("declared");
                state.lookup.set(lookup);
            },
        );
        let (sender, _receiver) =
            crate::store::mailbox::bounded(std::num::NonZeroUsize::new(1).unwrap());
        #[expect(
            clippy::arc_with_non_send_sync,
            reason = "Local mount binding requires Arc ingress but remains on its owning thread"
        )]
        let ingress = Arc::new(crate::store::ingress::CommandIngress::new(
            sender,
            crate::effect::lineage::ControlWake::new(),
        ));
        owner.bind(&ingress, &lifecycle);
        let mut mounts = vec![owner];
        let mut nodes = Vec::new();
        crate::store::mount::nested::reconcile_roster::<_, (), crate::store::Local>(
            &mut mounts,
            true,
            &state,
            &mut scopes,
            &mut ScopeTransitionBatch::default(),
            &mut nodes,
            #[cfg(feature = "diagnostics")]
            None,
        );
        let live = lookup == ParentLookup::Live;
        assert_eq!(mounts.len(), 2);
        assert_eq!(mounts[1].installed_scope().is_some(), live);
        assert_eq!(nodes.len(), 1 + usize::from(live));
        assert_eq!(lifecycle.is_closed(), lookup == ParentLookup::Close);
        let expected: &[&str] = if live {
            &["declared", "identity", "make"]
        } else {
            &["declared"]
        };
        assert_eq!(&*trace.borrow(), expected, "{removable} {lookup:?}");
        drop((mounts, nodes));
        assert_eq!(drops.get(), 3 + usize::from(live), "{removable} {lookup:?}");
    }
}

#[test]
fn local_parent_lookup_closure_prevents_descendant_recipe() {
    for lookup in [ParentLookup::Live, ParentLookup::Close] {
        let (lifecycle, mut scopes) = ScopeRegistry::for_test();
        let state = NestedLookupState {
            row: NestedLookupRow {
                child: Some(1),
                lookup: Cell::new(ParentLookup::Live),
            },
            lifecycle: Arc::clone(&lifecycle),
        };
        let trace = Rc::new(RefCell::new(Vec::new()));
        let drops = Rc::new(Cell::new(0));
        let parent_trace = Rc::clone(&trace);
        let parent_drops = Rc::clone(&drops);
        let recipe_trace = Rc::clone(&trace);
        let child_drops = Rc::clone(&drops);
        let recipe_capture = LocalCapture(Rc::clone(&drops));
        let (owner, _) = crate::store::mount::local::children::attach::<_, (), _, _>(
            profile::OptionalMount::new(
                nested_parent,
                |state: &mut NestedLookupState| Some(&mut state.row),
                |_| {
                    Some(MountIdentity {
                        key: (),
                        replacement: 0,
                        configuration: 0,
                    })
                },
                move |state: &NestedLookupRow| {
                    parent_trace.borrow_mut().push("parent");
                    // The next structural lookup belongs to recipe discovery.
                    state.lookup.set(lookup);
                    NestedLookupFeature(LocalCapture(Rc::clone(&parent_drops)))
                },
            ),
            move |_, children| {
                let _ = &recipe_capture;
                recipe_trace.borrow_mut().push("recipe");
                let trace = Rc::clone(&recipe_trace);
                let drops = Rc::clone(&child_drops);
                children.optional(profile::OptionalMount::new(
                    |row: &NestedLookupRow| row.child.as_ref(),
                    |row| row.child.as_mut(),
                    |_| {
                        Some(MountIdentity {
                            key: (),
                            replacement: 0,
                            configuration: 0,
                        })
                    },
                    move |_| {
                        trace.borrow_mut().push("child");
                        LookupFeature(LocalCapture(Rc::clone(&drops)))
                    },
                ));
            },
        );
        let (sender, _receiver) =
            crate::store::mailbox::bounded(std::num::NonZeroUsize::new(1).unwrap());
        #[expect(
            clippy::arc_with_non_send_sync,
            reason = "Local mount binding requires Arc ingress but remains on its owning thread"
        )]
        let ingress = Arc::new(crate::store::ingress::CommandIngress::new(
            sender,
            crate::effect::lineage::ControlWake::new(),
        ));
        owner.bind(&ingress, &lifecycle);
        let mut mounts = vec![owner];
        let mut nodes = Vec::new();
        crate::store::mount::nested::reconcile_roster::<_, (), crate::store::Local>(
            &mut mounts,
            true,
            &state,
            &mut scopes,
            &mut ScopeTransitionBatch::default(),
            &mut nodes,
            #[cfg(feature = "diagnostics")]
            None,
        );
        let live = lookup == ParentLookup::Live;
        assert_eq!(mounts.len(), 1 + usize::from(live));
        assert_eq!(nodes.len(), 1 + usize::from(live));
        assert_eq!(lifecycle.is_closed(), !live);
        let expected: &[&str] = if live {
            &["parent", "recipe", "child"]
        } else {
            &["parent"]
        };
        assert_eq!(&*trace.borrow(), expected, "{lookup:?}");
        assert_eq!(drops.get(), 0);
        drop((mounts, nodes));
        assert_eq!(drops.get(), 2 + usize::from(live), "{lookup:?}");
    }
}
