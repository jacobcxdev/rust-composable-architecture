//! Parent refresh and nested collection retention share the actual actor path.
use super::*;

struct Pool {
    configuration: u64,
    value: usize,
    workers: CollectionState,
}

struct Pools {
    rows: KeyedState<u8, Pool>,
}

enum Action {
    Refresh(u64),
    Push,
    Pop,
}

struct Root;
impl Feature for Root {
    type State = Pools;
    type Action = Action;

    fn update(
        &mut self,
        state: &mut Pools,
        action: &Action,
        _: &mut FeatureContext<'_, Pools, Action>,
    ) {
        match action {
            Action::Refresh(configuration) => {
                let parent = state.rows.get_mut(&1).unwrap();
                parent.configuration = *configuration;
                parent.workers.rows.get_mut(&1).unwrap().value += 10;
                if *configuration == 1 {
                    parent.workers.rows.insert(2, row(2));
                } else {
                    parent.workers.rows.remove(&2);
                }
            }
            Action::Push => {
                state
                    .rows
                    .get_mut(&1)
                    .unwrap()
                    .workers
                    .rows
                    .insert(2, row(2));
                let mut parent = pool();
                parent.workers.rows.insert(2, row(2));
                state.rows.insert(2, parent);
            }
            Action::Pop => {
                state.rows.get_mut(&1).unwrap().workers.rows.remove(&2);
                state.rows.remove(&2);
            }
        }
    }
}

fn pool() -> Pool {
    Pool {
        configuration: 0,
        value: 0,
        workers: CollectionState {
            rows: [(1, row(1))].into_iter().collect(),
        },
    }
}

/// Keeps a parent-local counter and owner probe across configuration refreshes.
struct Parent {
    cache: usize,
    _capture: Capture,
    _owner: Arc<()>,
}

impl Feature for Parent {
    type State = Pool;
    type Action = ();

    fn update(&mut self, state: &mut Pool, (): &(), _: &mut FeatureContext<'_, Pool, ()>) {
        self.cache += 1;
        state.value = self.cache;
    }
}

type PoolSite = MountSite<Pools, Pool, (), Action>;
type WorkerSite = MountSite<Pools, EntryState, EntryAction, Action>;
/// Weak ownership probes: worker zero denotes the parent, other IDs denote children.
type Owners = Arc<Mutex<Vec<(u8, u8, Weak<()>)>>>;

/// Retains stale sites but only weak payload probes, so leaks remain observable.
struct Fixture {
    driver: Driver,
    store: Store<Pools, Action>,
    trace: Trace,
    parents: Arc<Mutex<Vec<(u8, PoolSite)>>>,
    workers: Arc<Mutex<Vec<(u8, u8, WorkerSite)>>>,
    owners: Owners,
}

impl Fixture {
    /// Finds the latest incarnation after a key has been removed and reinserted.
    fn worker_site(&self, parent: u8, worker: u8) -> WorkerSite {
        self.workers
            .lock()
            .iter()
            .rev()
            .find(|(pool, row, _)| (*pool, *row) == (parent, worker))
            .unwrap()
            .2
            .clone()
    }

    /// Counts one callback phase without deriving the expected lifecycle totals.
    fn count(&self, event: &'static str) -> usize {
        self.trace
            .lock()
            .iter()
            .filter(|(name, _)| *name == event)
            .count()
    }

