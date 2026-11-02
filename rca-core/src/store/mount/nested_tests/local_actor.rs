//! Real owner-local actor traversal, not manual scope installation.
use super::*;
use crate::runtime::local as runtime;
use crate::store::local as profile;
use std::cell::RefCell;
use std::rc::Rc;

/// Keeps non-Send actor tasks on the test thread for explicit polling.
/// Time stays at zero; yields suspend once and request another poll.
#[derive(Clone, Default)]
struct LocalDriver(Rc<RefCell<Vec<runtime::RuntimeFuture>>>);
impl fmt::Debug for LocalDriver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("NestedLocalDriver")
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
    /// Releases the queue borrow before polling; newly spawned tasks wait a turn.
    fn turn(&self) {
        let tasks = std::mem::take(&mut *self.0.borrow_mut());
        for mut task in tasks {
            if poll(task.as_mut()).is_pending() {
                self.0.borrow_mut().push(task);
            }
        }
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

struct LocalState {
    rows: KeyedState<String, LocalRow>,
}
struct LocalRow {
    child: Option<Rc<Cell<usize>>>,
}
struct LocalRootAction(Rc<()>);
struct LocalRowAction;
struct LocalChildAction(Rc<usize>);
struct LocalRoot;
impl profile::Feature for LocalRoot {
    type State = LocalState;
    type Action = LocalRootAction;
    fn update(
        &mut self,
        state: &mut LocalState,
        action: &LocalRootAction,
        _: &mut profile::FeatureContext<'_, LocalState, LocalRootAction>,
    ) {
        assert_eq!(Rc::strong_count(&action.0), 1);
        state.rows.remove(&"one".to_owned());
        state.rows.insert(
            "one".to_owned(),
            LocalRow {
                child: Some(Rc::new(Cell::new(5))),
            },
        );
    }
}
/// Records parent disposal independently of child disposal and construction.
struct LocalParent(Rc<RefCell<Vec<&'static str>>>);
impl Drop for LocalParent {
    fn drop(&mut self) {
        self.0.borrow_mut().push("parent drop");
    }
}
impl profile::Feature for LocalParent {
    type State = LocalRow;
    type Action = LocalRowAction;
    fn update(
        &mut self,
        _: &mut LocalRow,
        _: &LocalRowAction,
        _: &mut profile::FeatureContext<'_, LocalRow, LocalRowAction>,
    ) {
    }
}
/// Records child disposal while its reducer exercises an Rc-owned action.
struct LocalChild(Rc<RefCell<Vec<&'static str>>>);
impl Drop for LocalChild {
    fn drop(&mut self) {
        self.0.borrow_mut().push("child drop");
    }
}
impl profile::Feature for LocalChild {
    type State = Rc<Cell<usize>>;
    type Action = LocalChildAction;
    fn update(
        &mut self,
        state: &mut Self::State,
        action: &LocalChildAction,
        _: &mut profile::FeatureContext<'_, Self::State, LocalChildAction>,
    ) {
        state.set(state.get() + *action.0);
    }
}
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one local actor schedule keeps child discovery and parent replacement assertions together"
)]
fn owner_local_actor_discovers_rc_child_and_replaces_parent_incarnation() {
    let driver = LocalDriver::default();
    let trace = Rc::new(RefCell::new(Vec::new()));
    let sites = Rc::new(RefCell::new(Vec::new()));
    let factories = std::rc::Rc::clone(&trace);
    let recipes = std::rc::Rc::clone(&trace);
    let recipe_sites = std::rc::Rc::clone(&sites);
    let (owner, _) =
        super::super::local::attach_selected_key::<LocalState, LocalRootAction, LocalParent, _, _>(
            |state| &state.rows,
            |state| &mut state.rows,
            "one".to_owned(),
            move |_| {
                factories.borrow_mut().push("parent make");
                LocalParent(std::rc::Rc::clone(&factories))
            },
            move |_, children| {
                recipes.borrow_mut().push("recipe");
                let trace = std::rc::Rc::clone(&recipes);
                let body = std::rc::Rc::clone(&recipes);
                let site = children.optional(
                    super::super::local::LocalOptionalMount::new(
                        |row: &LocalRow| row.child.as_ref(),
                        |row: &mut LocalRow| row.child.as_mut(),
                        |row: &LocalRow| {
                            row.child.as_ref().map(|_| MountIdentity {
                                key: (),
                                replacement: 0,
                                configuration: 0,
                            })
                        },
                        move |_| {
                            trace.borrow_mut().push("child make");
                            LocalChild(std::rc::Rc::clone(&trace))
                        },
                    )
                    .on_initial(
                        |state| Rc::new(state.get()),
                        move |change, _| {
                            let ReactionChange::Initial { current } = change else {
                                panic!("initial")
                            };
                            let trace = std::rc::Rc::clone(&body);
                            profile::Task::run(move |context| async move {
                                trace.borrow_mut().push("body");
                                drop(context.send(LocalChildAction(current)).await.unwrap());
                            })
                        },
                    ),
                );
                recipe_sites.borrow_mut().push(site);
            },
        );
    let store = profile::Store::builder(
        LocalState {
            rows: [(
                "one".to_owned(),
                LocalRow {
                    child: Some(Rc::new(Cell::new(5))),
                },
            )]
            .into_iter()
            .collect(),
        },
        LocalRoot,
        runtime::Runtime::new(driver.clone()),
    )
    .with_mount(owner)
    .build()
    .unwrap();
    driver.turn();
    assert_eq!(*trace.borrow(), ["parent make", "recipe", "child make"]);
    driver.turn();
    driver.turn();
    driver.turn();
    let site = sites.borrow()[0].clone();
    let mut acquire = Box::pin(store.acquire(&site));
    assert!(poll(acquire.as_mut()).is_pending());
    driver.turn();
    let Poll::Ready(Ok(Some(child))) = poll(acquire.as_mut()) else {
        panic!("local child")
    };
    let mut read = Box::pin(child.read(|state| Rc::new(state.get())));
    assert!(poll(read.as_mut()).is_pending());
    driver.turn();
    assert!(matches!(poll(read.as_mut()), Poll::Ready(Ok(value)) if *value == 10));
    trace.borrow_mut().clear();
    let mut replace = Box::pin(store.send(LocalRootAction(Rc::new(()))));
    assert!(poll(replace.as_mut()).is_pending());
    let mut queued_acquire = Box::pin(store.acquire(&site));
    assert!(poll(queued_acquire.as_mut()).is_pending());
    driver.turn();
    assert!(matches!(poll(replace.as_mut()), Poll::Ready(Ok(_))));
    assert_eq!(
        *trace.borrow(),
        [
            "child drop",
            "parent drop",
            "parent make",
            "recipe",
            "child make"
        ]
    );
    assert_eq!(sites.borrow().len(), 2);
    assert!(
        matches!(poll(queued_acquire.as_mut()), Poll::Ready(Err(error)) if error.reason() == TaskAccessReason::ScopeEnded)
    );
    let mut stale = Box::pin(child.read(|_| panic!("stale local selector")));
    assert!(poll(stale.as_mut()).is_pending());
    driver.turn();
    assert!(
        matches!(poll(stale.as_mut()), Poll::Ready(Err(error)) if error.reason() == TaskAccessReason::ScopeEnded)
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one actor mutation keeps mixed retirement, Changed factory and delayed task entry in a single trace"
)]
fn mixed_retirement_precedes_changed_factory_and_tasks() {
    /// Projects an active structural branch without inventing a new branch API.
    enum Branch {
        Active(usize),
        Inactive,
    }

    /// Keeps optional-state removal independent of the structural branch switch.
    struct MixedState {
        optional: Option<usize>,
        branch: Branch,
    }

    /// Removes both forms in the same exclusive actor mutation.
    struct MixedRoot;

    impl profile::Feature for MixedRoot {
        type State = MixedState;
        type Action = ();

        fn update(
            &mut self,
            state: &mut MixedState,
            (): &(),
            _: &mut profile::FeatureContext<'_, MixedState, ()>,
        ) {
            state.optional = None;
            state.branch = Branch::Inactive;
        }
    }

    /// Records actual Feature disposal, not state projection or declaration removal.
    struct RetiringFeature {
        trace: Rc<RefCell<Vec<&'static str>>>,
        event: &'static str,
    }

    impl profile::Feature for RetiringFeature {
        type State = usize;
        type Action = ();

        fn update(
            &mut self,
            _: &mut usize,
            (): &(),
            _: &mut profile::FeatureContext<'_, usize, ()>,
        ) {
        }
    }

    impl Drop for RetiringFeature {
        fn drop(&mut self) {
            self.trace.borrow_mut().push(self.event);
        }
    }

    let driver = LocalDriver::default();
    let trace = Rc::new(RefCell::new(Vec::new()));
    let optional_trace = Rc::clone(&trace);
    let optional = profile::OptionalMount::new(
        |state: &MixedState| state.optional.as_ref(),
        |state: &mut MixedState| state.optional.as_mut(),
        |state: &MixedState| {
            state.optional.map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| RetiringFeature {
            trace: Rc::clone(&optional_trace),
            event: "optional drop",
        },
    );
    let branch_trace = Rc::clone(&trace);
    let branch = profile::OptionalMount::new(
        |state: &MixedState| match &state.branch {
            Branch::Active(value) => Some(value),
            Branch::Inactive => None,
        },
        |state: &mut MixedState| match &mut state.branch {
            Branch::Active(value) => Some(value),
            Branch::Inactive => None,
        },
        |state: &MixedState| {
            matches!(state.branch, Branch::Active(_)).then_some(MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| RetiringFeature {
            trace: Rc::clone(&branch_trace),
            event: "branch drop",
        },
    );
    let changed_trace = Rc::clone(&trace);
    let child_trace = Rc::clone(&trace);

    // Declaration topology: root -> [optional, branch -> [first, second]].
    // Root declarations reconcile forwards. Retiring the branch drops its
    // children in reverse roster order, before their parent Feature.
    let (builder, _) = profile::RootMount::new(MixedRoot)
        .on_change(
            |state| {
                (
                    state.optional.is_some(),
                    matches!(state.branch, Branch::Active(_)),
                )
            },
            crate::InitialPolicy::Skip,
            move |change, _| {
                assert!(matches!(
                    change,
                    ReactionChange::Changed {
                        previous: (true, true),
                        current: (false, false),
                    }
                ));
                changed_trace.borrow_mut().push("changed factory");
                let trace = Rc::clone(&changed_trace);
                profile::Task::run(move |_| async move {
                    trace.borrow_mut().push("changed task");
                })
            },
        )
        .builder(
            MixedState {
                optional: Some(1),
                branch: Branch::Active(2),
            },
            runtime::Runtime::new(driver.clone()),
        )
        .mount(optional)
        .mount_with_children(branch, move |_, children| {
            for event in ["branch first drop", "branch second drop"] {
                let trace = Rc::clone(&child_trace);
                children.optional(profile::OptionalMount::new(
                    |state: &usize| Some(state),
                    |state: &mut usize| Some(state),
                    |_| {
                        Some(MountIdentity {
                            key: (),
                            replacement: 0,
                            configuration: 0,
                        })
                    },
                    move |_| RetiringFeature {
                        trace: Rc::clone(&trace),
                        event,
                    },
                ));
            }
        });
    let store = builder.build().unwrap();
    driver.turn();
    assert!(
        trace.borrow().is_empty(),
        "InitialPolicy::Skip creates no work"
    );

    let mut mutation = Box::pin(store.send(()));
    assert!(poll(mutation.as_mut()).is_pending());
    driver.turn();
    let Poll::Ready(Ok(task)) = poll(mutation.as_mut()) else {
        panic!("the combined retirement mutation must commit");
    };
    drop(mutation);

    assert_eq!(
        *trace.borrow(),
        [
            "optional drop",
            "branch second drop",
            "branch first drop",
            "branch drop",
            "changed factory",
        ]
    );
    assert!(!task.is_finished(), "the Changed task has not entered");
    let mut finished = Box::pin(task.finish());
    assert!(poll(finished.as_mut()).is_pending());

    driver.turn();
    assert!(poll(finished.as_mut()).is_ready());
    assert_eq!(
        *trace.borrow(),
        [
            "optional drop",
            "branch second drop",
            "branch first drop",
            "branch drop",
            "changed factory",
            "changed task",
        ]
    );

    let mut shutdown = Box::pin(store.shutdown_into_state());
    assert!(poll(shutdown.as_mut()).is_pending());
    driver.turn();
    let Poll::Ready(Ok(state)) = poll(shutdown.as_mut()) else {
        panic!("the actor must finish after the Changed task");
    };
    assert!(state.optional.is_none());
    assert!(matches!(state.branch, Branch::Inactive));
    assert!(driver.0.borrow().is_empty());
}
