//! Owned retirement transfers through surviving parent authority, never a stale child.

use std::cell::Cell;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use rca_core::{
    Feature, FeatureContext, MonotonicInstant, MountIdentity, MountSite, OptionalMount,
    OutputDelivery, OutputFlow, RemovalSite, RootMount, Runtime, RuntimeDriver, RuntimeFuture,
};

#[cfg(feature = "allocation-measurement")]
#[global_allocator]
/// Counts allocations inside the isolated removal-cost test.
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

/// Runs the existing allocation assay in its own process.
#[cfg(feature = "allocation-measurement")]
#[path = "../src/allocation_test/child.rs"]
mod allocation_test;

/// Adds authority-boundary schedules that use this host's driver.
#[path = "removed_child/authority_schedules.rs"]
mod authority_schedules;
/// Adds selector, key and callback schedules that use this host's driver.
#[path = "removed_child/semantic_matrix.rs"]
mod semantic_matrix;

/// Drives explicit task turns and records accepted spawns.
///
/// The clock stays at zero. Callers must drive shutdown or drop extracted tasks
/// before dropping the last driver handle. Queued actor tasks retain this driver.
#[derive(Clone, Default)]
struct Driver(
    /// Tasks owned by the host until a turn completes or takes them.
    Arc<Mutex<Vec<RuntimeFuture>>>,
    /// Accepted spawns, independent from completed or cancelled work.
    Arc<AtomicUsize>,
    /// One callback taken after the next spawn releases the queue lock.
    DropHook,
    /// One callback taken when the mounted child feature is destroyed.
    DropHook,
);

/// Shares one callback without invoking it while its slot is locked.
type DropHook = Arc<Mutex<Option<Box<dyn FnOnce() + Send>>>>;

impl std::fmt::Debug for Driver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RemovalDriver")
    }
}

