use super::{Events, ordered, poll, support};
use rca_core::diagnostics::{CancellationCause, LifecycleEvent, LifecycleKind};
use rca_core::{
    EntryMount, Feature, FeatureContext, KeyedState, MountChildren, MountIdentity, MountSite,
    OptionalMount, RootMount, RuntimeFuture, Store, TaskAccessReason, TaskContext, TaskProgress,
};
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;

/// Separates child configuration from root configuration and sibling identity.
struct State {
    left: Option<Cell<usize>>,
    right: Option<Cell<usize>>,
    /// Refreshes only the left child's authority, not its Feature.
    leaf_marker: u64,
    /// Refreshes root authority and both child contexts.
    root_marker: u64,
    selections: Arc<AtomicUsize>,
}

enum Action {
    LeafMarker(u64),
    RootMarker(u64),
    RemoveBoth,
    Noop,
}

struct Root;

impl Feature for Root {
    type State = State;
    type Action = Action;

    fn update(
        &mut self,
        state: &mut State,
        action: &Action,
        _: &mut FeatureContext<'_, State, Action>,
    ) {
        match action {
            Action::LeafMarker(marker) => state.leaf_marker = *marker,
            Action::RootMarker(marker) => state.root_marker = *marker,
            Action::RemoveBoth => {
                state.left = None;
                state.right = None;
            }
            Action::Noop => {}
        }
    }
}

#[derive(Debug)]
enum ChildAction {
    BumpCache,
    ReportCache,
}

/// Keeps a reducer cache apart from State to expose refresh versus replacement.
struct Child {
    cache: Cell<usize>,
    drops: Arc<AtomicUsize>,
}

impl Drop for Child {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

impl Feature for Child {
    type State = Cell<usize>;
    type Action = ChildAction;

    fn update(
        &mut self,
        state: &mut Cell<usize>,
        action: &ChildAction,
        _: &mut FeatureContext<'_, Cell<usize>, ChildAction>,
    ) {
        match action {
            ChildAction::BumpCache => self.cache.set(self.cache.get() + 1),
            ChildAction::ReportCache => state.set(self.cache.get()),
        }
    }
}

type Site = MountSite<State, Cell<usize>, ChildAction, Action>;

/// Holds the live actor, two independent sites and their lifecycle counters.
struct Fixture {
    /// Keeps the manual runtime alive while the actor and Store run.
    _runtime: support::ManualRuntime,
    store: Store<State, Action>,
    actor: RuntimeFuture,
    events: Arc<Mutex<Events>>,
    selections: Arc<AtomicUsize>,
    constructions: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
    /// Distinct weak declarations share equal initial State values.
    left: Site,
    right: Site,
}

/// Installs equal-valued siblings with distinct declarations and one event sink.
fn fixture() -> Fixture {
    let runtime = support::ManualRuntime::new();
    let events = Arc::new(Mutex::new(Events::default()));
    let sink = Arc::clone(&events);
    let selections = Arc::new(AtomicUsize::new(0));
    let constructions = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let left_makes = Arc::clone(&constructions);
    let left_drops = Arc::clone(&drops);
    let left = OptionalMount::new(
        |state: &State| {
            state.selections.fetch_add(1, Ordering::SeqCst);
            state.left.as_ref()
        },
        |state: &mut State| {
            state.selections.fetch_add(1, Ordering::SeqCst);
            state.left.as_mut()
        },
        |state: &State| {
            state.left.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: state.leaf_marker,
            })
        },
        move |_| {
            left_makes.fetch_add(1, Ordering::SeqCst);
            Child {
                cache: Cell::new(0),
                drops: Arc::clone(&left_drops),
            }
        },
    );
    let right_makes = Arc::clone(&constructions);
    let right_drops = Arc::clone(&drops);
    let right = OptionalMount::new(
        |state: &State| {
            state.selections.fetch_add(1, Ordering::SeqCst);
            state.right.as_ref()
        },
        |state: &mut State| {
            state.selections.fetch_add(1, Ordering::SeqCst);
            state.right.as_mut()
        },
        |state: &State| {
            state.right.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| {
            right_makes.fetch_add(1, Ordering::SeqCst);
            Child {
                cache: Cell::new(0),
                drops: Arc::clone(&right_drops),
            }
        },
    );
    let (builder, left) = RootMount::new(Root)
        .configuration(|state: &State| state.root_marker)
        .builder(
            State {
                left: Some(Cell::new(4)),
                right: Some(Cell::new(4)),
                leaf_marker: 41,
                root_marker: 700,
                selections: Arc::clone(&selections),
            },
            runtime.runtime(),
        )
        .mount_with_site(left);
    let (builder, right) = builder.mount_with_site(right);
    let store = builder
        .with_lifecycle_diagnostics(move |event| sink.lock().unwrap().record(event))
        .build()
        .unwrap();
    let mut actor = runtime.take_only_queued_task();
    assert!(poll(actor.as_mut()).is_pending());
    Fixture {
        _runtime: runtime,
        store,
        actor,
        events,
        selections,
        constructions,
        drops,
        left,
        right,
    }
}

