//! Checks owner-local removal with non-`Send` values and independent lifetime witnesses.
#![cfg(feature = "local")]

use rca_core::local::{
    Feature, FeatureContext, MountSite, OptionalMount, RemovalSite, Runtime, RuntimeDriver,
    RuntimeFuture, Store, TaskContext,
};
use rca_core::{MonotonicInstant, MountIdentity, OutputDelivery, OutputFlow};
use std::cell::{Cell, RefCell};
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

#[cfg(feature = "allocation-measurement")]
#[global_allocator]
/// Counts allocations inside the isolated removal-cost test.
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

/// Runs the existing allocation assay in its own process.
#[cfg(feature = "allocation-measurement")]
#[path = "../src/allocation_test/child.rs"]
mod allocation_test;

/// Drives explicit local task turns and records accepted spawns.
///
/// The clock stays at zero. Callers must drive shutdown or drop extracted tasks
/// before dropping the last driver handle. Queued actor tasks retain this driver.
#[derive(Clone, Default)]
struct Driver(
    /// Tasks owned by the host until a turn completes or takes them.
    Rc<RefCell<Vec<RuntimeFuture>>>,
    /// Accepted spawns, independent from completed or cancelled work.
    Rc<Cell<usize>>,
    /// One callback taken after the next spawn releases its queue borrow.
    DropHook,
    /// One callback taken when the mounted child feature is destroyed.
    DropHook,
);

/// Shares one callback without invoking it while its slot is borrowed.
type DropHook = Rc<RefCell<Option<Box<dyn FnOnce()>>>>;

impl std::fmt::Debug for Driver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LocalRemovalDriver")
    }
}

impl RuntimeDriver for Driver {
    fn spawn(&self, task: RuntimeFuture) {
        self.1.set(self.1.get() + 1);
        self.0.borrow_mut().push(task);
        let hook = self.2.borrow_mut().take();
        if let Some(hook) = hook {
            hook();
        }
    }

    fn now(&self) -> MonotonicInstant {
        MonotonicInstant::ZERO
    }

    fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
        if deadline <= self.now() {
            Box::pin(std::future::ready(()))
        } else {
            Box::pin(std::future::pending())
        }
    }

    fn yield_now(&self) -> RuntimeFuture {
        let mut yielded = false;
        Box::pin(std::future::poll_fn(move |cx| {
            if yielded {
                Poll::Ready(())
            } else {
                yielded = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }))
    }
}

impl Driver {
    /// Polls the current task batch without retaining a queue borrow.
    fn turn(&self) {
        let pending = std::mem::take(&mut *self.0.borrow_mut());
        for mut future in pending {
            if poll(future.as_mut()).is_pending() {
                self.0.borrow_mut().push(future);
            }
        }
    }

    /// Drives a finite operation, failing after 48 unsuccessful caller polls.
    ///
    /// The allocation assay deliberately includes this box and each turn's queue work.
    fn finish<F: Future>(&self, future: F) -> F::Output {
        let mut future = Box::pin(future);
        for _ in 0..48 {
            if let Poll::Ready(value) = poll(future.as_mut()) {
                return value;
            }
            self.turn();
        }
        panic!("finite Local removal did not finish");
    }
}

/// Polls once without scheduling an automatic host turn.
fn poll<F: Future + ?Sized>(future: Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}

/// Completes elapsed sleeps while future and maximum deadlines remain pending.
#[test]
fn removal_driver_sleep_respects_its_fixed_clock() {
    let driver = Driver::default();
    assert_eq!(driver.now(), MonotonicInstant::ZERO);
    for deadline in [
        MonotonicInstant::ZERO.saturating_add(std::time::Duration::from_nanos(1)),
        MonotonicInstant::MAX,
    ] {
        let mut sleep = driver.sleep_until(deadline);
        assert!(poll(sleep.as_mut()).is_pending());
        assert!(poll(sleep.as_mut()).is_pending());
    }

    let mut elapsed = driver.sleep_until(MonotonicInstant::ZERO);
    assert!(poll(elapsed.as_mut()).is_ready());
}

/// Defers completion once and requests exactly one wake.
#[test]
fn removal_driver_yield_defers_completion_and_wakes_once() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Wake;

    /// Counts wake requests independently from poll readiness.
    #[derive(Default)]
    struct WakeCount(AtomicUsize);

    impl Wake for WakeCount {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    let notifications = Arc::new(WakeCount::default());
    let waker = Waker::from(Arc::clone(&notifications));
    let mut cx = Context::from_waker(&waker);
    let mut yielding = Driver::default().yield_now();

    assert!(yielding.as_mut().poll(&mut cx).is_pending());
    assert_eq!(notifications.0.load(Ordering::Relaxed), 1);
    assert!(yielding.as_mut().poll(&mut cx).is_ready());
    assert_eq!(notifications.0.load(Ordering::Relaxed), 1);
}

/// Separates payload identity from physical destruction of its owning wrapper.
struct Owned {
    /// Shared local value whose pointer distinguishes the original from replacements.
    value: Rc<Cell<usize>>,
    /// Counts this wrapper's destruction independently from the retained value handle.
    drops: Rc<Cell<usize>>,
}
impl Drop for Owned {
    fn drop(&mut self) {
        self.drops.set(self.drops.get() + 1);
    }
}
/// Gives the removable mount one non-`Send` owned payload.
struct Child(Owned);
/// Keeps active child state separate from removal bookkeeping.
struct Parent {
    /// The child field that owning removal can extract or replace.
    child: Option<Child>,
    /// Counts parent updates and supplies the reconfiguration witness.
    received: usize,
}
/// Receives transferred values above their surviving parent.
struct Root {
    /// The parent whose authority can survive child retirement.
    parent: Option<Parent>,
    /// The exact owned value delivered through a strict ancestor.
    outcome: Option<Owned>,
}
/// Retires the parent when the root receives its only action.
struct RootFeature;
/// Leaves parent mutations to explicit task-context operations.
struct ParentFeature;
/// Counts physical feature destruction, then invokes its one-shot retirement hook.
struct ChildFeature(Rc<Cell<usize>>, DropHook);
impl Drop for ChildFeature {
    fn drop(&mut self) {
        self.0.set(self.0.get() + 1);
        let hook = self.1.borrow_mut().take();
        if let Some(hook) = hook {
            hook();
        }
    }
}
impl Feature for RootFeature {
    type State = Root;
    type Action = ();
    fn update(&mut self, state: &mut Root, (): &(), _: &mut FeatureContext<'_, Root, ()>) {
        state.parent = None;
    }
}
impl Feature for ParentFeature {
    type State = Parent;
    type Action = ();
    fn update(&mut self, _: &mut Parent, (): &(), _: &mut FeatureContext<'_, Parent, ()>) {}
}
impl Feature for ChildFeature {
    type State = Child;
    type Action = ();
    fn update(&mut self, _: &mut Child, (): &(), _: &mut FeatureContext<'_, Child, ()>) {}
}
/// Pairs child access with its parent's owning-removal declaration.
type Sites = (
    MountSite<Root, Child, (), ()>,
    RemovalSite<Parent, Child, ()>,
);
/// Holds independent authority, identity and callback witnesses for one nested Store.
struct Fixture {
    /// Manually polls actor and task futures.
    driver: Driver,
    /// Keeps the root actor live until explicit teardown.
    store: Store<Root, ()>,
    /// Declares the output source and the surviving optional parent.
    parent_site: MountSite<Root, Parent, (), ()>,
    /// Retains the declaration used by gate acquisition checks after retirement.
    #[cfg(feature = "test-util")]
    child_site: MountSite<Root, Child, (), ()>,
    /// Surviving authority used to remove and transfer the child.
    parent: TaskContext<Parent, ()>,
    /// Captures the original child lifetime, never its replacement.
    child: TaskContext<Child, ()>,
    /// Declares which parent owns the removable field.
    removal: RemovalSite<Parent, Child, ()>,
    /// Transfers the original wrapper to the root.
    output: rca_core::local::Output<Owned>,
    /// Retains payload identity without retaining the owning wrapper.
    old: Rc<Cell<usize>>,
    /// Counts original wrapper destruction independently from feature retirement.
    drops: Rc<Cell<usize>>,
    /// Counts mounted feature destruction, including replacement features.
    feature_drops: Rc<Cell<usize>>,
    /// Detects prohibited delivery to the source parent itself.
    self_handlers: Rc<Cell<usize>>,
}

/// Builds the ordinary fixture without a held transaction frontier.
fn fixture() -> Fixture {
    fixture_with_gate(
        #[cfg(feature = "test-util")]
        None,
    )
}
/// Installs nested declarations and acknowledges initial gated installation when configured.
#[cfg_attr(
    feature = "test-util",
    expect(
        clippy::needless_pass_by_value,
        reason = "The temporary owned gate ends with fixture construction, after its retained Store clone is installed"
    )
)]
#[expect(
    clippy::too_many_lines,
    reason = "Keep nested removal, output routing and optional gated startup in one fixture with a single owned lifetime"
)]
fn fixture_with_gate(
    #[cfg(feature = "test-util")] gate: Option<rca_core::testing::local::TransactionGate<Root, ()>>,
) -> Fixture {
    let driver = Driver::default();
    let old = Rc::new(Cell::new(4));
    let drops = Rc::new(Cell::new(0));
    let feature_drops = Rc::new(Cell::new(0));
    let sites: Rc<RefCell<Option<Sites>>> = Rc::new(RefCell::new(None));
    let exported = std::rc::Rc::clone(&sites);
    let feature_capture = std::rc::Rc::clone(&feature_drops);
    let retirement_hook = std::rc::Rc::clone(&driver.3);
    let (builder, parent_site) = Store::builder(
        Root {
            parent: Some(Parent {
                child: Some(Child(Owned {
                    value: std::rc::Rc::clone(&old),
                    drops: std::rc::Rc::clone(&drops),
                })),
                received: 0,
            }),
            outcome: None,
        },
        RootFeature,
        Runtime::new(driver.clone()),
    )
    .mailbox_capacity(1)
    .mount_with_children(
        OptionalMount::new(
            |root: &Root| root.parent.as_ref(),
            |root: &mut Root| root.parent.as_mut(),
            |root| {
                root.parent.as_ref().map(|parent| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: (parent.received / 1000) as u64,
                })
            },
            |_| ParentFeature,
        ),
        move |_, children| {
            let feature_capture = std::rc::Rc::clone(&feature_capture);
            let retirement_hook = std::rc::Rc::clone(&retirement_hook);
            *exported.borrow_mut() = Some(children.optional_removable(
                |parent: &Parent| &parent.child,
                |parent: &mut Parent| &mut parent.child,
                |parent| {
                    parent.child.as_ref().map(|_| MountIdentity {
                        key: (),
                        replacement: 0,
                        configuration: 0,
                    })
                },
                move |_| {
                    ChildFeature(
                        std::rc::Rc::clone(&feature_capture),
                        std::rc::Rc::clone(&retirement_hook),
                    )
                },
            ));
        },
    );
    let self_handlers = Rc::new(Cell::new(0));
    let calls = std::rc::Rc::clone(&self_handlers);
    let (builder, output) = builder.output(&parent_site, |handlers| {
        handlers.at(&parent_site, move |_, value, _| {
            calls.set(calls.get() + 1);
            OutputFlow::Continue(value)
        });
        handlers.at_root(|root, value, _| {
            root.outcome = Some(value);
            OutputFlow::Consumed
        });
    });
    #[cfg(feature = "test-util")]
    let builder = match &gate {
        Some(gate) => builder.transaction_gate(gate.clone()),
        None => builder,
    };
    let store = builder.build().unwrap();
    #[cfg(feature = "test-util")]
    if let Some(gate) = &gate {
        driver.turn();
        let boundary = gate.boundary().unwrap();
        drop(
            driver
                .finish(gate.release_pending_with_receipt().unwrap().finish())
                .unwrap(),
        );
        assert!(gate.acknowledge(boundary));
    }
    driver.finish(store.ready()).unwrap();
    let (child_site, removal) = sites.borrow_mut().take().unwrap();
    let parent = driver.finish(store.acquire(&parent_site)).unwrap().unwrap();
    let child = driver.finish(store.acquire(&child_site)).unwrap().unwrap();
    Fixture {
        driver,
        store,
        parent_site,
        #[cfg(feature = "test-util")]
        child_site,
        parent,
        child,
        removal,
        output,
        old,
        drops,
        feature_drops,
        self_handlers,
    }
}