impl RuntimeDriver for Driver {
    fn spawn(&self, task: RuntimeFuture) {
        self.1.fetch_add(1, Ordering::SeqCst);
        self.0.lock().unwrap().push(task);
        let hook = self.2.lock().unwrap().take();
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
    /// Polls the current task batch outside the queue lock.
    fn turn(&self) {
        let pending = std::mem::take(&mut *self.0.lock().unwrap());
        for mut future in pending {
            if poll(future.as_mut()).is_pending() {
                self.0.lock().unwrap().push(future);
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
        panic!("finite removal operation did not finish");
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

/// Keeps a stable allocation identity without requiring a `Sync` payload.
struct Child {
    /// Mutable value whose address distinguishes removal from reconstruction.
    value: Box<Cell<usize>>,
}

/// Owns the optional child that removal can replace.
struct Parent {
    /// The active child, separate from any returned retired value.
    child: Option<Child>,
}

/// Receives transferred values above their surviving parent.
struct Root {
    /// The parent whose authority can survive child retirement.
    parent: Option<Parent>,
    /// The exact owned value delivered through a strict ancestor.
    outcome: Option<Box<Cell<usize>>>,
}

/// Retires the parent when the root receives its only action.
struct RootFeature;
/// Leaves parent mutations to explicit task-context operations.
struct ParentFeature;
/// Provides a child mount without action-driven mutations.
struct ChildFeature;
/// Runs the one-shot retirement hook when reconciliation destroys the feature.
struct ClosingChildFeature(DropHook);

impl Drop for ClosingChildFeature {
    fn drop(&mut self) {
        let hook = self.0.lock().unwrap().take();
        if let Some(hook) = hook {
            hook();
        }
    }
}
impl Feature for ClosingChildFeature {
    type State = Child;
    type Action = ();
    fn update(&mut self, _: &mut Child, (): &(), _: &mut FeatureContext<'_, Child, ()>) {}
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

/// Counts physical destruction without asserting or calling application code.
struct DropWatch(Arc<AtomicUsize>);
impl Drop for DropWatch {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// Holds independent authority, identity and callback witnesses for one nested Store.
struct Fixture {
    /// Manually polls actor and task futures.
    driver: Driver,
    /// Keeps the root actor live until explicit teardown.
    store: rca_core::Store<Root, ()>,
    /// Surviving authority used to remove and transfer the child.
    parent: rca_core::TaskContext<Parent, ()>,
    /// Captures the original child lifetime, never its replacement.
    child: rca_core::TaskContext<Child, ()>,
    /// Acquires the current optional child after replacement or reconfiguration.
    child_site: MountSite<Root, Child, (), ()>,
    /// Declares which parent owns the removable field.
    removal: RemovalSite<Parent, Child, ()>,
    /// Transfers the original boxed value to the root.
    output: rca_core::Output<Box<Cell<usize>>>,
    /// Transfers a separately counted destruction witness.
    tracked: rca_core::Output<DropWatch>,
    /// Records actual entry into the tracked root handler.
    handlers: Arc<AtomicUsize>,
    /// Counts child features across replacement and reconfiguration.
    constructors: Arc<AtomicUsize>,
    /// Original payload address, retained only for identity comparisons.
    old_identity: usize,
}

/// Builds the ordinary fixture without a held transaction frontier.
fn fixture() -> Fixture {
    fixture_with_gate(
        #[cfg(feature = "test-util")]
        None,
    )
}

/// Installs nested removal and output declarations, then completes initial acquisition.
#[expect(
    clippy::too_many_lines,
    reason = "One owned-removal fixture preserves nested mount, output and transaction-gate setup order"
)]
fn fixture_with_gate(
    #[cfg(feature = "test-util")] gate: Option<&rca_core::testing::TransactionGate<Root, ()>>,
) -> Fixture {
    let driver = Driver::default();
    let old = Box::new(Cell::new(4));
    let old_identity = (&raw const *old) as usize;
    let sites: Arc<Mutex<Option<Sites>>> = Arc::new(Mutex::new(None));
    let export = Arc::clone(&sites);
    let constructors = Arc::new(AtomicUsize::new(0));
    let makes = Arc::clone(&constructors);
    let feature_close = Arc::clone(&driver.3);
    let (builder, parent_site) = RootMount::new(RootFeature)
        .builder(
            Root {
                parent: Some(Parent {
                    child: Some(Child { value: old }),
                }),
                outcome: None,
            },
            Runtime::new(driver.clone()),
        )
        .mailbox_capacity(1)
        .mount_with_children(
            OptionalMount::new(
                |state: &Root| state.parent.as_ref(),
                |state: &mut Root| state.parent.as_mut(),
                |state: &Root| {
                    state.parent.as_ref().map(|_| MountIdentity {
                        key: (),
                        replacement: 0,
                        configuration: 0,
                    })
                },
                |_| ParentFeature,
            ),
            move |_, children| {
                let makes = Arc::clone(&makes);
                let feature_close = Arc::clone(&feature_close);
                let pair = children.optional_removable(
                    |state: &Parent| &state.child,
                    |state: &mut Parent| &mut state.child,
                    |state: &Parent| {
                        state.child.as_ref().map(|child| MountIdentity {
                            key: (),
                            replacement: 0,
                            configuration: (child.value.get() / 1000) as u64,
                        })
                    },
                    move |_| {
                        makes.fetch_add(1, Ordering::SeqCst);
                        ClosingChildFeature(Arc::clone(&feature_close))
                    },
                );
                *export.lock().unwrap() = Some(pair);
            },
        );
    let (builder, output) = builder.output::<Box<Cell<usize>>>(&parent_site, |handlers| {
        handlers.at_root(|state, value, _| {
            state.outcome = Some(value);
            OutputFlow::Consumed
        });
    });
    let handlers = Arc::new(AtomicUsize::new(0));
    let called = std::sync::Arc::clone(&handlers);
    let (builder, tracked) = builder.output::<DropWatch>(&parent_site, |handlers| {
        handlers.at_root(move |_, value, _| {
            called.fetch_add(1, Ordering::SeqCst);
            drop(value);
            OutputFlow::Consumed
        });
    });
    #[cfg(feature = "test-util")]
    let builder = if let Some(gate) = gate {
        builder.transaction_gate(gate.clone())
    } else {
        builder
    };
    let store = builder.build().unwrap();
    #[cfg(feature = "test-util")]
    if let Some(gate) = gate {
        driver.turn();
        assert_eq!(
            gate.pending_kind(),
            Some(rca_core::testing::TransactionKind::Structural)
        );
        assert!(gate.release_pending());
        driver.turn();
        assert!(gate.acknowledge(gate.boundary().unwrap()));
    }
    driver.finish(store.ready()).unwrap();
    let (child_site, removal) = sites.lock().unwrap().take().unwrap();
    let parent = driver.finish(store.acquire(&parent_site)).unwrap().unwrap();
    let child = driver.finish(store.acquire(&child_site)).unwrap().unwrap();
    Fixture {
        driver,
        store,
        parent,
        child,
        child_site,
        removal,
        output,
        tracked,
        handlers,
        constructors,
        old_identity,
    }
}

/// Keeps the removal receipt pending until parent-owned cleanup releases the retired child.
#[test]
fn optional_removal_receipt_awaits_parent_owned_cleanup() {
    let Fixture {
        driver,
        store,
        parent,
        child,
        removal,
        old_identity,
        ..
    } = fixture();
    let (release, wait) = futures_channel::oneshot::channel::<()>();
    let drops = Arc::new(AtomicUsize::new(0));
    let owner = DropWatch(Arc::clone(&drops));
    let task = driver
        .finish(parent.remove_child(
            &removal,
            &child,
            |child| child.value.set(7),
            move |parent, removed, context| {
                parent.child = Some(Child {
                    value: Box::new(Cell::new(100)),
                });
                let original = (removed.into_inner(), owner);
                context.task(rca_core::Task::run(move |_| async move {
                    wait.await.unwrap();
                    assert_eq!((&raw const *original.0.value) as usize, old_identity);
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
            .finish(parent.read(|parent| parent.child.as_ref().unwrap().value.get()))
            .unwrap(),
        100
    );
    let mut completion = Box::pin(task.finish());
    driver.turn();
    assert!(poll(completion.as_mut()).is_pending());
    assert!(!task.is_finished());
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    release.send(()).unwrap();
    driver.finish(completion);
    assert!(task.is_finished());
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(
        driver
            .finish(parent.read(|parent| parent.child.as_ref().unwrap().value.get()))
            .unwrap(),
        100
    );
    driver.finish(store.shutdown()).unwrap();
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

/// Preserves both owned final values while parent inputs commit between their inspections.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One sibling-removal schedule keeps extraction, queued pings, gated inspection and exact disposal together"
)]
fn owned_sibling_values_survive_queued_parent_inputs() {
    /// Owns one move-only value and records its physical disposal.
    struct Owned {
        value: Box<Cell<usize>>,
        _owner: DropWatch,
    }

    /// Keeps child storage separate from queued parent input state.
    struct Siblings {
        children: [Option<Owned>; 2],
        pings: usize,
    }

    /// Extracts both children or records an ordinary queued parent input.
    enum Input {
        Remove,
        Ping,
    }

    /// Transfers both originals into tracked work with an explicit inspection gate.
    struct Parent {
        gates: Option<(
            futures_channel::oneshot::Sender<()>,
            futures_channel::oneshot::Receiver<()>,
        )>,
        observed: Arc<Mutex<Vec<(usize, usize)>>>,
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
                    state.children[0].as_ref().unwrap().value.set(1);
                    state.children[1].as_ref().unwrap().value.set(2);
                    let first = state.children[0].take().unwrap();
                    let second = state.children[1].take().unwrap();
                    let (first_ping, release) = self.gates.take().unwrap();
                    let observed = Arc::clone(&self.observed);

                    context.task(rca_core::Task::run(move |parent| async move {
                        observed
                            .lock()
                            .unwrap()
                            .push(((&raw const *first.value).addr(), first.value.get()));
                        // The reply proves commit; its receipt includes this producing task.
                        drop(parent.send(Input::Ping).await.unwrap());
                        first_ping.send(()).unwrap();
                        release.await.unwrap();

                        observed
                            .lock()
                            .unwrap()
                            .push(((&raw const *second.value).addr(), second.value.get()));
                        drop(parent.send(Input::Ping).await.unwrap());
                        assert_eq!(first.value.get(), 1);
                        assert_eq!(second.value.get(), 2);
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

    /// Counts feature retirement separately from the extracted value's lifetime.
    struct MountedChild {
        _owner: DropWatch,
    }

    impl Feature for MountedChild {
        type State = Owned;
        type Action = ();

        fn update(&mut self, _: &mut Owned, (): &(), _: &mut FeatureContext<'_, Owned, ()>) {}
    }

    let driver = Driver::default();
    let weak_queue = Arc::downgrade(&driver.0);
    let owners = Arc::new(AtomicUsize::new(0));
    let features = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new(Mutex::new(Vec::new()));
    let first = Box::new(Cell::new(0));
    let second = Box::new(Cell::new(0));
    let identities = [(&raw const *first).addr(), (&raw const *second).addr()];
    let (notify_first_ping, first_ping) = futures_channel::oneshot::channel();
    let (release, wait) = futures_channel::oneshot::channel();
    let builder = rca_core::Store::builder(
        Siblings {
            children: [first, second].map(|value| {
                Some(Owned {
                    value,
                    _owner: DropWatch(Arc::clone(&owners)),
                })
            }),
            pings: 0,
        },
        Parent {
            gates: Some((notify_first_ping, wait)),
            observed: Arc::clone(&observed),
        },
        Runtime::new(driver.clone()),
    );
    let feature_drops = Arc::clone(&features);
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
        move |_| MountedChild {
            _owner: DropWatch(Arc::clone(&feature_drops)),
        },
    ));
    let feature_drops = Arc::clone(&features);
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
        move |_| MountedChild {
            _owner: DropWatch(Arc::clone(&feature_drops)),
        },
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
    assert_eq!(*observed.lock().unwrap(), [(identities[0], 1)]);
    for child in &children {
        let error = driver.finish(child.read(|_| ())).unwrap_err();
        assert_eq!(error.reason(), rca_core::TaskAccessReason::ScopeEnded);
        assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
    }
    assert_eq!(features.load(Ordering::SeqCst), 2);
    assert_eq!(owners.load(Ordering::SeqCst), 0);
    let mut completion = Box::pin(work.finish());
    assert!(poll(completion.as_mut()).is_pending());
    assert!(!work.is_finished());

    release.send(()).unwrap();
    driver.finish(completion);
    assert!(work.is_finished());
    assert_eq!(driver.finish(store.read(|state| state.pings)).unwrap(), 2);
    assert_eq!(
        *observed.lock().unwrap(),
        [(identities[0], 1), (identities[1], 2)]
    );
    assert_eq!(owners.load(Ordering::SeqCst), 2);
    assert_eq!(features.load(Ordering::SeqCst), 2);
    driver.finish(store.shutdown()).unwrap();
    assert!(driver.0.lock().unwrap().is_empty());
    drop((store, children, sites, work, driver));
    assert!(weak_queue.upgrade().is_none());
    assert_eq!(owners.load(Ordering::SeqCst), 2);
}

/// Measures plain extraction without counting fixture construction or semantic checks.
///
/// # Panics
/// Panics if removal ownership or allocation counts change.
#[cfg(feature = "allocation-measurement")]
pub fn plain_removal_work_costs() {
    if allocation_test::run_in_child("plain_removal_work_costs") {
        return;
    }
    let profiler = dhat::Profiler::builder().testing().build();
    let Fixture {
        driver,
        store,
        parent,
        child,
        removal,
        old_identity,
        ..
    } = fixture();
    let extracted = Arc::new(Mutex::new(None));
    let export = Arc::clone(&extracted);
    let spawns = driver.1.load(Ordering::SeqCst);
    let operation = parent.remove_child(
        &removal,
        &child,
        |child| child.value.set(7),
        move |_, removed, _| *export.lock().unwrap() = Some(removed.into_inner()),
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
    assert_eq!(driver.1.load(Ordering::SeqCst), spawns);
    let original = extracted.lock().unwrap().take().unwrap();
    assert_eq!((&raw const *original.value) as usize, old_identity);
    assert_eq!(original.value.get(), 7);
    assert!(
        driver
            .finish(parent.read(|state| state.child.is_none()))
            .unwrap()
    );
    assert_eq!(
        driver.finish(child.read(|_| ())).unwrap_err().reason(),
        rca_core::TaskAccessReason::ScopeEnded
    );
    eprintln!("plain Shared removal blocks/requested bytes: {cost:?}");
    drop(original);
    driver.finish(store.shutdown()).unwrap();
    drop(profiler);
}

/// Distinguishes store identity, replacement incarnation and refreshed child configuration.
#[test]
fn removal_history_distinguishes_stores_replacements_and_child_configuration() {
    let first = fixture();
    let other = fixture();
    let origins = Arc::new(Mutex::new(Vec::new()));
    let remove = |fixture: &Fixture, child: &rca_core::TaskContext<Child, ()>, replacement| {
        let origins = std::sync::Arc::clone(&origins);
        let _ = fixture
            .driver
            .finish(fixture.parent.remove_child(
                &fixture.removal,
                child,
                |_| {},
                move |parent, removed, _| {
                    origins.lock().unwrap().push(removed.origin().clone());
                    let _plain = removed.into_inner();
                    parent.child = replacement;
                },
            ))
            .unwrap();
    };
    remove(
        &first,
        &first.child,
        Some(Child {
            value: Box::new(Cell::new(1004)),
        }),
    );
    remove(&other, &other.child, None);
    let replacement = first
        .driver
        .finish(first.store.acquire(&first.child_site))
        .unwrap()
        .unwrap();
    let _ = first
        .driver
        .finish(replacement.update(|child| child.value.set(2004)))
        .unwrap();
    let configured = first
        .driver
        .finish(first.store.acquire(&first.child_site))
        .unwrap()
        .unwrap();
    remove(&first, &configured, None);
    let origins = origins.lock().unwrap();
    assert_eq!(origins[0], origins[0].clone());
    assert_ne!(
        origins[0], origins[1],
        "equal numeric lifetimes in different Stores are not equal"
    );
    assert_ne!(
        origins[0], origins[2],
        "equal-ID replacement has another incarnation"
    );
    assert_eq!(origins[0].configuration(), 0);
    assert_eq!(
        origins[2].configuration(),
        1,
        "history captures the child's actual refreshed revision"
    );
    assert_eq!(
        format!("{:?}", origins[0]),
        "RemovalOrigin { configuration: 0, .. }"
    );
    drop(origins);
    first.driver.finish(first.store.shutdown()).unwrap();
    other.driver.finish(other.store.shutdown()).unwrap();
}

/// Cancels backpressured transfer when an earlier root action retires its parent.
#[cfg(feature = "test-util")]
#[test]
fn full_capacity_one_ingress_and_parent_retirement_cancel_waiting_transfer() {
    let gate = rca_core::testing::TransactionGate::<Root, ()>::new();
    let Fixture {
        driver,
        store,
        parent,
        child,
        removal,
        tracked,
        handlers,
        ..
    } = fixture_with_gate(Some(&gate));
    let values = Arc::new(AtomicUsize::new(0));
    let captures = Arc::new(AtomicUsize::new(0));
    let received = Arc::new(AtomicUsize::new(0));
    let value = DropWatch(Arc::clone(&values));
    let capture = DropWatch(Arc::clone(&captures));
    let receive = Arc::clone(&received);
    let mut remove = Box::pin(parent.remove_child(
        &removal,
        &child,
        |_| {},
        move |_, removed, context| {
            removed
                .into_output(
                    context,
                    &tracked,
                    move |_| value,
                    move |_| {
                        receive.fetch_add(1, Ordering::SeqCst);
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
    // The held removal is outside the mailbox. This earlier root action fills
    // its one slot before the removal producer can even be registered.
    let mut retire = Box::pin(store.send(()));
    assert!(poll(retire.as_mut()).is_pending());
    assert!(
        store.try_send(()).unwrap_err().is_full(),
        "the one ordinary mailbox slot is occupied"
    );
    assert!(gate.release_pending());
    driver.turn();
    let Poll::Ready(Ok(task)) = poll(remove.as_mut()) else {
        panic!("removal committed")
    };
    for _ in 0..3 {
        driver.turn();
    }
    assert_eq!(
        values.load(Ordering::SeqCst),
        0,
        "owned output waits behind full ingress"
    );
    assert_eq!(handlers.load(Ordering::SeqCst), 0);
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
    assert_eq!(handlers.load(Ordering::SeqCst), 0);
    assert_eq!(received.load(Ordering::SeqCst), 0);
    assert_eq!(values.load(Ordering::SeqCst), 1);
    assert_eq!(captures.load(Ordering::SeqCst), 1);
    assert_eq!(
        driver.finish(parent.read(|_| ())).unwrap_err().reason(),
        rca_core::TaskAccessReason::ScopeEnded
    );
    assert!(
        gate.pending_kind().is_none(),
        "cancelled transfer creates no phantom frontier"
    );
    drop(retire);
    driver.finish(store.shutdown()).unwrap();
    drop(store);
    assert!(
        driver.0.lock().unwrap().is_empty(),
        "fixture releases its actor"
    );
    let queue = Arc::downgrade(&driver.0);
    drop(driver);
    assert!(queue.upgrade().is_none());
}

/// Retains inert removal history after every actor and payload owner disappears.
#[test]
fn historical_origin_survives_every_runtime_owner_without_retaining_payloads() {
    let Fixture {
        driver,
        store,
        parent,
        child,
        child_site,
        removal,
        output,
        tracked,
        ..
    } = fixture();
    let history = Arc::new(Mutex::new(None));
    let export = Arc::clone(&history);
    let task = driver
        .finish(parent.remove_child(
            &removal,
            &child,
            |_| {},
            move |_, removed, _| {
                *export.lock().unwrap() = Some(removed.origin().clone());
                drop(removed.into_inner());
            },
        ))
        .unwrap();
    driver.finish(task.finish());
    let origin = history.lock().unwrap().take().unwrap();
    drop((store, parent, child, child_site, removal, output, tracked));
    for _ in 0..4 {
        driver.turn();
    }
    assert!(driver.0.lock().unwrap().is_empty());
    drop(driver);
    assert_eq!(origin, origin.clone());
    assert_eq!(origin.configuration(), 0);
    assert_eq!(
        format!("{origin:?}"),
        "RemovalOrigin { configuration: 0, .. }"
    );
}

/// Preserves authored launches while closure suppresses transfer callbacks at two boundaries.
#[test]
fn reconciliation_destructor_and_earlier_spawn_close_preserve_launch_law() {
    for close_in_spawn in [false, true] {
        let Fixture {
            driver,
            store,
            parent,
            child,
            removal,
            tracked,
            handlers,
            ..
        } = fixture();
        let closes = Arc::new(AtomicUsize::new(0));
        let close_count = Arc::clone(&closes);
        let hook = if close_in_spawn { &driver.2 } else { &driver.3 };
        *hook.lock().unwrap() = Some(Box::new(move || {
            close_count.fetch_add(1, Ordering::SeqCst);
            drop(store);
        }));
        let values = Arc::new(AtomicUsize::new(0));
        let captures = Arc::new(AtomicUsize::new(0));
        let received = Arc::new(AtomicUsize::new(0));
        let value = DropWatch(Arc::clone(&values));
        let capture = DropWatch(Arc::clone(&captures));
        let receive = Arc::clone(&received);
        let spawns = driver.1.load(Ordering::SeqCst);
        let task = driver
            .finish(parent.remove_child(
                &removal,
                &child,
                |_| {},
                move |_, removed, context| {
                    if close_in_spawn {
                        context.task(rca_core::Task::run(|_| async {}));
                    }
                    removed
                        .into_output(
                            context,
                            &tracked,
                            move |_| value,
                            move |_| {
                                receive.fetch_add(1, Ordering::SeqCst);
                                drop(capture);
                            },
                        )
                        .unwrap();
                },
            ))
            .unwrap();
        driver.finish(task.finish());
        driver.turn(); // Logical cancellation precedes physical runtime-wrapper destruction.
        assert_eq!(closes.load(Ordering::SeqCst), 1);
        assert_eq!(handlers.load(Ordering::SeqCst), 0);
        assert_eq!(received.load(Ordering::SeqCst), 0);
        assert_eq!(values.load(Ordering::SeqCst), 1);
        assert_eq!(captures.load(Ordering::SeqCst), 1);
        assert_eq!(
            driver.1.load(Ordering::SeqCst),
            spawns + if close_in_spawn { 2 } else { 1 }
        );
    }
}

/// Preserves sibling entries and prevents retired declarations from targeting equal-key replacements.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One keyed removal sequence preserves other entries and replacement non-retargeting"
)]
fn keyed_owned_removal_preserves_other_entries_and_never_retargets_replacement() {
    /// Ordered children with independent entry lifetimes.
    type Rows = rca_core::KeyedState<u32, Child>;
    /// Leaves row changes to owning task-context operations.
    struct RowsFeature;
    impl Feature for RowsFeature {
        type State = Rows;
        type Action = ();
        fn update(&mut self, _: &mut Rows, (): &(), _: &mut FeatureContext<'_, Rows, ()>) {}
    }
    let driver = Driver::default();
    let old = Box::new(Cell::new(4));
    let identity = (&raw const *old) as usize;
    let rows: Rows = [
        (1, Child { value: old }),
        (
            2,
            Child {
                value: Box::new(Cell::new(20)),
            },
        ),
    ]
    .into_iter()
    .collect();
    let contexts = Arc::new(Mutex::new(None));
    let export_context = std::sync::Arc::clone(&contexts);
    let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let release_task = std::sync::Arc::clone(&release);
    let sites = Arc::new(Mutex::new(Vec::new()));
    let export_sites = std::sync::Arc::clone(&sites);
    let child_outputs = Arc::new(Mutex::new(Vec::new()));
    let export_outputs = Arc::clone(&child_outputs);
    let builder = RootMount::new(RowsFeature)
        .on_initial(
            |_| (),
            move |_, _| {
                let export = std::sync::Arc::clone(&export_context);
                let release = std::sync::Arc::clone(&release_task);
                rca_core::Task::run(move |context| async move {
                    *export.lock().unwrap() = Some(context);
                    std::future::poll_fn(move |_| {
                        if release.load(Ordering::SeqCst) {
                            Poll::Ready(())
                        } else {
                            Poll::Pending
                        }
                    })
                    .await;
                })
            },
        )
        .builder(rows, Runtime::new(driver.clone()))
        .keyed_removable(
            |rows| rows,
            |rows| rows,
            move |key, _| {
                let export = std::sync::Arc::clone(&export_sites);
                let outputs = Arc::clone(&export_outputs);
                let key = *key;
                (
                    rca_core::EntryMount::new(ChildFeature),
                    move |_: &Child,
                          children: &mut rca_core::MountChildren<'_, Rows, (), Child, ()>,
                          removal| {
                        let site = children.site().clone();
                        let child_output = children.output::<Box<Cell<usize>>>(&site, |_| {});
                        outputs.lock().unwrap().push((key, child_output));
                        export
                            .lock()
                            .unwrap()
                            .push((key, children.site().clone(), removal));
                    },
                )
            },
        );
    let (builder, output) = builder.root_output::<Box<Cell<usize>>>(|_| {});
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    driver.turn();
    let parent = contexts.lock().unwrap().take().unwrap();
    let (site, removal) = {
        let sites = sites.lock().unwrap();
        let (_, site, removal) = sites.iter().find(|(key, _, _)| *key == 1).unwrap();
        (site.clone(), removal.clone())
    };
    let child = driver.finish(store.acquire(&site)).unwrap().unwrap();
    drop(
        driver
            .finish(child.update(|child| child.value.set(6)))
            .unwrap(),
    );
    assert_eq!(
        driver
            .finish(store.read(|rows| (
                rows.get(&1).unwrap().value.get(),
                rows.get(&2).unwrap().value.get(),
            )))
            .unwrap(),
        (6, 20)
    );
    let child_output = child_outputs
        .lock()
        .unwrap()
        .iter()
        .find(|(key, _)| *key == 1)
        .unwrap()
        .1
        .clone();
    let received = Arc::new(AtomicUsize::new(0));
    let receive = std::sync::Arc::clone(&received);
    let task = driver
        .finish(parent.remove_child(
            &removal,
            &child,
            |child| {
                assert_eq!(child.value.get(), 6);
                child.value.set(7);
            },
            move |rows, removed, context| {
                rows.insert(
                    1,
                    Child {
                        value: Box::new(Cell::new(100)),
                    },
                );
                let error = removed
                    .into_output(
                        context,
                        &child_output,
                        |_| panic!("retired child-owned declaration must reject before conversion"),
                        |_| panic!("rejected child output cannot receive"),
                    )
                    .unwrap_err();
                let (removed, error) = error.into_parts();
                assert_eq!(error.reason(), rca_core::TaskAccessReason::ScopeEnded);
                removed
                    .into_output(
                        context,
                        &output,
                        |child| child.value,
                        move |result| {
                            let OutputDelivery::Unhandled { value, .. } = result.unwrap() else {
                                panic!("root output")
                            };
                            assert_eq!((&raw const *value) as usize, identity);
                            assert_eq!(value.get(), 7);
                            receive.fetch_add(1, Ordering::SeqCst);
                        },
                    )
                    .unwrap();
            },
        ))
        .unwrap();
    let before = Arc::new(AtomicUsize::new(0));
    let entered = std::sync::Arc::clone(&before);
    let error = driver
        .finish(parent.remove_child(
            &removal,
            &child,
            move |_| {
                entered.fetch_add(1, Ordering::SeqCst);
            },
            |_, _, _| panic!("stale entry"),
        ))
        .unwrap_err();
    assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
    assert_eq!(before.load(Ordering::SeqCst), 0);
    release.store(true, Ordering::SeqCst);
    driver.finish(task.finish());
    assert_eq!(received.load(Ordering::SeqCst), 1);
    assert_eq!(
        driver
            .finish(store.read(|rows| (
                rows.get(&1).unwrap().value.get(),
                rows.get(&2).unwrap().value.get()
            )))
            .unwrap(),
        (100, 20)
    );
    assert!(
        driver.finish(store.acquire(&site)).is_err(),
        "retired entry declaration cannot retarget"
    );
    let replacement_site = sites.lock().unwrap().last().unwrap().1.clone();
    assert_eq!(
        driver
            .finish(
                driver
                    .finish(store.acquire(&replacement_site))
                    .unwrap()
                    .unwrap()
                    .read(|child| child.value.get())
            )
            .unwrap(),
        100
    );
    driver.finish(store.shutdown()).unwrap();
}

/// Inspects removal origin without consuming the held output, even when inspection panics.
#[cfg(feature = "test-util")]
#[test]
fn removal_origin_uses_exact_pending_output_without_consuming_frontier_or_payload() {
    let gate = rca_core::testing::TransactionGate::<Root, ()>::new();
    let Fixture {
        driver,
        store,
        parent,
        child,
        removal,
        output,
        tracked,
        old_identity,
        ..
    } = fixture_with_gate(Some(&gate));
    let historical = Arc::new(Mutex::new(None));
    let export = std::sync::Arc::clone(&historical);
    let transfer = output.clone();
    let mut remove = Box::pin(parent.remove_child(
        &removal,
        &child,
        |_| {},
        move |_, removed, context| {
            *export.lock().unwrap() = Some(removed.origin().clone());
            removed
                .into_output(context, &transfer, |child| child.value, |_| {})
                .unwrap();
        },
    ));
    assert!(poll(remove.as_mut()).is_pending());
    driver.turn();
    assert_eq!(
        gate.pending_kind(),
        Some(rca_core::testing::TransactionKind::Update),
        "owning removal is a mutation, not an action"
    );
    assert!(gate.release_pending());
    driver.turn();
    assert!(poll(remove.as_mut()).is_ready());
    assert!(gate.acknowledge(gate.boundary().unwrap()));
    for _ in 0..4 {
        driver.turn();
    }
    assert_eq!(
        gate.pending_kind(),
        Some(rca_core::testing::TransactionKind::Output)
    );
    assert!(gate.output_origin(&tracked).is_none());
    assert_eq!(gate.output_origin(&output), *historical.lock().unwrap());
    assert_eq!(
        gate.inspect_output(&output, |value| (&raw const **value) as usize),
        Some(old_identity)
    );
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        gate.inspect_output(&output, |_| panic!("matcher"))
    }));
    let panic = panic.expect_err("the output matcher panics");
    assert_eq!(panic.downcast_ref::<&str>(), Some(&"matcher"));
    assert_eq!(gate.output_origin(&output), *historical.lock().unwrap());
    assert_eq!(gate.inspect_output(&output, |value| value.get()), Some(4));
    assert!(gate.release_pending());
    driver.turn();
    assert!(gate.acknowledge(gate.boundary().unwrap()));
    driver.finish(store.shutdown()).unwrap();
    drop(store);
    assert!(
        driver.0.lock().unwrap().is_empty(),
        "fixture releases its actor"
    );
    let queue = Arc::downgrade(&driver.0);
    drop(driver);
    assert!(queue.upgrade().is_none());
}

/// Delivers the original allocation while a replacement keeps separate state and authority.
#[test]
fn owned_old_child_outcome_survives_equal_identity_replacement() {
    let Fixture {
        driver,
        store,
        parent,
        child,
        removal,
        output,
        constructors,
        old_identity,
        ..
    } = fixture();
    // Neither the owning parent nor the child payload implements Debug.
    assert_eq!(format!("{removal:?}"), "RemovalSite { .. }");
    let received = Arc::new(AtomicUsize::new(0));
    let completed = Arc::clone(&received);
    let operation = parent.remove_child(
        &removal,
        &child,
        |child| child.value.set(child.value.get() + 3),
        move |state, removed, context| {
            assert_eq!(format!("{removed:?}"), "RemovedChild { .. }");
            state.child = Some(Child {
                value: Box::new(Cell::new(100)),
            });
            removed
                .into_output(
                    context,
                    &output,
                    |child| child.value,
                    move |result| {
                        assert!(matches!(result, Ok(OutputDelivery::Consumed { .. })));
                        completed.fetch_add(1, Ordering::SeqCst);
                    },
                )
                .unwrap();
        },
    );
    // Returned future owns only weak metadata and callbacks, not these borrows.
    drop((parent, removal));
    let task = driver.finish(operation).unwrap();
    driver.finish(task.finish());
    assert_eq!(received.load(Ordering::SeqCst), 1);
    assert_eq!(constructors.load(Ordering::SeqCst), 2);
    assert_eq!(
        driver
            .finish(store.read(|state| {
                let output = state.outcome.as_ref().unwrap();
                (
                    (&raw const **output) as usize,
                    output.get(),
                    state
                        .parent
                        .as_ref()
                        .unwrap()
                        .child
                        .as_ref()
                        .unwrap()
                        .value
                        .get(),
                )
            }))
            .unwrap(),
        (old_identity, 7, 100)
    );
    assert_eq!(
        driver
            .finish(child.read(|state| state.value.get()))
            .unwrap_err()
            .reason(),
        rca_core::TaskAccessReason::ScopeEnded
    );
    driver.finish(store.shutdown()).unwrap();
}

/// Returns root output unchanged and runs separately authored effects on the surviving parent.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One root-removal sequence preserves parent update and unhandled owned-output recovery"
)]
fn actual_root_removal_updates_parent_and_returns_unhandled_owned_output() {
    /// Applies the follow-up action to the replacement, not the retired child.
    struct RemovalParent;
    impl Feature for RemovalParent {
        type State = Parent;
        type Action = ();
        fn update(&mut self, state: &mut Parent, (): &(), _: &mut FeatureContext<'_, Parent, ()>) {
            let child = state.child.as_ref().unwrap();
            child.value.set(child.value.get() + 1);
        }
    }
    let driver = Driver::default();
    let context = Arc::new(Mutex::new(None));
    let exported = std::sync::Arc::clone(&context);
    let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let release_task = std::sync::Arc::clone(&release);
    let old = Box::new(Cell::new(4));
    let identity = (&raw const *old) as usize;
    let (builder, site, removal) = RootMount::new(RemovalParent)
        .on_initial(
            |_| (),
            move |_, _| {
                let exported = std::sync::Arc::clone(&exported);
                let release = std::sync::Arc::clone(&release_task);
                rca_core::Task::run(move |context| async move {
                    *exported.lock().unwrap() = Some(context);
                    std::future::poll_fn(move |_| {
                        if release.load(Ordering::SeqCst) {
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
                child: Some(Child { value: old }),
            },
            Runtime::new(driver.clone()),
        )
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
            |_| ChildFeature,
        );
    let called = Arc::new(AtomicUsize::new(0));
    let handler = std::sync::Arc::clone(&called);
    let (builder, output) = builder.root_output::<Box<Cell<usize>>>(|handlers| {
        handlers.at_root(move |_, value, _| {
            handler.fetch_add(1, Ordering::SeqCst);
            drop(value);
            OutputFlow::Consumed
        });
    });
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    driver.turn();
    let parent = context.lock().unwrap().take().unwrap();
    let child = driver.finish(store.acquire(&site)).unwrap().unwrap();
    let (unbuilt, unbuilt_output) = RootMount::new(ParentFeature)
        .builder(Parent { child: None }, Runtime::new(driver.clone()))
        .root_output::<Box<Cell<usize>>>(|_| {});
    let rejected_conversions = Arc::new(AtomicUsize::new(0));
    let conversions = Arc::clone(&rejected_conversions);
    let rejected_capture_drops = Arc::new(AtomicUsize::new(0));
    let rejected_capture = DropWatch(Arc::clone(&rejected_capture_drops));
    let effect_calls = Arc::new(AtomicUsize::new(0));
    let effect_observed = Arc::clone(&effect_calls);
    let effect_capture_drops = Arc::new(AtomicUsize::new(0));
    let effect_capture = DropWatch(Arc::clone(&effect_capture_drops));
    let received = Arc::new(AtomicUsize::new(0));
    let receipt = std::sync::Arc::clone(&received);
    let task = driver
        .finish(parent.remove_child(
            &removal,
            &child,
            |child| child.value.set(7),
            move |state, removed, context| {
                state.child = Some(Child {
                    value: Box::new(Cell::new(100)),
                });
                let (removed, error) = removed
                    .into_output(
                        context,
                        &unbuilt_output,
                        move |child| {
                            conversions.fetch_add(1, Ordering::SeqCst);
                            child.value
                        },
                        move |_| {
                            drop(rejected_capture);
                            panic!("an unbuilt root cannot receive a removed value");
                        },
                    )
                    .unwrap_err()
                    .into_parts();
                assert_eq!(error.reason(), rca_core::TaskAccessReason::ScopeEnded);
                assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
                context.effect(rca_core::Effect::future(async move {
                    effect_observed.fetch_add(1, Ordering::SeqCst);
                    drop(effect_capture);
                    Some(())
                }));
                removed
                    .into_output(
                        context,
                        &output,
                        |child| child.value,
                        move |result| {
                            let OutputDelivery::Unhandled { value, .. } = result.unwrap() else {
                                panic!("root has no strict ancestor")
                            };
                            assert_eq!((&raw const *value) as usize, identity);
                            assert_eq!(value.get(), 7);
                            receipt.fetch_add(1, Ordering::SeqCst);
                        },
                    )
                    .unwrap();
            },
        ))
        .unwrap();
    release.store(true, Ordering::SeqCst);
    driver.finish(task.finish());
    assert_eq!(received.load(Ordering::SeqCst), 1);
    assert_eq!(called.load(Ordering::SeqCst), 0);
    assert_eq!(rejected_conversions.load(Ordering::SeqCst), 0);
    assert_eq!(rejected_capture_drops.load(Ordering::SeqCst), 1);
    assert_eq!(effect_calls.load(Ordering::SeqCst), 1);
    assert_eq!(effect_capture_drops.load(Ordering::SeqCst), 1);
    assert_eq!(
        driver
            .finish(store.read(|state| state.child.as_ref().unwrap().value.get()))
            .unwrap(),
        101
    );
    assert_eq!(
        driver
            .finish(child.read(|state| state.value.get()))
            .unwrap_err()
            .reason(),
        rca_core::TaskAccessReason::ScopeEnded
    );
    driver.finish(store.shutdown()).unwrap();
    drop(unbuilt);
}

/// Rejects a foreign output before conversion and returns the original child.
#[test]
fn foreign_output_returns_unconverted_owned_removal() {
    let Fixture {
        driver,
        store,
        parent,
        child,
        removal,
        ..
    } = fixture();
    let other = fixture();
    let output = other.tracked.clone();
    let converted = Arc::new(AtomicUsize::new(0));
    let converts = std::sync::Arc::clone(&converted);
    let receive_drops = Arc::new(AtomicUsize::new(0));
    let capture = DropWatch(std::sync::Arc::clone(&receive_drops));
    let recovered = Arc::new(AtomicUsize::new(0));
    let old_value = std::sync::Arc::clone(&recovered);
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
                        move |_| {
                            converts.fetch_add(1, Ordering::SeqCst);
                            DropWatch(Arc::new(AtomicUsize::new(0)))
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
                old_value.store(removed.into_inner().value.get(), Ordering::SeqCst);
            },
        ))
        .unwrap();
    assert_eq!(converted.load(Ordering::SeqCst), 0);
    assert_eq!(receive_drops.load(Ordering::SeqCst), 1);
    assert_eq!(recovered.load(Ordering::SeqCst), 4);
    driver.finish(store.shutdown()).unwrap();
    other.driver.finish(other.store.shutdown()).unwrap();
    drop((store, other));
}
/// Rejects a sibling source without consuming or converting the retired child.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "Keep sibling source authority, owning removal and rejected transfer ownership in one timeline"
)]
fn sibling_source_output_returns_unconverted_removal_before_conversion() {
    /// Gives live siblings distinct output authority within one actor.
    struct Pair(Parent, Parent);
    /// Keeps both sibling parents unchanged during the rejected transfer.
    struct PairFeature;
    impl Feature for PairFeature {
        type State = Pair;
        type Action = ();
        fn update(&mut self, _: &mut Pair, (): &(), _: &mut FeatureContext<'_, Pair, ()>) {}
    }
    let driver = Driver::default();
    let original = Box::new(Cell::new(41));
    let identity = (&raw const *original) as usize;
    let sites = Arc::new(Mutex::new(None));
    let exported = Arc::clone(&sites);
    let (builder, parent_site) = RootMount::new(PairFeature)
        .builder(
            Pair(
                Parent {
                    child: Some(Child { value: original }),
                },
                Parent { child: None },
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
                *exported.lock().unwrap() = Some(children.optional_removable(
                    |parent: &Parent| &parent.child,
                    |parent: &mut Parent| &mut parent.child,
                    |parent| {
                        parent.child.as_ref().map(|_| MountIdentity {
                            key: (),
                            replacement: 0,
                            configuration: 0,
                        })
                    },
                    |_| ChildFeature,
                ));
            },
        );
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
        |_, _| {},
    );
    let handlers = Arc::new(AtomicUsize::new(0));
    let calls = Arc::clone(&handlers);
    let (builder, output) = builder.output::<Box<Cell<usize>>>(&sibling_site, |handlers| {
        handlers.at_root(move |_, value, _| {
            calls.fetch_add(1, Ordering::SeqCst);
            drop(value);
            OutputFlow::Consumed
        });
    });
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    let (child_site, removal) = sites.lock().unwrap().take().unwrap();
    let parent = driver.finish(store.acquire(&parent_site)).unwrap().unwrap();
    let child = driver.finish(store.acquire(&child_site)).unwrap().unwrap();
    let sibling = driver
        .finish(store.acquire(&sibling_site))
        .unwrap()
        .unwrap();
    let receives = Arc::new(AtomicUsize::new(0));
    let capture = DropWatch(Arc::clone(&receives));
    let recovered = Arc::new(Mutex::new(None));
    let receive = Arc::clone(&recovered);
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
                        |_| -> Box<Cell<usize>> { panic!("wrong source cannot convert") },
                        move |_| {
                            drop(capture);
                            panic!("wrong source cannot receive");
                        },
                    )
                    .unwrap_err()
                    .into_parts();
                assert_eq!(error.reason(), rca_core::TaskAccessReason::ScopeEnded);
                assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
                *receive.lock().unwrap() = Some(removed.into_inner());
            },
        ))
        .unwrap();
    driver.finish(task.finish());
    assert_eq!(receives.load(Ordering::SeqCst), 1);
    assert_eq!(handlers.load(Ordering::SeqCst), 0);
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
    let removed = recovered.lock().unwrap().take().unwrap();
    assert_eq!((&raw const *removed.value) as usize, identity);
    assert_eq!(removed.value.get(), 41);
    driver.finish(store.shutdown()).unwrap();
    assert_eq!(
        removed.value.get(),
        41,
        "returned value outlives both live declarations"
    );
    drop(removed);
}

/// Rejects foreign authority and stale child lifetimes before either callback enters.
#[test]
fn foreign_parent_child_site_and_stale_replacement_reject_before_callbacks() {
    for mode in 0..4 {
        let current = fixture();
        let other = fixture();
        if mode == 3 {
            let old = current
                .driver
                .finish(current.parent.update(|state| state.child.take()))
                .unwrap();
            drop(old);
            let _ = current
                .driver
                .finish(current.parent.update(|state| {
                    state.child = Some(Child {
                        value: Box::new(Cell::new(100)),
                    });
                }))
                .unwrap();
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
        let site = if mode == 2 {
            &other.removal
        } else {
            &current.removal
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let before = std::sync::Arc::clone(&calls);
        let after = std::sync::Arc::clone(&calls);
        let mut operation = Box::pin(parent.remove_child(
            site,
            child,
            move |_| {
                before.fetch_add(1, Ordering::SeqCst);
            },
            move |_, _, _| {
                after.fetch_add(1, Ordering::SeqCst);
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
            .expect("finite foreign request completes")
            .unwrap_err();
        assert_eq!(error.reason(), rca_core::TaskAccessReason::ScopeEnded);
        assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            current
                .driver
                .finish(current.child.read(|state| state.value.get()))
                .is_ok(),
            mode != 3
        );
        drop(operation);
        current.driver.finish(current.store.shutdown()).unwrap();
        other.driver.finish(other.store.shutdown()).unwrap();
    }
}

/// Releases converted values without spawning after converter entry or capture destruction closes ownership.
#[test]
fn conversion_closure_and_converter_capture_drop_append_no_producer() {
    for explicit_close in [true, false] {
        let Fixture {
            driver,
            store,
            parent,
            child,
            removal,
            tracked,
            handlers,
            ..
        } = fixture();
        let output_drops = Arc::new(AtomicUsize::new(0));
        let value_drops = std::sync::Arc::clone(&output_drops);
        let receive_drops = Arc::new(AtomicUsize::new(0));
        let capture = DropWatch(std::sync::Arc::clone(&receive_drops));
        let spawns = driver.1.load(Ordering::SeqCst);
        let task = driver
            .finish(parent.remove_child(
                &removal,
                &child,
                |_| {},
                move |_, removed, context| {
                    removed
                        .into_output(
                            context,
                            &tracked,
                            move |_| {
                                let _keep_alive = &store;
                                if explicit_close {
                                    drop(store);
                                }
                                DropWatch(value_drops)
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
        assert_eq!(
            driver.1.load(Ordering::SeqCst),
            spawns,
            "no removal runtime spawn"
        );
        assert_eq!(handlers.load(Ordering::SeqCst), 0);
        assert_eq!(output_drops.load(Ordering::SeqCst), 1);
        assert_eq!(receive_drops.load(Ordering::SeqCst), 1);
    }
}

/// Preserves ordinary launch accounting while late closure suppresses transfer delivery.
#[test]
fn later_close_preserves_ordinary_launches_but_suppresses_transfer_callbacks() {
    let Fixture {
        driver,
        store,
        parent,
        child,
        removal,
        tracked,
        handlers,
        ..
    } = fixture();
    let output_drops = Arc::new(AtomicUsize::new(0));
    let value_drops = std::sync::Arc::clone(&output_drops);
    let receive_drops = Arc::new(AtomicUsize::new(0));
    let capture = DropWatch(std::sync::Arc::clone(&receive_drops));
    let ordinary_drops = Arc::new(AtomicUsize::new(0));
    let ordinary = DropWatch(std::sync::Arc::clone(&ordinary_drops));
    let spawns = driver.1.load(Ordering::SeqCst);
    let task = driver
        .finish(parent.remove_child(
            &removal,
            &child,
            |_| {},
            move |_, removed, context| {
                removed
                    .into_output(
                        context,
                        &tracked,
                        move |_| DropWatch(value_drops),
                        move |_| {
                            drop(capture);
                            panic!("late closed transfer cannot receive");
                        },
                    )
                    .unwrap();
                context.task(rca_core::Task::run(move |_| async move {
                    drop(ordinary);
                }));
                drop(store);
            },
        ))
        .unwrap();
    assert_eq!(
        driver.1.load(Ordering::SeqCst),
        spawns + 2,
        "ordinary committed work retains public runtime launch"
    );
    driver.finish(task.finish());
    // Cancelled causal completion does not destroy futures retained by a driver.
    driver.turn();
    assert_eq!(handlers.load(Ordering::SeqCst), 0);
    assert_eq!(output_drops.load(Ordering::SeqCst), 1);
    assert_eq!(receive_drops.load(Ordering::SeqCst), 1);
    assert_eq!(ordinary_drops.load(Ordering::SeqCst), 1);
}

/// Suppresses receipt callbacks when closure occurs before production or after handler entry.
#[test]
fn close_before_producer_poll_or_after_output_reply_suppresses_receive() {
    for after_output in [false, true] {
        let Fixture {
            driver,
            store,
            parent,
            child,
            removal,
            tracked,
            handlers,
            ..
        } = fixture();
        let output_drops = Arc::new(AtomicUsize::new(0));
        let value_drops = std::sync::Arc::clone(&output_drops);
        let receive_drops = Arc::new(AtomicUsize::new(0));
        let capture = DropWatch(std::sync::Arc::clone(&receive_drops));
        let task = driver
            .finish(parent.remove_child(
                &removal,
                &child,
                |_| {},
                move |_, removed, context| {
                    removed
                        .into_output(
                            context,
                            &tracked,
                            move |_| DropWatch(value_drops),
                            move |_| {
                                drop(capture);
                                panic!("closed producer cannot receive");
                            },
                        )
                        .unwrap();
                },
            ))
            .unwrap();
        assert_eq!(handlers.load(Ordering::SeqCst), 0);
        if after_output {
            driver.turn();
        }
        assert_eq!(handlers.load(Ordering::SeqCst), usize::from(after_output));
        drop(store);
        driver.finish(task.finish());
        assert_eq!(output_drops.load(Ordering::SeqCst), 1);
        assert_eq!(receive_drops.load(Ordering::SeqCst), 1);
    }
}

/// Stops after the entered before-callback closes the Store, without taking its child.
#[test]
fn before_closure_stops_before_take_and_after_callback() {
    let Fixture {
        driver,
        store,
        parent,
        child,
        removal,
        ..
    } = fixture();
    let calls = Arc::new(AtomicUsize::new(0));
    let after = std::sync::Arc::clone(&calls);
    let error = driver
        .finish(parent.remove_child(
            &removal,
            &child,
            move |state| {
                state.value.set(7);
                drop(store);
            },
            move |_, _, _| {
                after.fetch_add(1, Ordering::SeqCst);
            },
        ))
        .unwrap_err();
    assert_eq!(error.reason(), rca_core::TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), rca_core::TaskProgress::MayHaveRun);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

/// Rechecks ownership and presence after the owning selector changes either boundary.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One public removal fixture compares selector-driven closure with selector-driven absence and exact payload retention"
)]
fn owning_optional_selector_rechecks_liveness_and_presence_before_callbacks() {
    /// Mutates the owning field exactly once during removal selection.
    type Selector = Box<dyn FnOnce(&mut Option<Child>) + Send>;
    /// Records the original field's disposition when the actor releases its state.
    struct State {
        child: Option<Child>,
        selector: Option<Selector>,
        disposed: Arc<Mutex<Vec<usize>>>,
    }
    impl Drop for State {
        fn drop(&mut self) {
            let mut disposed = self.disposed.lock().unwrap();
            if let Some(child) = &self.child {
                disposed.extend([(&raw const *child.value) as usize, child.value.get()]);
            } else {
                disposed.extend([0, 0]);
            }
        }
    }
    /// Exposes selector-driven removal without action-driven state changes.
    struct Selectable;
    impl Feature for Selectable {
        type State = State;
        type Action = ();
        fn update(&mut self, _: &mut State, (): &(), _: &mut FeatureContext<'_, State, ()>) {}
    }

    for close in [false, true] {
        let driver = Driver::default();
        let context = Arc::new(Mutex::new(None));
        let exported = Arc::clone(&context);
        let disposed = Arc::new(Mutex::new(Vec::new()));
        let original = Box::new(Cell::new(7));
        let identity = (&raw const *original) as usize;
        let (builder, site, removal) = RootMount::new(Selectable)
            .on_initial(
                |_| (),
                move |_, _| {
                    let exported = Arc::clone(&exported);
                    rca_core::Task::run(move |context| async move {
                        *exported.lock().unwrap() = Some(context);
                        std::future::pending::<()>().await;
                    })
                },
            )
            .builder(
                State {
                    child: Some(Child { value: original }),
                    selector: None,
                    disposed: Arc::clone(&disposed),
                },
                Runtime::new(driver.clone()),
            )
            .mount_removable(
                |state: &State| &state.child,
                |state: &mut State| {
                    if let Some(selector) = state.selector.take() {
                        selector(&mut state.child);
                    }
                    &mut state.child
                },
                |state| {
                    state.child.as_ref().map(|_| MountIdentity {
                        key: (),
                        replacement: 0,
                        configuration: 0,
                    })
                },
                |_| ChildFeature,
            );
        let store = builder.build().unwrap();
        driver.finish(store.ready()).unwrap();
        driver.turn();
        let parent = context.lock().unwrap().take().unwrap();
        let child = driver.finish(store.acquire(&site)).unwrap().unwrap();
        let closed = store.closed();
        let mut retained_store = Some(store);
        let closing = close.then(|| retained_store.take().unwrap());
        let selected = Arc::new(AtomicUsize::new(0));
        let selection = Arc::clone(&selected);
        let extracted = Arc::new(Mutex::new(None));
        let extraction = Arc::clone(&extracted);
        driver
            .finish(parent.update(move |state| {
                state.selector = Some(Box::new(move |field| {
                    selection.fetch_add(1, Ordering::SeqCst);
                    if let Some(store) = closing {
                        drop(store);
                    } else {
                        *extraction.lock().unwrap() = field.take();
                    }
                }));
            }))
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let before = Arc::clone(&calls);
        let after = Arc::clone(&calls);
        let drops = Arc::new(AtomicUsize::new(0));
        let before_capture = DropWatch(Arc::clone(&drops));
        let after_capture = DropWatch(Arc::clone(&drops));
        let error = driver
            .finish(parent.remove_child(
                &removal,
                &child,
                move |value| {
                    let _ = &before_capture;
                    before.fetch_add(1, Ordering::SeqCst);
                    value.value.set(99);
                },
                move |_, _, _| {
                    let _ = &after_capture;
                    after.fetch_add(1, Ordering::SeqCst);
                },
            ))
            .unwrap_err();
        assert_eq!(
            error.reason(),
            if close {
                rca_core::TaskAccessReason::StoreClosed
            } else {
                rca_core::TaskAccessReason::ScopeEnded
            }
        );
        assert_eq!(error.progress(), rca_core::TaskProgress::MayHaveRun);
        assert_eq!(selected.load(Ordering::SeqCst), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(drops.load(Ordering::SeqCst), 2);
        if let Some(store) = retained_store {
            assert!(
                driver
                    .finish(store.read(|state| state.child.is_none()))
                    .unwrap()
            );
            let extracted = extracted.lock().unwrap().take().unwrap();
            assert_eq!((&raw const *extracted.value) as usize, identity);
            assert_eq!(extracted.value.get(), 7);
            driver.finish(store.shutdown()).unwrap();
        } else {
            assert!(extracted.lock().unwrap().is_none());
        }
        driver.finish(closed);
        driver.turn();
        assert!(driver.0.lock().unwrap().is_empty());
        assert_eq!(
            *disposed.lock().unwrap(),
            if close { vec![identity, 7] } else { vec![0, 0] }
        );
    }
}

/// Retains the request so capture destruction can inspect its reply ordering.
type RemovalFuture =
    Pin<Box<dyn Future<Output = Result<rca_core::ActionTask, rca_core::TaskAccessError>> + Send>>;
/// Observes the reply before the actor publishes its terminal failure.
struct ReplyOrder {
    /// Request already admitted before the actor invokes the failing callback.
    future: Arc<Mutex<Option<RemovalFuture>>>,
    /// One means pending at capture destruction; two means premature completion.
    observed: Arc<AtomicUsize>,
}
impl Drop for ReplyOrder {
    fn drop(&mut self) {
        let mut future = self.future.lock().unwrap();
        let result = poll(future.as_mut().unwrap().as_mut());
        self.observed
            .store(if result.is_pending() { 1 } else { 2 }, Ordering::SeqCst);
    }
}

/// Destroys unentered callback captures before publishing an entered panic failure.
#[test]
fn panic_destroys_unentered_after_capture_before_terminal_reply() {
    let Fixture {
        driver,
        store,
        parent,
        child,
        removal,
        ..
    } = fixture();
    let future = Arc::new(Mutex::new(None));
    let observed = Arc::new(AtomicUsize::new(0));
    let capture = ReplyOrder {
        future: std::sync::Arc::clone(&future),
        observed: std::sync::Arc::clone(&observed),
    };
    let operation = parent.remove_child(
        &removal,
        &child,
        |_| panic!("before entered"),
        move |_, _, _| {
            drop(capture);
        },
    );
    *future.lock().unwrap() = Some(Box::pin(operation) as RemovalFuture);
    assert!(poll(future.lock().unwrap().as_mut().unwrap().as_mut()).is_pending());
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| driver.turn()));
    let panic = panic.expect_err("the before callback must panic");
    assert_eq!(panic.downcast_ref::<&str>(), Some(&"before entered"));
    assert_eq!(
        observed.load(Ordering::SeqCst),
        1,
        "owned callback drops before terminal acknowledgement"
    );
    let operation = future.lock().unwrap().take().unwrap();
    let error = driver.finish(operation).unwrap_err();
    assert_eq!(error.reason(), rca_core::TaskAccessReason::ActorFailed);
    assert_eq!(error.progress(), rca_core::TaskProgress::MayHaveRun);
    drop(store);
}
