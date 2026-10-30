//! Public command phase costs, with identical driver overhead in each control.
//!
//! This host has one test, so ordinary, mapped and Local phases run sequentially.
//! Printed allocation deltas are observations; explicit assertions are the budgets.

#![cfg(feature = "allocation-measurement")]

use rca_core::{Feature, FeatureContext, MountIdentity, OptionalMount, Runtime, Store};
use std::sync::{Arc, Mutex};
use std::task::Poll;

#[cfg(feature = "local")]
#[path = "fold/local_driver.rs"]
mod local_driver;
#[path = "fold/support.rs"]
mod support;
use support::{Driver, poll};

#[global_allocator]
/// Counts allocations across the sequential phases of this single-test host.
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

/// Measures requested allocation counts and bytes for the supplied phase.
fn measure<T>(operation: impl FnOnce() -> T) -> (T, (u64, u64)) {
    let before = dhat::HeapStats::get();
    let result = operation();
    let after = dhat::HeapStats::get();
    (
        result,
        (
            after.total_blocks - before.total_blocks,
            after.total_bytes - before.total_bytes,
        ),
    )
}

/// Separates first-poll admission, one actor turn and reply completion.
fn phases<F: Future>(label: &str, driver: &Driver, future: F) -> F::Output {
    let bytes = std::mem::size_of_val(&future);
    let mut future = std::pin::pin!(future);
    let (admitted, admission) = measure(|| poll(future.as_mut()));
    assert!(admitted.is_pending());
    let ((), actor) = measure(|| driver.turn());
    let (completed, completion) = measure(|| poll(future.as_mut()));
    let Poll::Ready(value) = completed else {
        panic!("one actor turn must complete the measured command")
    };
    eprintln!(
        "command phase {label}: inline_future={bytes} admission={admission:?} actor_with_driver={actor:?} completion={completion:?}; costs=(blocks,bytes)"
    );
    value
}

/// Keeps the optional child unchanged when an ordinary root action is sent.
struct Parent;

impl Feature for Parent {
    type State = Option<usize>;
    type Action = ();

    fn update(
        &mut self,
        _: &mut Self::State,
        (): &(),
        _: &mut FeatureContext<'_, Self::State, ()>,
    ) {
    }
}

/// Adds the mapped input to the installed child value.
struct Child;

impl Feature for Child {
    type State = usize;
    type Action = usize;

    fn update(
        &mut self,
        state: &mut usize,
        action: &usize,
        _: &mut FeatureContext<'_, usize, usize>,
    ) {
        *state += action;
    }
}