/// Keeps the removal receipt pending until parent-owned cleanup releases the retired child.
#[test]
fn local_optional_removal_receipt_awaits_parent_owned_cleanup() {
    let Fixture {
        driver,
        store,
        parent,
        child,
        removal,
        old,
        drops,
        ..
    } = fixture();
    let (release, wait) = futures_channel::oneshot::channel::<()>();
    let replacement_drops = Rc::new(Cell::new(0));
    let replacement = Child(Owned {
        value: Rc::new(Cell::new(100)),
        drops: Rc::clone(&replacement_drops),
    });
    let identity = Rc::as_ptr(&old);
    let task = driver
        .finish(parent.remove_child(
            &removal,
            &child,
            |child| child.0.value.set(7),
            move |parent, removed, context| {
                parent.child = Some(replacement);
                let original = removed.into_inner();
                context.task(rca_core::local::Task::run(move |_| async move {
                    wait.await.unwrap();
                    assert_eq!(Rc::as_ptr(&original.0.value), identity);
                    assert_eq!(original.0.value.get(), 7);
                    drop(original);
                }));
            },
        ))
        .unwrap();
    assert_eq!(
        driver.finish(child.read(|_| ())).unwrap_err().reason(),
        rca_core::TaskAccessReason::ScopeEnded
    );
    assert_eq!(
        driver
            .finish(parent.read(|parent| parent.child.as_ref().unwrap().0.value.get()))
            .unwrap(),
        100
    );
    let mut completion = Box::pin(task.finish());
    driver.turn();
    assert!(poll(completion.as_mut()).is_pending());
    assert!(!task.is_finished());
    assert_eq!(drops.get(), 0);
    assert_eq!(replacement_drops.get(), 0);
    release.send(()).unwrap();
    driver.finish(completion);
    assert!(task.is_finished());
    assert_eq!(drops.get(), 1);
    assert_eq!(replacement_drops.get(), 0);
    assert_eq!(
        driver
            .finish(parent.read(|parent| parent.child.as_ref().unwrap().0.value.get()))
            .unwrap(),
        100
    );
    driver.finish(store.shutdown()).unwrap();
    assert_eq!(drops.get(), 1);
    assert_eq!(replacement_drops.get(), 1);
}