/// Drains only events published since the previous transition.
fn take(events: &Mutex<Events>) -> Vec<LifecycleEvent> {
    let (events, count) = events.lock().unwrap().take();
    events[..count].iter().map(|event| event.unwrap()).collect()
}

struct KeyedRoot;
impl Feature for KeyedRoot {
    type State = KeyedState<u8, Cell<usize>>;
    type Action = ();

    fn update(
        &mut self,
        state: &mut Self::State,
        (): &(),
        _: &mut FeatureContext<'_, Self::State, ()>,
    ) {
        assert_eq!(state.remove(&1).unwrap().get(), 4);
        state.insert(3, Cell::new(8));
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one keyed replacement schedule checks descendant retirement before construction and retained sibling authority"
)]
fn keyed_pruning_reports_the_departing_subtree_before_constructing_its_successor() {
    type Rows = KeyedState<u8, Cell<usize>>;
    let runtime = support::ManualRuntime::new();
    let events = Arc::new(Mutex::new(Events::default()));
    let sink = Arc::clone(&events);
    let drops = Arc::new(AtomicUsize::new(0));
    let made_drops = Arc::clone(&drops);
    let constructions = Arc::new(AtomicUsize::new(0));
    let made = Arc::clone(&constructions);
    let sites = Arc::new(Mutex::new(Vec::new()));
    let saved_sites = Arc::clone(&sites);
    let store = Store::builder(
        [(1, Cell::new(4)), (2, Cell::new(6))]
            .into_iter()
            .collect::<Rows>(),
        KeyedRoot,
        runtime.runtime(),
    )
    .keyed_with_children(
        |rows| rows,
        |rows| rows,
        move |key, _| {
            let key = *key;
            if key == 3 {
                assert_eq!(made_drops.load(Ordering::SeqCst), 2);
            }
            made.fetch_add(1, Ordering::SeqCst);
            let drops = Arc::clone(&made_drops);
            let sites = Arc::clone(&saved_sites);
            (
                EntryMount::new(Child {
                    cache: Cell::new(0),
                    drops: Arc::clone(&made_drops),
                }),
                move |_: &Cell<usize>, children: &mut MountChildren<'_, Rows, (), Cell<usize>, ChildAction>| {
                    sites.lock().unwrap().push((key, children.site().clone()));
                    let drops = Arc::clone(&drops);
                    children.optional(OptionalMount::new(
                        |state| Some(state),
                        |state| Some(state),
                        |_| Some(MountIdentity { key: (), replacement: 0, configuration: 0 }),
                        move |_| Child { cache: Cell::new(0), drops: Arc::clone(&drops) },
                    ));
                },
            )
        },
    )
    .with_lifecycle_diagnostics(move |event| sink.lock().unwrap().record(event))
    .build()
    .unwrap();
    let mut actor = runtime.take_only_queued_task();
    assert!(poll(actor.as_mut()).is_pending());
    let installed = take(&events);
    assert_eq!(installed.len(), 5);
    let lifetimes: Vec<_> = installed
        .iter()
        .map(|event| match event.kind() {
            LifecycleKind::Installed { lifetime, .. } => lifetime,
            other => panic!("unexpected startup event: {other:?}"),
        })
        .collect();
    assert_eq!(lifetimes[0], 0);
    assert_eq!(constructions.load(Ordering::SeqCst), 2);
    let original = ordered(&mut actor, store.acquire(&sites.lock().unwrap()[0].1))
        .unwrap()
        .unwrap();
    let sibling = ordered(&mut actor, store.acquire(&sites.lock().unwrap()[1].1))
        .unwrap()
        .unwrap();
    let prior = ordered(&mut actor, sibling.update(|state| state.get()))
        .unwrap()
        .revision();
    assert!(take(&events).is_empty());
    drop(ordered(&mut actor, store.send(())).unwrap());
    let changed = take(&events);
    let retired: Vec<_> = changed
        .iter()
        .filter_map(|event| match event.kind() {
            LifecycleKind::Retired { lifetime } => Some(lifetime),
            _ => None,
        })
        .collect();
    assert_eq!(retired.len(), 2);
    assert!(retired.contains(&lifetimes[1]));
    assert!(retired.contains(&lifetimes[2]));
    assert!(
        changed[..2]
            .iter()
            .all(|event| matches!(event.kind(), LifecycleKind::Retired { .. }))
    );
    assert!(
        changed
            .iter()
            .all(|event| event.last_committed_version() == prior)
    );
    let successor: Vec<_> = changed
        .iter()
        .filter_map(|event| match event.kind() {
            LifecycleKind::Installed { lifetime, .. } => Some(lifetime),
            _ => None,
        })
        .collect();
    assert_eq!(successor.len(), 2);
    assert!(
        successor
            .iter()
            .all(|lifetime| !lifetimes.contains(lifetime))
    );
    assert_eq!(constructions.load(Ordering::SeqCst), 3);
    assert_eq!(drops.load(Ordering::SeqCst), 2);
    assert_eq!(ordered(&mut actor, sibling.read(Cell::get)).unwrap(), 6);
    let mut stale = std::pin::pin!(original.read::<(), _>(|_| panic!("retired callback")));
    let error = match poll(stale.as_mut()) {
        Poll::Ready(result) => result.unwrap_err(),
        Poll::Pending => {
            assert!(poll(actor.as_mut()).is_pending());
            let Poll::Ready(result) = poll(stale.as_mut()) else {
                panic!("retired read resolves")
            };
            result.unwrap_err()
        }
    };
    assert_eq!(
        (error.reason(), error.progress()),
        (TaskAccessReason::ScopeEnded, TaskProgress::NotStarted)
    );
    assert!(take(&events).is_empty());
    drop(store);
    assert!(poll(actor.as_mut()).is_ready());
    assert_eq!(drops.load(Ordering::SeqCst), 6);
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one partial-projection recovery schedule distinguishes uninstalled ownership from a retired lifetime"
)]
fn keyed_entry_hidden_before_installation_is_disposed_before_reconstruction() {
    type Rows = KeyedState<u8, Cell<usize>>;
    struct Projection {
        rows: Option<Rows>,
        visible: Arc<AtomicBool>,
    }
    struct Restore;
    impl Feature for Restore {
        type State = Projection;
        type Action = bool;
        fn update(
            &mut self,
            state: &mut Projection,
            action: &bool,
            _: &mut FeatureContext<'_, Projection, bool>,
        ) {
            state.visible.store(*action, Ordering::SeqCst);
        }
    }
    struct Panel(Arc<AtomicUsize>);
    impl Feature for Panel {
        type State = Rows;
        type Action = ();
        fn update(&mut self, _: &mut Rows, (): &(), _: &mut FeatureContext<'_, Rows, ()>) {
            panic!("projection recovery must not reduce the parent");
        }
    }
    impl Drop for Panel {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    let runtime = support::ManualRuntime::new();
    let events = Arc::new(Mutex::new(Events::default()));
    let sink = Arc::clone(&events);
    let visible = Arc::new(AtomicBool::new(true));
    let constructions = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let parent_drops = Arc::new(AtomicUsize::new(0));
    let made_parent_drops = Arc::clone(&parent_drops);
    let panel = OptionalMount::new(
        |state: &Projection| {
            if state.visible.load(Ordering::SeqCst) {
                state.rows.as_ref()
            } else {
                None
            }
        },
        |state: &mut Projection| {
            if state.visible.load(Ordering::SeqCst) {
                state.rows.as_mut()
            } else {
                None
            }
        },
        |state: &Projection| {
            state.rows.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| Panel(Arc::clone(&made_parent_drops)),
    );
    let made = Arc::clone(&constructions);
    let made_drops = Arc::clone(&drops);
    let made_visible = Arc::clone(&visible);
    let construction_events = Arc::clone(&events);
    let (builder, parent_site) = Store::builder(
        Projection {
            rows: Some([(1, Cell::new(7))].into_iter().collect()),
            visible: Arc::clone(&visible),
        },
        Restore,
        runtime.runtime(),
    )
    .mount_with_children(panel, move |_, children| {
        let made = Arc::clone(&made);
        let drops = Arc::clone(&made_drops);
        let visible = Arc::clone(&made_visible);
        let events = Arc::clone(&construction_events);
        children.keyed(
            |rows| rows,
            |rows| rows,
            move |_, row| {
                assert_eq!(row.get(), 7);
                let construction = made.fetch_add(1, Ordering::SeqCst);
                if construction == 0 {
                    // The collection has selected its rows, but the discovered
                    // entry has not yet selected its composed parent path.
                    visible.store(false, Ordering::SeqCst);
                } else {
                    assert_eq!(construction, 1);
                    assert_eq!(drops.load(Ordering::SeqCst), 1);
                    let events = events.lock().unwrap();
                    assert_eq!(events.length, 2);
                    assert!(events.values[..2].iter().all(|event| matches!(
                        event.unwrap().kind(),
                        LifecycleKind::Installed { .. }
                    )));
                }
                EntryMount::new(Child {
                    cache: Cell::new(0),
                    drops: Arc::clone(&drops),
                })
            },
        );
    });
    let store = builder
        .with_lifecycle_diagnostics(move |event| sink.lock().unwrap().record(event))
        .build()
        .unwrap();
    let mut actor = runtime.take_only_queued_task();
    assert!(poll(actor.as_mut()).is_pending());
    assert!(!visible.load(Ordering::SeqCst));
    assert_eq!(constructions.load(Ordering::SeqCst), 1);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    let startup = events.lock().unwrap();
    assert_eq!(startup.length, 2);
    let LifecycleKind::Installed {
        lifetime: parent_lifetime,
        parent: Some(0),
        configuration: Some(0),
    } = startup.values[1].unwrap().kind()
    else {
        panic!("only the parent, not the hidden row, was installed");
    };
    let initial_events = [startup.values[0], startup.values[1]];
    drop(startup);

    drop(ordered(&mut actor, store.send(false)).unwrap());
    assert_eq!(
        ordered(
            &mut actor,
            store.read(|state| (
                state.visible.load(Ordering::SeqCst),
                state.rows.as_ref().unwrap().get(&1).unwrap().get(),
            ))
        )
        .unwrap(),
        (false, 7)
    );
    assert_eq!(constructions.load(Ordering::SeqCst), 1);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    assert_eq!(parent_drops.load(Ordering::SeqCst), 0);
    let hidden = events.lock().unwrap();
    assert_eq!(hidden.length, 2);
    assert_eq!(&hidden.values[..2], &initial_events);
    drop(hidden);

    drop(ordered(&mut actor, store.send(true)).unwrap());
    let installed = take(&events);
    assert_eq!(installed.len(), 3);
    assert!(
        installed
            .iter()
            .all(|event| matches!(event.kind(), LifecycleKind::Installed { .. }))
    );
    assert!(matches!(
        installed[2].kind(),
        LifecycleKind::Installed { lifetime, parent: Some(parent), configuration: Some(0) }
            if parent == parent_lifetime && lifetime != parent_lifetime
    ));
    assert_eq!(constructions.load(Ordering::SeqCst), 2);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(parent_drops.load(Ordering::SeqCst), 0);
    let parent = ordered(&mut actor, store.acquire(&parent_site))
        .unwrap()
        .unwrap();
    assert_eq!(
        ordered(&mut actor, parent.read(|rows| rows.get(&1).unwrap().get())).unwrap(),
        7
    );
    assert!(take(&events).is_empty());
    drop(store);
    assert!(poll(actor.as_mut()).is_ready());
    assert_eq!(drops.load(Ordering::SeqCst), 2);
    assert_eq!(parent_drops.load(Ordering::SeqCst), 1);
}

/// Extracts distinct sibling lifetimes from the exact startup event order.
fn installed_lifetimes(events: &[LifecycleEvent]) -> (u64, u64) {
    assert_eq!(events.len(), 3);
    assert_eq!(
        events[0].kind(),
        LifecycleKind::Installed {
            lifetime: 0,
            parent: None,
            configuration: Some(0),
        }
    );
    let LifecycleKind::Installed {
        lifetime: left,
        parent: Some(0),
        configuration: Some(0),
    } = events[1].kind()
    else {
        panic!("first declared child must be installed under root");
    };
    let LifecycleKind::Installed {
        lifetime: right,
        parent: Some(0),
        configuration: Some(0),
    } = events[2].kind()
    else {
        panic!("second declared child must be installed under root");
    };
    assert_ne!(
        left, right,
        "equal domain keys do not merge sibling lifetimes"
    );
    assert!(
        events
            .iter()
            .all(|event| event.last_committed_version() == 0)
    );
    (left, right)
}

/// Rejects an old context before any selector enters or emits another event.
fn stale(
    actor: &mut RuntimeFuture,
    child: &TaskContext<Cell<usize>, ChildAction>,
    selections: &AtomicUsize,
) {
    selections.store(0, Ordering::SeqCst);
    let mut request =
        Box::pin(child.read::<(), _>(|_| panic!("stale authority must reject before selection")));
    let result = match poll(request.as_mut()) {
        Poll::Ready(result) => result,
        Poll::Pending => {
            assert!(poll(actor.as_mut()).is_pending());
            let Poll::Ready(result) = poll(request.as_mut()) else {
                panic!("ordered stale read must resolve");
            };
            result
        }
    };
    let error = result.unwrap_err();
    assert_eq!(
        (error.reason(), error.progress()),
        (TaskAccessReason::ScopeEnded, TaskProgress::NotStarted)
    );
    assert_eq!(selections.load(Ordering::SeqCst), 0);
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one ordered authority-refresh schedule binds each stale endpoint to its actual event revision"
)]
fn leaf_and_root_refresh_report_actual_configuration_revisions_without_replacing_lifetimes() {
    let mut fixture = fixture();
    let (left_lifetime, _) = installed_lifetimes(&take(&fixture.events));
    let original = ordered(&mut fixture.actor, fixture.store.acquire(&fixture.left))
        .unwrap()
        .unwrap();
    let sibling = ordered(&mut fixture.actor, fixture.store.acquire(&fixture.right))
        .unwrap()
        .unwrap();
    drop(ordered(&mut fixture.actor, original.send(ChildAction::BumpCache)).unwrap());
    let prior = ordered(&mut fixture.actor, original.update(|state| state.get()))
        .unwrap()
        .revision();
    assert!(take(&fixture.events).is_empty());

    drop(
        ordered(
            &mut fixture.actor,
            fixture.store.send(Action::LeafMarker(9001)),
        )
        .unwrap(),
    );
    let changed = take(&fixture.events);
    assert_eq!(changed.len(), 1);
    assert_eq!(
        changed[0].kind(),
        LifecycleKind::ConfigurationChanged {
            lifetime: left_lifetime,
            previous: 0,
            current: 1
        }
    );
    assert_eq!(changed[0].last_committed_version(), prior);
    stale(&mut fixture.actor, &original, &fixture.selections);
    assert_eq!(
        ordered(&mut fixture.actor, sibling.read(Cell::get)).unwrap(),
        4
    );
    let refreshed = ordered(&mut fixture.actor, fixture.store.acquire(&fixture.left))
        .unwrap()
        .unwrap();
    drop(ordered(&mut fixture.actor, refreshed.send(ChildAction::ReportCache)).unwrap());
    assert_eq!(
        ordered(&mut fixture.actor, refreshed.read(Cell::get)).unwrap(),
        1
    );
    assert_eq!(fixture.constructions.load(Ordering::SeqCst), 2);
    assert_eq!(fixture.drops.load(Ordering::SeqCst), 0);
    let prior = ordered(&mut fixture.actor, refreshed.update(|state| state.get()))
        .unwrap()
        .revision();

    drop(
        ordered(
            &mut fixture.actor,
            fixture.store.send(Action::RootMarker(9900)),
        )
        .unwrap(),
    );
    let changed = take(&fixture.events);
    assert_eq!(changed.len(), 1);
    assert_eq!(
        changed[0].kind(),
        LifecycleKind::ConfigurationChanged {
            lifetime: 0,
            previous: 0,
            current: 1
        }
    );
    assert_eq!(changed[0].last_committed_version(), prior);
    stale(&mut fixture.actor, &refreshed, &fixture.selections);
    stale(&mut fixture.actor, &sibling, &fixture.selections);
    let current = ordered(&mut fixture.actor, fixture.store.acquire(&fixture.left))
        .unwrap()
        .unwrap();
    drop(ordered(&mut fixture.actor, current.send(ChildAction::ReportCache)).unwrap());
    assert_eq!(
        ordered(&mut fixture.actor, current.read(Cell::get)).unwrap(),
        1
    );
    let prior = ordered(&mut fixture.actor, current.update(|state| state.get()))
        .unwrap()
        .revision();

    // Returning to the original user marker advances actual authority again.
    drop(
        ordered(
            &mut fixture.actor,
            fixture.store.send(Action::RootMarker(700)),
        )
        .unwrap(),
    );
    let changed = take(&fixture.events);
    assert_eq!(changed.len(), 1);
    assert_eq!(
        changed[0].kind(),
        LifecycleKind::ConfigurationChanged {
            lifetime: 0,
            previous: 1,
            current: 2
        }
    );
    assert_eq!(changed[0].last_committed_version(), prior);
    stale(&mut fixture.actor, &current, &fixture.selections);
    stale(&mut fixture.actor, &original, &fixture.selections);
    drop(
        ordered(
            &mut fixture.actor,
            fixture.store.send(Action::RootMarker(700)),
        )
        .unwrap(),
    );
    assert!(
        take(&fixture.events).is_empty(),
        "unchanged marker is not another refresh"
    );
    assert_eq!(fixture.constructions.load(Ordering::SeqCst), 2);
    assert_eq!(fixture.drops.load(Ordering::SeqCst), 0);
    drop(fixture.store);
    assert!(poll(fixture.actor.as_mut()).is_ready());
    assert_eq!(fixture.drops.load(Ordering::SeqCst), 2);
}

#[test]
fn equal_key_sibling_retirement_preserves_declaration_order_and_precommit_stamp() {
    let mut fixture = fixture();
    let (left, right) = installed_lifetimes(&take(&fixture.events));
    let left_context = ordered(&mut fixture.actor, fixture.store.acquire(&fixture.left))
        .unwrap()
        .unwrap();
    let right_context = ordered(&mut fixture.actor, fixture.store.acquire(&fixture.right))
        .unwrap()
        .unwrap();
    let prior = ordered(&mut fixture.actor, left_context.update(|state| state.get()))
        .unwrap()
        .revision();
    assert!(take(&fixture.events).is_empty());
    drop(ordered(&mut fixture.actor, fixture.store.send(Action::RemoveBoth)).unwrap());
    let retired = take(&fixture.events);
    assert_eq!(
        retired.iter().map(LifecycleEvent::kind).collect::<Vec<_>>(),
        [
            LifecycleKind::Retired { lifetime: left },
            LifecycleKind::Retired { lifetime: right },
            LifecycleKind::CancellationRequested {
                cause: CancellationCause::ScopeRetirement,
                scope: Some(left)
            },
            LifecycleKind::CancellationRequested {
                cause: CancellationCause::ScopeRetirement,
                scope: Some(right)
            },
        ]
    );
    assert!(
        retired
            .iter()
            .all(|event| event.last_committed_version() == prior)
    );
    assert_eq!(fixture.drops.load(Ordering::SeqCst), 2);
    stale(&mut fixture.actor, &left_context, &fixture.selections);
    stale(&mut fixture.actor, &right_context, &fixture.selections);
    drop(ordered(&mut fixture.actor, fixture.store.send(Action::Noop)).unwrap());
    assert!(
        take(&fixture.events).is_empty(),
        "already absent siblings do not retire again"
    );
    drop(fixture.store);
    assert!(poll(fixture.actor.as_mut()).is_ready());
    assert_eq!(fixture.drops.load(Ordering::SeqCst), 2);
}
