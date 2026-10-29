//! Real actor commits capture owned changes without parent actions or inline task polling.

#[path = "changed_reactions/boundaries.rs"]
mod boundaries;
#[path = "changed_reactions/keyed.rs"]
mod keyed;
#[path = "changed_reactions/lifetimes.rs"]
mod lifetimes;
#[macro_use]
#[path = "changed_reactions/closing.rs"]
mod closing;
closing_cases!(closing_shared, rca_core);
#[macro_use]
#[path = "changed_reactions/root_closing.rs"]
mod root_closing;
root_closing_cases!(root_closing_shared, rca_core);

use rca_core::ReactionChange;
use rca_core::{
    Feature, FeatureContext, InitialPolicy, MountIdentity, OptionalMount, RootMount, Runtime,
    RuntimeDriver, RuntimeFuture, Task,
};
use std::cell::Cell;
use std::rc::Rc;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::task::{Context, Poll, Waker};

/// Polls queued actors explicitly while keeping the clock fixed at zero.
#[derive(Clone, Default)]
struct Driver(Arc<Mutex<Vec<RuntimeFuture>>>);
impl std::fmt::Debug for Driver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ReactionDriver")
    }
}
impl RuntimeDriver for Driver {
    fn spawn(&self, task: RuntimeFuture) {
        self.0.lock().unwrap().push(task);
    }
    fn now(&self) -> rca_core::MonotonicInstant {
        rca_core::MonotonicInstant::ZERO
    }
    fn sleep_until(&self, deadline: rca_core::MonotonicInstant) -> RuntimeFuture {
        if deadline <= self.now() {
            Box::pin(std::future::ready(()))
        } else {
            Box::pin(std::future::pending())
        }
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
#[test]
fn manual_driver_clock_waits_respect_the_fixed_origin() {
    let driver = Driver::default();
    assert_eq!(driver.now(), rca_core::MonotonicInstant::ZERO);
    let mut context = Context::from_waker(Waker::noop());
    let mut elapsed = driver.sleep_until(rca_core::MonotonicInstant::ZERO);
    assert!(elapsed.as_mut().poll(&mut context).is_ready());
    for deadline in [
        rca_core::MonotonicInstant::from_elapsed(std::time::Duration::from_nanos(1)),
        rca_core::MonotonicInstant::MAX,
    ] {
        let mut future = driver.sleep_until(deadline);
        assert!(future.as_mut().poll(&mut context).is_pending());
    }
    assert!(driver.0.lock().unwrap().is_empty());
}

#[test]
fn manual_driver_yield_suspends_and_wakes_before_completion() {
    /// Counts scheduling requests without polling the actor from the waker.
    struct WakeCount(AtomicUsize);
    impl std::task::Wake for WakeCount {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let driver = Driver::default();
    let notifications = Arc::new(WakeCount(AtomicUsize::new(0)));
    let waker = Waker::from(Arc::clone(&notifications));
    let mut context = Context::from_waker(&waker);
    let mut future = driver.yield_now();
    assert_eq!(notifications.0.load(Ordering::SeqCst), 0);
    assert!(future.as_mut().poll(&mut context).is_pending());
    assert_eq!(notifications.0.load(Ordering::SeqCst), 1);
    assert!(future.as_mut().poll(&mut context).is_ready());
    assert_eq!(notifications.0.load(Ordering::SeqCst), 1);
    assert!(driver.0.lock().unwrap().is_empty());
}

impl Driver {
    /// Polls each previously queued task once, without holding the queue lock.
    fn turn(&self) {
        let tasks = std::mem::take(&mut *self.0.lock().unwrap());
        for mut task in tasks {
            if task
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
            {
                self.0.lock().unwrap().push(task);
            }
        }
    }
    /// Drives one finite operation, failing rather than hiding a stalled actor.
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
        panic!("finite reaction operation did not finish")
    }
}

struct State {
    child: Option<Cell<i32>>,
}
/// Counts parent reductions so child commits cannot silently become parent actions.
struct Root(Arc<AtomicUsize>);
struct Child(Cell<usize>);
struct ParentAction;
struct ChildAction(i32);
/// Non-`Clone`, non-`Send` history consumed before the factory returns `Send` work.
/// The state and feature also deliberately omit `Clone`.
#[derive(PartialEq)]
struct Selected(Rc<i32>);
impl Feature for Root {
    type State = State;
    type Action = ParentAction;
    fn reduce(
        &mut self,
        _: &mut State,
        _: &ParentAction,
        _: &mut FeatureContext<'_, State, ParentAction>,
    ) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
impl Feature for Child {
    type State = Cell<i32>;
    type Action = ChildAction;
    fn reduce(
        &mut self,
        state: &mut Cell<i32>,
        action: &ChildAction,
        _: &mut FeatureContext<'_, Cell<i32>, ChildAction>,
    ) {
        self.0.set(self.0.get() + 1);
        state.set(state.get() + action.0);
    }
}
/// Hook order with an absent previous value only for initial delivery.
type Trace = Arc<Mutex<Vec<(&'static str, Option<i32>, i32)>>>;
/// Retains both owned values in the literal history oracle before dropping them.
fn record(trace: &Trace, name: &'static str, change: ReactionChange<Selected>) {
    let event = match change {
        ReactionChange::Initial { current } => (name, None, *current.0),
        ReactionChange::Changed { previous, current } => (name, Some(*previous.0), *current.0),
    };
    trace.lock().unwrap().push(event);
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One reaction sequence preserves owned child updates, postorder and per-hook initial policy"
)]
fn changed_child_updates_are_owned_postorder_and_initial_policy_is_per_hook() {
    let driver = Driver::default();
    let trace = Trace::default();
    let reductions = Arc::new(AtomicUsize::new(0));
    let bodies = Arc::new(AtomicUsize::new(0));
    let skip_selects = Arc::new(AtomicUsize::new(0));
    let explicit = std::sync::Arc::clone(&trace);
    let run = std::sync::Arc::clone(&trace);
    let skip = std::sync::Arc::clone(&trace);
    let selected = std::sync::Arc::clone(&skip_selects);
    let child_bodies = std::sync::Arc::clone(&bodies);
    let child = OptionalMount::new(
        |state: &State| state.child.as_ref(),
        |state: &mut State| state.child.as_mut(),
        |state| {
            state.child.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        |_| Child(Cell::new(0)),
    )
    .on_initial(
        |state| Selected(Rc::new(state.get())),
        move |change, context| {
            // Formatting must not expose dependencies or require domain Debug.
            assert_eq!(format!("{context:?}"), "ReactionContext { .. }");
            record(&explicit, "explicit", change);
            Task::run(|_| async {})
        },
    )
    .on_change(
        |state| Selected(Rc::new(state.get())),
        InitialPolicy::Run,
        move |change, _| {
            record(&run, "child run", change);
            let bodies = std::sync::Arc::clone(&child_bodies);
            Task::run(move |_| async move {
                bodies.fetch_add(1, Ordering::SeqCst);
            })
        },
    )
    .on_change(
        move |state| {
            selected.fetch_add(1, Ordering::SeqCst);
            Selected(Rc::new(state.get()))
        },
        InitialPolicy::Skip,
        move |change, _| {
            record(&skip, "child skip", change);
            Task::run(|_| async {})
        },
    );
    let parent_trace = std::sync::Arc::clone(&trace);
    let (builder, site) = RootMount::new(Root(std::sync::Arc::clone(&reductions)))
        .on_change(
            |state| Selected(Rc::new(state.child.as_ref().unwrap().get())),
            InitialPolicy::Skip,
            move |change, _| {
                record(&parent_trace, "root", change);
                Task::run(|_| async {})
            },
        )
        .builder(
            State {
                child: Some(Cell::new(4)),
            },
            Runtime::new(driver.clone()),
        )
        .mount_with_site(child);
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    assert_eq!(
        *trace.lock().unwrap(),
        [("explicit", None, 4), ("child run", None, 4)]
    );
    assert_eq!(skip_selects.load(Ordering::SeqCst), 0);
    assert_eq!(
        bodies.load(Ordering::SeqCst),
        0,
        "readiness does not poll reaction tasks"
    );
    let child = driver.finish(store.acquire(&site)).unwrap().unwrap();
    trace.lock().unwrap().clear();
    let prior_bodies = bodies.load(Ordering::SeqCst);
    driver.finish(child.update(|state| state.set(5))).unwrap();
    assert_eq!(
        *trace.lock().unwrap(),
        [
            ("child run", Some(4), 5),
            ("child skip", Some(4), 5),
            ("root", Some(4), 5),
        ]
    );
    assert_eq!(skip_selects.load(Ordering::SeqCst), 2);
    assert_eq!(reductions.load(Ordering::SeqCst), 0);
    assert_eq!(
        bodies.load(Ordering::SeqCst),
        prior_bodies,
        "source commit returns before new work polls"
    );
    trace.lock().unwrap().clear();
    driver.finish(child.update(|state| state.set(5))).unwrap();
    assert!(
        trace.lock().unwrap().is_empty(),
        "equal endpoints suppress factories"
    );
    let _ = driver.finish(child.send(ChildAction(-1))).unwrap();
    assert_eq!(
        *trace.lock().unwrap(),
        [
            ("child run", Some(5), 4),
            ("child skip", Some(5), 4),
            ("root", Some(5), 4),
        ]
    );
    drop(store);
    driver.turn();
}

#[test]
fn mixed_equal_and_changed_hooks_share_pairing_but_only_changed_work_gets_a_route() {
    let driver = Driver::default();
    let selections = Arc::new(AtomicUsize::new(0));
    let parity_selections = Arc::clone(&selections);
    let value_selections = Arc::clone(&selections);
    let routed = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&routed);
    let mount = OptionalMount::new(
        |state: &State| state.child.as_ref(),
        |state: &mut State| state.child.as_mut(),
        |state| {
            state.child.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        |_| Child(Cell::new(0)),
    )
    .on_change(
        move |state| {
            parity_selections.fetch_add(1, Ordering::SeqCst);
            state.get() % 2
        },
        InitialPolicy::Skip,
        |_, _| panic!("equal parity does not create work"),
    )
    .on_change(
        move |state| {
            value_selections.fetch_add(1, Ordering::SeqCst);
            Selected(Rc::new(state.get()))
        },
        InitialPolicy::Skip,
        move |change, _| {
            let ReactionChange::Changed { previous, current } = change else {
                panic!("non-initial")
            };
            assert_eq!((*previous.0, *current.0), (4, 6));
            let routed = Arc::clone(&observed);
            Task::run(move |context| async move {
                assert_eq!(context.read(Cell::get).await.unwrap(), 6);
                routed.fetch_add(1, Ordering::SeqCst);
            })
        },
    );
    let (builder, site) = rca_core::Store::feature_builder(
        State {
            child: Some(Cell::new(4)),
        },
        Root(Arc::new(AtomicUsize::new(0))),
        Runtime::new(driver.clone()),
    )
    .mount_with_site(mount);
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    assert_eq!(selections.load(Ordering::SeqCst), 0);
    let child = driver.finish(store.acquire(&site)).unwrap().unwrap();
    let task = driver.finish(child.send(ChildAction(2))).unwrap();
    assert_eq!(selections.load(Ordering::SeqCst), 4);
    driver.finish(task.finish());
    assert_eq!(routed.load(Ordering::SeqCst), 1);
    drop(store);
    driver.turn();
}

#[cfg(feature = "local")]
mod local {
    //! Repeats reaction ownership checks with genuinely non-`Send` state and work.
    closing_cases!(closing_local, rca_core::local);
    root_closing_cases!(root_closing_local, rca_core::local);
    use rca_core::ReactionChange;
    use rca_core::local::{
        Feature, FeatureContext, OptionalMount, Runtime, RuntimeDriver, RuntimeFuture, Store, Task,
    };
    use rca_core::{InitialPolicy, MountIdentity};
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    use std::task::{Context, Poll, Waker};

    /// Owner-thread queue with a fixed clock and explicit actor turns.
    #[derive(Clone, Default)]
    struct Driver(Rc<RefCell<Vec<RuntimeFuture>>>);
    impl std::fmt::Debug for Driver {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("LocalReactionDriver")
        }
    }
    impl RuntimeDriver for Driver {
        fn spawn(&self, task: RuntimeFuture) {
            self.0.borrow_mut().push(task);
        }
        fn now(&self) -> rca_core::MonotonicInstant {
            rca_core::MonotonicInstant::ZERO
        }
        fn sleep_until(&self, deadline: rca_core::MonotonicInstant) -> RuntimeFuture {
            if deadline <= self.now() {
                Box::pin(std::future::ready(()))
            } else {
                Box::pin(std::future::pending())
            }
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
    #[test]
    fn manual_driver_clock_waits_respect_the_fixed_origin() {
        let driver = Driver::default();
        assert_eq!(driver.now(), rca_core::MonotonicInstant::ZERO);
        let mut context = Context::from_waker(Waker::noop());
        let mut elapsed = driver.sleep_until(rca_core::MonotonicInstant::ZERO);
        assert!(elapsed.as_mut().poll(&mut context).is_ready());
        for deadline in [
            rca_core::MonotonicInstant::from_elapsed(std::time::Duration::from_nanos(1)),
            rca_core::MonotonicInstant::MAX,
        ] {
            let mut future = driver.sleep_until(deadline);
            assert!(future.as_mut().poll(&mut context).is_pending());
        }
        assert!(driver.0.borrow().is_empty());
    }

    #[test]
    fn manual_driver_yield_suspends_and_wakes_before_completion() {
        /// Counts wake requests without executing the owner-thread task.
        struct WakeCount(std::sync::atomic::AtomicUsize);
        impl std::task::Wake for WakeCount {
            fn wake(self: std::sync::Arc<Self>) {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let driver = Driver::default();
        let notifications = std::sync::Arc::new(WakeCount(std::sync::atomic::AtomicUsize::new(0)));
        let waker = Waker::from(std::sync::Arc::clone(&notifications));
        let mut context = Context::from_waker(&waker);
        let mut future = driver.yield_now();
        assert_eq!(notifications.0.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(future.as_mut().poll(&mut context).is_pending());
        assert_eq!(notifications.0.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(future.as_mut().poll(&mut context).is_ready());
        assert_eq!(notifications.0.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(driver.0.borrow().is_empty());
    }

    impl Driver {
        /// Releases the queue borrow before polling user work, which may spawn.
        fn turn(&self) {
            let tasks = std::mem::take(&mut *self.0.borrow_mut());
            for mut task in tasks {
                if task
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
                {
                    self.0.borrow_mut().push(task);
                }
            }
        }
        /// Bounds actor progress while awaiting one finite operation.
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
            panic!("finite Local reaction operation")
        }
    }
    struct State(Option<Rc<Cell<i32>>>);
    struct Root;
    struct Child;
    impl Feature for Root {
        type State = State;
        type Action = ();
        fn reduce(&mut self, _: &mut State, (): &(), _: &mut FeatureContext<'_, State, ()>) {}
    }
    impl Feature for Child {
        type State = Rc<Cell<i32>>;
        type Action = Rc<i32>;
        fn reduce(
            &mut self,
            state: &mut Self::State,
            action: &Self::Action,
            _: &mut FeatureContext<'_, Self::State, Self::Action>,
        ) {
            state.set(state.get() + **action);
        }
    }
    /// Keeps owned `Rc` history inside Local work, without adding `Send` or `Clone` bounds.
    #[derive(PartialEq)]
    struct Selected(Rc<i32>);

    /// Injects equality or old-history destruction faults in the five-phase matrix.
    struct PanicProjection {
        value: i32,
        /// Two faults in equality; four faults when the old value is destroyed.
        phase: usize,
    }
    impl PartialEq for PanicProjection {
        fn eq(&self, other: &Self) -> bool {
            assert_ne!(self.phase, 2, "Local equality panic");
            self.value == other.value
        }
    }
    impl Drop for PanicProjection {
        fn drop(&mut self) {
            assert!(
                !(self.phase == 4 && self.value == 4),
                "Local old history panic"
            );
        }
    }

    #[test]
    fn local_projection_equality_factory_and_drop_panics_keep_entered_progress() {
        for phase in 0..5 {
            let driver = Driver::default();
            let selections = Rc::new(Cell::new(0));
            let selected = std::rc::Rc::clone(&selections);
            let mutations = Rc::new(Cell::new(0));
            let mutated = std::rc::Rc::clone(&mutations);
            let mount = OptionalMount::new(
                |state: &State| state.0.as_ref(),
                |state: &mut State| state.0.as_mut(),
                |state| {
                    state.0.as_ref().map(|_| MountIdentity {
                        key: (),
                        replacement: 0,
                        configuration: 0,
                    })
                },
                |_| Child,
            )
            .on_change(
                move |state| {
                    let index = selected.replace(selected.get() + 1);
                    assert!(
                        !((phase == 0 && index == 0) || (phase == 1 && index == 1)),
                        "Local selector panic"
                    );
                    PanicProjection {
                        value: state.get(),
                        phase,
                    }
                },
                InitialPolicy::Skip,
                move |change, _| {
                    assert_ne!(phase, 3, "Local factory panic");
                    drop(change);
                    Task::run(|_| async {})
                },
            );
            let (builder, site) = Store::feature_builder(
                State(Some(Rc::new(Cell::new(4)))),
                Root,
                Runtime::new(driver.clone()),
            )
            .mount_with_site(mount);
            let store = builder.build().unwrap();
            driver.finish(store.ready()).unwrap();
            let child = driver.finish(store.acquire(&site)).unwrap().unwrap();
            let mut update = Box::pin(child.update(move |state| {
                mutated.set(mutated.get() + 1);
                state.set(5);
            }));
            assert!(
                update
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| driver.turn())).is_err()
            );
            let error = driver.finish(update).unwrap_err();
            assert_eq!(error.reason(), rca_core::TaskAccessReason::ActorFailed);
            assert_eq!(error.progress(), rca_core::TaskProgress::MayHaveRun);
            assert_eq!(mutations.get(), usize::from(phase != 0));
            drop(store);
            driver.turn();
        }
    }

    /// Holds the last public store handle so a real callback can close the root.
    type Owner = Rc<RefCell<Option<Store<State, ()>>>>;
    /// Closes the root when unmatched selected history is discarded.
    struct ClosingProjection {
        value: i32,
        owner: Owner,
    }
    impl PartialEq for ClosingProjection {
        fn eq(&self, other: &Self) -> bool {
            self.value == other.value
        }
    }
    impl Drop for ClosingProjection {
        fn drop(&mut self) {
            drop(self.owner.borrow_mut().take());
        }
    }
    struct DropCount(Rc<Cell<usize>>);
    impl Drop for DropCount {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    #[test]
    fn local_closing_initial_factories_never_create_fresh_authority_or_start_work() {
        for changed_initial in [false, true] {
            let driver = Driver::default();
            let owner = Owner::default();
            let callback_owner = std::rc::Rc::clone(&owner);
            let bodies = Rc::new(Cell::new(0));
            let drops = Rc::new(Cell::new(0));
            let task_bodies = std::rc::Rc::clone(&bodies);
            let task_drops = std::rc::Rc::clone(&drops);
            let mount = OptionalMount::new(
                |state: &State| state.0.as_ref(),
                |state: &mut State| state.0.as_mut(),
                |state| {
                    state.0.as_ref().map(|_| MountIdentity {
                        key: (),
                        replacement: 0,
                        configuration: 0,
                    })
                },
                |_| Child,
            );
            let factory =
                move |_: ReactionChange<i32>,
                      _: rca_core::ReactionContext<'_, Rc<Cell<i32>>, Rc<i32>>| {
                    drop(callback_owner.borrow_mut().take());
                    let bodies = std::rc::Rc::clone(&task_bodies);
                    let guard = DropCount(std::rc::Rc::clone(&task_drops));
                    Task::run(move |_| async move {
                        bodies.set(bodies.get() + 1);
                        drop(guard);
                    })
                };
            let mount = if changed_initial {
                mount.on_change(|value| value.get(), InitialPolicy::Run, factory)
            } else {
                mount.on_initial(|value| value.get(), factory)
            };
            let store = Store::feature_builder(
                State(Some(Rc::new(Cell::new(4)))),
                Root,
                Runtime::new(driver.clone()),
            )
            .mount(mount)
            .build()
            .unwrap();
            *owner.borrow_mut() = Some(store);
            driver.turn();
            assert!(owner.borrow().is_none());
            assert_eq!(drops.get(), 1);
            driver.turn();
            assert_eq!(bodies.get(), 0);
        }
    }

    #[test]
    fn local_unmatched_history_closes_root_before_returned_work_registration() {
        let driver = Driver::default();
        let owner = Owner::default();
        let selected_owner = std::rc::Rc::clone(&owner);
        let factories = Rc::new(Cell::new(0));
        let bodies = Rc::new(Cell::new(0));
        let drops = Rc::new(Cell::new(0));
        let factory_count = std::rc::Rc::clone(&factories);
        let task_bodies = std::rc::Rc::clone(&bodies);
        let task_drops = std::rc::Rc::clone(&drops);
        let mount = OptionalMount::new(
            |state: &State| state.0.as_ref(),
            |state: &mut State| state.0.as_mut(),
            |state| {
                state.0.as_ref().map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            |_| Child,
        )
        .on_change(
            move |state| ClosingProjection {
                value: state.get(),
                owner: std::rc::Rc::clone(&selected_owner),
            },
            InitialPolicy::Skip,
            |_, _| panic!("removed Local child cannot react"),
        );
        let (send, receive) = futures_channel::oneshot::channel();
        let mut send = Some(send);
        let store = rca_core::local::RootMount::new(Root)
            .on_initial(
                |_| (),
                move |_, _| {
                    let send = send.take().unwrap();
                    Task::run(move |context| async move {
                        let _ = send.send(context);
                        std::future::pending::<()>().await;
                    })
                },
            )
            .on_change(
                |state| state.0.is_some(),
                InitialPolicy::Skip,
                move |_, _| {
                    factory_count.set(factory_count.get() + 1);
                    let bodies = std::rc::Rc::clone(&task_bodies);
                    let guard = DropCount(std::rc::Rc::clone(&task_drops));
                    Task::run(move |_| async move {
                        bodies.set(bodies.get() + 1);
                        drop(guard);
                    })
                },
            )
            .builder(
                State(Some(Rc::new(Cell::new(4)))),
                Runtime::new(driver.clone()),
            )
            .mount(mount)
            .build()
            .unwrap();
        driver.finish(store.ready()).unwrap();
        let context = driver.finish(receive).unwrap();
        *owner.borrow_mut() = Some(store);
        let committed = driver
            .finish(context.update(|state| state.0 = None))
            .unwrap();
        assert!(committed.revision() > 0);
        assert!(owner.borrow().is_none());
        assert_eq!(factories.get(), 1);
        assert_eq!(drops.get(), 1);
        driver.turn();
        assert_eq!(bodies.get(), 0);
    }

    #[test]
    fn local_changed_uses_real_actor_and_owned_rc_projections() {
        let driver = Driver::default();
        let trace = Rc::new(RefCell::new(Vec::new()));
        let factory_trace = std::rc::Rc::clone(&trace);
        let mount = OptionalMount::new(
            |state: &State| state.0.as_ref(),
            |state: &mut State| state.0.as_mut(),
            |state| {
                state.0.as_ref().map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            |_| Child,
        )
        .on_change(
            |state| Selected(Rc::new(state.get())),
            InitialPolicy::Skip,
            move |change, _| {
                let ReactionChange::Changed { previous, current } = change else {
                    panic!("non-initial hook")
                };
                factory_trace.borrow_mut().push((*previous.0, *current.0));
                Task::run(move |_| async move {
                    drop((previous, current));
                })
            },
        );
        let (builder, site) = Store::feature_builder(
            State(Some(Rc::new(Cell::new(4)))),
            Root,
            Runtime::new(driver.clone()),
        )
        .mount_with_site(mount);
        let store = builder.build().unwrap();
        driver.finish(store.ready()).unwrap();
        assert!(trace.borrow().is_empty());
        let child = driver.finish(store.acquire(&site)).unwrap().unwrap();
        driver.finish(child.update(|state| state.set(5))).unwrap();
        assert_eq!(*trace.borrow(), [(4, 5)]);
        let _ = driver.finish(child.send(Rc::new(2))).unwrap();
        assert_eq!(*trace.borrow(), [(4, 5), (5, 7)]);
        drop(store);
        driver.turn();
    }

    #[test]
    fn local_root_reacts_last_and_configuration_refresh_discards_old_history() {
        use rca_core::local::RootMount;
        let driver = Driver::default();
        let trace = Rc::new(RefCell::new(Vec::new()));
        let child_trace = std::rc::Rc::clone(&trace);
        let explicit_trace = std::rc::Rc::clone(&trace);
        let root_trace = std::rc::Rc::clone(&trace);
        let selected = |state: &State| Selected(Rc::new(state.0.as_ref().unwrap().get()));
        let mount = OptionalMount::new(
            |state: &State| state.0.as_ref(),
            |state: &mut State| state.0.as_mut(),
            |state| {
                state.0.as_ref().map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            |_| Child,
        )
        .on_change(
            |state| Selected(Rc::new(state.get())),
            InitialPolicy::Run,
            move |change, _| {
                child_trace
                    .borrow_mut()
                    .push(("child", matches!(change, ReactionChange::Initial { .. })));
                Task::run(move |_| async move {
                    drop(change);
                })
            },
        );
        let (builder, site) = RootMount::new(Root)
            .configuration(|state| u64::try_from(state.0.as_ref().unwrap().get()).unwrap() / 10)
            .on_initial(selected, move |change, _| {
                explicit_trace
                    .borrow_mut()
                    .push(("explicit", matches!(change, ReactionChange::Initial { .. })));
                Task::run(move |_| async move {
                    drop(change);
                })
            })
            .on_change(selected, InitialPolicy::Run, move |change, _| {
                root_trace
                    .borrow_mut()
                    .push(("root", matches!(change, ReactionChange::Initial { .. })));
                Task::run(move |_| async move {
                    drop(change);
                })
            })
            .builder(
                State(Some(Rc::new(Cell::new(4)))),
                Runtime::new(driver.clone()),
            )
            .mount_with_site(mount);
        let store = builder.build().unwrap();
        driver.finish(store.ready()).unwrap();
        assert_eq!(
            *trace.borrow(),
            [("child", true), ("explicit", true), ("root", true)]
        );
        trace.borrow_mut().clear();
        let child = driver.finish(store.acquire(&site)).unwrap().unwrap();
        driver.finish(child.update(|state| state.set(5))).unwrap();
        assert_eq!(*trace.borrow(), [("child", false), ("root", false)]);
        trace.borrow_mut().clear();
        driver.finish(child.update(|state| state.set(15))).unwrap();
        assert!(
            trace.borrow().is_empty(),
            "root configuration invalidates every old authority without re-running Initial"
        );
        let fresh = driver.finish(store.acquire(&site)).unwrap().unwrap();
        driver.finish(fresh.update(|state| state.set(16))).unwrap();
        assert_eq!(*trace.borrow(), [("child", false), ("root", false)]);
        drop(store);
        driver.turn();
    }
}