/// Preserves both local owned values while parent inputs commit between their inspections.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One owner-local sibling schedule keeps extraction, queued pings, gated inspection and exact disposal together"
)]
fn owned_sibling_values_survive_queued_parent_inputs() {
    /// Keeps non-Send children separate from queued parent input state.
    struct Siblings {
        children: [Option<Child>; 2],
        pings: usize,
    }

    /// Extracts both children or records an ordinary queued parent input.
    enum Input {
        Remove,
        Ping,
    }

    /// Transfers both originals into owner-local work with an explicit inspection gate.
    struct Parent {
        gates: Option<(
            futures_channel::oneshot::Sender<()>,
            futures_channel::oneshot::Receiver<()>,
        )>,
        observed: Rc<RefCell<Vec<(usize, usize)>>>,
    }

    impl Feature for Parent {
        type State = Siblings;
        type Action = Input;

        fn update(
            &mut self,
            state: &mut Siblings,
            action: &Input,
            context: &mut FeatureContext<'_, Siblings, Input>,
        ) {
            match action {
                Input::Remove => {
                    state.children[0].as_ref().unwrap().0.value.set(1);
                    state.children[1].as_ref().unwrap().0.value.set(2);
                    let first = state.children[0].take().unwrap();
                    let second = state.children[1].take().unwrap();
                    let (first_ping, release) = self.gates.take().unwrap();
                    let observed = Rc::clone(&self.observed);

                    context.task(rca_core::local::Task::run(move |parent| async move {
                        observed
                            .borrow_mut()
                            .push((Rc::as_ptr(&first.0.value).addr(), first.0.value.get()));
                        // The reply proves commit; its receipt includes this producing task.
                        drop(parent.send(Input::Ping).await.unwrap());
                        first_ping.send(()).unwrap();
                        release.await.unwrap();

                        observed
                            .borrow_mut()
                            .push((Rc::as_ptr(&second.0.value).addr(), second.0.value.get()));
                        drop(parent.send(Input::Ping).await.unwrap());
                        assert_eq!(first.0.value.get(), 1);
                        assert_eq!(second.0.value.get(), 2);
                        drop((first, second));
                    }));
                }
                Input::Ping => {
                    assert!(state.children.iter().all(Option::is_none));
                    state.pings += 1;
                }
            }
        }
    }

    let driver = Driver::default();
    let weak_queue = Rc::downgrade(&driver.0);
    let owners = Rc::new(Cell::new(0));
    let features = Rc::new(Cell::new(0));
    let observed = Rc::new(RefCell::new(Vec::new()));
    let first = Rc::new(Cell::new(0));
    let second = Rc::new(Cell::new(0));
    let identities = [Rc::as_ptr(&first).addr(), Rc::as_ptr(&second).addr()];
    let weak_values = [Rc::downgrade(&first), Rc::downgrade(&second)];
    let (notify_first_ping, first_ping) = futures_channel::oneshot::channel();
    let (release, wait) = futures_channel::oneshot::channel();
    let builder = Store::builder(
        Siblings {
            children: [first, second].map(|value| {
                Some(Child(Owned {
                    value,
                    drops: Rc::clone(&owners),
                }))
            }),
            pings: 0,
        },
        Parent {
            gates: Some((notify_first_ping, wait)),
            observed: Rc::clone(&observed),
        },
        Runtime::new(driver.clone()),
    );
    let feature_drops = Rc::clone(&features);
    let (builder, first_site) = builder.mount_with_site(OptionalMount::new(
        |state: &Siblings| state.children[0].as_ref(),
        |state: &mut Siblings| state.children[0].as_mut(),
        |state: &Siblings| {
            state.children[0].as_ref().map(|_| MountIdentity {
                key: 0_usize,
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| ChildFeature(Rc::clone(&feature_drops), Rc::new(RefCell::new(None))),
    ));
    let feature_drops = Rc::clone(&features);
    let (builder, second_site) = builder.mount_with_site(OptionalMount::new(
        |state: &Siblings| state.children[1].as_ref(),
        |state: &mut Siblings| state.children[1].as_mut(),
        |state: &Siblings| {
            state.children[1].as_ref().map(|_| MountIdentity {
                key: 1_usize,
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| ChildFeature(Rc::clone(&feature_drops), Rc::new(RefCell::new(None))),
    ));
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    let sites = [first_site, second_site];
    let children = sites
        .each_ref()
        .map(|site| driver.finish(store.acquire(site)).unwrap().unwrap());

    let work = driver.finish(store.send(Input::Remove)).unwrap();
    driver.finish(first_ping).unwrap();
    assert_eq!(driver.finish(store.read(|state| state.pings)).unwrap(), 1);
    assert_eq!(*observed.borrow(), [(identities[0], 1)]);
    for child in &children {
        let error = driver.finish(child.read(|_| ())).unwrap_err();
        assert_eq!(error.reason(), rca_core::TaskAccessReason::ScopeEnded);
        assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
    }
    assert_eq!(features.get(), 2);
    assert_eq!(owners.get(), 0);
    assert!(weak_values.iter().all(|value| value.strong_count() == 1));
    let mut completion = Box::pin(work.finish());
    assert!(poll(completion.as_mut()).is_pending());
    assert!(!work.is_finished());

    release.send(()).unwrap();
    driver.finish(completion);
    assert!(work.is_finished());
    assert_eq!(driver.finish(store.read(|state| state.pings)).unwrap(), 2);
    assert_eq!(*observed.borrow(), [(identities[0], 1), (identities[1], 2)]);
    assert_eq!(owners.get(), 2);
    assert_eq!(features.get(), 2);
    assert!(weak_values.iter().all(|value| value.upgrade().is_none()));
    driver.finish(store.shutdown()).unwrap();
    assert!(driver.0.borrow().is_empty());
    drop((store, children, sites, work, driver));
    assert!(weak_queue.upgrade().is_none());
    assert_eq!(owners.get(), 2);
}

/// Keeps keyed cleanup pending while preserving siblings and the equal-key replacement.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One keyed lifetime keeps retirement, pending cleanup and replacement ownership together"
)]
fn local_keyed_removal_receipt_awaits_parent_owned_cleanup() {
    /// Ordered children with independently stamped entry lifetimes.
    type Rows = rca_core::KeyedState<u8, Child>;
    /// Pairs a child declaration with its collection's removal authority.
    type RowSites = (
        MountSite<Option<Rows>, Child, (), ()>,
        RemovalSite<Rows, Child, ()>,
    );
    /// Hosts the optional collection without action-driven mutations.
    struct RowsRoot;
    /// Leaves collection changes to owning task-context operations.
    struct RowsFeature;
    impl Feature for RowsRoot {
        type State = Option<Rows>;
        type Action = ();
        fn update(
            &mut self,
            _: &mut Self::State,
            (): &(),
            _: &mut FeatureContext<'_, Self::State, ()>,
        ) {
        }
    }
    impl Feature for RowsFeature {
        type State = Rows;
        type Action = ();
        fn update(&mut self, _: &mut Rows, (): &(), _: &mut FeatureContext<'_, Rows, ()>) {}
    }
    let driver = Driver::default();
    let drops = Rc::new(Cell::new(0));
    let old = Rc::new(Cell::new(4));
    let sibling_drops = Rc::new(Cell::new(0));
    let rows = [
        (
            1,
            Child(Owned {
                value: Rc::clone(&old),
                drops: Rc::clone(&drops),
            }),
        ),
        (
            2,
            Child(Owned {
                value: Rc::new(Cell::new(20)),
                drops: Rc::clone(&sibling_drops),
            }),
        ),
    ]
    .into_iter()
    .collect();
    let sites = Rc::new(RefCell::new(Vec::<(u8, RowSites)>::new()));
    let exported = Rc::clone(&sites);
    let (builder, parent_site) = Store::builder(Some(rows), RowsRoot, Runtime::new(driver.clone()))
        .mount_with_children(
            OptionalMount::new(
                Option::as_ref,
                Option::as_mut,
                |rows: &Option<Rows>| {
                    rows.as_ref().map(|_| MountIdentity {
                        key: (),
                        replacement: 0,
                        configuration: 0,
                    })
                },
                |_| RowsFeature,
            ),
            move |_, children| {
                let exported = Rc::clone(&exported);
                children.keyed_removable(
                    |rows| rows,
                    |rows| rows,
                    move |key, _| {
                        let key = *key;
                        let exported = Rc::clone(&exported);
                        (
                            rca_core::local::EntryMount::new(ChildFeature(
                                Rc::new(Cell::new(0)),
                                DropHook::default(),
                            )),
                            move |_: &Child,
                                  children: &mut rca_core::local::MountChildren<
                                '_,
                                Option<Rows>,
                                (),
                                Child,
                                (),
                            >,
                                  removal| {
                                exported
                                    .borrow_mut()
                                    .push((key, (children.site().clone(), removal)));
                            },
                        )
                    },
                );
            },
        );
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    let parent = driver.finish(store.acquire(&parent_site)).unwrap().unwrap();
    let (site, removal) = sites
        .borrow()
        .iter()
        .find(|(key, _)| *key == 1)
        .unwrap()
        .1
        .clone();
    let child = driver.finish(store.acquire(&site)).unwrap().unwrap();
    let (release, wait) = futures_channel::oneshot::channel::<()>();
    let replacement_drops = Rc::new(Cell::new(0));
    let replacement = Child(Owned {
        value: Rc::new(Cell::new(100)),
        drops: Rc::clone(&replacement_drops),
    });
    let identity = Rc::as_ptr(&old);
    let task = driver
        .finish(parent.remove_child(
            &removal,
            &child,
            |child| child.0.value.set(7),
            move |rows, removed, context| {
                rows.insert(1, replacement);
                let original = removed.into_inner();
                context.task(rca_core::local::Task::run(move |_| async move {
                    wait.await.unwrap();
                    assert_eq!(Rc::as_ptr(&original.0.value), identity);
                    assert_eq!(original.0.value.get(), 7);
                    drop(original);
                }));
            },
        ))
        .unwrap();
    assert_eq!(
        driver.finish(child.read(|_| ())).unwrap_err().reason(),
        rca_core::TaskAccessReason::ScopeEnded
    );
    let read_values = |rows: &Rows| {
        rows.iter()
            .map(|(key, child)| (*key, child.0.value.get()))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        driver.finish(parent.read(read_values)).unwrap(),
        [(2, 20), (1, 100)]
    );
    let mut completion = Box::pin(task.finish());
    driver.turn();
    assert!(poll(completion.as_mut()).is_pending());
    assert!(!task.is_finished());
    assert_eq!(drops.get(), 0);
    assert_eq!(replacement_drops.get(), 0);
    release.send(()).unwrap();
    driver.finish(completion);
    assert!(task.is_finished());
    assert_eq!(drops.get(), 1);
    assert_eq!(sibling_drops.get(), 0);
    assert_eq!(replacement_drops.get(), 0);
    assert_eq!(
        driver.finish(parent.read(read_values)).unwrap(),
        [(2, 20), (1, 100)]
    );
    driver.finish(store.shutdown()).unwrap();
    assert_eq!(drops.get(), 1);
    assert_eq!(sibling_drops.get(), 1);
    assert_eq!(replacement_drops.get(), 1);
}

/// Rejects a retired child declaration while independent root reads remain usable.
#[cfg(feature = "test-util")]
#[test]
fn retired_nested_site_rejects_gate_precheck_while_root_stays_live() {
    let Fixture {
        driver,
        store,
        child_site,
        drops,
        feature_drops,
        ..
    } = fixture();
    drop(driver.finish(store.send(())).unwrap());
    assert!(
        driver
            .finish(store.read(|root| root.parent.is_none()))
            .unwrap()
    );
    assert_eq!(drops.get(), 1);
    assert_eq!(feature_drops.get(), 1);

    let gate = rca_core::testing::local::TransactionGate::new();
    let error = gate.validate_acquisition(&store, &child_site).unwrap_err();
    assert_eq!(
        (error.reason(), error.progress()),
        (
            rca_core::TaskAccessReason::StoreClosed,
            rca_core::TaskProgress::NotStarted
        )
    );
    let error = driver
        .finish(gate.acquire(&store, &child_site))
        .unwrap_err();
    assert_eq!(
        (error.reason(), error.progress()),
        (
            rca_core::TaskAccessReason::StoreClosed,
            rca_core::TaskProgress::NotStarted
        )
    );
    assert!(
        driver
            .finish(store.read(|root| root.parent.is_none()))
            .unwrap()
    );
    drop(store);
    driver.turn();
    assert!(driver.0.borrow().is_empty());
}

/// Preserves original ownership while delivery skips the source parent and reaches its ancestor.
#[test]
fn local_owned_child_transfer_preserves_identity_and_uses_strict_ancestors() {
    let Fixture {
        driver,
        store,
        parent,
        child,
        removal,
        output,
        old,
        drops,
        feature_drops,
        self_handlers,
        ..
    } = fixture();
    // The Rc-owned child payload intentionally has no Debug implementation.
    assert_eq!(format!("{removal:?}"), "RemovalSite { .. }");
    let received = Rc::new(Cell::new(0));
    let receipt = std::rc::Rc::clone(&received);
    let successor_drops = Rc::new(Cell::new(0));
    let task = driver
        .finish(parent.remove_child(
            &removal,
            &child,
            |child| child.0.value.set(7),
            move |parent, removed, context| {
                assert_eq!(format!("{removed:?}"), "RemovedChild { .. }");
                assert_eq!(removed.origin().configuration(), 0);
                parent.received += 1;
                parent.child = Some(Child(Owned {
                    value: Rc::new(Cell::new(100)),
                    drops: successor_drops,
                }));
                removed
                    .into_output(
                        context,
                        &output,
                        |child| child.0,
                        move |result| {
                            assert!(matches!(result, Ok(OutputDelivery::Consumed { .. })));
                            receipt.set(receipt.get() + 1);
                        },
                    )
                    .unwrap();
            },
        ))
        .unwrap();
    driver.finish(task.finish());
    assert_eq!(received.get(), 1);
    assert_eq!(
        feature_drops.get(),
        1,
        "old Feature physically drops despite retained old ports"
    );
    assert_eq!(
        self_handlers.get(),
        0,
        "the source parent is not its own ancestor"
    );
    assert_eq!(drops.get(), 0);
    let actual = driver
        .finish(store.read(|root| {
            let parent = root.parent.as_ref().unwrap();
            (
                std::rc::Rc::clone(&root.outcome.as_ref().unwrap().value),
                parent.received,
                parent.child.as_ref().unwrap().0.value.get(),
            )
        }))
        .unwrap();
    assert!(Rc::ptr_eq(&actual.0, &old));
    assert_eq!((actual.0.get(), actual.1, actual.2), (7, 1, 100));
    assert_eq!(
        driver
            .finish(child.read(|child| child.0.value.get()))
            .unwrap_err()
            .reason(),
        rca_core::TaskAccessReason::ScopeEnded
    );
    drop(store);
    driver.turn();
    assert_eq!(drops.get(), 1);
    assert_eq!(feature_drops.get(), 2);
}

/// Returns root-owned output unchanged and rejects its declaration after Store disposal.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One Local root-removal sequence preserves owned-output recovery without self-delivery"
)]
fn local_root_removal_returns_owned_output_without_self_delivery() {
    let driver = Driver::default();
    let exported = Rc::new(RefCell::new(None));
    let export = std::rc::Rc::clone(&exported);
    let release = Rc::new(Cell::new(false));
    let release_task = std::rc::Rc::clone(&release);
    let old = Rc::new(Cell::new(4));
    let drops = Rc::new(Cell::new(0));
    let feature_drops = Rc::new(Cell::new(0));
    let drop_feature = std::rc::Rc::clone(&feature_drops);
    let retirement_hook = std::rc::Rc::clone(&driver.3);
    let (builder, site, removal) = rca_core::local::RootMount::new(ParentFeature)
        .on_initial(
            |_| (),
            move |_, _| {
                let export = std::rc::Rc::clone(&export);
                let release = std::rc::Rc::clone(&release_task);
                rca_core::local::Task::run(move |context| async move {
                    *export.borrow_mut() = Some(context);
                    std::future::poll_fn(move |_| {
                        if release.get() {
                            Poll::Ready(())
                        } else {
                            Poll::Pending
                        }
                    })
                    .await;
                })
            },
        )
        .builder(
            Parent {
                child: Some(Child(Owned {
                    value: std::rc::Rc::clone(&old),
                    drops: std::rc::Rc::clone(&drops),
                })),
                received: 0,
            },
            Runtime::new(driver.clone()),
        )
        .mailbox_capacity(1)
        .mount_removable(
            |parent: &Parent| &parent.child,
            |parent: &mut Parent| &mut parent.child,
            |parent| {
                parent.child.as_ref().map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            move |_| {
                ChildFeature(
                    std::rc::Rc::clone(&drop_feature),
                    std::rc::Rc::clone(&retirement_hook),
                )
            },
        );
    let handlers = Rc::new(Cell::new(0));
    let called = std::rc::Rc::clone(&handlers);
    let (builder, output) = builder.root_output::<Owned>(|handlers| {
        handlers.at_root(move |_, value, _| {
            called.set(called.get() + 1);
            drop(value);
            OutputFlow::Consumed
        });
    });
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    driver.turn();
    let parent = exported.borrow_mut().take().unwrap();
    let child = driver.finish(store.acquire(&site)).unwrap().unwrap();
    let (unbuilt, unbuilt_output) = rca_core::local::RootMount::new(ParentFeature)
        .builder(
            Parent {
                child: None,
                received: 0,
            },
            Runtime::new(driver.clone()),
        )
        .root_output::<Owned>(|_| {});
    let conversions = Rc::new(Cell::new(0));
    let rejected_receives = Rc::new(Cell::new(0));
    let rejected_capture_drops = Rc::new(Cell::new(0));
    let conversion = Rc::clone(&conversions);
    let rejected_receive = Rc::clone(&rejected_receives);
    let rejected_capture = DropWatch(Rc::clone(&rejected_capture_drops));
    let effect_calls = Rc::new(Cell::new(0));
    let effect_called = Rc::clone(&effect_calls);
    let effect_drops = Rc::new(Cell::new(0));
    let effect_capture = DropWatch(Rc::clone(&effect_drops));
    let returned = Rc::new(RefCell::new(None));
    let receive = std::rc::Rc::clone(&returned);
    let task = driver
        .finish(parent.remove_child(
            &removal,
            &child,
            |child| child.0.value.set(7),
            move |parent, removed, context| {
                parent.received += 1;
                let error = removed
                    .into_output(
                        context,
                        &unbuilt_output,
                        move |child| {
                            conversion.set(conversion.get() + 1);
                            child.0
                        },
                        move |_| {
                            let _ = &rejected_capture;
                            rejected_receive.set(rejected_receive.get() + 1);
                        },
                    )
                    .unwrap_err();
                let (removed, error) = error.into_parts();
                assert_eq!(error.reason(), rca_core::TaskAccessReason::ScopeEnded);
                assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
                removed
                    .into_output(
                        context,
                        &output,
                        |child| child.0,
                        move |result| {
                            let OutputDelivery::Unhandled { value, .. } = result.unwrap() else {
                                panic!("root has no ancestor")
                            };
                            *receive.borrow_mut() = Some(value);
                        },
                    )
                    .unwrap();
                context.effect(rca_core::local::Effect::run(move |_| async move {
                    effect_called.set(effect_called.get() + 1);
                    drop(effect_capture);
                }));
            },
        ))
        .unwrap();
    release.set(true);
    driver.finish(task.finish());
    assert_eq!(conversions.get(), 0);
    assert_eq!(rejected_receives.get(), 0);
    assert_eq!(rejected_capture_drops.get(), 1);
    assert_eq!(effect_calls.get(), 1);
    assert_eq!(effect_drops.get(), 1);
    assert_eq!(Rc::strong_count(&effect_calls), 1);
    drop(unbuilt);
    assert_eq!(handlers.get(), 0);
    assert_eq!(feature_drops.get(), 1);
    assert_eq!(
        driver
            .finish(store.read(|parent| (parent.received, parent.child.is_none())))
            .unwrap(),
        (1, true)
    );
    let value = returned.borrow_mut().take().unwrap();
    assert!(Rc::ptr_eq(&old, &value.value));
    assert_eq!(value.value.get(), 7);
    assert_eq!(drops.get(), 0);
    drop(value);
    assert_eq!(drops.get(), 1);

    // A removal declaration is independent of the contexts supplied to remove_child.
    // Keeping it must not resurrect its disposed root Store or panic on submission.
    drop(store);
    driver.turn();
    assert!(driver.0.borrow().is_empty());
    let current = fixture();
    let callbacks = Rc::new(Cell::new(0));
    let before = Rc::clone(&callbacks);
    let after = Rc::clone(&callbacks);
    let capture_drops = Rc::new(Cell::new(0));
    let before_capture = DropWatch(Rc::clone(&capture_drops));
    let after_capture = DropWatch(Rc::clone(&capture_drops));
    let error = current
        .driver
        .finish(current.parent.remove_child(
            &removal,
            &current.child,
            move |_| {
                let _ = &before_capture;
                before.set(before.get() + 1);
            },
            move |_, _, _| {
                let _ = &after_capture;
                after.set(after.get() + 1);
            },
        ))
        .unwrap_err();
    assert_eq!(error.reason(), rca_core::TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
    assert_eq!(callbacks.get(), 0);
    assert_eq!(capture_drops.get(), 2);
    assert_eq!(current.drops.get(), 0);
    assert_eq!(current.feature_drops.get(), 0);
    drop(current.store);
    current.driver.turn();
    assert_eq!(current.drops.get(), 1);
    assert_eq!(current.feature_drops.get(), 1);
}

/// Counts physical destruction without asserting or calling application code.
struct DropWatch(Rc<Cell<usize>>);
impl Drop for DropWatch {
    fn drop(&mut self) {
        self.0.set(self.0.get() + 1);
    }
}

/// Releases captures without spawning when conversion or converter destruction closes ownership.
#[test]
fn local_conversion_close_appends_no_producer_and_drops_owned_captures() {
    for explicit_close in [true, false] {
        let Fixture {
            driver,
            store,
            parent,
            child,
            removal,
            output,
            drops,
            ..
        } = fixture();
        let receive_drops = Rc::new(Cell::new(0));
        let capture = DropWatch(std::rc::Rc::clone(&receive_drops));
        let spawns = driver.1.get();
        let task = driver
            .finish(parent.remove_child(
                &removal,
                &child,
                |_| {},
                move |_, removed, context| {
                    removed
                        .into_output(
                            context,
                            &output,
                            move |child| {
                                let _keep_capture = &store;
                                if explicit_close {
                                    drop(store);
                                }
                                child.0
                            },
                            move |_| {
                                drop(capture);
                                panic!("closed conversion cannot receive");
                            },
                        )
                        .unwrap();
                },
            ))
            .unwrap();
        driver.finish(task.finish());
        assert_eq!(driver.1.get(), spawns);
        assert_eq!(drops.get(), 1);
        assert_eq!(receive_drops.get(), 1);
    }
}

/// Preserves ordinary launch accounting while late closure suppresses transfer delivery.
#[test]
fn local_later_close_preserves_ordinary_launch_but_suppresses_transfer_callbacks() {
    let Fixture {
        driver,
        store,
        parent,
        child,
        removal,
        output,
        drops,
        ..
    } = fixture();
    let receive_drops = Rc::new(Cell::new(0));
    let capture = DropWatch(std::rc::Rc::clone(&receive_drops));
    let ordinary_drops = Rc::new(Cell::new(0));
    let ordinary = DropWatch(std::rc::Rc::clone(&ordinary_drops));
    let spawns = driver.1.get();
    let task = driver
        .finish(parent.remove_child(
            &removal,
            &child,
            |_| {},
            move |_, removed, context| {
                removed
                    .into_output(
                        context,
                        &output,
                        |child| child.0,
                        move |_| {
                            drop(capture);
                            panic!("late closed transfer cannot receive");
                        },
                    )
                    .unwrap();
                context.task(rca_core::local::Task::run(move |_| async move {
                    drop(ordinary);
                }));
                drop(store);
            },
        ))
        .unwrap();
    assert_eq!(driver.1.get(), spawns + 2);
    driver.finish(task.finish());
    driver.turn();
    assert_eq!(drops.get(), 1);
    assert_eq!(receive_drops.get(), 1);
    assert_eq!(ordinary_drops.get(), 1);
}

/// Suppresses receipt callbacks before production and after output handler entry.
#[test]
fn local_close_before_producer_or_after_output_reply_suppresses_receive() {
    for after_output in [false, true] {
        let Fixture {
            driver,
            store,
            parent,
            child,
            removal,
            output,
            drops,
            ..
        } = fixture();
        let receive_drops = Rc::new(Cell::new(0));
        let capture = DropWatch(std::rc::Rc::clone(&receive_drops));
        let task = driver
            .finish(parent.remove_child(
                &removal,
                &child,
                |_| {},
                move |_, removed, context| {
                    removed
                        .into_output(
                            context,
                            &output,
                            |child| child.0,
                            move |_| {
                                drop(capture);
                                panic!("closed producer cannot receive");
                            },
                        )
                        .unwrap();
                },
            ))
            .unwrap();
        if after_output {
            driver.turn();
        }
        drop(store);
        driver.finish(task.finish());
        driver.turn();
        assert_eq!(drops.get(), 1);
        assert_eq!(receive_drops.get(), 1);
    }
}

/// Returns the original child without conversion through a foreign declaration.
#[test]
fn local_foreign_output_returns_unconverted_child_and_destroys_receive_capture() {
    let Fixture {
        driver,
        store,
        parent,
        child,
        removal,
        old,
        drops,
        ..
    } = fixture();
    let other = fixture();
    let output = other.output.clone();
    let converted = Rc::new(Cell::new(0));
    let converts = std::rc::Rc::clone(&converted);
    let receive_drops = Rc::new(Cell::new(0));
    let capture = DropWatch(std::rc::Rc::clone(&receive_drops));
    let recovered = Rc::new(RefCell::new(None));
    let receive = std::rc::Rc::clone(&recovered);
    let _ = driver
        .finish(parent.remove_child(
            &removal,
            &child,
            |_| {},
            move |_, removed, context| {
                let error = removed
                    .into_output(
                        context,
                        &output,
                        move |child| {
                            converts.set(converts.get() + 1);
                            child.0
                        },
                        move |_| {
                            drop(capture);
                            panic!("rejected transfer cannot receive");
                        },
                    )
                    .unwrap_err();
                assert_eq!(
                    format!("{error:?}"),
                    "RemovedOutputError { error: TaskAccessError { reason: ScopeEnded, progress: NotStarted }, .. }"
                );
                assert_eq!(
                    error.to_string(),
                    "typed task access failed: ScopeEnded (NotStarted)"
                );
                let (removed, error) = error.into_parts();
                assert_eq!(error.reason(), rca_core::TaskAccessReason::ScopeEnded);
                assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
                *receive.borrow_mut() = Some(removed.into_inner());
            },
        ))
        .unwrap();
    assert_eq!(converted.get(), 0);
    assert_eq!(receive_drops.get(), 1);
    let child = recovered.borrow_mut().take().unwrap();
    assert!(Rc::ptr_eq(&child.0.value, &old));
    assert_eq!(drops.get(), 0);
    drop(child);
    assert_eq!(drops.get(), 1);
    let foreign_queue = Rc::downgrade(&other.driver.0);
    driver.finish(store.shutdown()).unwrap();
    other.driver.finish(other.store.shutdown()).unwrap();
    drop((store, other));
    assert_eq!(
        (
            driver.0.borrow().is_empty(),
            foreign_queue.upgrade().is_none()
        ),
        (true, true),
        "both fixtures release their actors"
    );
    let queue = Rc::downgrade(&driver.0);
    drop(driver);
    assert!(queue.upgrade().is_none());
}
/// Rejects a sibling output source before conversion or receipt entry.
#[test]
fn local_sibling_source_output_returns_unconverted_removal_before_conversion() {
    sibling_output_rejection(false);
}

/// Rejects a sibling-owned declaration even when its stated source is the live parent.
#[test]
fn local_live_parent_source_with_sibling_declaration_owner_rejects_before_conversion() {
    sibling_output_rejection(true);
}

/// Rejects an uninstalled declaration despite live source authority.
#[test]
fn local_unbuilt_output_owner_rejects_live_source_without_conversion() {
    uninstalled_output_rejection(false);
}

/// Rejects a destroyed declaration owner without converting the removed child.
#[test]
fn local_dropped_output_owner_rejects_live_source_without_conversion() {
    uninstalled_output_rejection(true);
}

/// Separates declaration ownership from source liveness and returned payload ownership.
fn uninstalled_output_rejection(drop_declaration: bool) {
    let f = fixture();
    let handler_drops = Rc::new(Cell::new(0));
    let handler_capture = DropWatch(Rc::clone(&handler_drops));
    let handlers = Rc::new(Cell::new(0));
    let calls = Rc::clone(&handlers);
    let (unbuilt, output) = Store::builder(
        Root {
            parent: None,
            outcome: None,
        },
        RootFeature,
        Runtime::new(f.driver.clone()),
    )
    .output(&f.parent_site, |handlers| {
        handlers.at_root(move |_, value: Owned, _| {
            let _ = &handler_capture;
            calls.set(calls.get() + 1);
            drop(value);
            OutputFlow::Consumed
        });
    });
    let mut unbuilt = Some(unbuilt);
    if drop_declaration {
        drop(unbuilt.take());
    }
    assert_eq!(handler_drops.get(), usize::from(drop_declaration));
    let reason = if drop_declaration {
        rca_core::TaskAccessReason::StoreClosed
    } else {
        rca_core::TaskAccessReason::ScopeEnded
    };
    let receive_drops = Rc::new(Cell::new(0));
    let capture = DropWatch(Rc::clone(&receive_drops));
    let recovered = Rc::new(RefCell::new(None));
    let receive = Rc::clone(&recovered);
    let task = f
        .driver
        .finish(f.parent.remove_child(
            &f.removal,
            &f.child,
            |_| {},
            move |_, removed, context| {
                let (removed, error) = removed
                    .into_output(
                        context,
                        &output,
                        |_| -> Owned { panic!("unbuilt output cannot convert") },
                        move |_| {
                            drop(capture);
                            panic!("unbuilt output cannot receive");
                        },
                    )
                    .unwrap_err()
                    .into_parts();
                assert_eq!(error.reason(), reason);
                assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
                *receive.borrow_mut() = Some(removed.into_inner());
            },
        ))
        .unwrap();
    f.driver.finish(task.finish());
    assert_eq!(receive_drops.get(), 1);
    assert_eq!(handlers.get(), 0);
    assert_eq!(
        handler_drops.get(),
        usize::from(drop_declaration),
        "only a retained declaration still owns its handler"
    );
    assert!(
        f.driver
            .finish(f.parent.read(|parent| parent.child.is_none()))
            .unwrap()
    );
    let removed = recovered.borrow_mut().take().unwrap();
    assert!(Rc::ptr_eq(&removed.0.value, &f.old));
    assert_eq!(removed.0.value.get(), 4);
    assert_eq!(f.drops.get(), 0);
    drop(unbuilt);
    assert_eq!(handler_drops.get(), 1);
    assert_eq!(handlers.get(), 0);
    f.driver.finish(f.store.shutdown()).unwrap();
    assert_eq!(f.drops.get(), 0);
    drop(removed);
    assert_eq!(f.drops.get(), 1);
    assert_eq!(Rc::strong_count(&f.old), 1);
}

/// Rejects the wrong source or declaration owner within the same live actor.
#[expect(
    clippy::too_many_lines,
    reason = "Keep sibling source/declaration authority, owning removal and rejected transfer ownership in one timeline"
)]
fn sibling_output_rejection(sibling_owner: bool) {
    /// Gives live siblings distinct source and declaration authority.
    struct Pair(Parent, Parent);
    /// Keeps both sibling parents unchanged during the rejected transfer.
    struct PairFeature;
    impl Feature for PairFeature {
        type State = Pair;
        type Action = ();
        fn update(&mut self, _: &mut Pair, (): &(), _: &mut FeatureContext<'_, Pair, ()>) {}
    }
    let driver = Driver::default();
    let old = Rc::new(Cell::new(41));
    let drops = Rc::new(Cell::new(0));
    let original = Owned {
        value: Rc::clone(&old),
        drops: Rc::clone(&drops),
    };
    let sites = Rc::new(RefCell::new(None));
    let exported = Rc::clone(&sites);
    let (builder, parent_site) = rca_core::local::RootMount::new(PairFeature)
        .builder(
            Pair(
                Parent {
                    child: Some(Child(original)),
                    received: 0,
                },
                Parent {
                    child: None,
                    received: 0,
                },
            ),
            Runtime::new(driver.clone()),
        )
        .mount_with_children(
            OptionalMount::new(
                |pair: &Pair| Some(&pair.0),
                |pair: &mut Pair| Some(&mut pair.0),
                |_| {
                    Some(MountIdentity {
                        key: (),
                        replacement: 0,
                        configuration: 0,
                    })
                },
                |_| ParentFeature,
            ),
            move |_, children| {
                *exported.borrow_mut() = Some(children.optional_removable(
                    |parent: &Parent| &parent.child,
                    |parent: &mut Parent| &mut parent.child,
                    |parent| {
                        parent.child.as_ref().map(|_| MountIdentity {
                            key: (),
                            replacement: 0,
                            configuration: 0,
                        })
                    },
                    |_| ChildFeature(Rc::new(Cell::new(0)), Rc::new(RefCell::new(None))),
                ));
            },
        );
    let sibling_output = Rc::new(RefCell::new(None));
    let exported_output = Rc::clone(&sibling_output);
    let parent_source = parent_site.clone();
    let handlers = Rc::new(Cell::new(0));
    let sibling_calls = Rc::clone(&handlers);
    let (builder, sibling_site) = builder.mount_with_children(
        OptionalMount::new(
            |pair: &Pair| Some(&pair.1),
            |pair: &mut Pair| Some(&mut pair.1),
            |_| {
                Some(MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            |_| ParentFeature,
        ),
        move |_, children| {
            if sibling_owner {
                let calls = Rc::clone(&sibling_calls);
                *exported_output.borrow_mut() = Some(children.output(&parent_source, |handlers| {
                    handlers.at_root(move |_, value: Owned, _| {
                        calls.set(calls.get() + 1);
                        drop(value);
                        OutputFlow::Consumed
                    });
                }));
            }
        },
    );
    let calls = Rc::clone(&handlers);
    let (builder, output) = builder.output::<Owned>(&sibling_site, |handlers| {
        handlers.at_root(move |_, value, _| {
            calls.set(calls.get() + 1);
            drop(value);
            OutputFlow::Consumed
        });
    });
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    let output = if sibling_owner {
        sibling_output.borrow_mut().take().unwrap()
    } else {
        output
    };
    let (child_site, removal) = sites.borrow_mut().take().unwrap();
    let parent = driver.finish(store.acquire(&parent_site)).unwrap().unwrap();
    let child = driver.finish(store.acquire(&child_site)).unwrap().unwrap();
    let sibling = driver
        .finish(store.acquire(&sibling_site))
        .unwrap()
        .unwrap();
    let receives = Rc::new(Cell::new(0));
    let capture = DropWatch(Rc::clone(&receives));
    let recovered = Rc::new(RefCell::new(None));
    let receive = Rc::clone(&recovered);
    let task = driver
        .finish(parent.remove_child(
            &removal,
            &child,
            |_| {},
            move |_, removed, context| {
                let (removed, error) = removed
                    .into_output(
                        context,
                        &output,
                        |_| -> Owned { panic!("invalid transfer authority cannot convert") },
                        move |_| {
                            drop(capture);
                            panic!("invalid transfer authority cannot receive");
                        },
                    )
                    .unwrap_err()
                    .into_parts();
                assert_eq!(error.reason(), rca_core::TaskAccessReason::ScopeEnded);
                assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
                *receive.borrow_mut() = Some(removed.into_inner());
            },
        ))
        .unwrap();
    driver.finish(task.finish());
    assert_eq!(receives.get(), 1);
    assert_eq!(handlers.get(), 0);
    assert!(
        driver
            .finish(parent.read(|parent| parent.child.is_none()))
            .unwrap()
    );
    assert!(
        driver
            .finish(sibling.read(|parent| parent.child.is_none()))
            .unwrap()
    );
    let removed = recovered.borrow_mut().take().unwrap();
    assert!(Rc::ptr_eq(&removed.0.value, &old));
    assert_eq!(removed.0.value.get(), 41);
    assert_eq!(drops.get(), 0);
    driver.finish(store.shutdown()).unwrap();
    assert_eq!(
        removed.0.value.get(),
        41,
        "returned value outlives both live declarations"
    );
    assert_eq!(drops.get(), 0);
    drop(removed);
    assert_eq!(drops.get(), 1);
    assert_eq!(Rc::strong_count(&old), 1);
}

/// Stops after the entered before-callback closes the Store, without taking its child.
#[test]
fn local_before_close_stops_before_after_callback() {
    let Fixture {
        driver,
        store,
        parent,
        child,
        removal,
        ..
    } = fixture();
    let calls = Rc::new(Cell::new(0));
    let after = std::rc::Rc::clone(&calls);
    let error = driver
        .finish(parent.remove_child(
            &removal,
            &child,
            move |child| {
                child.0.value.set(7);
                drop(store);
            },
            move |_, _, _| {
                after.set(after.get() + 1);
            },
        ))
        .unwrap_err();
    assert_eq!(error.reason(), rca_core::TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), rca_core::TaskProgress::MayHaveRun);
    assert_eq!(calls.get(), 0);
}

/// Rejects foreign authority and stale child lifetimes before either callback enters.
#[test]
fn local_foreign_parent_child_site_and_replaced_child_reject_before_callbacks() {
    for mode in 0..4 {
        let current = fixture();
        let other = fixture();
        if mode == 3 {
            drop(
                current
                    .driver
                    .finish(current.parent.update(|state| state.child.take()))
                    .unwrap(),
            );
            drop(
                current
                    .driver
                    .finish(current.parent.update(|state| {
                        state.child = Some(Child(Owned {
                            value: Rc::new(Cell::new(100)),
                            drops: Rc::new(Cell::new(0)),
                        }));
                    }))
                    .unwrap(),
            );
        }
        let parent = if mode == 0 {
            &other.parent
        } else {
            &current.parent
        };
        let child = if mode == 1 {
            &other.child
        } else {
            &current.child
        };
        let removal = if mode == 2 {
            &other.removal
        } else {
            &current.removal
        };
        let calls = Rc::new(Cell::new(0));
        let before = std::rc::Rc::clone(&calls);
        let after = std::rc::Rc::clone(&calls);
        let mut operation = Box::pin(parent.remove_child(
            removal,
            child,
            move |_| {
                before.set(before.get() + 1);
            },
            move |_, _, _| {
                after.set(after.get() + 1);
            },
        ));
        let mut completed = None;
        for _ in 0..48 {
            if let Poll::Ready(result) = poll(operation.as_mut()) {
                completed = Some(result);
                break;
            }
            current.driver.turn();
            other.driver.turn();
        }
        let error = completed
            .expect("foreign or stale removal reaches its terminal reply")
            .unwrap_err();
        assert_eq!(error.reason(), rca_core::TaskAccessReason::ScopeEnded);
        assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
        assert_eq!(calls.get(), 0);
        drop(operation);
        current.driver.finish(current.store.shutdown()).unwrap();
        other.driver.finish(other.store.shutdown()).unwrap();
        drop((current.store, other.store));
        assert_eq!(
            (
                current.driver.0.borrow().is_empty(),
                other.driver.0.borrow().is_empty()
            ),
            (true, true),
            "both fixtures release their actors"
        );
        let queues = (
            Rc::downgrade(&current.driver.0),
            Rc::downgrade(&other.driver.0),
        );
        drop((current.driver, other.driver));
        assert!(queues.0.upgrade().is_none());
        assert!(queues.1.upgrade().is_none());
    }
}

/// Rejects a disposed foreign declaration without retaining callbacks or changing the live child.
#[test]
fn local_disposed_foreign_removal_site_rejects_without_retaining_callbacks() {
    let current = fixture();
    let Fixture {
        driver,
        store,
        removal,
        old,
        drops,
        feature_drops,
        ..
    } = fixture();
    drop(store);
    driver.turn();
    assert!(driver.0.borrow().is_empty());
    assert_eq!(drops.get(), 1);
    assert_eq!(feature_drops.get(), 1);
    assert_eq!(Rc::strong_count(&old), 1);

    let calls = Rc::new(Cell::new(0));
    let before = Rc::clone(&calls);
    let after = Rc::clone(&calls);
    let capture_drops = Rc::new(Cell::new(0));
    let before_capture = DropWatch(Rc::clone(&capture_drops));
    let after_capture = DropWatch(Rc::clone(&capture_drops));
    let error = current
        .driver
        .finish(current.parent.remove_child(
            &removal,
            &current.child,
            move |_| {
                let _ = &before_capture;
                before.set(before.get() + 1);
            },
            move |_, _, _| {
                let _ = &after_capture;
                after.set(after.get() + 1);
            },
        ))
        .unwrap_err();
    assert_eq!(error.reason(), rca_core::TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
    assert_eq!(calls.get(), 0);
    assert_eq!(capture_drops.get(), 2);
    let observed = current
        .driver
        .finish(current.store.read(|root| {
            let parent = root.parent.as_ref().unwrap();
            (
                Rc::clone(&parent.child.as_ref().unwrap().0.value),
                parent.received,
                root.outcome.is_none(),
            )
        }))
        .unwrap();
    assert!(Rc::ptr_eq(&observed.0, &current.old));
    assert_eq!((observed.0.get(), observed.1, observed.2), (4, 0, true));
    assert_eq!(current.drops.get(), 0);
    assert_eq!(current.feature_drops.get(), 0);
    assert_eq!(current.self_handlers.get(), 0);
    drop(current.store);
    current.driver.turn();
    assert!(current.driver.0.borrow().is_empty());
    assert_eq!(current.drops.get(), 1);
    assert_eq!(current.feature_drops.get(), 1);
}

/// Retains the request so capture destruction can inspect its reply ordering.
type RemovalFuture =
    Pin<Box<dyn Future<Output = Result<rca_core::ActionTask, rca_core::TaskAccessError>>>>;
/// Observes the reply before the actor publishes its terminal failure.
struct ReplyOrder {
    /// Request already admitted before the actor invokes the failing callback.
    future: Rc<RefCell<Option<RemovalFuture>>>,
    /// Records whether capture destruction preceded reply completion.
    observed: Rc<Cell<bool>>,
}
impl Drop for ReplyOrder {
    fn drop(&mut self) {
        self.observed
            .set(poll(self.future.borrow_mut().as_mut().unwrap().as_mut()).is_pending());
    }
}

/// Destroys unentered callback captures before publishing an entered panic failure.
#[test]
fn local_panic_destroys_after_capture_before_terminal_reply() {
    let Fixture {
        driver,
        store,
        parent,
        child,
        removal,
        ..
    } = fixture();
    let future = Rc::new(RefCell::new(None));
    let observed = Rc::new(Cell::new(false));
    let capture = ReplyOrder {
        future: std::rc::Rc::clone(&future),
        observed: std::rc::Rc::clone(&observed),
    };
    let operation = parent.remove_child(
        &removal,
        &child,
        |_| panic!("before entered"),
        move |_, _, _| {
            drop(capture);
        },
    );
    *future.borrow_mut() = Some(Box::pin(operation) as RemovalFuture);
    assert!(poll(future.borrow_mut().as_mut().unwrap().as_mut()).is_pending());
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| driver.turn()))
        .expect_err("the before callback must panic");
    assert_eq!(panic.downcast_ref::<&str>(), Some(&"before entered"));
    assert!(
        observed.get(),
        "capture destructor sees the original reply still pending"
    );
    let operation = future.borrow_mut().take().unwrap();
    let error = driver.finish(operation).unwrap_err();
    assert_eq!(error.reason(), rca_core::TaskAccessReason::ActorFailed);
    assert_eq!(error.progress(), rca_core::TaskProgress::MayHaveRun);
    drop(store);
}

/// Suppresses delivery after feature destruction or an earlier spawn closes ownership.
#[test]
fn local_retired_feature_drop_and_earlier_runtime_spawn_close_suppress_transfer_callbacks() {
    for close_in_spawn in [false, true] {
        let Fixture {
            driver,
            store,
            parent,
            child,
            removal,
            output,
            drops,
            feature_drops,
            ..
        } = fixture();
        let closes = Rc::new(Cell::new(0));
        let close_count = std::rc::Rc::clone(&closes);
        let hook = if close_in_spawn { &driver.2 } else { &driver.3 };
        *hook.borrow_mut() = Some(Box::new(move || {
            close_count.set(close_count.get() + 1);
            drop(store);
        }));
        let receives = Rc::new(Cell::new(0));
        let delivery_count = std::rc::Rc::clone(&receives);
        let capture_drops = Rc::new(Cell::new(0));
        let capture = DropWatch(std::rc::Rc::clone(&capture_drops));
        let spawns = driver.1.get();
        let task = driver
            .finish(parent.remove_child(
                &removal,
                &child,
                |_| {},
                move |_, removed, context| {
                    if close_in_spawn {
                        context.task(rca_core::local::Task::run(|_| async {}));
                    }
                    removed
                        .into_output(
                            context,
                            &output,
                            |child| child.0,
                            move |_| {
                                delivery_count.set(delivery_count.get() + 1);
                                drop(capture);
                            },
                        )
                        .unwrap();
                },
            ))
            .unwrap();
        driver.finish(task.finish());
        driver.turn(); // Logical cancellation precedes the runtime wrapper's physical drop.
        assert_eq!(closes.get(), 1);
        assert_eq!(feature_drops.get(), 1);
        assert_eq!(receives.get(), 0);
        assert_eq!(drops.get(), 1);
        assert_eq!(capture_drops.get(), 1);
        assert_eq!(
            driver.1.get(),
            spawns + if close_in_spawn { 2 } else { 1 },
            "late closure does not erase already-authored ordinary launches"
        );
    }
}

/// Retains inert history without retaining child state, features or runtime owners.
#[test]
fn local_inert_origin_outlives_all_store_ports_values_and_runtime_owners() {
    let (origin, drops, feature_drops) = {
        let Fixture {
            driver,
            store,
            parent,
            child,
            removal,
            output,
            drops,
            feature_drops,
            ..
        } = fixture();
        let origin = Rc::new(RefCell::new(None));
        let captured = std::rc::Rc::clone(&origin);
        let task = driver
            .finish(parent.remove_child(
                &removal,
                &child,
                |_| {},
                move |_, removed, _| {
                    *captured.borrow_mut() = Some(removed.origin().clone());
                    drop(removed.into_inner());
                },
            ))
            .unwrap();
        driver.finish(task.finish());
        drop((parent, child, removal, output, store));
        driver.turn();
        drop(driver);
        let origin = origin.borrow_mut().take().unwrap();
        (origin, drops, feature_drops)
    };
    assert_eq!(drops.get(), 1);
    assert_eq!(feature_drops.get(), 1);
    assert_eq!(origin.configuration(), 0);
    assert_eq!(origin, origin.clone());
    assert_eq!(
        format!("{origin:?}"),
        "RemovalOrigin { configuration: 0, .. }"
    );
}

/// Extracts ownership without authoring an output task or invoking an output handler.
#[test]
fn local_plain_extraction_authors_no_output_work() {
    let Fixture {
        driver,
        store,
        parent,
        child,
        removal,
        drops,
        self_handlers,
        ..
    } = fixture();
    let spawns = driver.1.get();
    let task = driver
        .finish(parent.remove_child(
            &removal,
            &child,
            |_| {},
            |_, removed, _| {
                drop(removed.into_inner());
            },
        ))
        .unwrap();
    driver.finish(task.finish());
    assert_eq!(driver.1.get(), spawns);
    assert_eq!(drops.get(), 1);
    assert_eq!(self_handlers.get(), 0);
    assert!(
        driver
            .finish(store.read(|root| root.outcome.is_none()))
            .unwrap()
    );
    driver.finish(store.shutdown()).unwrap();
    drop(store);
    assert!(driver.0.borrow().is_empty(), "fixture releases its actor");
    let queue = Rc::downgrade(&driver.0);
    drop(driver);
    assert!(queue.upgrade().is_none());
}

/// Measures plain extraction without counting fixture construction or semantic checks.
#[cfg(feature = "allocation-measurement")]
#[test]
fn local_plain_removal_work_costs() {
    if allocation_test::run_in_child("local_plain_removal_work_costs") {
        return;
    }
    let profiler = dhat::Profiler::builder().testing().build();
    let Fixture {
        driver,
        store,
        parent,
        child,
        removal,
        old,
        drops,
        ..
    } = fixture();
    let extracted = Rc::new(RefCell::new(None));
    let export = Rc::clone(&extracted);
    let spawns = driver.1.get();
    let operation = parent.remove_child(
        &removal,
        &child,
        |child| child.0.value.set(7),
        move |_, removed, _| *export.borrow_mut() = Some(removed.into_inner()),
    );

    // This phase includes the finite driver's finish box and Vec requeue work.
    // Fixture construction and the following semantic checks are excluded.
    let before = dhat::HeapStats::get();
    let task = driver.finish(operation).unwrap();
    let after = dhat::HeapStats::get();
    let cost = (
        after.total_blocks - before.total_blocks,
        after.total_bytes - before.total_bytes,
    );

    driver.finish(task.finish());
    assert_eq!(driver.1.get(), spawns);
    let original = extracted.borrow_mut().take().unwrap();
    assert!(Rc::ptr_eq(&original.0.value, &old));
    assert_eq!(original.0.value.get(), 7);
    assert!(
        driver
            .finish(parent.read(|state| state.child.is_none()))
            .unwrap()
    );
    assert_eq!(
        driver.finish(child.read(|_| ())).unwrap_err().reason(),
        rca_core::TaskAccessReason::ScopeEnded
    );
    assert_eq!(drops.get(), 0);
    eprintln!("plain Local removal blocks/requested bytes: {cost:?}");
    drop(original);
    assert_eq!(drops.get(), 1);
    driver.finish(store.shutdown()).unwrap();
    drop(profiler);
}

/// Returns backpressured output when its parent configuration changes before delivery.
#[cfg(feature = "test-util")]
#[test]
fn local_parent_reconfiguration_returns_waiting_owned_transfer() {
    let gate = rca_core::testing::local::TransactionGate::<Root, ()>::new();
    let Fixture {
        driver,
        store,
        parent,
        child,
        removal,
        output,
        drops,
        ..
    } = fixture_with_gate(Some(gate.clone()));
    let captures = Rc::new(Cell::new(0));
    let capture = DropWatch(std::rc::Rc::clone(&captures));
    let receives = Rc::new(Cell::new(0));
    let delivery_count = std::rc::Rc::clone(&receives);
    let mut remove = Box::pin(parent.remove_child(
        &removal,
        &child,
        |_| {},
        move |_, removed, context| {
            removed
                .into_output(
                    context,
                    &output,
                    |child| child.0,
                    move |result| {
                        let Err(rca_core::OutputError::Returned { value, error }) = result else {
                            panic!("reconfigured parent returns owned output");
                        };
                        assert_eq!(error.reason(), rca_core::TaskAccessReason::ScopeEnded);
                        assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
                        delivery_count.set(delivery_count.get() + 1);
                        drop((value, capture));
                    },
                )
                .unwrap();
        },
    ));
    assert!(poll(remove.as_mut()).is_pending());
    driver.turn();
    let mut refresh = Box::pin(parent.update(|state| state.received = 1000));
    assert!(poll(refresh.as_mut()).is_pending());
    assert!(gate.release_pending());
    driver.turn();
    let Poll::Ready(Ok(task)) = poll(remove.as_mut()) else {
        panic!("removal committed");
    };
    for _ in 0..3 {
        driver.turn();
    }
    assert_eq!(drops.get(), 0);
    assert!(gate.acknowledge(gate.boundary().unwrap()));
    driver.turn();
    assert_eq!(
        gate.pending_kind(),
        Some(rca_core::testing::TransactionKind::Update)
    );
    assert!(gate.release_pending());
    driver.turn();
    assert!(gate.acknowledge(gate.boundary().unwrap()));
    for _ in 0..6 {
        driver.turn();
    }
    assert!(poll(refresh.as_mut()).is_ready());
    driver.finish(task.finish());
    assert_eq!(drops.get(), 1);
    assert_eq!(captures.get(), 1);
    assert_eq!(receives.get(), 1);
    assert_eq!(
        driver.finish(parent.read(|_| ())).unwrap_err().reason(),
        rca_core::TaskAccessReason::ScopeEnded
    );
    assert!(gate.pending_kind().is_none());
    assert!(
        driver
            .finish(store.read(|root| root.outcome.is_none()))
            .unwrap()
    );
    driver.finish(store.shutdown()).unwrap();
    drop(store);
    assert!(driver.0.borrow().is_empty(), "fixture releases its actor");
    let queue = Rc::downgrade(&driver.0);
    drop(driver);
    assert!(queue.upgrade().is_none());
}

/// Returns the owned output when its parent operation is cancelled before delivery.
#[cfg(feature = "test-util")]
#[test]
fn local_parent_operation_cancellation_rejects_transfer_without_handler() {
    let gate = rca_core::testing::local::TransactionGate::<Root, ()>::new();
    let Fixture {
        driver,
        store,
        parent,
        child,
        removal,
        output,
        drops,
        ..
    } = fixture_with_gate(Some(gate.clone()));
    let captures = Rc::new(Cell::new(0));
    let capture = DropWatch(std::rc::Rc::clone(&captures));
    let receives = Rc::new(Cell::new(0));
    let delivery_count = std::rc::Rc::clone(&receives);
    let mut remove = Box::pin(parent.remove_child(
        &removal,
        &child,
        |_| {},
        move |_, removed, context| {
            removed
                .into_output(
                    context,
                    &output,
                    |child| child.0,
                    move |result| {
                        let Err(rca_core::OutputError::Returned { value, error }) = result else {
                            panic!("cancelled output returns before handler entry");
                        };
                        assert_eq!(error.reason(), rca_core::TaskAccessReason::Cancelled);
                        assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
                        delivery_count.set(delivery_count.get() + 1);
                        drop((value, capture));
                    },
                )
                .unwrap();
        },
    ));
    assert!(poll(remove.as_mut()).is_pending());
    driver.turn();
    assert!(gate.release_pending());
    driver.turn();
    let Poll::Ready(Ok(task)) = poll(remove.as_mut()) else {
        panic!("removal committed");
    };
    task.cancel();
    assert!(gate.acknowledge(gate.boundary().unwrap()));
    driver.finish(task.finish());
    driver.turn();
    assert_eq!(drops.get(), 1);
    assert_eq!(captures.get(), 1);
    assert_eq!(receives.get(), 1);
    assert!(gate.pending_kind().is_none());
    assert!(
        driver
            .finish(store.read(|root| root.outcome.is_none()))
            .unwrap()
    );
    driver.finish(store.shutdown()).unwrap();
    drop(store);
    assert!(driver.0.borrow().is_empty(), "fixture releases its actor");
    let queue = Rc::downgrade(&driver.0);
    drop(driver);
    assert!(queue.upgrade().is_none());
}

/// Rejects backpressured removal after an earlier action retires its owning parent.
#[test]
fn local_full_ingress_removal_rejects_after_earlier_parent_retirement_without_callbacks() {
    let Fixture {
        driver,
        store,
        parent,
        child,
        removal,
        drops,
        feature_drops,
        ..
    } = fixture();
    let retire = store.try_send(()).unwrap();
    assert!(
        store.try_send(()).unwrap_err().is_full(),
        "the C=1 ordinary queue is full before removal is polled"
    );
    let captures = Rc::new(Cell::new(0));
    let before = DropWatch(std::rc::Rc::clone(&captures));
    let after = DropWatch(std::rc::Rc::clone(&captures));
    let mut removal = Box::pin(parent.remove_child(
        &removal,
        &child,
        move |_| {
            drop(before);
            panic!("retired parent must reject before callback");
        },
        move |_, _, _| {
            drop(after);
            panic!("retired parent must reject before callback");
        },
    ));
    assert!(poll(removal.as_mut()).is_pending());
    assert_eq!(
        captures.get(),
        0,
        "backpressured request retains both callback captures"
    );
    drop(driver.finish(retire.finish()).unwrap());
    let error = driver.finish(removal).unwrap_err();
    assert_eq!(error.reason(), rca_core::TaskAccessReason::ScopeEnded);
    assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
    assert_eq!(captures.get(), 2);
    assert_eq!(drops.get(), 1);
    assert_eq!(feature_drops.get(), 1);
    driver.finish(store.shutdown()).unwrap();
    drop(store);
    assert!(driver.0.borrow().is_empty(), "fixture releases its actor");
    let queue = Rc::downgrade(&driver.0);
    drop(driver);
    assert!(queue.upgrade().is_none());
}

/// Cancels backpressured transfer when an earlier root action retires its parent.
#[cfg(feature = "test-util")]
#[test]
fn local_full_ingress_and_parent_retirement_cancel_waiting_transfer() {
    let gate = rca_core::testing::local::TransactionGate::<Root, ()>::new();
    let Fixture {
        driver,
        store,
        parent,
        child,
        removal,
        output,
        drops,
        ..
    } = fixture_with_gate(Some(gate.clone()));
    let captures = Rc::new(Cell::new(0));
    let capture = DropWatch(std::rc::Rc::clone(&captures));
    let receives = Rc::new(Cell::new(0));
    let receive = std::rc::Rc::clone(&receives);
    let mut remove = Box::pin(parent.remove_child(
        &removal,
        &child,
        |_| {},
        move |_, removed, context| {
            removed
                .into_output(
                    context,
                    &output,
                    |child| child.0,
                    move |_| {
                        receive.set(receive.get() + 1);
                        drop(capture);
                    },
                )
                .unwrap();
        },
    ));
    assert!(poll(remove.as_mut()).is_pending());
    driver.turn();
    assert_eq!(
        gate.pending_kind(),
        Some(rca_core::testing::TransactionKind::Update)
    );
    let mut retire = Box::pin(store.send(()));
    assert!(poll(retire.as_mut()).is_pending());
    assert!(store.try_send(()).unwrap_err().is_full());
    assert!(gate.release_pending());
    driver.turn();
    let Poll::Ready(Ok(task)) = poll(remove.as_mut()) else {
        panic!("removal committed");
    };
    for _ in 0..3 {
        driver.turn();
    }
    assert_eq!(
        drops.get(),
        0,
        "the original Rc output remains owned while ingress is full"
    );
    assert_eq!(captures.get(), 0);
    assert!(gate.acknowledge(gate.boundary().unwrap()));
    driver.turn();
    assert_eq!(
        gate.pending_kind(),
        Some(rca_core::testing::TransactionKind::Action)
    );
    assert!(gate.release_pending());
    driver.turn();
    assert!(gate.acknowledge(gate.boundary().unwrap()));
    for _ in 0..6 {
        driver.turn();
    }
    assert!(poll(retire.as_mut()).is_ready());
    driver.finish(task.finish());
    assert_eq!(receives.get(), 0);
    assert_eq!(drops.get(), 1);
    assert_eq!(captures.get(), 1);
    assert_eq!(
        driver.finish(parent.read(|_| ())).unwrap_err().reason(),
        rca_core::TaskAccessReason::ScopeEnded
    );
    assert!(
        gate.pending_kind().is_none(),
        "cancelled transfer creates no phantom output frontier"
    );
    assert!(
        driver
            .finish(store.read(|root| root.outcome.is_none()))
            .unwrap()
    );
    drop(retire);
    driver.finish(store.shutdown()).unwrap();
    drop(store);
    assert!(driver.0.borrow().is_empty(), "fixture releases its actor");
    let queue = Rc::downgrade(&driver.0);
    drop(driver);
    assert!(queue.upgrade().is_none());
}

/// Rejects unadmitted removal immediately after owner closure, including full-mailbox admission.
#[test]
fn local_owner_close_rejects_unadmitted_removal_before_callbacks() {
    for backpressured in [false, true] {
        let Fixture {
            driver,
            store,
            parent,
            child,
            removal,
            drops,
            feature_drops,
            ..
        } = fixture();
        let filler = backpressured.then(|| store.try_send(()).unwrap());
        if backpressured {
            assert!(store.try_send(()).unwrap_err().is_full());
        }
        let captures = Rc::new(Cell::new(0));
        let before = DropWatch(Rc::clone(&captures));
        let after = DropWatch(Rc::clone(&captures));
        let mut remove = Box::pin(parent.remove_child(
            &removal,
            &child,
            move |_| {
                drop(before);
                panic!("closed Store must reject before entering removal");
            },
            move |_, _, _| {
                drop(after);
                panic!("closed Store must reject before entering removal");
            },
        ));
        if backpressured {
            assert!(poll(remove.as_mut()).is_pending());
        }
        assert_eq!(captures.get(), 0);
        let closed = store.closed();
        drop(store);
        let Poll::Ready(Err(error)) = poll(remove.as_mut()) else {
            panic!("owner closure must reject unadmitted removal without an actor turn");
        };
        assert_eq!(error.reason(), rca_core::TaskAccessReason::StoreClosed);
        assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
        assert_eq!(captures.get(), 2);
        assert_eq!(drops.get(), 0, "the actor still owns the child state");
        driver.finish(closed);
        driver.turn();
        if let Some(filler) = filler {
            driver.finish(filler.finish()).unwrap_err();
        }
        assert_eq!(drops.get(), 1);
        assert_eq!(feature_drops.get(), 1);
        assert!(driver.0.borrow().is_empty());
    }
}

/// Rechecks liveness after snapshot capture and mutable parent projection.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one entered-removal fixture distinguishes capture shutdown, missing parent projection and projection shutdown"
)]
fn local_removal_entry_rechecks_capture_and_mutable_parent_projection() {
    /// Separates parent absence, snapshot capture and mutable projection hooks.
    struct SelectingRoot {
        parent: Option<Parent>,
        missing: Rc<Cell<bool>>,
        capture_hook: DropHook,
        write_hook: DropHook,
        selections: Rc<Cell<usize>>,
    }
    /// Rejects accidental action reduction during task-context removal.
    struct SelectingFeature;
    impl Feature for SelectingFeature {
        type State = SelectingRoot;
        type Action = ();

        fn update(
            &mut self,
            _: &mut SelectingRoot,
            (): &(),
            _: &mut FeatureContext<'_, SelectingRoot, ()>,
        ) {
            panic!("rejected removal must not enter the root reducer");
        }
    }

    for mode in 0..3 {
        let driver = Driver::default();
        let owner = Rc::new(RefCell::new(None));
        let missing = Rc::new(Cell::new(false));
        let capture_hook: DropHook = Rc::new(RefCell::new(None));
        let write_hook: DropHook = Rc::new(RefCell::new(None));
        let selections = Rc::new(Cell::new(0));
        let value = Rc::new(Cell::new(7));
        let value_drops = Rc::new(Cell::new(0));
        let feature_drops = Rc::new(Cell::new(0));
        let feature_capture = Rc::clone(&feature_drops);
        let feature_hook = Rc::clone(&driver.3);
        let sites = Rc::new(RefCell::new(None));
        let export = Rc::clone(&sites);
        let (builder, parent_site) = rca_core::local::RootMount::new(SelectingFeature)
            .on_change(
                |root: &SelectingRoot| {
                    root.selections.set(root.selections.get() + 1);
                    let hook = root.capture_hook.borrow_mut().take();
                    if let Some(hook) = hook {
                        hook();
                    }
                },
                rca_core::InitialPolicy::Skip,
                |_, _| panic!("failed removal must not launch a reaction"),
            )
            .builder(
                SelectingRoot {
                    parent: Some(Parent {
                        child: Some(Child(Owned {
                            value: Rc::clone(&value),
                            drops: Rc::clone(&value_drops),
                        })),
                        received: 0,
                    }),
                    missing: Rc::clone(&missing),
                    capture_hook: Rc::clone(&capture_hook),
                    write_hook: Rc::clone(&write_hook),
                    selections: Rc::clone(&selections),
                },
                Runtime::new(driver.clone()),
            )
            .mount_with_children(
                OptionalMount::new(
                    |root: &SelectingRoot| {
                        if root.missing.get() {
                            None
                        } else {
                            root.parent.as_ref()
                        }
                    },
                    |root: &mut SelectingRoot| {
                        let hook = root.write_hook.borrow_mut().take();
                        if let Some(hook) = hook {
                            hook();
                        }
                        if root.missing.get() {
                            None
                        } else {
                            root.parent.as_mut()
                        }
                    },
                    |root: &SelectingRoot| {
                        root.parent.as_ref().map(|_| MountIdentity {
                            key: (),
                            replacement: 0,
                            configuration: 0,
                        })
                    },
                    |_| ParentFeature,
                ),
                move |_, children| {
                    let feature_capture = Rc::clone(&feature_capture);
                    let feature_hook = Rc::clone(&feature_hook);
                    *export.borrow_mut() = Some(children.optional_removable(
                        |parent: &Parent| &parent.child,
                        |parent: &mut Parent| &mut parent.child,
                        |parent: &Parent| {
                            parent.child.as_ref().map(|_| MountIdentity {
                                key: (),
                                replacement: 0,
                                configuration: 0,
                            })
                        },
                        move |_| {
                            ChildFeature(Rc::clone(&feature_capture), Rc::clone(&feature_hook))
                        },
                    ));
                },
            );
        let store = builder.build().unwrap();
        driver.finish(store.ready()).unwrap();
        let parent = driver.finish(store.acquire(&parent_site)).unwrap().unwrap();
        let (child_site, removal) = sites.borrow_mut().take().unwrap();
        let child = driver.finish(store.acquire(&child_site)).unwrap().unwrap();
        let selected = selections.get();
        let closed = store.closed();
        *owner.borrow_mut() = Some(store);
        if mode == 1 {
            missing.set(true);
        } else {
            let closing_owner = Rc::clone(&owner);
            let hook = Box::new(move || drop(closing_owner.borrow_mut().take()));
            if mode == 0 {
                *capture_hook.borrow_mut() = Some(hook);
            } else {
                *write_hook.borrow_mut() = Some(hook);
            }
        }
        let captures = Rc::new(Cell::new(0));
        let before = DropWatch(Rc::clone(&captures));
        let after = DropWatch(Rc::clone(&captures));
        let error = driver
            .finish(parent.remove_child(
                &removal,
                &child,
                move |_| {
                    drop(before);
                    panic!("entered rejection must suppress the before callback");
                },
                move |_, _, _| {
                    drop(after);
                    panic!("entered rejection must suppress the after callback");
                },
            ))
            .unwrap_err();
        assert_eq!(
            error.reason(),
            if mode == 1 {
                rca_core::TaskAccessReason::ScopeEnded
            } else {
                rca_core::TaskAccessReason::StoreClosed
            }
        );
        assert_eq!(error.progress(), rca_core::TaskProgress::MayHaveRun);
        assert_eq!(captures.get(), 2);
        assert_eq!(selections.get(), selected + 1);
        assert_eq!(value.get(), 7);
        if mode == 1 {
            assert_eq!(value_drops.get(), 0);
            let state = driver
                .finish(owner.borrow_mut().take().unwrap().shutdown_into_state())
                .unwrap();
            let recovered = state.parent.unwrap().child.unwrap().0;
            assert!(Rc::ptr_eq(&recovered.value, &value));
            assert_eq!(recovered.value.get(), 7);
            assert_eq!(value_drops.get(), 0);
            drop(recovered);
        } else {
            assert!(owner.borrow().is_none());
            driver.finish(closed);
            // Closure notification precedes physical actor-owned state disposal.
            driver.turn();
        }
        assert_eq!(value_drops.get(), 1);
        assert_eq!(feature_drops.get(), 1);
        assert!(driver.0.borrow().is_empty());
    }
}

/// Preserves held ownership and distinguishes removal origin from a later ordinary output.
#[cfg(feature = "test-util")]
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One held Local transfer preserves parent output, inert child origin and owned-value lifetime"
)]
fn local_held_transfer_keeps_parent_output_and_inert_child_origin_with_owned_value() {
    let gate = rca_core::testing::local::TransactionGate::<Root, ()>::new();
    let Fixture {
        driver,
        store,
        parent,
        child,
        removal,
        output,
        old,
        drops,
        ..
    } = fixture_with_gate(Some(gate.clone()));
    let origin = Rc::new(RefCell::new(None));
    let captured = std::rc::Rc::clone(&origin);
    let transfer_output = output.clone();
    let mut removal = Box::pin(parent.remove_child(
        &removal,
        &child,
        |_| {},
        move |_, removed, context| {
            *captured.borrow_mut() = Some(removed.origin().clone());
            removed
                .into_output(
                    context,
                    &transfer_output,
                    |child| child.0,
                    |result| {
                        assert!(matches!(result, Ok(OutputDelivery::Consumed { .. })));
                    },
                )
                .unwrap();
        },
    ));
    assert!(poll(removal.as_mut()).is_pending());
    driver.turn();
    let boundary = gate.boundary().unwrap();
    assert_eq!(
        gate.pending_kind(),
        Some(rca_core::testing::TransactionKind::Update),
        "owning removal is a mutation, not an action"
    );
    assert_eq!(
        gate.inspect_mounted(&child, |()| panic!("owning removal has no action input")),
        Ok(None::<()>)
    );
    assert_eq!(gate.boundary(), Some(boundary));
    assert!(poll(removal.as_mut()).is_pending());
    assert_eq!(drops.get(), 0);
    drop(
        driver
            .finish(gate.release_pending_with_receipt().unwrap().finish())
            .unwrap(),
    );
    let task = driver.finish(removal).unwrap();
    assert!(gate.acknowledge(boundary));
    driver.turn();
    let boundary = gate.boundary().unwrap();
    assert_eq!(
        gate.pending_kind(),
        Some(rca_core::testing::TransactionKind::Output)
    );
    assert_eq!(gate.output_origin(&output), *origin.borrow());
    assert_eq!(
        gate.inspect_output(&output, |value| Rc::ptr_eq(&value.value, &old)),
        Some(true)
    );
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        gate.inspect_output(&output, |_| panic!("inspection unwinds"));
    }))
    .expect_err("the output matcher panics");
    assert_eq!(panic.downcast_ref::<&str>(), Some(&"inspection unwinds"));
    assert_eq!(gate.output_origin(&output), *origin.borrow());
    assert_eq!(
        gate.inspect_output(&output, |value| value.value.get()),
        Some(4)
    );
    assert_eq!(drops.get(), 0);
    drop(
        driver
            .finish(gate.release_pending_with_receipt().unwrap().finish())
            .unwrap(),
    );
    assert!(gate.acknowledge(boundary));
    driver.finish(task.finish());

    let mut ordinary = Box::pin(parent.output(
        &output,
        Owned {
            value: Rc::new(Cell::new(9)),
            drops: Rc::new(Cell::new(0)),
        },
    ));
    assert!(poll(ordinary.as_mut()).is_pending());
    driver.turn();
    let boundary = gate.boundary().unwrap();
    assert_eq!(
        gate.output_origin(&output),
        None,
        "ordinary output does not inherit removal metadata"
    );
    drop(
        driver
            .finish(gate.release_pending_with_receipt().unwrap().finish())
            .unwrap(),
    );
    assert!(gate.acknowledge(boundary));
    assert!(matches!(
        driver.finish(ordinary),
        Ok(OutputDelivery::Consumed { .. })
    ));
    assert_eq!(
        drops.get(),
        1,
        "replacing the stored result drops the old outcome exactly once"
    );
    driver.finish(store.shutdown()).unwrap();
    drop(store);
    assert!(driver.0.borrow().is_empty(), "fixture releases its actor");
    let queue = Rc::downgrade(&driver.0);
    drop(driver);
    assert!(queue.upgrade().is_none());
}