    /// Reports surviving callback owners without extending their lifetimes.
    fn live_owners(&self) -> Vec<(u8, u8)> {
        self.owners
            .lock()
            .iter()
            .filter(|(_, _, owner)| owner.upgrade().is_some())
            .map(|(parent, worker, _)| (*parent, *worker))
            .collect()
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "one nested fixture retains real feature, configuration and recipe ownership evidence"
)]
/// Builds two collection levels with separate parent, worker and capture probes.
fn fixture() -> Fixture {
    let driver = Driver::default();
    let trace = Trace::default();
    let parents = Arc::new(Mutex::new(Vec::new()));
    let workers = Arc::new(Mutex::new(Vec::new()));
    let owners = Owners::default();
    let make_trace = Arc::clone(&trace);
    let make_parents = Arc::clone(&parents);
    let make_workers = Arc::clone(&workers);
    let make_owners = Arc::clone(&owners);
    let store = Store::builder(
        Pools {
            rows: [(1, pool())].into_iter().collect(),
        },
        Root,
        Runtime::new(driver.clone()),
    )
    .keyed_with_children(
        |state| &state.rows,
        |state| &mut state.rows,
        move |id, _| {
            let parent = *id;
            make_trace.lock().push(("pool make", parent));
            let owner = Arc::new(());
            make_owners.lock().push((parent, 0, Arc::downgrade(&owner)));
            let configuration_owner = Arc::clone(&owner);
            let recipe_owner = Arc::clone(&owner);
            let configuration_capture =
                Capture("pool config drop", parent, Arc::clone(&make_trace));
            let entry = EntryMount::new(Parent {
                cache: 0,
                _capture: Capture("pool drop", parent, Arc::clone(&make_trace)),
                _owner: owner,
            })
            .configuration(move |state| {
                let _ = (&configuration_owner, &configuration_capture);
                state.configuration
            });
            let recipe_capture = Capture("pool recipe drop", parent, Arc::clone(&make_trace));
            let parents = Arc::clone(&make_parents);
            let workers = Arc::clone(&make_workers);
            let owners = Arc::clone(&make_owners);
            let trace = Arc::clone(&make_trace);
            (entry, move |_, children| {
                let _ = (&recipe_owner, &recipe_capture);
                trace.lock().push(("pool recipe", parent));
                parents.lock().push((parent, children.site().clone()));
                let workers = Arc::clone(&workers);
                let owners = Arc::clone(&owners);
                let trace = Arc::clone(&trace);
                let make_owner = Arc::clone(&recipe_owner);
                let make_capture = Capture("worker factory drop", parent, Arc::clone(&trace));
                children.keyed_with_children(
                    |state| &state.workers.rows,
                    |state| &mut state.workers.rows,
                    move |id, _| {
                        let _ = (&make_owner, &make_capture);
                        let worker = *id;
                        let tag = parent * 10 + worker;
                        trace.lock().push(("worker make", tag));
                        let owner = Arc::new(());
                        owners.lock().push((parent, worker, Arc::downgrade(&owner)));
                        let configuration_owner = Arc::clone(&owner);
                        let configuration_capture =
                            Capture("worker config drop", tag, Arc::clone(&trace));
                        let initial_owner = Arc::clone(&owner);
                        let initial_capture =
                            Capture("worker initial drop", tag, Arc::clone(&trace));
                        let initial_trace = Arc::clone(&trace);
                        let entry = EntryMount::new(EntryFeature {
                            cache: Cell::new(0),
                            _capture: Capture("worker drop", tag, Arc::clone(&trace)),
                        })
                        .configuration(move |state| {
                            let _ = (&configuration_owner, &configuration_capture);
                            state.configuration
                        })
                        .on_initial(
                            |_| (),
                            move |_, _| {
                                let _ = (&initial_owner, &initial_capture);
                                initial_trace.lock().push(("worker initial", tag));
                                Task::run(|_| async {})
                            },
                        );
                        let recipe_capture = Capture("worker recipe drop", tag, Arc::clone(&trace));
                        let workers = Arc::clone(&workers);
                        let trace = Arc::clone(&trace);
                        (entry, move |_, children| {
                            let _ = (&owner, &recipe_capture);
                            trace.lock().push(("worker recipe", tag));
                            workers
                                .lock()
                                .push((parent, worker, children.site().clone()));
                        })
                    },
                );
            })
        },
    )
    .build()
    .unwrap();
    run(&driver, store.ready()).unwrap();
    Fixture {
        driver,
        store,
        trace,
        parents,
        workers,
        owners,
    }
}

/// Requires stale admission to fail before its deliberately panicking selector runs.
fn stale<S: 'static, A: Send + 'static>(driver: &Driver, context: &crate::TaskContext<S, A>) {
    let error = run(
        driver,
        context.read(|_| -> () { panic!("stale context must reject before selector entry") }),
    )
    .unwrap_err();
    assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
    assert_eq!(error.progress(), crate::TaskProgress::NotStarted);
}

#[test]
fn parent_refresh_preserves_same_transaction_nested_edits_and_insertions() {
    let f = fixture();
    let parent_site = f.parents.lock()[0].1.clone();
    let worker_site = f.worker_site(1, 1);
    let parent = run(&f.driver, f.store.acquire(&parent_site))
        .unwrap()
        .unwrap();
    let worker = run(&f.driver, f.store.acquire(&worker_site))
        .unwrap()
        .unwrap();
    drop(run(&f.driver, parent.send(())).unwrap());
    drop(run(&f.driver, worker.send(EntryAction::Touch)).unwrap());

    drop(run(&f.driver, f.store.send(Action::Refresh(1))).unwrap());
    stale(&f.driver, &worker);
    stale(&f.driver, &parent);
    let refreshed_parent = run(&f.driver, f.store.acquire(&parent_site))
        .unwrap()
        .unwrap();
    let refreshed_worker = run(&f.driver, f.store.acquire(&worker_site))
        .unwrap()
        .unwrap();
    assert_eq!(
        run(&f.driver, refreshed_worker.read(|state| state.value)).unwrap(),
        11
    );
    let inserted_site = f.worker_site(1, 2);
    let inserted = run(&f.driver, f.store.acquire(&inserted_site))
        .unwrap()
        .unwrap();
    assert_eq!(
        run(&f.driver, inserted.read(|state| state.value)).unwrap(),
        2
    );
    drop(run(&f.driver, refreshed_parent.send(())).unwrap());
    drop(run(&f.driver, refreshed_worker.send(EntryAction::Touch)).unwrap());
    assert_eq!(
        run(&f.driver, refreshed_parent.read(|state| state.value)).unwrap(),
        2
    );
    assert_eq!(
        run(&f.driver, refreshed_worker.read(|state| state.value)).unwrap(),
        2
    );

    drop(run(&f.driver, f.store.send(Action::Refresh(0))).unwrap());
    stale(&f.driver, &worker);
    stale(&f.driver, &refreshed_worker);
    stale(&f.driver, &inserted);
    let current = run(&f.driver, f.store.acquire(&worker_site))
        .unwrap()
        .unwrap();
    assert_eq!(
        run(&f.driver, current.read(|state| state.value)).unwrap(),
        12
    );
    drop(run(&f.driver, current.send(EntryAction::Touch)).unwrap());
    assert_eq!(
        run(&f.driver, current.read(|state| state.value)).unwrap(),
        3
    );
    assert_eq!(f.count("pool make"), 1);
    assert_eq!(f.count("pool recipe"), 1);
    assert_eq!(f.count("worker make"), 2);
    assert_eq!(f.count("worker recipe"), 2);
    assert_eq!(f.count("worker initial"), 2);
    assert_eq!(f.count("worker drop"), 1);
    assert_eq!(f.live_owners(), [(1, 0), (1, 1)]);
    run(&f.driver, f.store.shutdown()).unwrap();
    f.driver.turn();
    assert!(f.live_owners().is_empty());
    assert_eq!(f.count("pool drop"), 1);
    assert_eq!(f.count("worker drop"), 2);
    assert!(f.driver.0.lock().is_empty());
}