/// Projects one optional child with a stable identity.
fn descriptor() -> OptionalMount<Option<usize>, Child, ()> {
    OptionalMount::new(
        Option::as_ref,
        Option::as_mut,
        |state: &Option<usize>| {
            state.map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        |_| Child,
    )
}

/// Checks recipe dispatch costs against ordinary root and mapped controls.
fn measured_recipe_dispatch_and_unchanged_ordinary_root_mapped_controls() {
    let profiler = dhat::Profiler::builder().testing().build();
    let driver = Driver::default();
    let declared = Arc::new(Mutex::new(None));
    let sites = Arc::clone(&declared);
    let (builder, ordinary_site) = Store::builder(Some(0), Parent, Runtime::new(driver.clone()))
        .mount_with_children(descriptor(), move |_, children| {
            *sites.lock().unwrap() = Some(children.command(|input: u8| usize::from(input)));
        });
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    let site = declared.lock().unwrap().take().unwrap();
    for _ in 0..2 {
        let (future, creation) = measure(|| store.acquire(&ordinary_site));
        eprintln!("ordinary acquisition creation={creation:?}");
        let ordinary = phases("ordinary acquisition", &driver, future)
            .unwrap()
            .unwrap();
        let (future, creation) = measure(|| ordinary.send(1));
        eprintln!("ordinary send creation={creation:?}");
        drop(phases("ordinary send", &driver, future).unwrap());
        let (future, creation) = measure(|| store.command_port(&site));
        eprintln!("recipe acquisition creation={creation:?}");
        let port = phases("recipe acquisition", &driver, future)
            .unwrap()
            .unwrap();
        let ((), cloned) = measure(|| drop(port.clone()));
        assert_eq!(cloned, (0, 0));
        let (future, creation) = measure(|| port.send(1));
        eprintln!("recipe send creation={creation:?}");
        drop(phases("recipe send", &driver, future).unwrap());
    }
    assert_eq!(driver.finish(store.read(|state| *state)).unwrap(), Some(4));
    drop(store);
    driver.turn();

    let (builder, _, site) = Store::builder(Some(0), Parent, Runtime::new(driver.clone()))
        .mount_with_command(descriptor(), |input: u8| usize::from(input));
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    for _ in 0..2 {
        let (future, creation) = measure(|| store.command_port(&site));
        eprintln!("root mapped acquisition creation={creation:?}");
        let port = phases("root mapped acquisition", &driver, future)
            .unwrap()
            .unwrap();
        let (future, creation) = measure(|| port.send(1));
        eprintln!("root mapped send creation={creation:?}");
        drop(phases("root mapped send", &driver, future).unwrap());
    }
    assert_eq!(driver.finish(store.read(|state| *state)).unwrap(), Some(2));
    drop(store);
    driver.turn();

    #[cfg(feature = "test-util")]
    {
        let gate = rca_core::testing::TransactionGate::new();
        let declared = Arc::new(Mutex::new(None));
        let sites = Arc::clone(&declared);
        let (builder, _) = Store::builder(Some(0), Parent, Runtime::new(driver.clone()))
            .transaction_gate(gate.clone())
            .mount_with_children(descriptor(), move |_, children| {
                *sites.lock().unwrap() = Some(children.command(|input: u8| usize::from(input)));
            });
        let store = builder.build().unwrap();
        driver.turn();
        let boundary = gate.boundary().unwrap();
        let receipt = gate.release_pending_with_receipt().unwrap();
        drop(driver.finish(receipt.finish()).unwrap());
        assert!(gate.acknowledge(boundary));
        driver.finish(store.ready()).unwrap();
        let site = declared.lock().unwrap().take().unwrap();
        let port = driver.finish(store.command_port(&site)).unwrap().unwrap();
        let mut pending = Box::pin(port.send(1));
        assert!(poll(pending.as_mut()).is_pending());
        driver.turn();
        for _ in 0..2 {
            let (future, creation) = measure(|| gate.command_port(&store, &site));
            eprintln!("held recipe acquisition creation={creation:?}");
            drop(
                phases("held recipe acquisition", &driver, future)
                    .unwrap()
                    .unwrap(),
            );
        }
        let boundary = gate.boundary().unwrap();
        let receipt = gate.release_pending_with_receipt().unwrap();
        drop(driver.finish(receipt.finish()).unwrap());
        assert!(gate.acknowledge(boundary));
        drop(driver.finish(pending).unwrap());
        assert_eq!(driver.finish(store.read(|state| *state)).unwrap(), Some(1));
        drop(store);
        driver.turn();
    };
    #[cfg(feature = "local")]
    local::exercise();
    collection_retention::exercise();
    drop(profiler);
}

/// Detects per-commit registration or capture leaks while the store remains alive.
mod collection_retention {
    use super::{Driver, Feature, FeatureContext, Runtime, Store};
    use rca_core::{EntryMount, KeyedState, MountSite};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    /// Tracks child, configuration and recipe captures separately.
    struct Counts {
        created: [AtomicUsize; 3],
        dropped: [AtomicUsize; 3],
    }

    /// Records creation and destruction for one of the three capture kinds.
    struct Capture(Arc<Counts>, usize);

    impl Capture {
        /// Records construction before transferring the counter into the capture.
        fn new(counts: &Arc<Counts>, kind: usize) -> Self {
            counts.created[kind].fetch_add(1, Ordering::Relaxed);
            Self(Arc::clone(counts), kind)
        }
    }

    impl Drop for Capture {
        fn drop(&mut self) {
            self.0.dropped[self.1].fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Keeps mutable child data separate from its refreshable authority value.
    struct Row {
        value: usize,
        configuration: u64,
    }
    /// Records root actions independently of child state and configuration changes.
    struct State {
        ticks: usize,
        rows: KeyedState<u8, Row>,
    }
    /// Refreshes all configurations only for a requested refresh action.
    struct Parent;

    impl Feature for Parent {
        type State = State;
        type Action = bool;

        fn update(
            &mut self,
            state: &mut State,
            action: &bool,
            _: &mut FeatureContext<'_, State, bool>,
        ) {
            state.ticks += 1;
            if *action {
                for (_, row) in state.rows.iter_mut() {
                    row.configuration += 1;
                }
            }
        }
    }

    /// Owns one counted lifetime and increments its row on each child action.
    struct Child(Capture);

    impl Feature for Child {
        type State = Row;
        type Action = ();

        fn update(&mut self, state: &mut Row, (): &(), _: &mut FeatureContext<'_, Row, ()>) {
            let _ = &self.0;
            state.value += 1;
        }
    }

    #[derive(Clone, Copy, Debug)]
    /// Selects which public operation exercises steady-state retained storage.
    enum Input {
        Parent,
        Child,
        Refresh,
    }

    #[expect(
        clippy::too_many_lines,
        reason = "keep live Store ownership and heap checkpoints in the same bounded workload"
    )]
    /// Checks every post-warmup input and destroys all captures at shutdown.
    pub(super) fn exercise() {
        for input in [Input::Parent, Input::Child, Input::Refresh] {
            let driver = Driver::default();
            let counts = Arc::new(Counts::default());
            let make_counts = Arc::clone(&counts);
            let site = Arc::new(Mutex::new(None::<MountSite<State, Row, (), bool>>));
            let make_site = Arc::clone(&site);
            let store = Store::builder(
                State {
                    ticks: 0,
                    rows: (0..4)
                        .map(|id| {
                            (
                                id,
                                Row {
                                    value: 0,
                                    configuration: 0,
                                },
                            )
                        })
                        .collect(),
                },
                Parent,
                Runtime::new(driver.clone()),
            )
            .keyed_with_children(
                |state| &state.rows,
                |state| &mut state.rows,
                move |id, _| {
                    let configuration = Capture::new(&make_counts, 1);
                    let entry = EntryMount::new(Child(Capture::new(&make_counts, 0)))
                        .configuration(move |row| {
                            let _ = &configuration;
                            row.configuration
                        });
                    let recipe = Capture::new(&make_counts, 2);
                    let site = Arc::clone(&make_site);
                    let selected = *id == 0;
                    (entry, move |_, children| {
                        let _ = &recipe;
                        if selected {
                            *site.lock().unwrap() = Some(children.site().clone());
                        }
                    })
                },
            )
            .build()
            .unwrap();
            driver.finish(store.ready()).unwrap();
            let site = site.lock().unwrap().take().unwrap();
            let mut baseline = None;
            // Warm fixed-capacity runtime/hash-table storage, then check every
            // completed input. No allocator-size tolerance hides linear growth.
            for iteration in 1..=5_100 {
                match input {
                    Input::Parent => {
                        let task = driver.finish(store.send(false)).unwrap();
                        driver.finish(task.finish());
                    }
                    Input::Child => {
                        let child = driver.finish(store.acquire(&site)).unwrap().unwrap();
                        let task = driver.finish(child.send(())).unwrap();
                        driver.finish(task.finish());
                    }
                    Input::Refresh => {
                        let previous = driver.finish(store.acquire(&site)).unwrap().unwrap();
                        let task = driver.finish(store.send(true)).unwrap();
                        driver.finish(task.finish());
                        assert_eq!(
                            driver
                                .finish(previous.read(|row| row.configuration))
                                .unwrap_err()
                                .reason(),
                            rca_core::TaskAccessReason::ScopeEnded
                        );
                        let current = driver.finish(store.acquire(&site)).unwrap().unwrap();
                        assert_eq!(
                            driver
                                .finish(current.read(|row| row.configuration))
                                .unwrap(),
                            iteration
                        );
                    }
                }
                driver.turn();
                for kind in 0..3 {
                    assert_eq!(counts.created[kind].load(Ordering::Relaxed), 4);
                    assert_eq!(counts.dropped[kind].load(Ordering::Relaxed), 0);
                }
                let heap = dhat::HeapStats::get();
                let retained = (heap.curr_blocks, heap.curr_bytes);
                if iteration == 100 {
                    baseline = Some(retained);
                } else if iteration > 100 {
                    assert_eq!(
                        Some(retained),
                        baseline,
                        "{input:?} input {iteration} retained per-commit metadata"
                    );
                }
            }
            let expected = match input {
                Input::Parent => (5_100, 0, 0),
                Input::Child => (0, 5_100, 0),
                Input::Refresh => (5_100, 0, 5_100),
            };
            assert_eq!(
                driver
                    .finish(store.read(|state| {
                        let row = state.rows.get(&0).unwrap();
                        (state.ticks, row.value, row.configuration)
                    }))
                    .unwrap(),
                expected
            );
            eprintln!(
                "keyed live retention {input:?}: warmup=100 inputs=5000 live_blocks_bytes={:?} owners_configuration_recipe=[4,4,4]",
                baseline.unwrap()
            );
            driver.finish(store.shutdown()).unwrap();
            drop(store);
            driver.turn();
            for kind in 0..3 {
                assert_eq!(counts.dropped[kind].load(Ordering::Relaxed), 4);
            }
        }
    }
}

#[cfg(feature = "local")]
/// Runs the same command phases with owner-thread state and callback captures.
mod local {
    use super::local_driver::Driver;
    use super::measure;
    use rca_core::MountIdentity;
    use rca_core::local::{Feature, FeatureContext, OptionalMount, Runtime, Store};
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    use std::task::Poll;

    /// Separates first-poll admission, one actor turn and reply completion.
    fn phases<F: Future>(label: &str, driver: &Driver, future: F) -> F::Output {
        let bytes = std::mem::size_of_val(&future);
        let mut future = std::pin::pin!(future);
        let (admitted, admission) = measure(|| super::poll(future.as_mut()));
        assert!(admitted.is_pending());
        let ((), actor) = measure(|| driver.turn());
        let (completed, completion) = measure(|| super::poll(future.as_mut()));
        let Poll::Ready(value) = completed else {
            panic!("one Local actor turn completes measured command")
        };
        eprintln!(
            "Local command phase {label}: inline_future={bytes} admission={admission:?} actor_with_driver={actor:?} completion={completion:?}"
        );
        value
    }

    /// Keeps the optional child unchanged during command delivery.
    struct Parent;

    impl Feature for Parent {
        type State = Option<usize>;
        type Action = ();

        fn update(
            &mut self,
            _: &mut Self::State,
            (): &(),
            _: &mut FeatureContext<'_, Self::State, ()>,
        ) {
        }
    }

    /// Adds the mapped input to the installed child value.
    struct Child;

    impl Feature for Child {
        type State = usize;
        type Action = usize;

        fn update(
            &mut self,
            state: &mut usize,
            action: &usize,
            _: &mut FeatureContext<'_, usize, usize>,
        ) {
            *state += action;
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "One command cost assay preserves construction, acquisition, delivery and teardown measurement boundaries"
    )]
    /// Checks Local mapping counts, phase costs and held-gate acquisition.
    pub(super) fn exercise() {
        let driver = Driver::default();
        let declared = Rc::new(RefCell::new(None));
        let sites = Rc::clone(&declared);
        let maps = Rc::new(Cell::new(0));
        let mapped = Rc::clone(&maps);
        let descriptor = OptionalMount::new(
            Option::as_ref,
            Option::as_mut,
            |state: &Option<usize>| {
                state.map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            |_| Child,
        );
        let (builder, ordinary_site) =
            Store::builder(Some(0), Parent, Runtime::new(driver.clone())).mount_with_children(
                descriptor,
                move |_, children| {
                    let mapped = Rc::clone(&mapped);
                    *sites.borrow_mut() = Some(children.command(move |input: u8| {
                        mapped.set(mapped.get() + 1);
                        usize::from(input)
                    }));
                },
            );
        let store = builder.build().unwrap();
        driver.finish(store.ready()).unwrap();
        let site = declared.borrow_mut().take().unwrap();
        for _ in 0..2 {
            let (future, creation) = measure(|| store.acquire(&ordinary_site));
            eprintln!("Local ordinary acquisition creation={creation:?}");
            let ordinary = phases("ordinary acquisition", &driver, future)
                .unwrap()
                .unwrap();
            let (future, creation) = measure(|| ordinary.send(1));
            eprintln!("Local ordinary send creation={creation:?}");
            drop(phases("ordinary send", &driver, future).unwrap());
            let (future, creation) = measure(|| store.command_port(&site));
            eprintln!("Local recipe acquisition creation={creation:?}");
            let port = phases("recipe acquisition", &driver, future)
                .unwrap()
                .unwrap();
            let (future, creation) = measure(|| port.send(1));
            eprintln!("Local recipe send creation={creation:?}");
            drop(phases("recipe send", &driver, future).unwrap());
        }
        assert_eq!(maps.get(), 2);
        assert_eq!(driver.finish(store.read(|state| *state)).unwrap(), Some(4));
        drop(store);
        driver.turn();

        #[cfg(feature = "test-util")]
        {
            let gate = rca_core::testing::local::TransactionGate::new();
            let declared = Rc::new(RefCell::new(None));
            let sites = Rc::clone(&declared);
            let descriptor = OptionalMount::new(
                Option::as_ref,
                Option::as_mut,
                |state: &Option<usize>| {
                    state.map(|_| MountIdentity {
                        key: (),
                        replacement: 0,
                        configuration: 0,
                    })
                },
                |_| Child,
            );
            let (builder, _) = Store::builder(Some(0), Parent, Runtime::new(driver.clone()))
                .transaction_gate(gate.clone())
                .mount_with_children(descriptor, move |_, children| {
                    *sites.borrow_mut() = Some(children.command(|input: u8| usize::from(input)));
                });
            let store = builder.build().unwrap();
            driver.turn();
            let boundary = gate.boundary().unwrap();
            let receipt = gate.release_pending_with_receipt().unwrap();
            drop(driver.finish(receipt.finish()).unwrap());
            assert!(gate.acknowledge(boundary));
            driver.finish(store.ready()).unwrap();
            let site = declared.borrow_mut().take().unwrap();
            let port = driver.finish(store.command_port(&site)).unwrap().unwrap();
            let mut pending = Box::pin(port.send(1));
            assert!(super::poll(pending.as_mut()).is_pending());
            driver.turn();
            for _ in 0..2 {
                let (future, creation) = measure(|| gate.command_port(&store, &site));
                eprintln!("Local held recipe acquisition creation={creation:?}");
                drop(
                    phases("held recipe acquisition", &driver, future)
                        .unwrap()
                        .unwrap(),
                );
            }
            let boundary = gate.boundary().unwrap();
            let receipt = gate.release_pending_with_receipt().unwrap();
            drop(driver.finish(receipt.finish()).unwrap());
            assert!(gate.acknowledge(boundary));
            drop(driver.finish(pending).unwrap());
            assert_eq!(driver.finish(store.read(|state| *state)).unwrap(), Some(1));
            drop(store);
            driver.turn();
        }
    }
}

/// Preserves the exact names of the isolated allocation cases.
const CASES: &[(&str, fn())] = &[(
    "measured_recipe_dispatch_and_unchanged_ordinary_root_mapped_controls",
    measured_recipe_dispatch_and_unchanged_ordinary_root_mapped_controls,
)];

include!("support/allocation_main.rs");