/// Rejects completed or cancelled operation ancestry before owning removal can enter.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one owner-local named parent distinguishes completion retention from cancelled provenance while preserving its child"
)]
fn local_named_parent_removal_rejects_completed_ancestry_and_cancelled_producer() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Wake;

    thread_local! {
        /// Owns one callback for the current thread's operation-completion wake.
        static OBSERVER: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
    }
    /// Bridges a standard waker to one owner-local observation without transferring local state.
    struct ObserverWake {
        /// Thread allowed to invoke the callback held in its thread-local slot.
        owner: std::thread::ThreadId,
        /// Counts wake entry independently from the callback's observations.
        calls: AtomicUsize,
    }
    impl Wake for ObserverWake {
        fn wake(self: Arc<Self>) {
            assert_eq!(std::thread::current().id(), self.owner);
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(callback) = OBSERVER.with(|slot| slot.borrow_mut().take()) {
                callback();
            }
        }
    }
    /// Exports one named parent operation and separately records its completion and destruction.
    struct Producer {
        export: Rc<RefCell<Option<TaskContext<Parent, u8>>>>,
        complete: Rc<Cell<bool>>,
        drops: Rc<Cell<usize>>,
        reductions: Rc<Cell<usize>>,
    }
    impl Feature for Producer {
        type State = Parent;
        type Action = u8;

        fn update(
            &mut self,
            _: &mut Parent,
            action: &u8,
            context: &mut FeatureContext<'_, Parent, u8>,
        ) {
            self.reductions.set(self.reductions.get() + 1);
            assert_eq!(*action, 0);
            let export = Rc::clone(&self.export);
            let complete = Rc::clone(&self.complete);
            let capture = DropWatch(Rc::clone(&self.drops));
            context.task(rca_core::local::Task::run_named(
                "parent",
                rca_core::OperationPolicy::Coexist,
                move |task| async move {
                    let _capture = capture;
                    *export.borrow_mut() = Some(task);
                    std::future::poll_fn(move |_| {
                        if complete.get() {
                            Poll::Ready(())
                        } else {
                            Poll::Pending
                        }
                    })
                    .await;
                },
            ));
        }
    }

    for cancelled in [false, true] {
        let driver = Driver::default();
        let export = Rc::new(RefCell::new(None));
        let complete = Rc::new(Cell::new(false));
        let producer_drops = Rc::new(Cell::new(0));
        let reductions = Rc::new(Cell::new(0));
        let value = Rc::new(Cell::new(7));
        let value_drops = Rc::new(Cell::new(0));
        let feature_drops = Rc::new(Cell::new(0));
        let feature_capture = Rc::clone(&feature_drops);
        let feature_hook = Rc::clone(&driver.3);
        let (builder, site, removal) = rca_core::local::RootMount::new(Producer {
            export: Rc::clone(&export),
            complete: Rc::clone(&complete),
            drops: Rc::clone(&producer_drops),
            reductions: Rc::clone(&reductions),
        })
        .builder(
            Parent {
                child: Some(Child(Owned {
                    value: Rc::clone(&value),
                    drops: Rc::clone(&value_drops),
                })),
                received: 0,
            },
            Runtime::new(driver.clone()),
        )
        .mount_removable(
            |parent| &parent.child,
            |parent| &mut parent.child,
            |parent| {
                parent.child.as_ref().map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            move |_| ChildFeature(Rc::clone(&feature_capture), Rc::clone(&feature_hook)),
        );
        let store = builder.build().unwrap();
        driver.finish(store.ready()).unwrap();
        let child = driver.finish(store.acquire(&site)).unwrap().unwrap();
        let mut actor = {
            let mut queued = driver.0.borrow_mut();
            assert_eq!(queued.len(), 1);
            queued.pop().unwrap()
        };
        let mut start = Box::pin(store.send(0));
        assert!(poll(start.as_mut()).is_pending());
        assert!(poll(actor.as_mut()).is_pending());
        let Poll::Ready(Ok(task)) = poll(start.as_mut()) else {
            panic!("Local named parent producer must be registered");
        };
        drop(start);
        let mut capture = Box::pin(store.operation("parent").capture());
        assert!(poll(capture.as_mut()).is_pending());
        assert!(poll(actor.as_mut()).is_pending());
        let Poll::Ready(Ok(wait)) = poll(capture.as_mut()) else {
            panic!("Local named operation must be captured while live");
        };
        drop(capture);
        let mut wait = Box::pin(wait);
        let mut run = {
            let mut queued = driver.0.borrow_mut();
            assert_eq!(queued.len(), 1);
            queued.pop().unwrap()
        };
        assert!(poll(run.as_mut()).is_pending());
        let parent = export.borrow_mut().take().unwrap();
        assert!(!task.is_finished());
        assert!(!task.is_cancelled());
        let calls = Rc::new(Cell::new(0));
        let callback_drops = Rc::new(Cell::new(0));
        let before = DropWatch(Rc::clone(&callback_drops));
        let after = DropWatch(Rc::clone(&callback_drops));
        let before_calls = Rc::clone(&calls);
        let after_calls = Rc::clone(&calls);
        let mut request = Box::pin(parent.remove_child(
            &removal,
            &child,
            move |child| {
                let _ = &before;
                before_calls.set(before_calls.get() + 1);
                child.0.value.set(100);
            },
            move |_, _, _| {
                let _ = &after;
                after_calls.set(after_calls.get() + 1);
            },
        ));

        if cancelled {
            assert!(poll(wait.as_mut()).is_pending());
            task.cancel();
            assert!(poll(actor.as_mut()).is_pending());
            let Poll::Ready(Err(error)) = poll(request.as_mut()) else {
                panic!("cancelled Local producer must reject parent removal before entry");
            };
            assert_eq!(error.reason(), rca_core::TaskAccessReason::Cancelled);
            assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
            assert_eq!(callback_drops.get(), 2);
            assert!(poll(run.as_mut()).is_ready());
            assert!(poll(actor.as_mut()).is_pending());
            assert!(task.is_finished());
            assert!(task.is_cancelled());
        } else {
            let observed = Rc::new(Cell::new(false));
            let observed_in_wake = Rc::clone(&observed);
            let task_in_wake = task.clone();
            let drops_in_wake = Rc::clone(&callback_drops);
            OBSERVER.with(|slot| {
                assert!(slot.borrow().is_none());
                *slot.borrow_mut() = Some(Box::new(move || {
                    // Observe only escaped access; never re-enter the actor or
                    // the task whose operation lease is being completed.
                    assert!(!task_in_wake.is_finished());
                    assert!(!task_in_wake.is_cancelled());
                    let Poll::Ready(Err(error)) = poll(request.as_mut()) else {
                        panic!("finished Local named ownership must not be resurrected by removal");
                    };
                    assert_eq!(error.reason(), rca_core::TaskAccessReason::Cancelled);
                    assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
                    assert_eq!(drops_in_wake.get(), 2);
                    observed_in_wake.set(true);
                }));
            });
            let wake = Arc::new(ObserverWake {
                owner: std::thread::current().id(),
                calls: AtomicUsize::new(0),
            });
            let waker = Waker::from(Arc::clone(&wake));
            assert!(
                wait.as_mut()
                    .poll(&mut Context::from_waker(&waker))
                    .is_pending()
            );
            complete.set(true);
            assert!(poll(run.as_mut()).is_ready());
            assert!(observed.get());
            assert_eq!(wake.calls.load(Ordering::SeqCst), 1);
            OBSERVER.with(|slot| assert!(slot.borrow().is_none()));
            assert!(
                !task.is_finished(),
                "terminal publication awaits actor harvest"
            );
            assert!(poll(actor.as_mut()).is_pending());
            assert!(task.is_finished());
            assert!(!task.is_cancelled());
        }
        assert_eq!(poll(wait.as_mut()), Poll::Ready(Ok(())));
        assert_eq!(calls.get(), 0);
        assert_eq!(callback_drops.get(), 2);
        assert_eq!(producer_drops.get(), 1);
        assert_eq!(reductions.get(), 1);
        assert_eq!(value.get(), 7);
        assert_eq!(value_drops.get(), 0);
        assert_eq!(feature_drops.get(), 0);
        let mut shutdown = Box::pin(store.shutdown_into_state());
        assert!(poll(shutdown.as_mut()).is_pending());
        assert!(poll(actor.as_mut()).is_ready());
        let Poll::Ready(Ok(state)) = poll(shutdown.as_mut()) else {
            panic!("rejected Local removal leaves its original child available to shutdown");
        };
        assert_eq!(state.received, 0);
        let owned = state.child.unwrap().0;
        assert!(Rc::ptr_eq(&owned.value, &value));
        assert_eq!(owned.value.get(), 7);
        assert_eq!(value_drops.get(), 0);
        drop(owned);
        assert_eq!(value_drops.get(), 1);
        assert_eq!(feature_drops.get(), 1);
        assert!(driver.0.borrow().is_empty());
    }
}