#[test]
fn nested_collection_churn_releases_retired_owners_without_remounting_survivors() {
    let f = fixture();
    let parent_site = f.parents.lock()[0].1.clone();
    let worker_site = f.worker_site(1, 1);
    let parent = run(&f.driver, f.store.acquire(&parent_site))
        .unwrap()
        .unwrap();
    let worker = run(&f.driver, f.store.acquire(&worker_site))
        .unwrap()
        .unwrap();
    let mut retired = Vec::<crate::TaskContext<EntryState, EntryAction>>::new();
    let mut retired_sites = Vec::new();
    // Reuse the same IDs at both depths: old routes must never retarget a successor.
    for cycle in 1..=32 {
        drop(run(&f.driver, f.store.send(Action::Push)).unwrap());
        for old in &retired {
            stale(&f.driver, old);
        }
        let inner_site = f.worker_site(1, 2);
        let outer_site = f.worker_site(2, 1);
        let inner = run(&f.driver, f.store.acquire(&inner_site))
            .unwrap()
            .unwrap();
        let outer = run(&f.driver, f.store.acquire(&outer_site))
            .unwrap()
            .unwrap();
        drop(run(&f.driver, parent.send(())).unwrap());
        drop(run(&f.driver, worker.send(EntryAction::Touch)).unwrap());
        assert_eq!(
            run(&f.driver, parent.read(|state| state.value)).unwrap(),
            cycle
        );
        assert_eq!(
            run(&f.driver, worker.read(|state| state.value)).unwrap(),
            cycle
        );
        assert_eq!(f.count("pool make"), 1 + cycle);
        assert_eq!(f.count("pool recipe"), 1 + cycle);
        assert_eq!(f.count("worker make"), 1 + 3 * cycle);
        assert_eq!(f.count("worker recipe"), 1 + 3 * cycle);
        assert_eq!(f.count("worker initial"), 1 + 3 * cycle);

        drop(run(&f.driver, f.store.send(Action::Pop)).unwrap());
        retired.extend([inner, outer]);
        retired_sites.extend([inner_site, outer_site]);
        for old in &retired_sites {
            run(&f.driver, f.store.acquire(old))
                .expect_err("retired site cannot reacquire its successor");
        }
        assert_eq!(f.live_owners(), [(1, 0), (1, 1)]);
        for event in [
            "pool drop",
            "pool config drop",
            "pool recipe drop",
            "worker factory drop",
        ] {
            assert_eq!(f.count(event), cycle, "{event} after cycle {cycle}");
        }
        for event in [
            "worker drop",
            "worker config drop",
            "worker recipe drop",
            "worker initial drop",
        ] {
            assert_eq!(f.count(event), 3 * cycle, "{event} after cycle {cycle}");
        }
    }
    run(&f.driver, f.store.shutdown()).unwrap();
    f.driver.turn();
    assert!(f.live_owners().is_empty());
    for event in [
        "pool drop",
        "pool config drop",
        "pool recipe drop",
        "worker factory drop",
    ] {
        assert_eq!(f.count(event), 33);
    }
    for event in [
        "worker drop",
        "worker config drop",
        "worker recipe drop",
        "worker initial drop",
    ] {
        assert_eq!(f.count(event), 97);
    }
    assert_eq!(
        retired.len(),
        64,
        "retained authority owns no retired callbacks"
    );
    assert_eq!(
        retired_sites.len(),
        64,
        "retained sites own no retired callbacks"
    );
    assert!(f.driver.0.lock().is_empty());
}
