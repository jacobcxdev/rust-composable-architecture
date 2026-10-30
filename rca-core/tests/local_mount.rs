#![cfg(feature = "local")]

//! Owner-thread mount lifetimes, typed requests and retained Rc values.
//!
//! The host advances time only when requested and polls tasks outside its queue
//! borrow. Literal values, pointer identity and destruction counts are separate
//! checks: a completed request alone does not prove correct routing or cleanup.

use rca_core::local::{
    Effect, Feature, FeatureContext, MountSite, OptionalMount, Runtime, RuntimeDriver,
    RuntimeFuture, Store, Task,
};
use rca_core::{MonotonicInstant, MountIdentity};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

#[derive(Clone, Default)]
/// Drives owner-local tasks and timers through bounded, explicit turns.
struct Driver {
    /// Retains pending work without holding a borrow during its next poll.
    tasks: Rc<RefCell<Vec<RuntimeFuture>>>,
    /// Shares one manually advanced clock among clones and existing waits.
    now: Rc<Cell<Duration>>,
}
impl std::fmt::Debug for Driver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LocalMountDriver")
    }
}
impl RuntimeDriver for Driver {
    fn spawn(&self, task: RuntimeFuture) {
        self.tasks.borrow_mut().push(task);
    }
    fn now(&self) -> MonotonicInstant {
        MonotonicInstant::from_elapsed(self.now.get())
    }
    fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
        let now = Rc::clone(&self.now);
        Box::pin(std::future::poll_fn(move |_| {
            if MonotonicInstant::from_elapsed(now.get()) < deadline {
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        }))
    }
    fn yield_now(&self) -> RuntimeFuture {
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
impl Driver {
    /// Moves time forwards and polls waits that may now have elapsed.
    fn advance_to(&self, elapsed: Duration) {
        assert!(elapsed >= self.now.get());
        self.now.set(elapsed);
        for _ in 0..8 {
            self.turn();
        }
    }
    /// Polls each currently queued task once, outside the `RefCell` borrow.
    fn turn(&self) {
        let tasks = std::mem::take(&mut *self.tasks.borrow_mut());
        for mut task in tasks {
            if task
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
            {
                self.tasks.borrow_mut().push(task);
            }
        }
    }
    /// Completes finite requests without making future timer deadlines elapse.
    fn finish<F: Future>(&self, future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        for _ in 0..32 {
            if let Poll::Ready(value) = future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
            {
                return value;
            }
            self.turn();
        }
        panic!("finite local mount operation did not finish")
    }
}

#[test]
fn local_driver_yield_suspends_and_wakes_once_before_completion() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Wake;

    /// Counts scheduling notifications without polling any other task.
    struct Notifications(AtomicUsize);

    impl Wake for Notifications {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    let driver = Driver::default();
    let notifications = Arc::new(Notifications(AtomicUsize::new(0)));
    let waker = Waker::from(Arc::clone(&notifications));
    let mut context = Context::from_waker(&waker);
    let mut future = driver.yield_now();
    assert_eq!(notifications.0.load(Ordering::Relaxed), 0);
    assert!(
        future.as_mut().poll(&mut context).is_pending(),
        "yield must offer a scheduling turn"
    );
    assert_eq!(notifications.0.load(Ordering::Relaxed), 1);
    assert!(future.as_mut().poll(&mut context).is_ready());
    assert_eq!(notifications.0.load(Ordering::Relaxed), 1);
    assert_eq!(driver.now(), MonotonicInstant::ZERO);
}

/// Holds the optional owner-local child used by the public mount fixtures.
struct RootState(Option<Rc<Cell<usize>>>);
/// Triggers parent reconciliation without changing child state directly.
struct RootAction;
/// Retains the original non-Send payload through admission and rejection.
struct ChildAction(Rc<usize>);
/// Leaves child mutations to their independently typed mounted feature.
struct Root;
/// Counts routed reductions independently from the shared child value.
struct Child(Rc<Cell<usize>>);
impl Feature for Root {
    type State = RootState;
    type Action = RootAction;
    fn reduce(
        &mut self,
        _: &mut RootState,
        _: &RootAction,
        _: &mut FeatureContext<'_, RootState, RootAction>,
    ) {
    }
}
impl Feature for Child {
    type State = Rc<Cell<usize>>;
    type Action = ChildAction;
    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &ChildAction,
        _: &mut FeatureContext<'_, Self::State, ChildAction>,
    ) {
        self.0.set(self.0.get() + 1);
        state.set(state.get() + *action.0);
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One actor schedule distinguishes private-state retention, replacement and stale authority."
)]
fn local_required_child_identity_replacement_resets_private_state() {
    /// Keeps the required child's identity separate from its reported private count.
    struct RequiredState {
        /// Selects the installed child incarnation.
        id: u64,
        /// Records the private count only after an explicit report action.
        reported: usize,
    }

    /// Changes configuration without replacing the required child identity.
    struct State {
        /// Remains present throughout replacement and configuration refresh.
        child: RequiredState,
        /// Expires acquired authority without recreating the child Feature.
        configuration: u64,
    }

    /// Selects unchanged identity, configuration refresh or identity replacement.
    enum ParentAction {
        /// Reconciles the same identity without changing configuration.
        Noop,
        /// Requires fresh authority for the existing Feature.
        Refresh,
        /// Changes identity without resetting the reported state value.
        Replace,
    }

    /// Separates mutation of private state from its observable report.
    #[derive(Debug)]
    enum PrivateAction {
        /// Mutates only the Feature's private count.
        Bump,
        /// Copies the private count into observable state.
        Report,
    }

    /// Leaves the reported value untouched during identity replacement.
    struct Parent;

    impl Feature for Parent {
        type State = State;
        type Action = ParentAction;

        fn reduce(
            &mut self,
            state: &mut State,
            action: &ParentAction,
            _: &mut FeatureContext<'_, State, ParentAction>,
        ) {
            match action {
                ParentAction::Noop => {}
                ParentAction::Refresh => state.configuration += 1,
                ParentAction::Replace => state.child.id += 1,
            }
        }
    }

    /// Owns a private count that must not survive a new child incarnation.
    struct PrivateChild {
        /// Belongs to the Feature, not the projected child state.
        count: usize,
        /// Records construction and destruction independently from state reports.
        trace: Rc<RefCell<Vec<(&'static str, usize)>>>,
    }

    impl Feature for PrivateChild {
        type State = RequiredState;
        type Action = PrivateAction;

        fn reduce(
            &mut self,
            state: &mut RequiredState,
            action: &PrivateAction,
            _: &mut FeatureContext<'_, RequiredState, PrivateAction>,
        ) {
            match action {
                PrivateAction::Bump => self.count += 1,
                PrivateAction::Report => state.reported = self.count,
            }
        }
    }

    impl Drop for PrivateChild {
        fn drop(&mut self) {
            self.trace.borrow_mut().push(("drop", self.count));
        }
    }

    let driver = Driver::default();
    let trace = Rc::new(RefCell::new(Vec::new()));
    let make_trace = Rc::clone(&trace);
    let descriptor = OptionalMount::new(
        |state: &State| Some(&state.child),
        |state: &mut State| Some(&mut state.child),
        |state: &State| {
            Some(MountIdentity {
                key: state.child.id,
                replacement: 0,
                configuration: state.configuration,
            })
        },
        move |_| {
            make_trace.borrow_mut().push(("make", 0));
            PrivateChild {
                count: 0,
                trace: Rc::clone(&make_trace),
            }
        },
    );
    let (builder, site) = Store::feature_builder(
        State {
            child: RequiredState { id: 0, reported: 9 },
            configuration: 0,
        },
        Parent,
        Runtime::new(driver.clone()),
    )
    .mount_with_site(descriptor);
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    let original = driver.finish(store.acquire(&site)).unwrap().unwrap();
    drop(driver.finish(original.send(PrivateAction::Report)).unwrap());
    assert_eq!(
        driver
            .finish(original.read(|state| state.reported))
            .unwrap(),
        0
    );
    drop(driver.finish(original.send(PrivateAction::Bump)).unwrap());
    drop(driver.finish(store.send(ParentAction::Noop)).unwrap());
    drop(driver.finish(original.send(PrivateAction::Report)).unwrap());
    assert_eq!(
        driver
            .finish(original.read(|state| state.reported))
            .unwrap(),
        1
    );

    drop(driver.finish(store.send(ParentAction::Refresh)).unwrap());
    let error = driver
        .finish(original.read(|_| panic!("refreshed authority must reject before selection")))
        .unwrap_err();
    assert_eq!(
        (error.reason(), error.progress()),
        (
            rca_core::TaskAccessReason::ScopeEnded,
            rca_core::TaskProgress::NotStarted
        )
    );
    let current = driver.finish(store.acquire(&site)).unwrap().unwrap();
    drop(driver.finish(current.send(PrivateAction::Report)).unwrap());
    assert_eq!(
        driver.finish(current.read(|state| state.reported)).unwrap(),
        1
    );
    assert_eq!(*trace.borrow(), [("make", 0)]);

    // Queue replacement before the old child's input without advancing the
    // actor. Admission cannot transfer the captured route to the successor.
    let mut replace = Box::pin(store.send(ParentAction::Replace));
    assert!(
        replace
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    let mut queued = Box::pin(current.send(PrivateAction::Bump));
    assert!(
        queued
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    assert_eq!(*trace.borrow(), [("make", 0)]);
    drop(driver.finish(replace).unwrap());
    assert!(matches!(
        driver.finish(queued),
        Err(rca_core::TaskSendError::Rejected { action: PrivateAction::Bump, error })
            if error.reason() == rca_core::TaskAccessReason::ScopeEnded
                && error.progress() == rca_core::TaskProgress::NotStarted
    ));
    assert_eq!(*trace.borrow(), [("make", 0), ("drop", 1), ("make", 0)]);
    for old in [&original, &current] {
        let error = driver
            .finish(old.read(|_| panic!("retired authority must reject before selection")))
            .unwrap_err();
        assert_eq!(
            (error.reason(), error.progress()),
            (
                rca_core::TaskAccessReason::ScopeEnded,
                rca_core::TaskProgress::NotStarted
            )
        );
        let error = driver.finish(old.send(PrivateAction::Bump)).unwrap_err();
        assert!(matches!(
            error,
            rca_core::TaskSendError::Rejected { action: PrivateAction::Bump, error }
                if error.reason() == rca_core::TaskAccessReason::ScopeEnded
                    && error.progress() == rca_core::TaskProgress::NotStarted
        ));
    }
    let replacement = driver.finish(store.acquire(&site)).unwrap().unwrap();
    assert_eq!(
        driver
            .finish(replacement.read(|state| (state.id, state.reported)))
            .unwrap(),
        (1, 1),
        "replacing identity does not reset the projected state value"
    );
    drop(
        driver
            .finish(replacement.send(PrivateAction::Report))
            .unwrap(),
    );
    assert_eq!(
        driver
            .finish(replacement.read(|state| state.reported))
            .unwrap(),
        0
    );

    driver.finish(store.shutdown()).unwrap();
    drop(store);
    driver.turn();
    assert_eq!(
        *trace.borrow(),
        [("make", 0), ("drop", 1), ("make", 0), ("drop", 0)]
    );
    assert_eq!(
        Rc::strong_count(&trace),
        1,
        "stale contexts retain no Feature owner"
    );
    assert!(driver.tasks.borrow().is_empty());
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One Local fixture exercises all keyed declaration variants with Rc keys and actual typed removal."
)]
fn local_keyed_recipes_accept_owner_thread_keys() {
    use rca_core::KeyedState;
    use rca_core::local::EntryMount;

    type Rows = KeyedState<Rc<usize>, Rc<Cell<usize>>>;
    struct RowsFeature;
    impl Feature for RowsFeature {
        type State = Rows;
        type Action = ();

        fn reduce(&mut self, _: &mut Rows, (): &(), _: &mut FeatureContext<'_, Rows, ()>) {}
    }

    let driver = Driver::default();
    let key = Rc::new(3);
    let value = Rc::new(Cell::new(7));
    let mut rows = Rows::new();
    rows.insert(Rc::clone(&key), Rc::clone(&value));
    let reductions = Rc::new(Cell::new(0));
    let factories = Rc::new(Cell::new(0));
    let selected = Rc::new(RefCell::new(None));
    let removable = Rc::new(RefCell::new(None));
    let exported = Rc::clone(&selected);
    let exported_removal = Rc::clone(&removable);
    let reduced = Rc::clone(&reductions);
    let made = Rc::clone(&factories);
    let (builder, parent_site) =
        Store::feature_builder(rows, RowsFeature, Runtime::new(driver.clone()))
            .mount_with_children(
                OptionalMount::new(
                    |rows: &Rows| Some(rows),
                    |rows: &mut Rows| Some(rows),
                    |_| {
                        Some(MountIdentity {
                            key: (),
                            replacement: 0,
                            configuration: 0,
                        })
                    },
                    |_| RowsFeature,
                ),
                move |_, children| {
                    let made_plain = Rc::clone(&made);
                    let reduced_plain = Rc::clone(&reduced);
                    children.keyed(
                        |rows| rows,
                        |rows| rows,
                        move |_, _| {
                            made_plain.set(made_plain.get() + 1);
                            EntryMount::new(Child(Rc::clone(&reduced_plain)))
                        },
                    );

                    let made_recipe = Rc::clone(&made);
                    let reduced_recipe = Rc::clone(&reduced);
                    let exported = Rc::clone(&exported);
                    children.keyed_with_children(
                        |rows| rows,
                        |rows| rows,
                        move |_, _| {
                            made_recipe.set(made_recipe.get() + 1);
                            let exported = Rc::clone(&exported);
                            (
                                EntryMount::new(Child(Rc::clone(&reduced_recipe))),
                                move |_, children| {
                                    *exported.borrow_mut() = Some(children.site().clone());
                                },
                            )
                        },
                    );

                    let made_removable = Rc::clone(&made);
                    let reduced_removable = Rc::clone(&reduced);
                    let exported = Rc::clone(&exported_removal);
                    children.keyed_removable(
                        |rows| rows,
                        |rows| rows,
                        move |_, _| {
                            made_removable.set(made_removable.get() + 1);
                            let exported = Rc::clone(&exported);
                            (
                                EntryMount::new(Child(Rc::clone(&reduced_removable))),
                                move |_, children, removal| {
                                    *exported.borrow_mut() =
                                        Some((children.site().clone(), removal));
                                },
                            )
                        },
                    );
                },
            );
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    assert_eq!(factories.get(), 3);
    let parent = driver.finish(store.acquire(&parent_site)).unwrap().unwrap();
    let selected = selected.borrow_mut().take().unwrap();
    let selected = driver.finish(store.acquire(&selected)).unwrap().unwrap();
    let task = driver
        .finish(selected.send(ChildAction(Rc::new(5))))
        .unwrap();
    driver.finish(task.finish());
    assert_eq!((value.get(), reductions.get()), (12, 1));
    let (site, removal) = removable.borrow_mut().take().unwrap();
    let child = driver.finish(store.acquire(&site)).unwrap().unwrap();
    let removed = Rc::new(RefCell::new(None));
    let transferred = Rc::clone(&removed);
    let task = driver
        .finish(parent.remove_child(
            &removal,
            &child,
            |_| {},
            move |_, child, _| {
                *transferred.borrow_mut() = Some(child.into_inner());
            },
        ))
        .unwrap();
    driver.finish(task.finish());
    assert!(Rc::ptr_eq(removed.borrow().as_ref().unwrap(), &value));
    assert!(driver.finish(store.read(KeyedState::is_empty)).unwrap());
    driver.finish(store.shutdown()).unwrap();
    drop((parent, selected, child, site, removal));
    assert_eq!(Rc::strong_count(&key), 1);

    let mut rows = Rows::new();
    rows.insert(Rc::clone(&key), Rc::clone(&value));
    let root_reductions = Rc::clone(&reductions);
    let recipe_reductions = Rc::clone(&reductions);
    let removal_reductions = Rc::clone(&reductions);
    let store = Store::feature_builder(rows, RowsFeature, Runtime::new(driver.clone()))
        .keyed(
            |rows| rows,
            |rows| rows,
            move |_, _| EntryMount::new(Child(Rc::clone(&root_reductions))),
        )
        .keyed_with_children(
            |rows| rows,
            |rows| rows,
            move |_, _| {
                (
                    EntryMount::new(Child(Rc::clone(&recipe_reductions))),
                    |_, _| {},
                )
            },
        )
        .keyed_removable(
            |rows| rows,
            |rows| rows,
            move |_, _| {
                (
                    EntryMount::new(Child(Rc::clone(&removal_reductions))),
                    |_, _, _| {},
                )
            },
        )
        .build()
        .unwrap();
    driver.finish(store.ready()).unwrap();
    driver.finish(store.shutdown()).unwrap();
    assert_eq!(Rc::strong_count(&key), 1);
}

#[test]
fn local_hidden_parent_suppresses_descendant_recipe() {
    for hide in [false, true] {
        let driver = Driver::default();
        let value = Rc::new(Cell::new(1));
        let replacement = Rc::new(Cell::new(0));
        let identity = Rc::clone(&replacement);
        let constructors = Rc::new(Cell::new(0));
        let makes = Rc::clone(&constructors);
        let features = Rc::new(Cell::new(0));
        let feature_owner = Rc::clone(&features);
        let recipes = Rc::new(Cell::new(0));
        let recipe_calls = Rc::clone(&recipes);
        let descendants = Rc::new(Cell::new(0));
        let descendant_makes = Rc::clone(&descendants);
        let descriptor = OptionalMount::new(
            |root: &RootState| root.0.as_ref().filter(|value| value.get() != 0),
            |root: &mut RootState| root.0.as_mut(),
            move |_| {
                Some(MountIdentity {
                    key: (),
                    replacement: identity.get(),
                    configuration: 0,
                })
            },
            move |state: &Rc<Cell<usize>>| {
                makes.set(makes.get() + 1);
                if hide && makes.get() == 1 {
                    state.set(0);
                }
                Child(Rc::clone(&feature_owner))
            },
        );
        let (builder, _) = Store::feature_builder(
            RootState(Some(Rc::clone(&value))),
            Root,
            Runtime::new(driver.clone()),
        )
        .mount_with_children(descriptor, move |_, children| {
            recipe_calls.set(recipe_calls.get() + 1);
            let makes = Rc::clone(&descendant_makes);
            children.optional(OptionalMount::new(
                |state: &Rc<Cell<usize>>| Some(state),
                |state: &mut Rc<Cell<usize>>| Some(state),
                |_| {
                    Some(MountIdentity {
                        key: (),
                        replacement: 0,
                        configuration: 0,
                    })
                },
                move |_| {
                    makes.set(makes.get() + 1);
                    Child(Rc::clone(&makes))
                },
            ));
        });
        let store = builder.build().unwrap();
        driver.finish(store.ready()).unwrap();
        assert_eq!(constructors.get(), 1);
        assert_eq!(recipes.get(), usize::from(!hide));
        assert_eq!(descendants.get(), usize::from(!hide));

        // Restore selection without replacing the parent. Unstarted discovery
        // runs once; subsequent turns do not replay an already-entered recipe.
        value.set(1);
        for _ in 0..2 {
            let task = driver.finish(store.send(RootAction)).unwrap();
            driver.finish(task.finish());
            assert_eq!(constructors.get(), 1);
            assert_eq!(recipes.get(), 1);
            assert_eq!(descendants.get(), 1);
        }

        replacement.set(1);
        let task = driver.finish(store.send(RootAction)).unwrap();
        driver.finish(task.finish());
        assert_eq!(constructors.get(), 2);
        assert_eq!(recipes.get(), 2);
        assert_eq!(descendants.get(), 2);
        driver.finish(store.shutdown()).unwrap();
        assert_eq!(Rc::strong_count(&features), 1);
        assert_eq!(Rc::strong_count(&recipes), 1);
        assert_eq!(Rc::strong_count(&descendants), 1);
        assert!(driver.tasks.borrow().is_empty());
    }
}

#[test]
fn local_identity_clone_closure_is_rechecked_before_child_path() {
    thread_local! {
        /// Lets identity cloning close the actual Store on this test thread.
        static OWNER: RefCell<Option<Store<RootState, RootAction>>> = const { RefCell::new(None) };
    }
    #[derive(PartialEq, Eq, Hash)]
    /// Cloning the identity deliberately closes the Store before selection.
    struct Key;
    impl Clone for Key {
        fn clone(&self) -> Self {
            let owner = OWNER.with(|slot| slot.borrow_mut().take());
            drop(owner);
            Self
        }
    }

    let driver = Driver::default();
    let selections = Rc::new(Cell::new(0));
    let descriptor = OptionalMount::new(
        |root: &RootState| {
            let selected = root.0.as_ref().unwrap();
            selected.set(selected.get() + 1);
            root.0.as_ref()
        },
        |root: &mut RootState| root.0.as_mut(),
        |_| {
            Some(MountIdentity {
                key: Key,
                replacement: 0,
                configuration: 0,
            })
        },
        |_| -> Child { panic!("closed identity clone cannot enter constructor") },
    );
    let store = Store::feature_builder(
        RootState(Some(Rc::clone(&selections))),
        Root,
        Runtime::new(driver.clone()),
    )
    .mount(descriptor)
    .build()
    .unwrap();
    let ready = store.ready();
    OWNER.with(|slot| assert!(slot.borrow_mut().replace(store).is_none()));

    assert_eq!(
        driver.finish(ready).unwrap_err().reason(),
        rca_core::TaskAccessReason::StoreClosed
    );
    assert_eq!(selections.get(), 0);
    OWNER.with(|slot| assert!(slot.borrow().is_none()));
    assert!(driver.tasks.borrow().is_empty());
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One declaration checks unbuilt rejection before its installed owner-local lifecycle and capture ownership."
)]
fn public_local_mount_pairs_independent_actions_and_owned_rc_context() {
    let driver = Driver::default();
    let reductions = Rc::new(Cell::new(0));
    let capture = std::rc::Rc::clone(&reductions);
    let initial_calls = Rc::new(Cell::new(0));
    let initial_called = Rc::clone(&initial_calls);
    let descriptor = OptionalMount::new(
        |root: &RootState| root.0.as_ref(),
        |root: &mut RootState| root.0.as_mut(),
        |root: &RootState| {
            root.0.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| Child(std::rc::Rc::clone(&capture)),
    );
    assert_eq!(
        format!("{descriptor:?}"),
        "OptionalMount { has_initial: false, .. }"
    );
    let descriptor = descriptor.on_initial(
        |state| Rc::new(state.get()),
        move |change, _| {
            initial_called.set(initial_called.get() + 1);
            let rca_core::ReactionChange::Initial { current } = change else {
                panic!("initial")
            };
            Task::run(move |context| async move {
                drop(context.send(ChildAction(current)).await.unwrap());
            })
        },
    );
    assert_eq!(
        format!("{descriptor:?}"),
        "OptionalMount { has_initial: true, .. }"
    );
    assert_eq!(initial_calls.get(), 0);
    assert_eq!(
        format!("{:?}", rca_core::local::RootMount::new(Root)),
        "RootMount { .. }"
    );
    assert_eq!(
        format!(
            "{:?}",
            rca_core::local::EntryMount::new(Child(Rc::clone(&reductions)))
        ),
        "EntryMount { .. }"
    );
    assert_eq!(initial_calls.get(), 0);
    let (builder, site): (
        _,
        MountSite<RootState, Rc<Cell<usize>>, ChildAction, RootAction>,
    ) = Store::feature_builder(
        RootState(Some(Rc::new(Cell::new(3)))),
        Root,
        Runtime::new(driver.clone()),
    )
    .mount_with_site(descriptor);
    let site_debug = format!("{site:?}");
    assert_eq!(site_debug, "MountSite { .. }");
    let ended_site = site.clone();

    // A live but unbuilt declaration must not manufacture a child context for
    // another Store, select its state or run the deferred initial task.
    let other = Store::feature_builder(RootState(None), Root, Runtime::new(driver.clone()))
        .build()
        .unwrap();
    driver.finish(other.ready()).unwrap();
    let error = driver
        .finish(other.acquire(&site))
        .expect_err("unbuilt declaration has no actor binding");
    assert_eq!(error.reason(), rca_core::TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
    assert_eq!((initial_calls.get(), reductions.get()), (0, 0));
    drop(other);
    driver.turn();

    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    for _ in 0..4 {
        driver.turn();
    }
    assert_eq!(initial_calls.get(), 1);
    let acquisition = store.acquire(&site);
    drop(site);
    let child = driver.finish(acquisition).unwrap().unwrap();
    assert_eq!(driver.finish(child.read(|state| state.get())).unwrap(), 6);
    assert_eq!(reductions.get(), 1);
    driver.finish(child.update(|state| state.set(8))).unwrap();
    drop(driver.finish(child.send(ChildAction(Rc::new(2)))).unwrap());
    assert_eq!(
        driver
            .finish(child.read(|state| Rc::new(state.get())))
            .unwrap()
            .as_ref(),
        &10
    );
    assert_eq!(reductions.get(), 2);
    drop(store);
    driver.turn();
    assert_eq!(format!("{ended_site:?}"), site_debug);
    assert_eq!(Rc::strong_count(&reductions), 1);

    let other = Store::feature_builder(RootState(None), Root, Runtime::new(driver.clone()))
        .build()
        .unwrap();
    driver.finish(other.ready()).unwrap();
    let error = driver
        .finish(other.acquire(&ended_site))
        .expect_err("the old site must not retain its declaration");
    assert_eq!(error.reason(), rca_core::TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
    #[cfg(feature = "test-util")]
    {
        let gate = rca_core::testing::local::TransactionGate::new();
        let validation = gate
            .validate_acquisition(&other, &ended_site)
            .expect_err("test validation cannot revive the expired declaration");
        let acquisition = driver
            .finish(gate.acquire(&other, &ended_site))
            .expect_err("test acquisition cannot revive the expired declaration");
        assert_eq!(gate.boundary(), None);
        assert!(!gate.has_pending());
        for error in [validation, acquisition] {
            assert_eq!(error.reason(), rca_core::TaskAccessReason::StoreClosed);
            assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
        }
    }
    drop(other);
    driver.turn();
}

#[test]
fn local_absent_output_source_returns_original_without_entering_handler() {
    /// Keeps live and absent declarations under the same actor identity.
    type State = (Option<Rc<Cell<usize>>>, Option<Rc<Cell<usize>>>);

    let driver = Driver::default();
    let reductions = Rc::new(Cell::new(0));
    let child_reductions = Rc::clone(&reductions);
    let live_mount = OptionalMount::new(
        |root: &State| root.0.as_ref(),
        |root: &mut State| root.0.as_mut(),
        |root: &State| {
            root.0.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| Child(Rc::clone(&child_reductions)),
    );
    let state = Rc::new(Cell::new(3));
    let root = rca_core::local::ReducerFeature::new(rca_core::local::Reduce::new(
        |_: &mut State, _: &RootAction, _: &mut rca_core::ReduceContext<'_>| Effect::none(),
    ));
    let (builder, live_site) = Store::feature_builder(
        (Some(Rc::clone(&state)), None),
        root,
        Runtime::new(driver.clone()),
    )
    .mount_with_site(live_mount);

    let child_reductions = Rc::clone(&reductions);
    let absent_mount = OptionalMount::new(
        |root: &State| root.1.as_ref(),
        |root: &mut State| root.1.as_mut(),
        |root: &State| {
            root.1.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| Child(Rc::clone(&child_reductions)),
    );
    let (builder, absent_site) = builder.mount_with_site(absent_mount);
    let calls = Rc::new(Cell::new(0));
    let observed = Rc::clone(&calls);
    let (builder, output) = builder.output::<Rc<usize>>(&absent_site, |handlers| {
        handlers.at_root(move |_, value, _| {
            observed.set(observed.get() + 1);
            rca_core::OutputFlow::Continue(value)
        });
    });
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    let context = driver.finish(store.acquire(&live_site)).unwrap().unwrap();
    assert!(
        driver
            .finish(store.acquire(&absent_site))
            .unwrap()
            .is_none()
    );

    let value = Rc::new(23);
    let identity = Rc::as_ptr(&value);
    let lifetime = Rc::downgrade(&value);
    let mut delivery = std::pin::pin!(context.output(&output, value));
    let Poll::Ready(Err(rca_core::OutputError::Returned { value, error })) = delivery
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    else {
        panic!("a bound source without a child cannot admit an output");
    };
    assert_eq!(error.reason(), rca_core::TaskAccessReason::ScopeEnded);
    assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
    assert_eq!(Rc::as_ptr(&value), identity);
    assert_eq!(Rc::strong_count(&value), 1);
    assert_eq!(*value, 23);
    assert_eq!((calls.get(), reductions.get(), state.get()), (0, 0, 3));
    drop(value);
    assert!(lifetime.upgrade().is_none());

    drop(store);
    driver.turn();
    assert!(driver.tasks.borrow().is_empty());
    assert_eq!(Rc::strong_count(&state), 1);
    assert_eq!(Rc::strong_count(&reductions), 1);
    assert_eq!(Rc::strong_count(&calls), 1);
}

#[test]
fn local_child_requests_reject_missing_ingress_and_return_original_rc() {
    let driver = Driver::default();
    let state = Rc::new(Cell::new(3));
    let reductions = Rc::new(Cell::new(0));
    let child_reductions = Rc::clone(&reductions);
    let descriptor = OptionalMount::new(
        |root: &RootState| root.0.as_ref(),
        |root: &mut RootState| root.0.as_mut(),
        |root: &RootState| {
            root.0.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| Child(Rc::clone(&child_reductions)),
    );
    let (builder, site) = Store::feature_builder(
        RootState(Some(Rc::clone(&state))),
        Root,
        Runtime::new(driver.clone()),
    )
    .mount_with_site(descriptor);
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    let child = driver.finish(store.acquire(&site)).unwrap().unwrap();
    assert_eq!(driver.tasks.borrow().len(), 1, "only the actor remains");
    drop(store);

    // No request held ingress at closure, and actor cleanup has not run yet.
    let callbacks = Rc::new(Cell::new(0));
    let read_capture = Rc::new(11);
    let read_weak = Rc::downgrade(&read_capture);
    let read_calls = Rc::clone(&callbacks);
    let mut read = Box::pin(child.read(move |state| {
        read_calls.set(read_calls.get() + 1);
        state.get() + *read_capture
    }));
    let update_capture = Rc::new(17);
    let update_weak = Rc::downgrade(&update_capture);
    let update_calls = Rc::clone(&callbacks);
    let mut update = Box::pin(child.update(move |state| {
        update_calls.set(update_calls.get() + 1);
        state.set(state.get() + *update_capture);
    }));
    let mut context = Context::from_waker(Waker::noop());
    let Poll::Ready(Err(read_error)) = read.as_mut().poll(&mut context) else {
        panic!("local read immediately rejects missing ingress");
    };
    let Poll::Ready(Err(update_error)) = update.as_mut().poll(&mut context) else {
        panic!("local update immediately rejects missing ingress");
    };
    for error in [read_error, update_error] {
        assert_eq!(error.reason(), rca_core::TaskAccessReason::StoreClosed);
        assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
    }
    assert_eq!(callbacks.get(), 0);
    assert!(read_weak.upgrade().is_none());
    assert!(update_weak.upgrade().is_none());
    let action = Rc::new(23);
    let identity = Rc::as_ptr(&action);
    let weak = Rc::downgrade(&action);
    let mut send = Box::pin(child.send(ChildAction(action)));
    let Poll::Ready(Err(rca_core::TaskSendError::Rejected {
        action: ChildAction(action),
        error,
    })) = send.as_mut().poll(&mut context)
    else {
        panic!("local send immediately returns its original action");
    };
    assert_eq!(error.reason(), rca_core::TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
    assert_eq!(Rc::as_ptr(&action), identity);
    assert_eq!(Rc::strong_count(&action), 1);
    assert_eq!(*action, 23);
    assert_eq!(state.get(), 3);
    assert_eq!(reductions.get(), 0);
    drop(action);
    assert!(weak.upgrade().is_none());
    driver.turn();
    assert_eq!(state.get(), 3);
    assert_eq!(reductions.get(), 0);
    assert_eq!(Rc::strong_count(&callbacks), 1);
    assert_eq!(Rc::strong_count(&reductions), 1);
    assert_eq!(Rc::strong_count(&state), 1);
    assert!(driver.tasks.borrow().is_empty());
}

#[test]
fn local_admitted_and_blocked_acquisitions_close_without_retaining_the_child() {
    let driver = Driver::default();
    let state = Rc::new(Cell::new(3));
    let reductions = Rc::new(Cell::new(0));
    let child_reductions = Rc::clone(&reductions);
    let descriptor = OptionalMount::new(
        |root: &RootState| root.0.as_ref(),
        |root: &mut RootState| root.0.as_mut(),
        |root: &RootState| {
            root.0.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| Child(Rc::clone(&child_reductions)),
    );
    let (builder, site) = Store::feature_builder(
        RootState(Some(Rc::clone(&state))),
        Root,
        Runtime::new(driver.clone()),
    )
    .mailbox_capacity(1)
    .mount_with_site(descriptor);
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();

    let mut admitted = Box::pin(store.acquire(&site));
    let mut blocked = Box::pin(store.acquire(&site));
    let mut context = Context::from_waker(Waker::noop());
    assert!(admitted.as_mut().poll(&mut context).is_pending());
    assert!(store.try_send(RootAction).unwrap_err().is_full());
    assert!(blocked.as_mut().poll(&mut context).is_pending());

    drop(store);
    let Poll::Ready(Err(blocked_error)) = blocked.as_mut().poll(&mut context) else {
        panic!("capacity waiter must reject before another actor turn");
    };
    assert_eq!(
        blocked_error.reason(),
        rca_core::TaskAccessReason::StoreClosed
    );
    assert_eq!(blocked_error.progress(), rca_core::TaskProgress::NotStarted);
    assert!(admitted.as_mut().poll(&mut context).is_pending());

    // The queued acquisition receives rejection from actor teardown, not admission.
    driver.turn();
    let Poll::Ready(Err(admitted_error)) = admitted.as_mut().poll(&mut context) else {
        panic!("admitted acquisition must receive its typed teardown result");
    };
    assert_eq!(
        admitted_error.reason(),
        rca_core::TaskAccessReason::StoreClosed
    );
    assert_eq!(
        admitted_error.progress(),
        rca_core::TaskProgress::NotStarted
    );
    assert_eq!((state.get(), reductions.get()), (3, 0));
    assert_eq!(Rc::strong_count(&state), 1);
    assert_eq!(Rc::strong_count(&reductions), 1);
    assert!(driver.tasks.borrow().is_empty());

    drop((admitted, blocked, site));
    assert_eq!((state.get(), reductions.get()), (3, 0));
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one full-mailbox closure schedule preserves acquisition and three request owners with exact pre-entry errors"
)]
fn local_mounted_access_waiting_for_capacity_returns_owners_when_store_closes() {
    /// Distinguishes returned payload ownership from callback execution.
    struct Owned {
        value: usize,
        drops: Rc<Cell<usize>>,
    }
    impl Drop for Owned {
        fn drop(&mut self) {
            self.drops.set(self.drops.get() + 1);
        }
    }
    /// Counts reductions separately from payload destruction during rejection.
    struct AdmissionChild(Rc<Cell<usize>>);
    impl Feature for AdmissionChild {
        type State = Rc<Cell<usize>>;
        type Action = Rc<Owned>;

        fn reduce(
            &mut self,
            state: &mut Self::State,
            action: &Self::Action,
            _: &mut FeatureContext<'_, Self::State, Self::Action>,
        ) {
            self.0.set(self.0.get() + 1);
            state.set(state.get() + action.value);
        }
    }

    let driver = Driver::default();
    let state = Rc::new(Cell::new(3));
    let reductions = Rc::new(Cell::new(0));
    let child_reductions = Rc::clone(&reductions);
    let descriptor = OptionalMount::new(
        |root: &RootState| root.0.as_ref(),
        |root: &mut RootState| root.0.as_mut(),
        |root: &RootState| {
            root.0.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| AdmissionChild(Rc::clone(&child_reductions)),
    );
    let (builder, site) = Store::feature_builder(
        RootState(Some(Rc::clone(&state))),
        Root,
        Runtime::new(driver.clone()),
    )
    .mailbox_capacity(1)
    .mount_with_site(descriptor);
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    let child = driver.finish(store.acquire(&site)).unwrap().unwrap();
    let occupied = store.try_send(RootAction).unwrap();
    assert!(store.try_send(RootAction).unwrap_err().is_full());
    let callbacks = Rc::new(Cell::new(0));
    let drops = Rc::new(Cell::new(0));
    let read_capture = Rc::new(Owned {
        value: 11,
        drops: Rc::clone(&drops),
    });
    let read_weak = Rc::downgrade(&read_capture);
    let read_calls = Rc::clone(&callbacks);
    let mut read = Box::pin(child.read(move |state| {
        read_calls.set(read_calls.get() + 1);
        state.get() + read_capture.value
    }));
    let update_capture = Rc::new(Owned {
        value: 17,
        drops: Rc::clone(&drops),
    });
    let update_weak = Rc::downgrade(&update_capture);
    let update_calls = Rc::clone(&callbacks);
    let mut update = Box::pin(child.update(move |state| {
        update_calls.set(update_calls.get() + 1);
        state.set(state.get() + update_capture.value);
    }));
    let action = Rc::new(Owned {
        value: 23,
        drops: Rc::clone(&drops),
    });
    let identity = Rc::as_ptr(&action);
    let action_weak = Rc::downgrade(&action);
    let mut send = Box::pin(child.send(action));
    let mut acquisition = Box::pin(store.acquire(&site));
    let mut context = Context::from_waker(Waker::noop());
    assert!(read.as_mut().poll(&mut context).is_pending());
    assert!(update.as_mut().poll(&mut context).is_pending());
    assert!(send.as_mut().poll(&mut context).is_pending());
    assert!(acquisition.as_mut().poll(&mut context).is_pending());
    assert_eq!((callbacks.get(), reductions.get(), drops.get()), (0, 0, 0));
    assert_eq!(state.get(), 3);

    drop(store);
    let Poll::Ready(Err(error)) = acquisition.as_mut().poll(&mut context) else {
        panic!("blocked acquisition must reject without another actor turn");
    };
    assert_eq!(error.reason(), rca_core::TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
    drop(acquisition);
    let Poll::Ready(Err(error)) = read.as_mut().poll(&mut context) else {
        panic!("blocked read must reject without another actor turn");
    };
    drop(read);
    assert_eq!(error.reason(), rca_core::TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
    assert!(read_weak.upgrade().is_none());
    assert_eq!(drops.get(), 1);
    let Poll::Ready(Err(error)) = update.as_mut().poll(&mut context) else {
        panic!("blocked update must reject without another actor turn");
    };
    drop(update);
    assert_eq!(error.reason(), rca_core::TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
    assert!(update_weak.upgrade().is_none());
    assert_eq!(drops.get(), 2);
    let Poll::Ready(Err(rca_core::TaskSendError::Rejected { action, error })) =
        send.as_mut().poll(&mut context)
    else {
        panic!("unpublished mounted send must return its original action");
    };
    drop(send);
    assert_eq!(error.reason(), rca_core::TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
    assert_eq!(Rc::as_ptr(&action), identity);
    assert_eq!(action.value, 23);
    assert_eq!(Rc::strong_count(&action), 1);
    assert_eq!(drops.get(), 2);
    drop(action);
    assert!(action_weak.upgrade().is_none());
    assert_eq!(drops.get(), 3);
    assert_eq!((callbacks.get(), reductions.get(), state.get()), (0, 0, 3));
    drop(occupied);
    driver.turn();
    assert_eq!((callbacks.get(), reductions.get(), drops.get()), (0, 0, 3));
    assert_eq!(Rc::strong_count(&callbacks), 1);
    assert_eq!(Rc::strong_count(&reductions), 1);
    assert_eq!(Rc::strong_count(&state), 1);
}

/// Starts one owned programme or appends an output digit to the child value.
enum SequenceAction {
    Start,
    Record(Rc<usize>),
}

/// Retains both output owners until their ordered sequence has consumed them.
struct SequenceChild {
    /// Moves into exactly one started sequence.
    inputs: Option<[Rc<usize>; 2]>,
    /// Records every intermediate value, not only the final checksum.
    history: Rc<RefCell<Vec<usize>>>,
}

#[derive(Clone, Copy, Debug)]
/// Varies inherited cancellation and the position of an explicit local key.
enum HandoffMode {
    Inherit,
    Transfer,
    Outer,
    Inner,
}

/// Separates parent cancellation, child work and the eventual owned output.
enum HandoffAction {
    Start,
    Child,
    Output(Rc<usize>),
    CancelSource,
    CancelLocal,
}

/// Holds a child operation until the test chooses which cancellation boundary ends.
struct HandoffFeature {
    mode: HandoffMode,
    /// Moves into the one child operation; no clone hides premature destruction.
    value: Option<Rc<usize>>,
    /// Confirms the cancellation test reached running child work.
    started: Rc<Cell<bool>>,
    /// Allows only a surviving operation to emit its output.
    ready: Rc<Cell<bool>>,
}

impl Feature for HandoffFeature {
    type State = Vec<usize>;
    type Action = HandoffAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        context: &mut FeatureContext<'_, Self::State, Self::Action>,
    ) {
        match action {
            HandoffAction::Start => {
                context.effect(Effect::none().without_inherited_cancellation());
                context.effect(Effect::send(HandoffAction::Child).cancellable("source"));
            }
            HandoffAction::Child => {
                let value = self.value.take().unwrap();
                let started = Rc::clone(&self.started);
                let ready = Rc::clone(&self.ready);
                let effect = Effect::run(move |sender| async move {
                    started.set(true);
                    std::future::poll_fn(|_| {
                        if ready.get() {
                            Poll::Ready(())
                        } else {
                            Poll::Pending
                        }
                    })
                    .await;
                    let _ = sender.send(HandoffAction::Output(value)).await;
                });
                context.effect(match self.mode {
                    HandoffMode::Inherit => effect,
                    HandoffMode::Transfer => effect.without_inherited_cancellation(),
                    HandoffMode::Outer => {
                        effect.without_inherited_cancellation().cancellable("local")
                    }
                    HandoffMode::Inner => {
                        effect.cancellable("local").without_inherited_cancellation()
                    }
                });
            }
            HandoffAction::Output(value) => state.push(**value),
            HandoffAction::CancelSource => context.effect(Effect::cancel("source")),
            HandoffAction::CancelLocal => context.effect(Effect::cancel("local")),
        }
    }
}

#[derive(Clone, Copy, Debug)]
/// Selects one cancellation boundary independently from the effect's modifiers.
enum HandoffEnd {
    Source,
    Local,
    Root,
    Shutdown,
}

#[test]
fn local_handoff_removes_only_inherited_keys_and_retains_owned_teardown() {
    for (mode, end, survives) in [
        (HandoffMode::Inherit, HandoffEnd::Source, false),
        (HandoffMode::Transfer, HandoffEnd::Source, true),
        (HandoffMode::Outer, HandoffEnd::Source, true),
        (HandoffMode::Inner, HandoffEnd::Source, true),
        (HandoffMode::Outer, HandoffEnd::Local, false),
        (HandoffMode::Inner, HandoffEnd::Local, false),
        (HandoffMode::Transfer, HandoffEnd::Root, false),
        (HandoffMode::Transfer, HandoffEnd::Shutdown, false),
    ] {
        let driver = Driver::default();
        let value = Rc::new(7);
        let weak = Rc::downgrade(&value);
        let started = Rc::new(Cell::new(false));
        let ready = Rc::new(Cell::new(false));
        let store = Store::feature(
            Vec::new(),
            HandoffFeature {
                mode,
                value: Some(value),
                started: Rc::clone(&started),
                ready: Rc::clone(&ready),
            },
            Runtime::new(driver.clone()),
        );
        assert_eq!(format!("{store:?}"), "Store { closed: false, .. }");
        assert!(!started.get());
        assert!(weak.upgrade().is_some());
        let root = driver.finish(store.send(HandoffAction::Start)).unwrap();
        for _ in 0..8 {
            if started.get() {
                break;
            }
            driver.turn();
        }
        assert!(started.get());
        assert!(!root.is_finished());
        match end {
            HandoffEnd::Source | HandoffEnd::Local => {
                let action = match end {
                    HandoffEnd::Source => HandoffAction::CancelSource,
                    _ => HandoffAction::CancelLocal,
                };
                let task = driver.finish(store.send(action)).unwrap();
                driver.finish(task.finish());
            }
            HandoffEnd::Root => root.cancel(),
            HandoffEnd::Shutdown => {
                driver.finish(store.shutdown()).unwrap();
                assert!(store.is_closed());
                assert_eq!(format!("{store:?}"), "Store { closed: true, .. }");
                assert!(root.is_cancelled());
                // Shutdown issues aborts; the host must still poll their wrappers.
                driver.turn();
            }
        }
        if survives {
            assert!(!root.is_finished());
            assert!(weak.upgrade().is_some());
            ready.set(true);
        }
        driver.finish(root.finish());
        assert!(weak.upgrade().is_none(), "{mode:?} / {end:?}");
        if !matches!(end, HandoffEnd::Shutdown) {
            let state = driver.finish(store.shutdown_into_state()).unwrap();
            assert_eq!(state, if survives { vec![7] } else { Vec::new() });
        }
    }
}

impl Feature for SequenceChild {
    type State = Rc<Cell<usize>>;
    type Action = SequenceAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &SequenceAction,
        context: &mut FeatureContext<'_, Self::State, SequenceAction>,
    ) {
        match action {
            SequenceAction::Start => {
                let [first, second] = self.inputs.take().expect("one sequence start");
                context.effect(Effect::concatenate([
                    Effect::future(async move {
                        let mut yielded = false;
                        std::future::poll_fn(|context| {
                            if yielded {
                                Poll::Ready(())
                            } else {
                                yielded = true;
                                context.waker().wake_by_ref();
                                Poll::Pending
                            }
                        })
                        .await;
                        Some(SequenceAction::Record(first))
                    }),
                    Effect::send(SequenceAction::Record(second)),
                ]));
            }
            SequenceAction::Record(value) => {
                state.set(state.get() * 10 + **value);
                self.history.borrow_mut().push(state.get());
            }
        }
    }
}

#[test]
fn local_mounted_concatenate_preserves_order_and_releases_owned_rc_actions() {
    let driver = Driver::default();
    let history = Rc::new(RefCell::new(Vec::new()));
    let capture = Rc::clone(&history);
    let values = [Rc::new(3), Rc::new(5)];
    let weak_values = values.each_ref().map(Rc::downgrade);
    let mut inputs = Some(values);
    let descriptor = OptionalMount::new(
        |root: &RootState| root.0.as_ref(),
        |root: &mut RootState| root.0.as_mut(),
        |root: &RootState| {
            root.0.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| SequenceChild {
            inputs: inputs.take(),
            history: Rc::clone(&capture),
        },
    );
    let state = Rc::new(Cell::new(1));
    let weak_state = Rc::downgrade(&state);
    let (builder, site): (
        _,
        MountSite<RootState, Rc<Cell<usize>>, SequenceAction, RootAction>,
    ) = Store::feature_builder(RootState(Some(state)), Root, Runtime::new(driver.clone()))
        .mount_with_site(descriptor);
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    let child = driver.finish(store.acquire(&site)).unwrap().unwrap();
    drop(site);
    let task = driver.finish(child.send(SequenceAction::Start)).unwrap();
    driver.finish(task.finish());

    assert_eq!(driver.finish(child.read(|state| state.get())).unwrap(), 135);
    assert_eq!(*history.borrow(), [13, 135]);

    drop(task);
    drop(child);
    drop(store);
    driver.turn();
    assert!(weak_values.iter().all(|value| value.upgrade().is_none()));
    assert!(weak_state.upgrade().is_none());
    assert_eq!(Rc::strong_count(&history), 1);
}

#[test]
#[should_panic(expected = "interval period must be non-zero")]
fn local_interval_rejects_zero_period_with_owned_rc_factory() {
    let value = Rc::new(3);
    drop(Effect::interval(std::time::Duration::ZERO, move || {
        Rc::clone(&value)
    }));
}

/// Preserves payload identity and counts destruction of an owned failure or action.
struct ReportedCause {
    /// Supplies a stable pointer identity across output and error routing.
    value: Rc<usize>,
    /// Counts destruction of the enclosing owner, not clones of its payload.
    drops: Rc<Cell<usize>>,
}

impl Drop for ReportedCause {
    fn drop(&mut self) {
        self.drops.set(self.drops.get() + 1);
    }
}

/// Moves a supplied effect into exactly one mounted start action.
struct EffectChild(Option<Effect<SequenceAction>>);

impl Feature for EffectChild {
    type State = Rc<Cell<usize>>;
    type Action = SequenceAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        context: &mut FeatureContext<'_, Self::State, Self::Action>,
    ) {
        match action {
            SequenceAction::Start => {
                context.effect(self.0.take().expect("one child effect programme"));
            }
            SequenceAction::Record(value) => state.set(state.get() * 10 + **value),
        }
    }
}

/// Changes route visibility before returning its owned work for admission.
struct PruningChild {
    /// Transfers once after the state visibility change.
    effect: Option<Effect<SequenceAction>>,
    /// Selects whether only the sibling or also the emitting route retires.
    keep_live: bool,
}

impl Feature for PruningChild {
    type State = Rc<Cell<usize>>;
    type Action = SequenceAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        context: &mut FeatureContext<'_, Self::State, Self::Action>,
    ) {
        match action {
            SequenceAction::Start => {
                state.set(usize::from(self.keep_live));
                context.effect(self.effect.take().expect("one pruning transaction"));
            }
            SequenceAction::Record(value) => state.set(state.get() * 10 + **value),
        }
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one paired Local lifecycle schedule contrasts live work with retired-route disposal"
)]
fn local_mounted_retirement_prunes_owned_work_but_retains_a_live_route() {
    for keep_live in [false, true] {
        let driver = Driver::default();
        let calls = Rc::new(Cell::new(0));
        let operation_calls = Rc::clone(&calls);
        let drops = Rc::new(Cell::new(0));
        let capture = ReportedCause {
            value: Rc::new(7),
            drops: Rc::clone(&drops),
        };
        let capture_weak = Rc::downgrade(&capture.value);
        let payload = Rc::new(9);
        let payload_weak = Rc::downgrade(&payload);
        let mut effect = Some(
            Effect::run(move |sender| {
                operation_calls.set(operation_calls.get() + 1);
                async move {
                    assert_eq!(*capture.value, 7);
                    sender.send(SequenceAction::Record(payload)).await.unwrap();
                    drop(capture);
                }
            })
            .cancellable("owned child work")
            .named("child operation", rca_core::OperationPolicy::Coexist),
        );
        let descriptor = OptionalMount::new(
            |root: &RootState| root.0.as_ref().filter(|state| state.get() > 0),
            |root: &mut RootState| root.0.as_mut().filter(|state| state.get() > 0),
            |root: &RootState| {
                root.0
                    .as_ref()
                    .filter(|state| state.get() > 0)
                    .map(|_| MountIdentity {
                        key: (),
                        replacement: 0,
                        configuration: 0,
                    })
            },
            move |_| PruningChild {
                effect: effect.take(),
                keep_live,
            },
        );
        // The sibling observes the same owner-local value but has its own route.
        // Both transactions retire it; only one also retires the emitting route.
        let sibling = OptionalMount::new(
            |root: &RootState| root.0.as_ref().filter(|state| state.get() == 2),
            |root: &mut RootState| root.0.as_mut().filter(|state| state.get() == 2),
            |root: &RootState| {
                root.0
                    .as_ref()
                    .filter(|state| state.get() == 2)
                    .map(|_| MountIdentity {
                        key: (),
                        replacement: 0,
                        configuration: 0,
                    })
            },
            |_| Child(Rc::new(Cell::new(0))),
        );
        let state = Rc::new(Cell::new(2));
        let (builder, site) = Store::feature_builder(
            RootState(Some(Rc::clone(&state))),
            Root,
            Runtime::new(driver.clone()),
        )
        .mount_with_site(descriptor);
        let (builder, sibling_site) = builder.mount_with_site(sibling);
        let store = builder.build().unwrap();
        driver.finish(store.ready()).unwrap();
        assert!(
            driver
                .finish(store.acquire(&sibling_site))
                .unwrap()
                .is_some()
        );
        let child = driver.finish(store.acquire(&site)).unwrap().unwrap();
        let task = driver.finish(child.send(SequenceAction::Start)).unwrap();
        driver.finish(task.finish());
        assert!(task.is_finished());
        assert_eq!(calls.get(), usize::from(keep_live));
        assert_eq!(state.get(), if keep_live { 19 } else { 0 });
        assert!(
            driver
                .finish(store.acquire(&sibling_site))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            driver.finish(store.acquire(&site)).unwrap().is_some(),
            keep_live
        );
        assert_eq!(drops.get(), 1);
        assert!(capture_weak.upgrade().is_none());
        assert!(payload_weak.upgrade().is_none());
        driver.advance_to(Duration::ZERO);
        assert_eq!(calls.get(), usize::from(keep_live));
        drop(child);
        drop(store);
        driver.turn();
        assert_eq!(drops.get(), 1);
        assert_eq!(Rc::strong_count(&state), 1);
        assert_eq!(Rc::strong_count(&calls), 1);
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one Local retirement timeline proves deferred ownership before release and ordered completion afterwards"
)]
fn sibling_retirement_preserves_a_local_deferred_sequence_and_its_owners() {
    let driver = Driver::default();
    let ready = Rc::new(Cell::new(false));
    let release = Rc::clone(&ready);
    let polls = Rc::new(Cell::new(0));
    let attempts = Rc::clone(&polls);
    let drops = Rc::new(Cell::new(0));
    let capture = ReportedCause {
        value: Rc::new(3),
        drops: Rc::clone(&drops),
    };
    let first_weak = Rc::downgrade(&capture.value);
    let suffix = Rc::new(5);
    let suffix_weak = Rc::downgrade(&suffix);
    let mut effect = Some(Effect::concatenate([
        Effect::future(async move {
            std::future::poll_fn(|_| {
                attempts.set(attempts.get() + 1);
                if release.get() {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            })
            .await;
            let action = SequenceAction::Record(Rc::clone(&capture.value));
            drop(capture);
            Some(action)
        }),
        Effect::send(SequenceAction::Record(suffix)),
    ]));
    let descriptor = OptionalMount::new(
        |root: &RootState| root.0.as_ref(),
        |root: &mut RootState| root.0.as_mut(),
        |root: &RootState| {
            root.0.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| EffectChild(effect.take()),
    );
    // A committed digit removes only this sibling; the sequence's route survives.
    let sibling = OptionalMount::new(
        |root: &RootState| root.0.as_ref().filter(|state| state.get() == 1),
        |root: &mut RootState| root.0.as_mut().filter(|state| state.get() == 1),
        |root: &RootState| {
            root.0
                .as_ref()
                .filter(|state| state.get() == 1)
                .map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
        },
        |_| Child(Rc::new(Cell::new(0))),
    );
    let state = Rc::new(Cell::new(1));
    let (builder, site) = Store::feature_builder(
        RootState(Some(Rc::clone(&state))),
        Root,
        Runtime::new(driver.clone()),
    )
    .mount_with_site(descriptor);
    let (builder, sibling_site) = builder.mount_with_site(sibling);
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    assert!(
        driver
            .finish(store.acquire(&sibling_site))
            .unwrap()
            .is_some()
    );
    let child = driver.finish(store.acquire(&site)).unwrap().unwrap();
    let task = driver.finish(child.send(SequenceAction::Start)).unwrap();
    driver.turn();
    assert!(polls.get() > 0);
    assert_eq!(state.get(), 1);
    assert!(!task.is_finished());

    let edit = driver
        .finish(child.send(SequenceAction::Record(Rc::new(2))))
        .unwrap();
    driver.finish(edit.finish());
    assert!(
        driver
            .finish(store.acquire(&sibling_site))
            .unwrap()
            .is_none()
    );
    assert!(driver.finish(store.acquire(&site)).unwrap().is_some());
    assert_eq!(state.get(), 12);
    assert!(!task.is_finished());
    assert!(!task.is_cancelled());
    assert_eq!(drops.get(), 0);
    assert!(first_weak.upgrade().is_some());
    assert!(suffix_weak.upgrade().is_some());

    ready.set(true);
    driver.finish(task.finish());
    assert_eq!(state.get(), 1235);
    assert!(!task.is_cancelled());
    assert_eq!(drops.get(), 1);
    assert!(first_weak.upgrade().is_none());
    assert!(suffix_weak.upgrade().is_none());
    driver.finish(store.shutdown()).unwrap();
    drop((child, store));
    driver.turn();
    assert_eq!(drops.get(), 1);
    assert_eq!(Rc::strong_count(&state), 1);
    assert!(driver.tasks.borrow().is_empty());
}

#[test]
fn local_mounted_timers_preserve_deadlines_order_and_cancel_owned_interval() {
    let driver = Driver::default();
    driver.advance_to(Duration::from_secs(10));
    let operation_calls = Rc::new(Cell::new(0));
    let calls = Rc::clone(&operation_calls);
    let delayed = Rc::new(3);
    let delayed_weak = Rc::downgrade(&delayed);
    let next = Rc::new(Cell::new(4));
    let interval_weak = Rc::downgrade(&next);
    let interval_calls = Rc::new(Cell::new(0));
    let ticks = Rc::clone(&interval_calls);
    let expired = Rc::new(2);
    let expired_weak = Rc::downgrade(&expired);
    let mut effect = Some(Effect::concatenate([
        Effect::send_at(
            MonotonicInstant::from_elapsed(Duration::from_secs(5)),
            SequenceAction::Record(expired),
        ),
        Effect::run(move |sender| {
            calls.set(calls.get() + 1);
            async move {
                sender.send(SequenceAction::Record(delayed)).await.unwrap();
            }
        })
        .delay(Duration::from_secs(2)),
        Effect::interval(Duration::from_secs(1), move || {
            ticks.set(ticks.get() + 1);
            let digit = next.get();
            next.set(digit + 1);
            SequenceAction::Record(Rc::new(digit))
        }),
    ]));
    let descriptor = OptionalMount::new(
        |root: &RootState| root.0.as_ref(),
        |root: &mut RootState| root.0.as_mut(),
        |root: &RootState| {
            root.0.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| EffectChild(effect.take()),
    );
    let state = Rc::new(Cell::new(1));
    let (builder, site) = Store::feature_builder(
        RootState(Some(Rc::clone(&state))),
        Root,
        Runtime::new(driver.clone()),
    )
    .mount_with_site(descriptor);
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    let child = driver.finish(store.acquire(&site)).unwrap().unwrap();
    assert_eq!(state.get(), 1);
    let task = driver.finish(child.send(SequenceAction::Start)).unwrap();
    driver.advance_to(Duration::from_secs(10));
    assert_eq!(state.get(), 12);
    assert!(expired_weak.upgrade().is_none());
    assert_eq!(operation_calls.get(), 0);
    assert!(delayed_weak.upgrade().is_some());
    driver.advance_to(Duration::from_secs(11));
    assert_eq!((state.get(), operation_calls.get()), (12, 0));
    driver.advance_to(Duration::from_secs(12));
    assert_eq!((state.get(), operation_calls.get()), (123, 1));
    assert!(delayed_weak.upgrade().is_none());
    assert_eq!(interval_calls.get(), 0);
    driver.advance_to(Duration::from_secs(13));
    assert_eq!((state.get(), interval_calls.get()), (1234, 1));
    driver.advance_to(Duration::from_secs(14));
    assert_eq!((state.get(), interval_calls.get()), (12345, 2));
    assert!(!task.is_finished());
    assert!(interval_weak.upgrade().is_some());
    task.cancel();
    driver.finish(task.finish());
    assert!(task.is_cancelled());
    assert!(interval_weak.upgrade().is_none());
    assert_eq!(Rc::strong_count(&interval_calls), 1);
    driver.advance_to(Duration::from_secs(20));
    assert_eq!((state.get(), interval_calls.get()), (12345, 2));
    drop(child);
    drop(site);
    drop(store);
    driver.turn();
    assert_eq!(Rc::strong_count(&state), 1);
    assert_eq!(Rc::strong_count(&operation_calls), 1);
}

#[test]
fn local_mounted_reported_failure_routes_owned_action_and_retains_original_cause() {
    let driver = Driver::default();
    let ready = Rc::new(Cell::new(false));
    let released = Rc::clone(&ready);
    let events = Rc::new(RefCell::new(Vec::new()));
    let operations = Rc::clone(&events);
    let factories = Rc::clone(&events);
    let consumers = Rc::clone(&events);
    let drops = Rc::new(Cell::new(0));
    let cause = ReportedCause {
        value: Rc::new(9),
        drops: Rc::clone(&drops),
    };
    let identity = Rc::as_ptr(&cause.value);
    let weak = Rc::downgrade(&cause.value);
    let outcome = Rc::new(RefCell::new(None));
    let retained = Rc::clone(&outcome);
    let line = line!() + 1;
    let effect = Effect::try_run_reported(
        move |sender| {
            operations.borrow_mut().push("operation");
            async move {
                std::future::poll_fn(|_| {
                    if released.get() {
                        Poll::Ready(())
                    } else {
                        Poll::Pending
                    }
                })
                .await;
                sender.send(7).await.unwrap();
                Err(rca_core::EffectError::Failed(cause))
            }
        },
        rca_core::IssueReporter::default(),
        move |cause| {
            assert_eq!(Rc::as_ptr(&cause.value), identity);
            factories.borrow_mut().push("issue");
            rca_core::Issue::new("child.failed", "mounted child operation failed")
        },
        move |report| {
            consumers.borrow_mut().push("outcome");
            *retained.borrow_mut() = Some(report);
        },
    )
    .map(|value| SequenceAction::Record(Rc::new(value)));
    assert!(events.borrow().is_empty());
    let mut effect = Some(effect);
    let descriptor = OptionalMount::new(
        |root: &RootState| root.0.as_ref(),
        |root: &mut RootState| root.0.as_mut(),
        |root: &RootState| {
            root.0.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| EffectChild(effect.take()),
    );
    let (builder, site) = Store::feature_builder(
        RootState(Some(Rc::new(Cell::new(1)))),
        Root,
        Runtime::new(driver.clone()),
    )
    .mount_with_site(descriptor);
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    let child = driver.finish(store.acquire(&site)).unwrap().unwrap();
    let task = driver.finish(child.send(SequenceAction::Start)).unwrap();
    assert!(events.borrow().is_empty());
    driver.turn();
    assert_eq!(*events.borrow(), ["operation"]);
    assert!(!task.is_finished());
    assert!(outcome.borrow().is_none());
    assert_eq!(drops.get(), 0);
    assert_eq!(driver.finish(child.read(|state| state.get())).unwrap(), 1);

    ready.set(true);
    driver.finish(task.finish());
    assert_eq!(driver.finish(child.read(|state| state.get())).unwrap(), 17);
    assert_eq!(*events.borrow(), ["operation", "issue", "outcome"]);
    let report = outcome.borrow_mut().take().unwrap();
    let cause = report.result.err().unwrap();
    assert_eq!(Rc::as_ptr(&cause.value), identity);
    let [rca_core::IssueDelivery::Disabled(issue)] = report.deliveries.as_slice() else {
        panic!("mounted failure lost its diagnostic evidence")
    };
    assert_eq!(issue.location.file(), file!());
    assert_eq!(issue.location.line(), line);
    assert_eq!(drops.get(), 0);
    drop(cause);
    assert_eq!(drops.get(), 1);
    assert!(weak.upgrade().is_none());
    drop(task);
    drop(child);
    drop(site);
    drop(store);
    driver.turn();
    assert_eq!(Rc::strong_count(&events), 1);
    assert_eq!(Rc::strong_count(&ready), 1);
}

/// Owns the branch at the first level of the nested recipe fixture.
struct Tree(Option<BranchState>);
/// Keeps the intermediate action type distinct from both parent and leaf.
struct BranchState(Option<InnerState>);
/// Holds the final optional Rc value below the intermediate recipe.
struct InnerState(Option<Rc<Cell<usize>>>);
/// Provides the root's distinct, intentionally inert action route.
struct TreeFeature;
/// Counts branch reductions independently from leaf mutations.
struct BranchFeature(Rc<Cell<usize>>);
/// Provides the intermediate bool action route without mutating its descendants.
struct InnerFeature;
impl Feature for TreeFeature {
    type State = Tree;
    type Action = RootAction;
    fn reduce(
        &mut self,
        _: &mut Tree,
        _: &RootAction,
        _: &mut FeatureContext<'_, Tree, RootAction>,
    ) {
    }
}
impl Feature for BranchFeature {
    type State = BranchState;
    type Action = u8;
    fn reduce(&mut self, _: &mut BranchState, _: &u8, _: &mut FeatureContext<'_, BranchState, u8>) {
        self.0.set(self.0.get() + 1);
    }
}
impl Feature for InnerFeature {
    type State = InnerState;
    type Action = bool;
    fn reduce(
        &mut self,
        _: &mut InnerState,
        _: &bool,
        _: &mut FeatureContext<'_, InnerState, bool>,
    ) {
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One nested Local recipe sequence preserves typed routes and postorder initial tasks"
)]
fn local_nested_recipes_preserve_typed_routes_and_postorder_initials() {
    let driver = Driver::default();
    let trace = Rc::new(RefCell::new(Vec::new()));
    let caches = Rc::new(Cell::new(0));
    let cache = std::rc::Rc::clone(&caches);
    let parent_trace = std::rc::Rc::clone(&trace);
    let descriptor = OptionalMount::new(
        |state: &Tree| state.0.as_ref(),
        |state: &mut Tree| state.0.as_mut(),
        |state| {
            state.0.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| BranchFeature(std::rc::Rc::clone(&cache)),
    )
    .on_initial(
        |_| Rc::new(0),
        move |_, _| {
            parent_trace.borrow_mut().push("branch");
            Task::run(|_| async {})
        },
    );
    let sites = Rc::new(RefCell::new(Vec::new()));
    let leaf_sites = std::rc::Rc::clone(&sites);
    let child_trace = std::rc::Rc::clone(&trace);
    let child_cache = std::rc::Rc::clone(&caches);
    let leaf = Rc::new(Cell::new(3));
    let state = Tree(Some(BranchState(Some(InnerState(Some(leaf))))));
    let (builder, parent_site) =
        Store::feature_builder(state, TreeFeature, Runtime::new(driver.clone()))
            .mount_with_children(descriptor, move |_, children| {
                let _actual_parent = children.site().clone();
                let factories = child_trace.borrow().len();
                assert_eq!(format!("{children:?}"), "MountChildren { depth: 0, .. }");
                assert_eq!(child_trace.borrow().len(), factories);
                let trace = std::rc::Rc::clone(&child_trace);
                let inner = OptionalMount::new(
                    |state: &BranchState| state.0.as_ref(),
                    |state: &mut BranchState| state.0.as_mut(),
                    |state| {
                        state.0.as_ref().map(|_| MountIdentity {
                            key: (),
                            replacement: 0,
                            configuration: 0,
                        })
                    },
                    |_| InnerFeature,
                )
                .on_initial(
                    |_| Rc::new(0),
                    move |_, _| {
                        trace.borrow_mut().push("inner");
                        Task::run(|_| async {})
                    },
                );
                let sites = std::rc::Rc::clone(&leaf_sites);
                let trace = std::rc::Rc::clone(&child_trace);
                let cache = std::rc::Rc::clone(&child_cache);
                children.optional_with_children(inner, move |_, children| {
                    let factories = trace.borrow().len();
                    assert_eq!(format!("{children:?}"), "MountChildren { depth: 1, .. }");
                    assert_eq!(trace.borrow().len(), factories);
                    let cache = std::rc::Rc::clone(&cache);
                    let trace = std::rc::Rc::clone(&trace);
                    let site = children.optional(
                        OptionalMount::new(
                            |state: &InnerState| state.0.as_ref(),
                            |state: &mut InnerState| state.0.as_mut(),
                            |state| {
                                state.0.as_ref().map(|_| MountIdentity {
                                    key: (),
                                    replacement: 0,
                                    configuration: 0,
                                })
                            },
                            move |_| Child(std::rc::Rc::clone(&cache)),
                        )
                        .on_initial(
                            |state| Rc::new(state.get()),
                            move |_, _| {
                                trace.borrow_mut().push("leaf");
                                Task::run(|_| async {})
                            },
                        ),
                    );
                    sites.borrow_mut().push(site);
                    assert_eq!(format!("{children:?}"), "MountChildren { depth: 1, .. }");
                });
                assert_eq!(format!("{children:?}"), "MountChildren { depth: 0, .. }");
            });
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    assert_eq!(*trace.borrow(), ["leaf", "inner", "branch"]);
    let child = driver
        .finish(store.acquire(&sites.borrow()[0]))
        .unwrap()
        .unwrap();
    assert_eq!(driver.finish(child.read(|value| value.get())).unwrap(), 3);
    driver.finish(child.update(|value| value.set(8))).unwrap();
    drop(driver.finish(child.send(ChildAction(Rc::new(2)))).unwrap());
    assert_eq!(driver.finish(child.read(|value| value.get())).unwrap(), 10);
    drop(parent_site);
    drop(store);
    driver.turn();
    assert_eq!(
        Rc::strong_count(&caches),
        1,
        "retained sites/context own only route metadata"
    );
}

#[test]
fn local_mounted_future_none_finishes_without_reduction_and_releases_capture() {
    let driver = Driver::default();
    let reductions = Rc::new(Cell::new(0));
    let calls = Rc::clone(&reductions);
    let starts = Rc::new(Cell::new(0));
    let observed = Rc::clone(&starts);
    let ready = Rc::new(Cell::new(false));
    let released = Rc::clone(&ready);
    let drops = Rc::new(Cell::new(0));
    let capture = ReportedCause {
        value: Rc::new(7),
        drops: Rc::clone(&drops),
    };
    let weak = Rc::downgrade(&capture.value);
    let mut effect = Some(Effect::<()>::future(async move {
        observed.set(observed.get() + 1);
        std::future::poll_fn(|_| {
            if released.get() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
        assert_eq!(*capture.value, 7);
        drop(capture);
        None
    }));
    let descriptor = OptionalMount::new(
        |root: &RootState| root.0.as_ref(),
        |root: &mut RootState| root.0.as_mut(),
        |root: &RootState| {
            root.0.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| {
            let mut effect = effect.take();
            let reductions = Rc::clone(&calls);
            rca_core::local::ReducerFeature::new(rca_core::local::Reduce::new(
                move |_: &mut Rc<Cell<usize>>, (): &(), _: &mut rca_core::ReduceContext<'_>| {
                    reductions.set(reductions.get() + 1);
                    effect.take().expect("one child start")
                },
            ))
        },
    );
    let state = Rc::new(Cell::new(4));
    let (builder, site) = Store::feature_builder(
        RootState(Some(Rc::clone(&state))),
        Root,
        Runtime::new(driver.clone()),
    )
    .mount_with_site(descriptor);
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    let child = driver.finish(store.acquire(&site)).unwrap().unwrap();
    let task = driver.finish(child.send(())).unwrap();
    assert_eq!((reductions.get(), starts.get(), drops.get()), (1, 0, 0));
    assert!(!task.is_finished());
    driver.turn();
    assert_eq!((reductions.get(), starts.get(), drops.get()), (1, 1, 0));
    assert!(weak.upgrade().is_some());
    assert!(!task.is_finished());
    assert_eq!(state.get(), 4);

    ready.set(true);
    driver.finish(task.finish());
    assert!(task.is_finished());
    assert!(!task.is_cancelled());
    assert_eq!((reductions.get(), starts.get(), drops.get()), (1, 1, 1));
    assert!(weak.upgrade().is_none());
    assert_eq!(driver.finish(child.read(|state| state.get())).unwrap(), 4);
    drop(task);
    drop(child);
    drop(site);
    drop(store);
    driver.turn();
    assert!(driver.tasks.borrow().is_empty());
    assert_eq!((reductions.get(), drops.get()), (1, 1));
    assert_eq!(Rc::strong_count(&state), 1);
}

#[test]
fn local_mounted_send_waiting_for_capacity_returns_action_after_actor_teardown() {
    let driver = Driver::default();
    let state = Rc::new(Cell::new(3));
    let reductions = Rc::new(Cell::new(0));
    let calls = Rc::clone(&reductions);
    let descriptor = OptionalMount::new(
        |root: &RootState| root.0.as_ref(),
        |root: &mut RootState| root.0.as_mut(),
        |root: &RootState| {
            root.0.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| {
            let reductions = Rc::clone(&calls);
            rca_core::local::ReducerFeature::new(rca_core::local::Reduce::new(
                move |state: &mut Rc<Cell<usize>>,
                      action: &ReportedCause,
                      _: &mut rca_core::ReduceContext<'_>| {
                    reductions.set(reductions.get() + 1);
                    state.set(state.get() + *action.value);
                    Effect::none()
                },
            ))
        },
    );
    let (builder, site) = Store::feature_builder(
        RootState(Some(Rc::clone(&state))),
        Root,
        Runtime::new(driver.clone()),
    )
    .mailbox_capacity(1)
    .mount_with_site(descriptor);
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    let child = driver.finish(store.acquire(&site)).unwrap().unwrap();
    let occupied = store.try_send(RootAction).unwrap();
    assert!(store.try_send(RootAction).unwrap_err().is_full());
    let drops = Rc::new(Cell::new(0));
    let action = ReportedCause {
        value: Rc::new(23),
        drops: Rc::clone(&drops),
    };
    let identity = Rc::as_ptr(&action.value);
    let weak = Rc::downgrade(&action.value);
    let mut send = Box::pin(child.send(action));
    let mut context = Context::from_waker(Waker::noop());
    assert!(send.as_mut().poll(&mut context).is_pending());
    assert_eq!((reductions.get(), drops.get(), state.get()), (0, 0, 3));

    drop(store);
    driver.turn();
    assert!(driver.tasks.borrow().is_empty());
    assert_eq!(Rc::strong_count(&state), 1);
    assert_eq!(Rc::strong_count(&reductions), 1);
    assert_eq!(drops.get(), 0);
    let Poll::Ready(Err(rca_core::TaskSendError::Rejected { action, error })) =
        send.as_mut().poll(&mut context)
    else {
        panic!("unpublished mounted send must return its action after actor teardown");
    };
    drop(send);
    assert_eq!(error.reason(), rca_core::TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
    assert_eq!(Rc::as_ptr(&action.value), identity);
    assert_eq!(*action.value, 23);
    assert_eq!(Rc::strong_count(&action.value), 1);
    assert_eq!((reductions.get(), drops.get(), state.get()), (0, 0, 3));
    drop(action);
    assert!(weak.upgrade().is_none());
    assert_eq!(drops.get(), 1);
    drop(occupied);
    drop(child);
    drop(site);
    driver.turn();
    assert!(driver.tasks.borrow().is_empty());
    assert_eq!((reductions.get(), drops.get(), state.get()), (0, 1, 3));
}

#[cfg(feature = "test-util")]
#[test]
fn local_closed_and_ancestor_stale_contexts_preserve_live_mounted_frontier() {
    use rca_core::testing::local::TransactionGate;

    let driver = Driver::default();
    let reductions = Rc::new(Cell::new(0));
    let constructions = Rc::new(Cell::new(0));
    let release = |gate: &TransactionGate<RootState, RootAction>| {
        let boundary = gate.boundary().unwrap();
        let receipt = gate.release_pending_with_receipt().unwrap();
        drop(driver.finish(receipt.finish()).unwrap());
        assert!(gate.acknowledge(boundary));
        driver.turn();
    };
    let mounted = |gate: &TransactionGate<RootState, RootAction>,
                   state: Rc<Cell<usize>>,
                   configuration: Rc<Cell<u64>>| {
        let calls = Rc::clone(&reductions);
        let created = Rc::clone(&constructions);
        let descriptor = OptionalMount::new(
            |root: &RootState| root.0.as_ref(),
            |root: &mut RootState| root.0.as_mut(),
            |root: &RootState| {
                root.0.as_ref().map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            move |_| {
                created.set(created.get() + 1);
                Child(Rc::clone(&calls))
            },
        );
        let (builder, site) = rca_core::local::RootMount::new(Root)
            .configuration(move |_| configuration.get())
            .builder(RootState(Some(state)), Runtime::new(driver.clone()))
            .mount_with_site(descriptor);
        let store = builder.transaction_gate(gate.clone()).build().unwrap();
        driver.turn();
        release(gate);
        (store, site)
    };
    let state = Rc::new(Cell::new(3));
    let configuration = Rc::new(Cell::new(0));
    let gate = TransactionGate::new();
    let (store, site) = mounted(&gate, Rc::clone(&state), Rc::clone(&configuration));
    let outdated = driver.finish(gate.acquire(&store, &site)).unwrap().unwrap();
    configuration.set(1);
    let mut configure = Box::pin(store.send(RootAction));
    let mut context = Context::from_waker(Waker::noop());
    assert!(configure.as_mut().poll(&mut context).is_pending());
    driver.turn();
    release(&gate);
    drop(driver.finish(configure).unwrap());
    let current = driver.finish(gate.acquire(&store, &site)).unwrap().unwrap();
    let foreign_gate = TransactionGate::new();
    let (foreign_store, foreign_site) =
        mounted(&foreign_gate, Rc::new(Cell::new(11)), Rc::new(Cell::new(0)));
    let acquisition = foreign_gate.acquire(&foreign_store, &foreign_site);
    let closed = driver.finish(acquisition).unwrap().unwrap();
    drop(foreign_store);
    driver.turn();
    let action = Rc::new(2);
    let mut send = Box::pin(current.send(ChildAction(Rc::clone(&action))));
    assert!(send.as_mut().poll(&mut context).is_pending());
    driver.turn();
    let boundary = gate.boundary().unwrap();
    for (invalid, reason) in [
        (&closed, rca_core::TaskAccessReason::StoreClosed),
        (&outdated, rca_core::TaskAccessReason::ScopeEnded),
    ] {
        let error = gate
            .inspect_mounted(invalid, |_| panic!("invalid mounted matcher must not run"))
            .unwrap_err();
        assert_eq!(error.reason(), reason);
        assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
        assert_eq!(gate.boundary(), Some(boundary));
        assert!(gate.has_pending());
        assert!(!gate.has_failed_match());
        assert!(send.as_mut().poll(&mut context).is_pending());
        assert_eq!((reductions.get(), state.get()), (0, 3));
        assert_eq!(Rc::strong_count(&action), 2);
        assert_eq!(
            gate.inspect_mounted(&current, |held| {
                assert!(Rc::ptr_eq(&held.0, &action));
                *held.0
            }),
            Ok(Some(2))
        );
        assert_eq!(gate.boundary(), Some(boundary));
        assert!(gate.has_pending());
    }
    assert_eq!(constructions.get(), 2);
    release(&gate);
    drop(driver.finish(send).unwrap());
    assert_eq!((reductions.get(), state.get()), (1, 5));
    assert_eq!(Rc::strong_count(&action), 1);
    drop(store);
    driver.turn();
    assert!(driver.tasks.borrow().is_empty());
    assert_eq!(Rc::strong_count(&state), 1);
}