mod endpoint_validation {
    //! Rejects mismatched declaration relationships while supplied contexts remain live.

    use super::{Driver, DropWatch};
    use rca_core::local::{
        EntryMount, Feature, FeatureContext, MountChildren, MountSite, OptionalMount, RemovalSite,
        Runtime, Store,
    };
    use rca_core::{KeyedState, MountIdentity, TaskAccessReason, TaskProgress};
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    /// Separates stable allocation identity from physical value destruction.
    struct Value {
        /// Boxed payload used to detect replacement or unintended selection.
        value: Box<Cell<usize>>,
        /// Counts all value destruction across the six mounted children.
        drops: Rc<Cell<usize>>,
    }
    impl Drop for Value {
        fn drop(&mut self) {
            self.drops.set(self.drops.get() + 1);
        }
    }
    /// Counts mounted feature destruction separately from value destruction.
    struct Leaf {
        /// Retains one feature-lifetime witness until physical destruction.
        _capture: DropWatch,
    }
    impl Feature for Leaf {
        type State = Value;
        type Action = ();
        fn update(&mut self, _: &mut Value, (): &(), _: &mut FeatureContext<'_, Value, ()>) {}
    }

    /// Supplies optional and keyed children under one distinguishable parent identity.
    struct Parent {
        /// Distinguishes live, fixed and keyed parents in exported declarations.
        id: u8,
        /// Optional child with a stable declaration across absence.
        optional: Option<Value>,
        /// Keyed child whose declaration retires with its entry.
        keyed: KeyedState<u8, Value>,
        /// Counts owning selectors to detect entry before validation succeeds.
        selections: Rc<Cell<usize>>,
    }
    /// Keeps one parent live while fixed and keyed siblings retire.
    struct Root {
        /// Supplies the still-live parent and children used for rejection controls.
        live: Option<Parent>,
        /// Supplies an optional declaration that becomes absent.
        fixed: Option<Parent>,
        /// Supplies an entry declaration that retires completely.
        keyed: KeyedState<u8, Parent>,
    }
    /// Retires the fixed and keyed parents while preserving the live control parent.
    struct RootFeature;
    /// Leaves child changes to explicit task-context operations.
    struct ParentFeature;
    impl Feature for RootFeature {
        type State = Root;
        type Action = ();
        fn update(&mut self, state: &mut Root, (): &(), _: &mut FeatureContext<'_, Root, ()>) {
            state.fixed = None;
            state.keyed = KeyedState::new();
        }
    }
    impl Feature for ParentFeature {
        type State = Parent;
        type Action = ();
        fn update(&mut self, _: &mut Parent, (): &(), _: &mut FeatureContext<'_, Parent, ()>) {}
    }
    /// Associates exported authority with its parent and child storage kind.
    struct Sites {
        /// Original parent identity captured during declaration.
        parent_id: u8,
        /// Distinguishes the keyed child from the optional child.
        keyed: bool,
        /// Access declaration for the parent that owns this field.
        parent: MountSite<Root, Parent, (), ()>,
        /// Access declaration for the exact installed child.
        child: MountSite<Root, Value, (), ()>,
        /// Owning-removal endpoint that must match both supplied contexts.
        removal: RemovalSite<Parent, Value, ()>,
    }
    /// Holds same-actor declarations and independent selection and destruction counters.
    struct Fixture {
        /// Drives all actor work outside local borrows.
        driver: Driver,
        /// Retains the root until explicit shutdown.
        store: Store<Root, ()>,
        /// Exports every parent/child/removal relationship for deliberate mismatches.
        sites: Vec<Sites>,
        /// Counts owning field selection, which rejection must not enter.
        selections: Rc<Cell<usize>>,
        /// Counts all six child values independently from their features.
        values: Rc<Cell<usize>>,
        /// Counts all six mounted features independently from their values.
        features: Rc<Cell<usize>>,
    }

    /// Installs six children across live, optional and keyed parent lifetimes.
    #[expect(
        clippy::too_many_lines,
        reason = "One actor provides fixed and keyed declaration lifetimes with identical public child operations"
    )]
    fn fixture() -> Fixture {
        let driver = Driver::default();
        let selections = Rc::new(Cell::new(0));
        let values = Rc::new(Cell::new(0));
        let features = Rc::new(Cell::new(0));
        let parent = |id| Parent {
            id,
            optional: Some(Value {
                value: Box::new(Cell::new(usize::from(id) * 10)),
                drops: Rc::clone(&values),
            }),
            keyed: [(
                1,
                Value {
                    value: Box::new(Cell::new(usize::from(id) * 10 + 1)),
                    drops: Rc::clone(&values),
                },
            )]
            .into_iter()
            .collect(),
            selections: Rc::clone(&selections),
        };
        let exports = Rc::new(RefCell::new(Vec::new()));
        let export = Rc::clone(&exports);
        let feature_drops = Rc::clone(&features);
        let recipe =
            move |state: &Parent, children: &mut MountChildren<'_, Root, (), Parent, ()>| {
                let parent_id = state.id;
                let parent_site = children.site().clone();
                let drops = Rc::clone(&feature_drops);
                let (child, removal) = children.optional_removable(
                    |parent| &parent.optional,
                    |parent| {
                        parent.selections.set(parent.selections.get() + 1);
                        &mut parent.optional
                    },
                    |parent| {
                        parent.optional.as_ref().map(|_| MountIdentity {
                            key: (),
                            replacement: 0,
                            configuration: 0,
                        })
                    },
                    move |_| Leaf {
                        _capture: DropWatch(Rc::clone(&drops)),
                    },
                );
                export.borrow_mut().push(Sites {
                    parent_id,
                    keyed: false,
                    parent: parent_site.clone(),
                    child,
                    removal,
                });
                let export = Rc::clone(&export);
                let drops = Rc::clone(&feature_drops);
                children.keyed_removable(
                    |parent| &parent.keyed,
                    |parent| {
                        parent.selections.set(parent.selections.get() + 1);
                        &mut parent.keyed
                    },
                    move |_, _| {
                        let export = Rc::clone(&export);
                        let parent_site = parent_site.clone();
                        (
                            EntryMount::new(Leaf {
                                _capture: DropWatch(Rc::clone(&drops)),
                            }),
                            move |_: &Value,
                                  children: &mut MountChildren<'_, Root, (), Value, ()>,
                                  removal| {
                                export.borrow_mut().push(Sites {
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
        let (builder, _) = Store::builder(
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
        let sites = std::mem::take(&mut *exports.borrow_mut());
        Fixture {
            driver,
            store,
            sites,
            selections,
            values,
            features,
        }
    }

    /// Selects the exact relationship or lifetime that invalidates removal authority.
    #[derive(Clone, Copy, Debug)]
    enum Mismatch {
        /// Supplies a sibling parent context for the live child's removal site.
        DirectParent,
        /// Supplies a still-live sibling removal endpoint with the live contexts.
        LiveEndpointParent,
        /// Uses an endpoint whose optional parent is absent.
        AbsentFixedParent,
        /// Uses an endpoint whose keyed parent entry retired.
        RetiredKeyedParent,
        /// Uses the absent optional child's endpoint with a live keyed child.
        AbsentFixedChild,
        /// Uses a retired keyed-child endpoint with a live optional child.
        RetiredKeyedChild,
    }

    /// Rejects six mismatches before selection while preserving the supplied live child's identity.
    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "Six exact endpoint validation boundaries share one public same-actor schedule and conservation assertions"
    )]
    fn local_stale_or_mismatched_sites_reject_live_contexts_before_owning_selection() {
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
            let captures = Rc::new(Cell::new(0));
            let before = DropWatch(Rc::clone(&captures));
            let after = DropWatch(Rc::clone(&captures));
            let selections = f.selections.get();
            let values = f.values.get();
            let features = f.features.get();
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
            assert_eq!(captures.get(), 2);
            assert_eq!(f.selections.get(), selections);
            assert_eq!(f.values.get(), values);
            assert_eq!(f.features.get(), features);
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
            assert_eq!(f.values.get(), 6);
            assert_eq!(f.features.get(), 6);
            assert!(f.driver.0.borrow().is_empty());
        }
    }
}
