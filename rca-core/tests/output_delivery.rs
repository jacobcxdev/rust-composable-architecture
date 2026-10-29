//! Owned output delivery, rejection and cleanup through real mounted stores.
//!
//! Pointer identity and drop counts track the original, non-cloneable payload.
//! The fixed-clock driver makes admission and shutdown order explicit.

#[cfg(all(feature = "allocation-measurement", feature = "test-util"))]
#[path = "../src/allocation_test/child.rs"]
mod allocation_test;

use std::cell::Cell;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll, Waker};

#[cfg(feature = "diagnostics")]
use rca_core::diagnostics::LifecycleKind;
use rca_core::{
    Feature, FeatureContext, MonotonicInstant, MountIdentity, OptionalMount, Output,
    OutputDelivery, OutputFlow, RootMount, Runtime, RuntimeDriver, RuntimeFuture, Task,
};

#[cfg(all(feature = "allocation-measurement", feature = "test-util"))]
#[global_allocator]
/// Measures allocations only inside the isolated child assay.
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

/// Polls retained tasks explicitly, with a clock that stays at zero.
#[derive(Clone, Default)]
struct Driver(Arc<Mutex<Vec<RuntimeFuture>>>);

impl std::fmt::Debug for Driver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OutputDriver")
    }
}

impl RuntimeDriver for Driver {
    fn spawn(&self, task: RuntimeFuture) {
        self.0.lock().unwrap().push(task);
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
    /// Polls each queued task once, without holding the queue lock during callbacks.
    fn turn(&self) {
        let pending = std::mem::take(&mut *self.0.lock().unwrap());
        for mut future in pending {
            if poll(future.as_mut()).is_pending() {
                self.0.lock().unwrap().push(future);
            }
        }
    }

    /// Completes finite work within 32 turns; its box is included in measured phases.
    fn finish<F: Future>(&self, future: F) -> F::Output {
        let mut future = Box::pin(future);
        for _ in 0..32 {
            if let Poll::Ready(value) = poll(future.as_mut()) {
                return value;
            }
            self.turn();
        }
        panic!("finite output operation did not finish");
    }
}

/// Polls once; the caller, rather than a wake callback, drives the next turn.
fn poll<F: Future + ?Sized>(future: Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}

#[test]
fn fixture_driver_completes_elapsed_sleep_without_advancing_future_deadlines() {
    let driver = Driver::default();
    let mut elapsed = driver.sleep_until(MonotonicInstant::ZERO);
    assert!(poll(elapsed.as_mut()).is_ready());

    for deadline in [
        MonotonicInstant::from_elapsed(std::time::Duration::from_nanos(1)),
        MonotonicInstant::MAX,
    ] {
        let mut future = driver.sleep_until(deadline);
        assert!(poll(future.as_mut()).is_pending());
        driver.turn();
        assert!(poll(future.as_mut()).is_pending());
        assert_eq!(driver.now(), MonotonicInstant::ZERO);
    }
}

#[test]
fn fixture_driver_yield_suspends_once_and_wakes_its_waiter() {
    /// Counts wake requests from the actual driver's yield future.
    #[derive(Default)]
    struct WakeCount(AtomicUsize);

    impl std::task::Wake for WakeCount {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    let driver = Driver::default();
    let notifications = Arc::new(WakeCount::default());
    let waker = Waker::from(Arc::clone(&notifications));
    let mut context = Context::from_waker(&waker);
    let mut future = driver.yield_now();

    assert_eq!(notifications.0.load(Ordering::SeqCst), 0);
    assert!(future.as_mut().poll(&mut context).is_pending());
    assert_eq!(notifications.0.load(Ordering::SeqCst), 1);
    assert!(future.as_mut().poll(&mut context).is_ready());
    assert_eq!(notifications.0.load(Ordering::SeqCst), 1);
}

/// Tracks one moved allocation without implementing `Clone`, `Debug` or `Sync`.
struct Outcome {
    value: Box<Cell<usize>>,
    drops: Arc<AtomicUsize>,
}

impl Outcome {
    /// Identifies the allocation without borrowing it across an actor turn.
    fn identity(&self) -> usize {
        (&raw const *self.value) as usize
    }
}

impl Drop for Outcome {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

/// Keeps the optional source and the value consumed by its root handler.
struct RootState {
    child: Option<()>,
    outcome: Option<Outcome>,
}

/// Requests a root transaction without changing state in the default feature.
struct RootAction;
/// Fills the child mailbox without changing its state.
struct ChildAction;
/// Leaves state changes to output handlers and specialised test features.
struct Parent;
/// Supplies an installed output source with no reducer behaviour.
struct Child;

impl Feature for Parent {
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
    type State = ();
    type Action = ChildAction;

    fn reduce(
        &mut self,
        (): &mut (),
        _: &ChildAction,
        _: &mut FeatureContext<'_, (), ChildAction>,
    ) {
    }
}

/// Checks the delivery future's thread-transfer and ownership bounds at compile time.
fn assert_owned_send<T: Send + 'static>(_: &T) {}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One remount timeline checks handler order, transformed ownership and stale authority together"
)]
fn remounted_output_keeps_ancestor_order_before_transform_and_consumption() {
    /// Gives the output source a mounted ancestor below the root.
    struct Branch {
        leaf: (),
    }
    /// Removes and restores the whole branch while retaining the root.
    struct State {
        branch: Option<Branch>,
    }

    for consume in [false, true] {
        let driver = Driver::default();
        let trace = Arc::new(Mutex::new(Vec::new()));
        let declarations = Arc::new(Mutex::new(Vec::new()));
        let recipe_trace = Arc::clone(&trace);
        let recipe_declarations = Arc::clone(&declarations);
        let root = rca_core::ReducerFeature::new(rca_core::Reduce::new(
            |state: &mut State, present: &bool, _: &mut rca_core::ReduceContext<'_>| {
                state.branch = present.then_some(Branch { leaf: () });
                rca_core::Effect::none()
            },
        ));
        let branch = OptionalMount::new(
            |state: &State| state.branch.as_ref(),
            |state: &mut State| state.branch.as_mut(),
            |state: &State| {
                state.branch.as_ref().map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            |_| {
                rca_core::ReducerFeature::new(rca_core::Reduce::new(
                    |_: &mut Branch, (): &(), _: &mut rca_core::ReduceContext<'_>| {
                        rca_core::Effect::none()
                    },
                ))
            },
        );
        let (builder, _) = RootMount::new(root)
            .builder(
                State {
                    branch: Some(Branch { leaf: () }),
                },
                Runtime::new(driver.clone()),
            )
            .mount_with_children(branch, move |_, children| {
                let parent = children.site().clone();
                let source = children.optional(OptionalMount::new(
                    |state: &Branch| Some(&state.leaf),
                    |state: &mut Branch| Some(&mut state.leaf),
                    |_| {
                        Some(MountIdentity {
                            key: (),
                            replacement: 0,
                            configuration: 0,
                        })
                    },
                    |()| Child,
                ));
                let root_trace = Arc::clone(&recipe_trace);
                let transform_trace = Arc::clone(&recipe_trace);
                let decision_trace = Arc::clone(&recipe_trace);
                let output = children.output::<Outcome>(&source, |handlers| {
                    // Register the root first so registration order cannot pass this test.
                    handlers.at_root(move |_, value, _| {
                        root_trace.lock().unwrap().push((
                            "root",
                            value.value.get(),
                            value.identity(),
                        ));
                        OutputFlow::Consumed
                    });
                    handlers.at(&parent, move |_, value, _| {
                        transform_trace.lock().unwrap().push((
                            "transform",
                            value.value.get(),
                            value.identity(),
                        ));
                        value.value.set(value.value.get() * 10);
                        OutputFlow::Continue(value)
                    });
                    handlers.at(&parent, move |_, value, _| {
                        decision_trace.lock().unwrap().push((
                            "decision",
                            value.value.get(),
                            value.identity(),
                        ));
                        if consume {
                            OutputFlow::Consumed
                        } else {
                            value.value.set(value.value.get() + 1);
                            OutputFlow::Continue(value)
                        }
                    });
                });
                recipe_declarations.lock().unwrap().push((source, output));
            });
        let store = builder.build().unwrap();
        driver.finish(store.ready()).unwrap();
        assert_eq!(declarations.lock().unwrap().len(), 1);

        for incarnation in 0..2 {
            let (source, output) = declarations.lock().unwrap()[incarnation].clone();
            let context = driver.finish(store.acquire(&source)).unwrap().unwrap();
            let drops = Arc::new(AtomicUsize::new(0));
            let value = Outcome {
                value: Box::new(Cell::new(1)),
                drops: Arc::clone(&drops),
            };
            let identity = value.identity();
            let Ok(OutputDelivery::Consumed { task }) =
                driver.finish(context.output(&output, value))
            else {
                panic!("the inner or root handler must consume the live output");
            };
            driver.finish(task.finish());
            let mut expected = vec![("transform", 1, identity), ("decision", 10, identity)];
            if !consume {
                expected.push(("root", 11, identity));
            }
            assert_eq!(std::mem::take(&mut *trace.lock().unwrap()), expected);
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            drop(task);

            if incarnation == 0 {
                driver.finish(driver.finish(store.send(false)).unwrap().finish());
                assert!(
                    driver
                        .finish(store.read(|state| state.branch.is_none()))
                        .unwrap()
                );
                let error = driver
                    .finish(store.acquire(&source))
                    .expect_err("the old nested declaration has retired");
                assert_eq!(error.reason(), rca_core::TaskAccessReason::StoreClosed);
                assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);

                driver.finish(driver.finish(store.send(true)).unwrap().finish());
                assert_eq!(declarations.lock().unwrap().len(), 2);
                let error = driver
                    .finish(store.acquire(&source))
                    .expect_err("remounting must not revive the old nested declaration");
                assert_eq!(error.reason(), rca_core::TaskAccessReason::StoreClosed);
                assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);

                let stale_drops = Arc::new(AtomicUsize::new(0));
                let value = Outcome {
                    value: Box::new(Cell::new(99)),
                    drops: Arc::clone(&stale_drops),
                };
                let identity = value.identity();
                let Err(rca_core::OutputError::Returned { value, error }) =
                    driver.finish(context.output(&output, value))
                else {
                    panic!("old authority must not deliver through the remounted branch");
                };
                assert_eq!(error.reason(), rca_core::TaskAccessReason::StoreClosed);
                assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
                assert_eq!((value.value.get(), value.identity()), (99, identity));
                assert!(trace.lock().unwrap().is_empty());
                assert_eq!(stale_drops.load(Ordering::SeqCst), 0);

                let fresh_output = declarations.lock().unwrap()[1].1.clone();
                let Err(rca_core::OutputError::Returned { value, error }) =
                    driver.finish(context.output(&fresh_output, value))
                else {
                    panic!("a fresh output must not revive old source authority");
                };
                assert_eq!(error.reason(), rca_core::TaskAccessReason::ScopeEnded);
                assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
                assert_eq!((value.value.get(), value.identity()), (99, identity));
                assert!(trace.lock().unwrap().is_empty());
                assert_eq!(stale_drops.load(Ordering::SeqCst), 0);
                drop(value);
                assert_eq!(stale_drops.load(Ordering::SeqCst), 1);
            }
        }

        drop(store);
        driver.turn();
        assert!(driver.0.lock().unwrap().is_empty());
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One output timeline checks retained routing and exact ownership before and after nested retirement"
)]
fn retiring_nested_collection_preserves_surviving_root_output_route() {
    /// Keeps the nested collection separate from the surviving output source.
    struct State {
        rows: Option<rca_core::KeyedState<u8, ()>>,
        source: (),
        outcome: Option<Outcome>,
    }
    let root = rca_core::ReducerFeature::new(rca_core::Reduce::new(
        |state: &mut State, (): &(), _: &mut rca_core::ReduceContext<'_>| {
            state.rows = None;
            rca_core::Effect::none()
        },
    ));
    let driver = Driver::default();
    let captures = Arc::new(OnceLock::new());
    let observed_captures = Arc::clone(&captures);
    let branch = OptionalMount::new(
        |state: &State| state.rows.as_ref(),
        |state: &mut State| state.rows.as_mut(),
        |state: &State| {
            state.rows.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        |_| {
            rca_core::ReducerFeature::new(rca_core::Reduce::new(
                |_: &mut rca_core::KeyedState<u8, ()>,
                 (): &(),
                 _: &mut rca_core::ReduceContext<'_>| rca_core::Effect::none(),
            ))
        },
    );
    let (builder, branch_site) = RootMount::new(root)
        .builder(
            State {
                rows: Some([(1, ()), (2, ())].into_iter().collect()),
                source: (),
                outcome: None,
            },
            Runtime::new(driver.clone()),
        )
        .mount_with_children(branch, move |_, children| {
            let capture = Arc::new(());
            observed_captures.set(Arc::downgrade(&capture)).unwrap();
            children.keyed(
                |rows| rows,
                |rows| rows,
                move |_, ()| {
                    let _ = &capture;
                    rca_core::EntryMount::new(Child)
                },
            );
        });
    let (builder, source_site) = builder.mount_with_site(OptionalMount::new(
        |state: &State| Some(&state.source),
        |state: &mut State| Some(&mut state.source),
        |_| {
            Some(MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        |()| Child,
    ));
    let (builder, output) = builder.output::<Outcome>(&source_site, |handlers| {
        handlers.at_root(|state, value, _| {
            state.outcome = Some(value);
            OutputFlow::Consumed
        });
    });
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    let source = driver.finish(store.acquire(&source_site)).unwrap().unwrap();
    let branch = driver.finish(store.acquire(&branch_site)).unwrap().unwrap();
    let drops = Arc::new(AtomicUsize::new(0));
    assert!(captures.get().unwrap().upgrade().is_some());
    for (index, value) in [17, 23].into_iter().enumerate() {
        let value = outcome(&drops, value);
        let identity = value.identity();
        let Ok(OutputDelivery::Consumed { task }) = driver.finish(source.output(&output, value))
        else {
            panic!("the surviving root handler must consume the output");
        };
        driver.finish(task.finish());
        assert_eq!(
            driver
                .finish(store.read(|state| state.outcome.as_ref().unwrap().identity()))
                .unwrap(),
            identity
        );
        if index == 0 {
            assert!(captures.get().unwrap().upgrade().is_some());
            driver.finish(driver.finish(store.send(())).unwrap().finish());
            assert!(captures.get().unwrap().upgrade().is_none());
            assert_eq!(
                driver.finish(branch.read(|_| ())).unwrap_err().reason(),
                rca_core::TaskAccessReason::ScopeEnded
            );
        }
    }
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    drop(store);
    driver.turn();
    assert_eq!(drops.load(Ordering::SeqCst), 2);
    assert!(driver.0.lock().unwrap().is_empty());
}

/// Checks diagnostics without exposing the payload or internal route storage.
fn assert_opaque_debug(value: &impl std::fmt::Debug, prefix: &str) {
    let diagnostic = format!("{value:?}");
    assert!(diagnostic.starts_with(prefix), "{diagnostic}");
    assert!(!diagnostic.contains("Outcome"));
    assert!(!diagnostic.contains("value:"));
    assert!(!diagnostic.contains("endpoint"));
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One output timeline qualifies ownership, handler traversal and lifecycle observations together."
)]
fn actual_child_task_output_is_consumed_by_root_without_cloning_its_owner() {
    let driver = Driver::default();
    let drops = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(AtomicUsize::new(0));
    let value = Outcome {
        value: Box::new(Cell::new(41)),
        drops: Arc::clone(&drops),
    };
    let identity = value.identity();
    let output_cell = Arc::new(OnceLock::<Output<Outcome>>::new());
    let factory_output = Arc::clone(&output_cell);
    let factory_started = Arc::clone(&started);
    let (complete, receive) = futures_channel::oneshot::channel();
    let mut input = Some((value, complete));
    let mount = OptionalMount::new(
        |state: &RootState| state.child.as_ref(),
        |state: &mut RootState| state.child.as_mut(),
        |state: &RootState| {
            state.child.as_ref().map(|()| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        |()| Child,
    )
    .on_initial(
        |()| (),
        move |_, _| {
            let input = input.take();
            let output = factory_output.get().unwrap().clone();
            let started = Arc::clone(&factory_started);
            Task::<(), ChildAction>::run(move |context| async move {
                let Some((value, complete)) = input else {
                    return;
                };
                started.fetch_add(1, Ordering::SeqCst);
                let delivery = context.output(&output, value);
                assert_owned_send(&delivery);
                let _ = complete.send(delivery.await);
            })
        },
    );
    let builder = RootMount::new(Parent)
        .builder(
            RootState {
                child: Some(()),
                outcome: None,
            },
            Runtime::new(driver.clone()),
        )
        .mailbox_capacity(1);
    #[cfg(feature = "diagnostics")]
    let events = Arc::new(Mutex::new(Vec::new()));
    #[cfg(feature = "diagnostics")]
    let builder = {
        let sink = Arc::clone(&events);
        builder.with_lifecycle_diagnostics(move |event| sink.lock().unwrap().push(*event))
    };
    let (builder, site) = builder.mount_with_site(mount);
    let handler_calls = Arc::clone(&calls);
    let private_count = Cell::new(0); // actor-owned handler is not Sync.
    let (builder, output) = builder.output::<Outcome>(&site, |handlers| {
        handlers.at_root(|_, value, _| {
            value.value.set(value.value.get() + 1);
            let flow = OutputFlow::Continue(value);
            assert_opaque_debug(&flow, "Continue(");
            flow
        });
        handlers.at_root(move |state, value, _| {
            private_count.set(private_count.get() + 1);
            handler_calls.fetch_add(1, Ordering::SeqCst);
            assert!(state.outcome.is_none());
            state.outcome = Some(value);
            let flow = OutputFlow::<Outcome>::Consumed;
            assert_eq!(format!("{flow:?}"), "Consumed");
            flow
        });
        assert_eq!(format!("{handlers:?}"), "OutputHandlers { count: 2 }");
    });
    assert_opaque_debug(&output, "Output {");
    output_cell
        .set(output)
        .expect("output cell is initially empty");
    assert_eq!(started.load(Ordering::SeqCst), 0);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    let result = driver.finish(receive).unwrap();
    assert_opaque_debug(result.as_ref().unwrap(), "Consumed { task:");
    let Ok(OutputDelivery::Consumed { task }) = result else {
        panic!("the actual root handler must consume the output");
    };
    assert_eq!(started.load(Ordering::SeqCst), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    let observed = driver
        .finish(store.read(|state| {
            let value = state.outcome.as_ref().unwrap();
            (value.identity(), value.value.get())
        }))
        .unwrap();
    assert_eq!(observed, (identity, 42));
    #[cfg(feature = "diagnostics")]
    let kinds: Vec<_> = events
        .lock()
        .unwrap()
        .iter()
        .map(rca_core::diagnostics::LifecycleEvent::kind)
        .collect();
    #[cfg(feature = "diagnostics")]
    assert!(
        matches!(
            kinds.as_slice(),
            [LifecycleKind::Installed { lifetime: 0, parent: None, .. },
             LifecycleKind::Installed { lifetime, parent: Some(0), .. }] if *lifetime != 0
        ),
        "output delivery does not invent lifecycle changes"
    );
    drop(task);
    drop(store);
    driver.turn();
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[cfg(feature = "test-util")]
/// Holds a ready child, a one-slot mailbox and two distinct output declarations.
struct Gated {
    driver: Driver,
    gate: rca_core::testing::TransactionGate<RootState, RootAction>,
    store: rca_core::Store<RootState, RootAction>,
    context: rca_core::TaskContext<(), ChildAction>,
    site: rca_core::MountSite<RootState, (), ChildAction, RootAction>,
    output: Output<Outcome>,
    /// Has no handlers and must never match an inspection of `output`.
    other: Output<Outcome>,
    calls: Arc<AtomicUsize>,
    /// Refreshes source authority without replacing the child lifetime.
    configuration: Arc<AtomicUsize>,
    /// Distinguishes authority refresh from reconstructing the child feature.
    constructions: Arc<AtomicUsize>,
}

#[cfg(feature = "test-util")]
/// Starts a gated store whose root consumes the original output.
fn gated() -> Gated {
    gated_with_handler(|state, value| {
        state.outcome = Some(value);
        OutputFlow::Consumed
    })
}

#[cfg(feature = "test-util")]
/// Acknowledges startup before exposing a gate with no pending transaction.
fn gated_with_handler(
    mut handler: impl FnMut(&mut RootState, Outcome) -> OutputFlow<Outcome> + Send + 'static,
) -> Gated {
    let driver = Driver::default();
    let gate = rca_core::testing::TransactionGate::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let configuration = Arc::new(AtomicUsize::new(0));
    let selected_configuration = Arc::clone(&configuration);
    let constructions = Arc::new(AtomicUsize::new(0));
    let constructed = Arc::clone(&constructions);
    let mount = OptionalMount::new(
        |state: &RootState| state.child.as_ref(),
        |state: &mut RootState| state.child.as_mut(),
        move |state: &RootState| {
            state.child.as_ref().map(|()| MountIdentity {
                key: (),
                replacement: 0,
                configuration: selected_configuration.load(Ordering::SeqCst) as u64,
            })
        },
        move |()| {
            constructed.fetch_add(1, Ordering::SeqCst);
            Child
        },
    );
    let builder = RootMount::new(Parent)
        .builder(
            RootState {
                child: Some(()),
                outcome: None,
            },
            Runtime::new(driver.clone()),
        )
        .transaction_gate(gate.clone())
        .mailbox_capacity(1);
    let (builder, site) = builder.mount_with_site(mount);
    let observed = Arc::clone(&calls);
    let (builder, output) = builder.output::<Outcome>(&site, |handlers| {
        handlers.at_root(move |state, value, _| {
            observed.fetch_add(1, Ordering::SeqCst);
            handler(state, value)
        });
    });
    let (builder, other) = builder.output::<Outcome>(&site, |_| {});
    let store = builder.build().unwrap();
    assert_eq!(
        gate.inspect_output(&output, |_| panic!("an empty gate has no output")),
        None::<()>
    );
    assert_eq!(gate.boundary(), None);
    driver.turn();
    assert_eq!(
        gate.pending_kind(),
        Some(rca_core::testing::TransactionKind::Structural)
    );
    let boundary = gate.boundary().unwrap();
    assert_eq!(
        gate.inspect_output(&output, |_| panic!("structural work is not an output")),
        None::<()>
    );
    assert_eq!(gate.boundary(), Some(boundary));
    assert_eq!(
        gate.pending_kind(),
        Some(rca_core::testing::TransactionKind::Structural)
    );
    let receipt = gate.release_pending_with_receipt().unwrap();
    drop(driver.finish(receipt.finish()).unwrap());
    assert!(gate.acknowledge(boundary));
    driver.finish(store.ready()).unwrap();
    let context = driver.finish(store.acquire(&site)).unwrap().unwrap();
    Gated {
        driver,
        gate,
        store,
        context,
        site,
        output,
        other,
        calls,
        configuration,
        constructions,
    }
}

#[test]
#[cfg(feature = "test-util")]
fn queued_output_rejects_refreshed_authority_without_replacing_the_source() {
    let fixture = gated();
    assert_eq!(fixture.constructions.load(Ordering::SeqCst), 1);
    let mut refresh = Box::pin(fixture.store.send(RootAction));
    assert!(poll(refresh.as_mut()).is_pending());
    let drops = Arc::new(AtomicUsize::new(0));
    let value = outcome(&drops, 89);
    let identity = value.identity();
    let mut delivery = Box::pin(fixture.context.output(&fixture.output, value));
    assert!(poll(delivery.as_mut()).is_pending());
    // The earlier root transaction observes the new configuration before this
    // queued output can enter, without changing the child's key or replacement.
    fixture.configuration.store(1, Ordering::SeqCst);
    fixture.driver.turn();
    assert_eq!(
        fixture.gate.pending_kind(),
        Some(rca_core::testing::TransactionKind::Action)
    );
    let boundary = fixture.gate.boundary().unwrap();
    let receipt = fixture.gate.release_pending_with_receipt().unwrap();
    drop(fixture.driver.finish(receipt.finish()).unwrap());
    drop(fixture.driver.finish(refresh).unwrap());
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    assert!(fixture.gate.acknowledge(boundary));

    let Err(rca_core::OutputError::Returned { value, error }) = fixture.driver.finish(delivery)
    else {
        panic!("configuration refresh must reject the queued original output before entry");
    };
    assert_eq!(error.reason(), rca_core::TaskAccessReason::ScopeEnded);
    assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
    assert_eq!(value.identity(), identity);
    assert_eq!(value.value.get(), 89);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.gate.boundary(), None);
    assert!(!fixture.gate.has_pending());
    assert_eq!(fixture.constructions.load(Ordering::SeqCst), 1);
    drop(value);
    assert_eq!(drops.load(Ordering::SeqCst), 1);

    let fresh = fixture
        .driver
        .finish(fixture.store.acquire(&fixture.site))
        .unwrap()
        .expect("configuration refresh preserves the installed source");
    let value = outcome(&drops, 97);
    let identity = value.identity();
    let mut delivery = Box::pin(fresh.output(&fixture.output, value));
    assert!(poll(delivery.as_mut()).is_pending());
    fixture.driver.turn();
    assert_eq!(
        fixture.gate.pending_kind(),
        Some(rca_core::testing::TransactionKind::Output)
    );
    assert_eq!(
        fixture
            .gate
            .inspect_output(&fixture.output, Outcome::identity),
        Some(identity)
    );
    let boundary = fixture.gate.boundary().unwrap();
    let receipt = fixture.gate.release_pending_with_receipt().unwrap();
    drop(fixture.driver.finish(receipt.finish()).unwrap());
    assert!(matches!(
        fixture.driver.finish(delivery),
        Ok(OutputDelivery::Consumed { .. })
    ));
    assert!(fixture.gate.acknowledge(boundary));
    assert_eq!(fixture.gate.boundary(), None);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.constructions.load(Ordering::SeqCst), 1);
    assert_eq!(
        fixture
            .driver
            .finish(fixture.store.read(|state| {
                let value = state.outcome.as_ref().unwrap();
                (value.identity(), value.value.get())
            }))
            .unwrap(),
        (identity, 97)
    );
    drop(fixture.store);
    fixture.driver.turn();
    assert_eq!(drops.load(Ordering::SeqCst), 2);
    assert!(fixture.driver.0.lock().unwrap().is_empty());
    assert_eq!(Arc::strong_count(&fixture.calls), 1);
    assert_eq!(Arc::strong_count(&fixture.configuration), 1);
    assert_eq!(Arc::strong_count(&fixture.constructions), 1);
}

#[test]
#[cfg(feature = "test-util")]
fn handler_unwind_reports_may_have_run_and_drops_consuming_value_once() {
    let fixture = gated_with_handler(|_, _value| panic!("handler unwind"));
    let drops = Arc::new(AtomicUsize::new(0));
    let mut delivery = Box::pin(fixture.context.output(&fixture.output, outcome(&drops, 5)));
    assert!(poll(delivery.as_mut()).is_pending());
    fixture.driver.turn();
    let receipt = fixture.gate.release_pending_with_receipt().unwrap();
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fixture.driver.turn()));
    assert_eq!(
        panic.unwrap_err().downcast_ref::<&str>(),
        Some(&"handler unwind")
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    let result = fixture.driver.finish(delivery);
    let failure = result.as_ref().unwrap_err();
    let diagnostic = format!("{failure:?}");
    assert!(diagnostic.starts_with("Failed {"));
    assert!(diagnostic.contains("ActorFailed"));
    assert!(diagnostic.contains("MayHaveRun"));
    assert!(!diagnostic.contains("Outcome"));
    assert!(!diagnostic.contains("value:"));
    let display = failure.to_string();
    assert!(display.contains("ActorFailed"));
    assert!(display.contains("MayHaveRun"));
    assert!(!display.contains("Outcome"));
    let Err(rca_core::OutputError::Failed { error }) = result else {
        panic!("entered consuming callback cannot return its already-dropped input");
    };
    assert_eq!(error.reason(), rca_core::TaskAccessReason::ActorFailed);
    assert_eq!(error.progress(), rca_core::TaskProgress::MayHaveRun);
    fixture
        .driver
        .finish(receipt.finish())
        .expect_err("handler unwind fails the receipt");
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn reaction_unwind_returns_continued_output_with_original_allocation() {
    let driver = Driver::default();
    let handlers = Arc::new(AtomicUsize::new(0));
    let reactions = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let capture = Arc::new(());
    let weak_capture = Arc::downgrade(&capture);
    let reacted = Arc::clone(&reactions);
    let pending_value = Arc::clone(&drops);
    let root = RootMount::new(Parent).on_change(
        |state| state.child.is_some(),
        rca_core::InitialPolicy::Skip,
        move |change, _| {
            let _ = &capture;
            assert!(matches!(
                change,
                rca_core::ReactionChange::Changed {
                    previous: true,
                    current: false
                }
            ));
            reacted.fetch_add(1, Ordering::SeqCst);
            assert_eq!(pending_value.load(Ordering::SeqCst), 0);
            panic!("reaction after the handler returned its unhandled value");
        },
    );
    let mount = OptionalMount::new(
        |state: &RootState| state.child.as_ref(),
        |state: &mut RootState| state.child.as_mut(),
        |state: &RootState| {
            state.child.map(|()| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        |()| Child,
    );
    let (builder, site) = root
        .builder(
            RootState {
                child: Some(()),
                outcome: None,
            },
            Runtime::new(driver.clone()),
        )
        .mount_with_site(mount);
    let handled = Arc::clone(&handlers);
    let (builder, output) = builder.output::<Outcome>(&site, |handlers| {
        handlers.at_root(move |state, value, _| {
            handled.fetch_add(1, Ordering::SeqCst);
            state.child = None;
            OutputFlow::Continue(value)
        });
    });
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    let context = driver.finish(store.acquire(&site)).unwrap().unwrap();
    let value = Outcome {
        value: Box::new(Cell::new(73)),
        drops: Arc::clone(&drops),
    };
    let identity = value.identity();
    let mut delivery = Box::pin(context.output(&output, value));
    assert!(poll(delivery.as_mut()).is_pending());
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| driver.turn()));
    assert_eq!(
        panic.unwrap_err().downcast_ref::<&str>(),
        Some(&"reaction after the handler returned its unhandled value")
    );
    let Poll::Ready(Err(rca_core::OutputError::Returned { value, error })) =
        poll(delivery.as_mut())
    else {
        panic!("post-handler unwind returns the retained unhandled output");
    };
    assert_eq!(error.reason(), rca_core::TaskAccessReason::ActorFailed);
    assert_eq!(error.progress(), rca_core::TaskProgress::MayHaveRun);
    assert_eq!((value.identity(), value.value.get()), (identity, 73));
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    assert_eq!(handlers.load(Ordering::SeqCst), 1);
    assert_eq!(reactions.load(Ordering::SeqCst), 1);
    assert!(weak_capture.upgrade().is_none());
    assert!(poll(Box::pin(store.closed()).as_mut()).is_ready());
    driver.turn();
    assert!(driver.0.lock().unwrap().is_empty());
    drop(delivery);
    drop(context);
    drop(store);
    driver.turn();
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(value);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(handlers.load(Ordering::SeqCst), 1);
    assert_eq!(reactions.load(Ordering::SeqCst), 1);
}

#[test]
#[cfg(feature = "test-util")]
fn absent_registered_handlers_return_the_same_owned_output_with_commit_receipt() {
    let fixture = gated();
    let drops = Arc::new(AtomicUsize::new(0));
    let value = outcome(&drops, 6);
    let identity = value.identity();
    let mut delivery = Box::pin(fixture.context.output(&fixture.other, value));
    assert!(poll(delivery.as_mut()).is_pending());
    fixture.driver.turn();
    let boundary = fixture.gate.boundary().unwrap();
    let receipt = fixture.gate.release_pending_with_receipt().unwrap();
    drop(fixture.driver.finish(receipt.finish()).unwrap());
    let result = fixture.driver.finish(delivery);
    let diagnostic = format!("{:?}", result.as_ref().unwrap());
    assert!(diagnostic.starts_with("Unhandled {"));
    assert!(diagnostic.contains("task:"));
    assert!(!diagnostic.contains("Outcome"));
    assert!(!diagnostic.contains("value:"));
    let Ok(OutputDelivery::Unhandled { value, task }) = result else {
        panic!("no implicit root fallback consumes output");
    };
    assert_eq!(value.identity(), identity);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    assert!(fixture.gate.acknowledge(boundary));
    fixture.driver.finish(task.finish());
    drop(value);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    drop(fixture.store);
    fixture.driver.turn();
}

#[test]
fn weak_output_handle_traits_do_not_inherit_payload_bounds() {
    fn send_sync<T: Send + Sync>() {}
    fn clone_debug<T: Clone + std::fmt::Debug>() {}
    fn error<T: std::error::Error>() {}
    send_sync::<Output<Outcome>>();
    clone_debug::<Output<std::rc::Rc<()>>>();
    error::<rca_core::OutputError<std::rc::Rc<()>>>();
}

#[cfg(all(feature = "allocation-measurement", feature = "test-util"))]
#[test]
fn output_phase_costs() {
    /// Counts requested allocations, including fixture work inside the supplied phase.
    fn measure<T>(run: impl FnOnce() -> T) -> (T, (u64, u64)) {
        let before = dhat::HeapStats::get();
        let result = run();
        let after = dhat::HeapStats::get();
        (
            result,
            (
                after.total_blocks - before.total_blocks,
                after.total_bytes - before.total_bytes,
            ),
        )
    }
    if allocation_test::run_in_child("output_phase_costs") {
        return;
    }

    let profiler = dhat::Profiler::builder().testing().build();
    let fixture = gated();
    let drops = Arc::new(AtomicUsize::new(0));
    let (handle, clone_cost) = measure(|| fixture.output.clone());
    assert_eq!(clone_cost, (0, 0));
    let mut samples = Vec::new();
    for value in [20, 21] {
        let value = outcome(&drops, value);
        let (delivery, creation) = measure(|| fixture.context.output(&handle, value));
        let inline = std::mem::size_of_val(&delivery);
        let mut delivery = Box::pin(delivery); // Fixture storage, outside measured admission.
        let (pending, admission) = measure(|| poll(delivery.as_mut()));
        assert!(pending.is_pending());
        // These drive phases include the finite Driver's Vec requeue allocation;
        // they are not labelled production-only actor allocation counts.
        let ((), publication) = measure(|| fixture.driver.turn());
        let boundary = fixture.gate.boundary().unwrap();
        let (matched, inspection) = measure(|| fixture.gate.inspect_output(&handle, |_| true));
        assert_eq!(matched, Some(true));
        // The typed reservation borrows O without allocating or copying it.
        assert_eq!(inspection, (0, 0));
        let (receipt, release) = measure(|| fixture.gate.release_pending_with_receipt().unwrap());
        let (task, driven) = measure(|| fixture.driver.finish(receipt.finish()).unwrap());
        drop(task);
        let (result, completion) = measure(|| fixture.driver.finish(delivery));
        assert!(matches!(result, Ok(OutputDelivery::Consumed { .. })));
        assert!(fixture.gate.acknowledge(boundary));
        samples.push((
            creation,
            inline,
            admission,
            publication,
            inspection,
            release,
            driven,
            completion,
        ));
    }
    eprintln!(
        "output blocks/requested bytes: clone={clone_cost:?}; cold/warm (future creation,inline bytes,first-poll admission,driver publication,inspection,release,driver commit,completion)={samples:?}; weak handle bytes={}",
        std::mem::size_of::<Output<Outcome>>()
    );
    drop(fixture.store);
    fixture.driver.turn();
    drop(handle);
    assert_eq!(drops.load(Ordering::SeqCst), 2);
    drop(profiler);
}

/// Creates a distinct owned value whose destruction increments the supplied counter.
fn outcome(drops: &Arc<AtomicUsize>, value: usize) -> Outcome {
    Outcome {
        value: Box::new(Cell::new(value)),
        drops: Arc::clone(drops),
    }
}

#[test]
#[cfg(feature = "test-util")]
fn output_matcher_mismatch_and_unwind_restore_exact_owned_value() {
    let fixture = gated();
    let drops = Arc::new(AtomicUsize::new(0));
    let value = outcome(&drops, 17);
    let identity = value.identity();
    let mut delivery = Box::pin(fixture.context.output(&fixture.output, value));
    assert!(poll(delivery.as_mut()).is_pending());
    fixture.driver.turn();
    let boundary = fixture.gate.boundary().unwrap();
    assert_eq!(
        fixture.gate.pending_kind(),
        Some(rca_core::testing::TransactionKind::Output)
    );
    assert_eq!(
        fixture
            .gate
            .inspect_pending(|_| panic!("an output must not enter an action inspector")),
        None::<()>
    );
    assert_eq!(fixture.gate.boundary(), Some(boundary));
    assert!(fixture.gate.has_pending());
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    assert_eq!(
        fixture.gate.inspect_output(&fixture.other, |_| panic!(
            "wrong declaration must not match"
        )),
        None::<()>
    );
    assert_eq!(
        fixture
            .gate
            .inspect_output(&fixture.output, |value| value.value.get() == 99),
        Some(false)
    );
    assert_eq!(fixture.gate.boundary(), Some(boundary));
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        fixture.gate.inspect_output(&fixture.output, |value| {
            assert_eq!(value.identity(), identity);
            assert_eq!(
                fixture.gate.inspect_output(&fixture.output, |_| panic!(
                    "recursive inspection cannot borrow the pinned value"
                )),
                None::<()>
            );
            assert_eq!(fixture.gate.output_origin(&fixture.output), None);
            panic!("deliberate typed matcher unwind");
        });
    }));
    assert_eq!(
        panic.unwrap_err().downcast_ref::<&str>(),
        Some(&"deliberate typed matcher unwind")
    );
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        fixture
            .gate
            .inspect_output(&fixture.output, Outcome::identity),
        Some(identity)
    );
    let receipt = fixture.gate.release_pending_with_receipt().unwrap();
    drop(fixture.driver.finish(receipt.finish()).unwrap());
    assert!(matches!(
        fixture.driver.finish(delivery),
        Ok(OutputDelivery::Consumed { .. })
    ));
    assert!(fixture.gate.acknowledge(boundary));
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    drop(fixture.store);
    fixture.driver.turn();
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
#[cfg(feature = "test-util")]
fn shutdown_waits_for_output_inspection_and_returns_original_owner() {
    let fixture = gated();
    let drops = Arc::new(AtomicUsize::new(0));
    let value = outcome(&drops, 23);
    let identity = value.identity();
    let mut delivery = Box::pin(fixture.context.output(&fixture.output, value));
    assert!(poll(delivery.as_mut()).is_pending());
    fixture.driver.turn();
    let boundary = fixture.gate.boundary().unwrap();
    let terminal = fixture.gate.pending_boundary().unwrap();
    fixture
        .gate
        .inspect_output(&fixture.output, |value| {
            drop(fixture.store);
            fixture.driver.turn();
            assert_eq!(value.identity(), identity);
            assert_eq!(drops.load(Ordering::SeqCst), 0);
            assert_eq!(fixture.gate.boundary(), Some(boundary));
            let replacement = RootMount::new(Parent)
                .builder(
                    RootState {
                        child: None,
                        outcome: None,
                    },
                    Runtime::new(fixture.driver.clone()),
                )
                .transaction_gate(fixture.gate.clone())
                .build();
            assert!(
                replacement.is_err(),
                "borrowed output reserves the actor attachment"
            );
        })
        .unwrap();
    let returned = fixture.driver.finish(delivery);
    let failure = returned.as_ref().unwrap_err();
    let diagnostic = format!("{failure:?}");
    assert!(diagnostic.starts_with("Returned {"));
    assert!(diagnostic.contains("StoreClosed"));
    assert!(diagnostic.contains("NotStarted"));
    assert!(!diagnostic.contains("Outcome"));
    assert!(!diagnostic.contains("value:"));
    let display = failure.to_string();
    assert!(display.contains("StoreClosed"));
    assert!(display.contains("NotStarted"));
    assert!(!display.contains("Outcome"));
    let Err(rca_core::OutputError::Returned { value, error }) = returned else {
        panic!("shutdown must return the restored, never-entered output");
    };
    assert_eq!(value.identity(), identity);
    assert_eq!(error.reason(), rca_core::TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(value);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    fixture.driver.turn();
    assert_eq!(fixture.gate.boundary(), Some(boundary));
    let mut detached_query = Box::pin(
        fixture
            .gate
            .assert_state(|_| panic!("a detached actor cannot run a state query"))
            .finish(),
    );
    assert!(matches!(poll(detached_query.as_mut()), Poll::Ready(Err(_))));
    let premature = RootMount::new(Parent)
        .builder(
            RootState {
                child: None,
                outcome: None,
            },
            Runtime::new(fixture.driver.clone()),
        )
        .transaction_gate(fixture.gate.clone())
        .build();
    assert!(premature.is_err(), "terminal debt reserves the attachment");
    assert_eq!(
        fixture.gate.match_terminal(&terminal, |error| {
            error.reason() == rca_core::TaskAccessReason::StoreClosed
                && error.progress() == rca_core::TaskProgress::NotStarted
        }),
        Some(true)
    );
    assert_eq!(fixture.gate.boundary(), Some(boundary));
    assert!(fixture.gate.acknowledge_terminal(&terminal));
    assert_eq!(fixture.gate.boundary(), None);
    let replacement = RootMount::new(Parent)
        .builder(
            RootState {
                child: None,
                outcome: None,
            },
            Runtime::new(fixture.driver.clone()),
        )
        .transaction_gate(fixture.gate.clone())
        .build()
        .expect("explicit terminal acknowledgement releases the attachment");
    drop(replacement);
    fixture.driver.turn();
}

#[test]
#[cfg(feature = "test-util")]
fn committed_output_debt_keeps_queued_output_behind_the_same_frontier() {
    let fixture = gated();
    let drops = Arc::new(AtomicUsize::new(0));
    let mut first = Box::pin(fixture.context.output(&fixture.output, outcome(&drops, 1)));
    assert!(poll(first.as_mut()).is_pending());
    fixture.driver.turn();
    let first_boundary = fixture.gate.boundary().unwrap();
    let mut second = Box::pin(fixture.context.output(&fixture.output, outcome(&drops, 2)));
    assert!(poll(second.as_mut()).is_pending());
    let receipt = fixture.gate.release_pending_with_receipt().unwrap();
    drop(fixture.driver.finish(receipt.finish()).unwrap());
    assert!(matches!(
        fixture.driver.finish(first),
        Ok(OutputDelivery::Consumed { .. })
    ));
    fixture.driver.turn();
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.gate.boundary(), Some(first_boundary));
    assert!(
        !fixture.gate.has_pending(),
        "committed component debt is not a second pending command"
    );
    assert!(poll(second.as_mut()).is_pending());
    assert!(fixture.gate.acknowledge(first_boundary));
    fixture.driver.turn();
    let second_boundary = fixture.gate.boundary().unwrap();
    assert_ne!(first_boundary, second_boundary);
    assert_eq!(
        fixture
            .gate
            .inspect_output(&fixture.output, |value| value.value.get()),
        Some(2)
    );
    let receipt = fixture.gate.release_pending_with_receipt().unwrap();
    drop(fixture.driver.finish(receipt.finish()).unwrap());
    assert!(matches!(
        fixture.driver.finish(second),
        Ok(OutputDelivery::Consumed { .. })
    ));
    assert!(fixture.gate.acknowledge(second_boundary));
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        drops.load(Ordering::SeqCst),
        1,
        "second handler replaced the first owned value"
    );
    drop(fixture.store);
    fixture.driver.turn();
    assert_eq!(drops.load(Ordering::SeqCst), 2);
}

#[test]
#[cfg(feature = "test-util")]
#[expect(
    clippy::too_many_lines,
    reason = "One output transaction proves partial projection, work disposal and terminal ownership together"
)]
fn continuing_handler_hides_later_route_and_returns_original_output() {
    use std::sync::atomic::AtomicBool;

    /// Separates stored child presence from whether its projection is visible.
    struct ProjectedRoot {
        state: RootState,
        visible: Arc<AtomicBool>,
    }
    /// Leaves projection changes to the first output handler.
    struct ProjectedParent;
    impl Feature for ProjectedParent {
        type State = ProjectedRoot;
        type Action = RootAction;
        fn reduce(
            &mut self,
            _: &mut ProjectedRoot,
            _: &RootAction,
            _: &mut FeatureContext<'_, ProjectedRoot, RootAction>,
        ) {
        }
    }
    let driver = Driver::default();
    let gate = rca_core::testing::TransactionGate::new();
    let visible = Arc::new(AtomicBool::new(true));
    let mount = OptionalMount::new(
        |state: &ProjectedRoot| {
            state
                .state
                .child
                .as_ref()
                .filter(|()| state.visible.load(Ordering::SeqCst))
        },
        |state: &mut ProjectedRoot| {
            state
                .state
                .child
                .as_mut()
                .filter(|()| state.visible.load(Ordering::SeqCst))
        },
        |state: &ProjectedRoot| {
            state.state.child.as_ref().map(|()| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        |()| Child,
    );
    let builder = RootMount::new(ProjectedParent)
        .builder(
            ProjectedRoot {
                state: RootState {
                    child: Some(()),
                    outcome: None,
                },
                visible: Arc::clone(&visible),
            },
            Runtime::new(driver.clone()),
        )
        .transaction_gate(gate.clone());
    let calls = Arc::new(AtomicUsize::new(0));
    let operations = Arc::new(AtomicUsize::new(0));
    let capture = Arc::new(());
    let capture_weak = Arc::downgrade(&capture);
    let mut capture = Some(capture);
    let sources = Arc::new(OnceLock::new());
    let outputs = Arc::new(OnceLock::new());
    let recipe_sources = Arc::clone(&sources);
    let recipe_outputs = Arc::clone(&outputs);
    let recipe_calls = Arc::clone(&calls);
    let recipe_operations = Arc::clone(&operations);
    let recipe_visible = Arc::clone(&visible);
    let (builder, _parent) = builder.mount_with_children(mount, move |(), children| {
        let parent = children.site().clone();
        let source = children.optional(OptionalMount::new(
            |state: &()| Some(state),
            |state: &mut ()| Some(state),
            |()| {
                Some(MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            |()| Child,
        ));
        let first_calls = Arc::clone(&recipe_calls);
        let effect_calls = Arc::clone(&recipe_operations);
        let hidden = Arc::clone(&recipe_visible);
        let mut capture = capture.take();
        let output = children.output::<Outcome>(&source, |handlers| {
            handlers.at(&parent, move |(), value, context| {
                first_calls.fetch_add(1, Ordering::SeqCst);
                hidden.store(false, Ordering::SeqCst);
                let capture = capture.take().expect("one entered output");
                let effect_calls = Arc::clone(&effect_calls);
                context.effect(rca_core::Effect::run(move |_| async move {
                    effect_calls.fetch_add(1, Ordering::SeqCst);
                    drop(capture);
                }));
                OutputFlow::Continue(value)
            });
            handlers.at(&parent, |(), _, _| panic!("the later projection is hidden"));
            handlers.at_root(|_, _, _| panic!("a route failure must not reach the root"));
        });
        recipe_sources.set(source).expect("one source lifetime");
        recipe_outputs.set(output).expect("one output lifetime");
    });
    let store = builder.build().unwrap();
    driver.turn();
    let startup = gate.boundary().unwrap();
    drop(
        driver
            .finish(gate.release_pending_with_receipt().unwrap().finish())
            .unwrap(),
    );
    assert!(gate.acknowledge(startup));
    driver.finish(store.ready()).unwrap();
    let site = sources.get().unwrap();
    let output = outputs.get().unwrap();
    let context = driver.finish(store.acquire(site)).unwrap().unwrap();
    let drops = Arc::new(AtomicUsize::new(0));
    let value = outcome(&drops, 61);
    let identity = value.identity();
    let mut delivery = Box::pin(context.output(output, value));
    assert!(poll(delivery.as_mut()).is_pending());
    driver.turn();
    let boundary = gate.pending_boundary().unwrap();
    assert_eq!(
        gate.pending_kind(),
        Some(rca_core::testing::TransactionKind::Output)
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    driver
        .finish(gate.release_pending_with_receipt().unwrap().finish())
        .expect_err("an entered output retains its route error");
    let Err(rca_core::OutputError::Returned { value, error }) = driver.finish(delivery) else {
        panic!("a hidden route must return the original output");
    };
    assert_eq!(error.reason(), rca_core::TaskAccessReason::ScopeEnded);
    assert_eq!(error.progress(), rca_core::TaskProgress::MayHaveRun);
    assert_eq!(value.identity(), identity);
    assert_eq!(value.value.get(), 61);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(operations.load(Ordering::SeqCst), 0);
    assert!(capture_weak.upgrade().is_none());
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    assert_eq!(
        gate.match_terminal(&boundary, |terminal| *terminal == error),
        Some(true)
    );
    assert!(gate.acknowledge_terminal(&boundary));
    assert_eq!(gate.boundary(), None);
    assert_eq!(
        driver
            .finish(
                store.read(|state| (state.state.child.is_some(), state.state.outcome.is_none()))
            )
            .unwrap(),
        (true, true)
    );
    visible.store(true, Ordering::SeqCst);
    assert!(driver.finish(store.acquire(site)).unwrap().is_some());
    drop(value);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    drop(store);
    driver.turn();
    assert!(driver.0.lock().unwrap().is_empty());
    assert_eq!(operations.load(Ordering::SeqCst), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one output transaction proves disposal before reaction, delivery ownership, and causal completion"
)]
fn output_prunes_retired_parent_work_after_root_consumes_original() {
    use std::sync::atomic::AtomicBool;

    /// Keeps root output storage alive after its child's projection disappears.
    struct ProjectedRoot {
        child: Option<()>,
        outcome: Option<Outcome>,
        visible: Arc<AtomicBool>,
    }
    /// Leaves parent retirement to the output handler.
    struct ProjectedParent;
    impl Feature for ProjectedParent {
        type State = ProjectedRoot;
        type Action = RootAction;

        fn reduce(
            &mut self,
            _: &mut ProjectedRoot,
            _: &RootAction,
            _: &mut FeatureContext<'_, ProjectedRoot, RootAction>,
        ) {
        }
    }
    /// Proves queued parent work is destroyed before the root reaction runs.
    struct Capture(Arc<AtomicUsize>);
    impl Drop for Capture {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    let driver = Driver::default();
    let visible = Arc::new(AtomicBool::new(true));
    let runs = Arc::new(AtomicUsize::new(0));
    let capture_drops = Arc::new(AtomicUsize::new(0));
    let reactions = Arc::new(AtomicUsize::new(0));
    let sources = Arc::new(OnceLock::new());
    let outputs = Arc::new(OnceLock::new());
    let mount = OptionalMount::new(
        |state: &ProjectedRoot| {
            state
                .child
                .as_ref()
                .filter(|()| state.visible.load(Ordering::SeqCst))
        },
        |state: &mut ProjectedRoot| {
            state
                .child
                .as_mut()
                .filter(|()| state.visible.load(Ordering::SeqCst))
        },
        |state: &ProjectedRoot| {
            state
                .child
                .as_ref()
                .filter(|()| state.visible.load(Ordering::SeqCst))
                .map(|()| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
        },
        |()| Child,
    );
    let reacted = Arc::clone(&reactions);
    let disposed_before_reaction = Arc::clone(&capture_drops);
    let (builder, parent_site) = RootMount::new(ProjectedParent)
        .on_change(
            |state| state.visible.load(Ordering::SeqCst),
            rca_core::InitialPolicy::Skip,
            move |change, _| {
                assert!(matches!(
                    change,
                    rca_core::ReactionChange::Changed {
                        previous: true,
                        current: false
                    }
                ));
                assert_eq!(disposed_before_reaction.load(Ordering::SeqCst), 1);
                reacted.fetch_add(1, Ordering::SeqCst);
                Task::run(|_| async {})
            },
        )
        .builder(
            ProjectedRoot {
                child: Some(()),
                outcome: None,
                visible: Arc::clone(&visible),
            },
            Runtime::new(driver.clone()),
        )
        .mount_with_children(mount, {
            let visible = Arc::clone(&visible);
            let runs = Arc::clone(&runs);
            let capture_drops = Arc::clone(&capture_drops);
            let sources = Arc::clone(&sources);
            let outputs = Arc::clone(&outputs);
            move |(), children| {
                let parent = children.site().clone();
                let source = children.optional(OptionalMount::new(
                    |state: &()| Some(state),
                    |state: &mut ()| Some(state),
                    |()| {
                        Some(MountIdentity {
                            key: (),
                            replacement: 0,
                            configuration: 0,
                        })
                    },
                    |()| Child,
                ));
                let hidden = Arc::clone(&visible);
                let effect_runs = Arc::clone(&runs);
                let mut capture = Some(Capture(Arc::clone(&capture_drops)));
                let output = children.output::<Outcome>(&source, |handlers| {
                    handlers.at(&parent, move |(), value, context| {
                        let capture = capture.take().expect("one parent output");
                        let effect_runs = Arc::clone(&effect_runs);
                        context.effect(rca_core::Effect::run(move |_| async move {
                            effect_runs.fetch_add(1, Ordering::SeqCst);
                            drop(capture);
                        }));
                        hidden.store(false, Ordering::SeqCst);
                        OutputFlow::Continue(value)
                    });
                    handlers.at_root(|state, value, _| {
                        state.outcome = Some(value);
                        OutputFlow::Consumed
                    });
                });
                sources.set(source).expect("one source lifetime");
                outputs.set(output).expect("one output lifetime");
            }
        });
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    let source = driver
        .finish(store.acquire(sources.get().unwrap()))
        .unwrap()
        .unwrap();
    let drops = Arc::new(AtomicUsize::new(0));
    let value = Outcome {
        value: Box::new(Cell::new(73)),
        drops: Arc::clone(&drops),
    };
    let identity = value.identity();
    let Ok(OutputDelivery::Consumed { task }) =
        driver.finish(source.output(outputs.get().unwrap(), value))
    else {
        panic!("the live root handler consumes the continued output");
    };
    driver.finish(task.finish());
    assert!(!visible.load(Ordering::SeqCst));
    assert!(
        driver
            .finish(store.acquire(&parent_site))
            .unwrap()
            .is_none()
    );
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    assert_eq!(reactions.load(Ordering::SeqCst), 1);
    assert_eq!(capture_drops.load(Ordering::SeqCst), 1);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    assert_eq!(
        driver
            .finish(store.read(|state| {
                let value = state.outcome.as_ref().unwrap();
                (value.identity(), value.value.get())
            }))
            .unwrap(),
        (identity, 73)
    );
    drop(task);
    drop(store);
    driver.turn();
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert!(driver.0.lock().unwrap().is_empty());
}

#[test]
#[cfg(feature = "test-util")]
#[expect(
    clippy::too_many_lines,
    reason = "One queued owner-retirement schedule preserves source authority, payload and terminal acknowledgement"
)]
fn queued_output_returns_original_when_only_its_child_owner_retires() {
    /// Removes only the child that owns the output declaration.
    struct RetireChild;
    impl Feature for RetireChild {
        type State = RootState;
        type Action = RootAction;
        fn reduce(
            &mut self,
            state: &mut RootState,
            _: &RootAction,
            _: &mut FeatureContext<'_, RootState, RootAction>,
        ) {
            state.child = None;
        }
    }
    let driver = Driver::default();
    let gate = rca_core::testing::TransactionGate::new();
    let outputs = Arc::new(OnceLock::new());
    let captures = Arc::new(OnceLock::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let recipe_outputs = Arc::clone(&outputs);
    let recipe_captures = Arc::clone(&captures);
    let recipe_calls = Arc::clone(&calls);
    let (builder, parent_site) = RootMount::new(RetireChild)
        .builder(
            RootState {
                child: Some(()),
                outcome: None,
            },
            Runtime::new(driver.clone()),
        )
        .transaction_gate(gate.clone())
        .mount_with_children(
            OptionalMount::new(
                |state: &RootState| Some(state),
                |state: &mut RootState| Some(state),
                |_| {
                    Some(MountIdentity {
                        key: (),
                        replacement: 0,
                        configuration: 0,
                    })
                },
                |_| Parent,
            ),
            move |_, children| {
                let parent = children.site().clone();
                let outputs = Arc::clone(&recipe_outputs);
                let captures = Arc::clone(&recipe_captures);
                let calls = Arc::clone(&recipe_calls);
                children.optional_with_children(
                    OptionalMount::new(
                        |state: &RootState| state.child.as_ref(),
                        |state: &mut RootState| state.child.as_mut(),
                        |state: &RootState| {
                            state.child.as_ref().map(|()| MountIdentity {
                                key: (),
                                replacement: 0,
                                configuration: 0,
                            })
                        },
                        |()| Child,
                    ),
                    move |(), grandchildren| {
                        let capture = Arc::new(());
                        captures
                            .set(Arc::downgrade(&capture))
                            .expect("one output owner");
                        let calls = Arc::clone(&calls);
                        let output = grandchildren.output::<Outcome>(&parent, |handlers| {
                            handlers.at_root(move |_, value, _| {
                                let _ = &capture;
                                calls.fetch_add(1, Ordering::SeqCst);
                                drop(value);
                                OutputFlow::Consumed
                            });
                        });
                        outputs.set(output).expect("one output declaration");
                    },
                );
            },
        );
    let store = builder.build().unwrap();
    driver.turn();
    let startup = gate.boundary().unwrap();
    drop(
        driver
            .finish(gate.release_pending_with_receipt().unwrap().finish())
            .unwrap(),
    );
    assert!(gate.acknowledge(startup));
    driver.finish(store.ready()).unwrap();
    let parent = driver.finish(store.acquire(&parent_site)).unwrap().unwrap();
    let output = outputs.get().unwrap();
    assert!(captures.get().unwrap().upgrade().is_some());

    let mut retire = Box::pin(store.send(RootAction));
    assert!(poll(retire.as_mut()).is_pending());
    driver.turn();
    assert_eq!(
        gate.pending_kind(),
        Some(rca_core::testing::TransactionKind::Action)
    );
    let boundary = gate.boundary().unwrap();
    let drops = Arc::new(AtomicUsize::new(0));
    let value = outcome(&drops, 47);
    let identity = value.identity();
    let mut delivery = Box::pin(parent.output(output, value));
    assert!(poll(delivery.as_mut()).is_pending());
    drop(
        driver
            .finish(gate.release_pending_with_receipt().unwrap().finish())
            .unwrap(),
    );
    drop(driver.finish(retire.as_mut()).unwrap());
    drop(retire);
    assert!(gate.acknowledge(boundary));
    assert!(captures.get().unwrap().upgrade().is_none());
    driver.turn();
    assert_eq!(
        gate.pending_kind(),
        Some(rca_core::testing::TransactionKind::Output)
    );
    let output_boundary = gate.pending_boundary().unwrap();
    assert_eq!(
        gate.inspect_output(output, |_| panic!(
            "retired declaration cannot expose its value"
        )),
        None::<()>
    );
    assert_eq!(gate.output_origin(output), None);
    assert_eq!(
        gate.pending_kind(),
        Some(rca_core::testing::TransactionKind::Output)
    );
    driver
        .finish(gate.release_pending_with_receipt().unwrap().finish())
        .expect_err("retired output owner rejects the queued delivery");
    let Err(rca_core::OutputError::Returned { value, error }) = driver.finish(delivery) else {
        panic!("retired child-owned output returns its original value");
    };
    assert_eq!(value.identity(), identity);
    assert_eq!(value.value.get(), 47);
    assert_eq!(error.reason(), rca_core::TaskAccessReason::ScopeEnded);
    assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    assert_eq!(
        gate.match_terminal(&output_boundary, |terminal| *terminal == error),
        Some(true)
    );
    assert!(gate.acknowledge_terminal(&output_boundary));
    assert!(
        driver
            .finish(parent.read(|state| state.child.is_none()))
            .unwrap()
    );
    assert!(
        driver
            .finish(store.acquire(&parent_site))
            .unwrap()
            .is_some()
    );
    assert_eq!(gate.pending_kind(), None);
    assert_eq!(gate.boundary(), None);
    drop(value);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    drop(store);
    driver.turn();
    assert!(driver.0.lock().unwrap().is_empty());
}

#[test]
fn expired_child_handler_registration_is_skipped_without_losing_the_output() {
    let driver = Driver::default();
    let foreign_driver = Driver::default();
    let mount = || {
        OptionalMount::new(
            |state: &RootState| state.child.as_ref(),
            |state: &mut RootState| state.child.as_mut(),
            |state: &RootState| {
                state.child.as_ref().map(|()| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            |()| Child,
        )
    };
    let builder = |driver| {
        RootMount::new(Parent).builder(
            RootState {
                child: Some(()),
                outcome: None,
            },
            Runtime::new(driver),
        )
    };
    let (foreign_builder, foreign_site) = builder(foreign_driver.clone()).mount_with_site(mount());
    let foreign = foreign_builder.build().unwrap();
    foreign_driver.finish(foreign.ready()).unwrap();
    let (builder, site) = builder(driver.clone()).mount_with_site(mount());
    let stale_calls = Arc::new(AtomicUsize::new(0));
    let stale_observed = Arc::clone(&stale_calls);
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&calls);
    let capture = Arc::new(());
    let weak_capture = Arc::downgrade(&capture);
    let (builder, output) = builder.output::<Outcome>(&site, |handlers| {
        handlers.at(&foreign_site, move |(), value, _| {
            let _ = &capture;
            stale_observed.fetch_add(1, Ordering::SeqCst);
            OutputFlow::Continue(value)
        });
        handlers.at_root(move |state, value, _| {
            observed.fetch_add(1, Ordering::SeqCst);
            state.outcome = Some(value);
            OutputFlow::Consumed
        });
    });
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    let context = driver.finish(store.acquire(&site)).unwrap().unwrap();
    drop(foreign);
    foreign_driver.turn();
    assert!(foreign_driver.0.lock().unwrap().is_empty());
    // The foreign declaration is gone, but this output still owns its handler
    // capture. The retained site is weak and cannot preserve the registration.
    assert!(weak_capture.upgrade().is_some());
    let drops = Arc::new(AtomicUsize::new(0));
    let value = Outcome {
        value: Box::new(Cell::new(107)),
        drops: Arc::clone(&drops),
    };
    let identity = value.identity();
    let Ok(OutputDelivery::Consumed { task }) = driver.finish(context.output(&output, value))
    else {
        panic!("an expired child handler cannot suppress the live root handler");
    };
    driver.finish(task.finish());
    assert_eq!(stale_calls.load(Ordering::SeqCst), 0);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    assert!(weak_capture.upgrade().is_some());
    assert_eq!(
        driver
            .finish(store.read(|state| {
                let value = state.outcome.as_ref().unwrap();
                (value.identity(), value.value.get())
            }))
            .unwrap(),
        (identity, 107)
    );
    drop(store);
    driver.turn();
    assert!(weak_capture.upgrade().is_none());
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(Arc::strong_count(&stale_calls), 1);
    assert_eq!(Arc::strong_count(&calls), 1);
    assert!(driver.0.lock().unwrap().is_empty());
}

#[test]
fn weak_output_does_not_keep_unbuilt_handler_captures_alive() {
    /// Counts capture disposal while a `Cell` prevents an accidental `Sync` bound.
    struct Capture(Arc<AtomicUsize>, Cell<usize>);
    impl Drop for Capture {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let driver = Driver::default();
    let drops = Arc::new(AtomicUsize::new(0));
    let captured = Capture(Arc::clone(&drops), Cell::new(0));
    let mount = OptionalMount::new(
        |state: &RootState| state.child.as_ref(),
        |state: &mut RootState| state.child.as_mut(),
        |state: &RootState| {
            state.child.as_ref().map(|()| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        |()| Child,
    );
    let (builder, site) = RootMount::new(Parent)
        .builder(
            RootState {
                child: Some(()),
                outcome: None,
            },
            Runtime::new(driver),
        )
        .mount_with_site(mount);
    let (builder, output) = builder.output::<Outcome>(&site, |handlers| {
        handlers.at_root(move |_, value, _| {
            captured.1.set(captured.1.get() + 1);
            let _keep_whole_capture = &captured;
            OutputFlow::Continue(value)
        });
    });
    let retained = output.clone();
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(builder);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    drop((site, output, retained));
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
#[cfg(feature = "test-util")]
#[expect(
    clippy::too_many_lines,
    reason = "One public routing timeline isolates root and scope rejection before the same owned value succeeds"
)]
fn output_requires_its_declared_root_and_scope_before_entering_a_frontier() {
    let driver = Driver::default();
    let gate = rca_core::testing::TransactionGate::new();
    let foreign_gate = rca_core::testing::TransactionGate::new();
    let mount = || {
        OptionalMount::new(
            |state: &RootState| state.child.as_ref(),
            |state: &mut RootState| state.child.as_mut(),
            |state: &RootState| {
                state.child.as_ref().map(|()| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            |()| Child,
        )
    };
    let (builder, site) = RootMount::new(Parent)
        .builder(
            RootState {
                child: Some(()),
                outcome: None,
            },
            Runtime::new(driver.clone()),
        )
        .transaction_gate(gate.clone())
        .mount_with_site(mount());
    let (builder, sibling_site) = builder.mount_with_site(mount());
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&calls);
    let (builder, output) = builder.output::<Outcome>(&site, |handlers| {
        handlers.at(&site, |(), _, _| {
            panic!("the source is not its own ancestor")
        });
        handlers.at(&sibling_site, |(), _, _| {
            panic!("a live sibling is not an output ancestor");
        });
        handlers.at_root(move |state, value, _| {
            observed.fetch_add(1, Ordering::SeqCst);
            assert!(state.outcome.is_none());
            state.outcome = Some(value);
            OutputFlow::Consumed
        });
    });
    let (unbuilt_builder, unbuilt_site) = RootMount::new(Parent)
        .builder(
            RootState {
                child: Some(()),
                outcome: None,
            },
            Runtime::new(driver.clone()),
        )
        .mount_with_site(mount());
    let capture = Arc::new(());
    let weak_capture = Arc::downgrade(&capture);
    let (builder, uninstalled_source) = builder.output::<Outcome>(&unbuilt_site, |handlers| {
        handlers.at_root(move |_, _, _| {
            let _ = &capture;
            panic!("a bound output cannot deliver from an unbuilt source");
        });
    });
    let store = builder.build().unwrap();
    // The declaration API accepts an external site. Its root must still be
    // rejected at delivery even while that source and both stores are live.
    let (foreign_builder, foreign_output) = RootMount::new(Parent)
        .builder(
            RootState {
                child: Some(()),
                outcome: None,
            },
            Runtime::new(driver.clone()),
        )
        .transaction_gate(foreign_gate.clone())
        .output::<Outcome>(&site, |handlers| {
            handlers.at_root(|_, _, _| panic!("foreign-root handler cannot run"));
        });
    let foreign = foreign_builder.build().unwrap();
    driver.turn();
    for current_gate in [&gate, &foreign_gate] {
        let startup = current_gate.boundary().unwrap();
        drop(
            driver
                .finish(
                    current_gate
                        .release_pending_with_receipt()
                        .unwrap()
                        .finish(),
                )
                .unwrap(),
        );
        assert!(current_gate.acknowledge(startup));
    }
    driver.finish(store.ready()).unwrap();
    driver.finish(foreign.ready()).unwrap();
    let source = driver.finish(store.acquire(&site)).unwrap().unwrap();
    let sibling = driver
        .finish(store.acquire(&sibling_site))
        .unwrap()
        .unwrap();
    let drops = Arc::new(AtomicUsize::new(0));
    let mut value = outcome(&drops, 101);
    let identity = value.identity();
    // Sibling: same root, distinct actual mount scopes. Foreign declaration:
    // correct source context, but its installed root differs from the binding.
    // The final declaration has a live owner but a retained, unbuilt source.
    for (context, declaration) in [
        (&sibling, &output),
        (&source, &foreign_output),
        (&source, &uninstalled_source),
    ] {
        let mut delivery = Box::pin(context.output(declaration, value));
        let Poll::Ready(Err(rca_core::OutputError::Returned {
            value: returned,
            error,
        })) = poll(delivery.as_mut())
        else {
            panic!("routing mismatch must return before another actor turn");
        };
        assert_eq!(error.reason(), rca_core::TaskAccessReason::ScopeEnded);
        assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
        assert_eq!(returned.identity(), identity);
        assert_eq!(returned.value.get(), 101);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        assert!(weak_capture.upgrade().is_some());
        for current_gate in [&gate, &foreign_gate] {
            assert_eq!(current_gate.pending_kind(), None);
            assert_eq!(current_gate.boundary(), None);
        }
        for current_store in [&store, &foreign] {
            assert_eq!(
                driver
                    .finish(
                        current_store
                            .read(|state| { (state.child.is_some(), state.outcome.is_none()) })
                    )
                    .unwrap(),
                (true, true)
            );
        }
        value = returned;
    }

    let mut delivery = Box::pin(source.output(&output, value));
    assert!(poll(delivery.as_mut()).is_pending());
    driver.turn();
    assert_eq!(
        gate.pending_kind(),
        Some(rca_core::testing::TransactionKind::Output)
    );
    assert_eq!(
        gate.inspect_output(&output, Outcome::identity),
        Some(identity)
    );
    let boundary = gate.boundary().unwrap();
    drop(
        driver
            .finish(gate.release_pending_with_receipt().unwrap().finish())
            .unwrap(),
    );
    assert!(matches!(
        driver.finish(delivery),
        Ok(OutputDelivery::Consumed { .. })
    ));
    assert!(gate.acknowledge(boundary));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    assert_eq!(foreign_gate.boundary(), None);
    assert_eq!(
        driver
            .finish(store.read(|state| {
                let value = state.outcome.as_ref().unwrap();
                (value.identity(), value.value.get())
            }))
            .unwrap(),
        (identity, 101)
    );
    drop((store, foreign));
    driver.turn();
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert!(weak_capture.upgrade().is_none());
    drop(unbuilt_builder);
    assert!(driver.0.lock().unwrap().is_empty());
}

#[test]
#[cfg(feature = "test-util")]
fn foreign_context_returns_original_output_before_any_handler_or_frontier() {
    let source = gated();
    let foreign = gated();
    let drops = Arc::new(AtomicUsize::new(0));
    let value = outcome(&drops, 91);
    let identity = value.identity();
    let result = foreign
        .driver
        .finish(foreign.context.output(&source.output, value));
    let Err(rca_core::OutputError::Returned { value, error }) = result else {
        panic!("a foreign source cannot enter the declaration");
    };
    assert_eq!(value.identity(), identity);
    assert_eq!(error.reason(), rca_core::TaskAccessReason::ScopeEnded);
    assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
    assert_eq!(source.gate.boundary(), None);
    assert_eq!(foreign.gate.boundary(), None);
    assert_eq!(source.calls.load(Ordering::SeqCst), 0);
    assert_eq!(foreign.calls.load(Ordering::SeqCst), 0);
    drop(value);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    drop(source.store);
    source.driver.turn();
    drop(foreign.store);
    foreign.driver.turn();
}

#[test]
#[cfg(feature = "test-util")]
fn output_after_last_store_drop_returns_original_value_before_and_after_actor_teardown() {
    for teardown in [false, true] {
        let fixture = gated();
        let drops = Arc::new(AtomicUsize::new(0));
        let value = outcome(&drops, 73);
        let identity = value.identity();
        drop(fixture.store);
        if teardown {
            fixture.driver.turn();
        }
        let mut delivery = Box::pin(fixture.context.output(&fixture.output, value));
        let Poll::Ready(Err(rca_core::OutputError::Returned { value, error })) =
            poll(delivery.as_mut())
        else {
            panic!("owner loss must reject before another actor turn");
        };
        assert_eq!(error.reason(), rca_core::TaskAccessReason::StoreClosed);
        assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
        assert_eq!(value.identity(), identity);
        assert_eq!(value.value.get(), 73);
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.gate.boundary(), None);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(value);
        drop(delivery);
        fixture.driver.turn();
        assert!(fixture.driver.0.lock().unwrap().is_empty());
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(Arc::strong_count(&drops), 1);
        assert_eq!(Arc::strong_count(&fixture.calls), 1);
    }
}

#[test]
#[cfg(feature = "test-util")]
fn admitted_output_returns_original_on_shutdown_or_host_abandonment() {
    for abandon_host in [false, true] {
        let capture = Arc::new(());
        let weak_capture = Arc::downgrade(&capture);
        let fixture = gated_with_handler(move |state, value| {
            let _ = &capture;
            state.outcome = Some(value);
            OutputFlow::Consumed
        });
        let drops = Arc::new(AtomicUsize::new(0));
        let value = outcome(&drops, 79);
        let identity = value.identity();
        let mut delivery = Box::pin(fixture.context.output(&fixture.output, value));
        assert!(poll(delivery.as_mut()).is_pending());
        assert_eq!(fixture.gate.boundary(), None);
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
        assert_eq!(drops.load(Ordering::SeqCst), 0);

        drop(fixture.store);
        // Admission already completed; only actor teardown can settle this reply.
        assert!(poll(delivery.as_mut()).is_pending());
        if abandon_host {
            let tasks = std::mem::take(&mut *fixture.driver.0.lock().unwrap());
            assert!(!tasks.is_empty());
            drop(tasks);
        } else {
            fixture.driver.turn();
        }
        let Poll::Ready(Err(rca_core::OutputError::Returned { value, error })) =
            poll(delivery.as_mut())
        else {
            panic!("an admitted output must return its original value during actor shutdown");
        };
        assert_eq!(value.identity(), identity);
        assert_eq!(value.value.get(), 79);
        assert_eq!(error.reason(), rca_core::TaskAccessReason::StoreClosed);
        assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
        assert_eq!(fixture.gate.boundary(), None);
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(value);
        drop(delivery);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(fixture.driver.0.lock().unwrap().is_empty());
        assert!(weak_capture.upgrade().is_none());
        assert_eq!(Arc::strong_count(&drops), 1);
        assert_eq!(Arc::strong_count(&fixture.calls), 1);
    }
}

#[test]
#[cfg(feature = "test-util")]
fn closing_full_mailbox_returns_original_output_without_entering_handler() {
    let fixture = gated();
    let mut filling = Box::pin(fixture.context.send(ChildAction));
    assert!(poll(filling.as_mut()).is_pending());
    let drops = Arc::new(AtomicUsize::new(0));
    let value = outcome(&drops, 83);
    let identity = value.identity();
    let mut delivery = Box::pin(fixture.context.output(&fixture.output, value));
    assert!(poll(delivery.as_mut()).is_pending());
    assert_eq!(fixture.gate.boundary(), None);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 0);

    drop(fixture.store);
    let Poll::Ready(Err(rca_core::OutputError::Returned { value, error })) =
        poll(delivery.as_mut())
    else {
        panic!("closing bounded admission must return the unentered original output");
    };
    assert_eq!(error.reason(), rca_core::TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
    assert_eq!(value.identity(), identity);
    assert_eq!(value.value.get(), 83);
    assert_eq!(fixture.gate.boundary(), None);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(value);
    drop(delivery);
    fixture.driver.turn();
    let Poll::Ready(Err(rca_core::TaskSendError::Rejected { error, .. })) = poll(filling.as_mut())
    else {
        panic!("the admitted but unentered child request must also be rejected");
    };
    assert_eq!(error.reason(), rca_core::TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
    drop(filling);
    assert!(fixture.driver.0.lock().unwrap().is_empty());
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(Arc::strong_count(&drops), 1);
    assert_eq!(Arc::strong_count(&fixture.calls), 1);
}

#[test]
#[cfg(feature = "test-util")]
fn queued_context_read_and_update_report_store_closed_without_entering_callbacks() {
    for (update, teardown_host) in [(false, false), (true, false), (false, true), (true, true)] {
        let fixture = gated();
        let callbacks = Arc::new(AtomicUsize::new(0));
        let captured_calls = Arc::clone(&callbacks);
        let capture = Arc::new(());
        let lifetime = Arc::downgrade(&capture);
        let context = fixture.context.clone();
        let mut request = Box::pin(async move {
            if update {
                context
                    .update(move |()| {
                        let _ = &capture;
                        captured_calls.fetch_add(1, Ordering::SeqCst);
                    })
                    .await
                    .map(|_| ())
            } else {
                context
                    .read(move |()| {
                        let _ = &capture;
                        captured_calls.fetch_add(1, Ordering::SeqCst);
                    })
                    .await
            }
        });
        assert!(poll(request.as_mut()).is_pending());
        assert!(lifetime.upgrade().is_some());
        assert_eq!(fixture.gate.boundary(), None);
        drop(fixture.store);
        if teardown_host {
            let tasks = std::mem::take(&mut *fixture.driver.0.lock().unwrap());
            assert!(!tasks.is_empty());
            drop(tasks);
        } else {
            fixture.driver.turn();
        }
        let Poll::Ready(Err(error)) = poll(request.as_mut()) else {
            panic!("queued context access must report ordinary owner loss");
        };
        assert_eq!(error.reason(), rca_core::TaskAccessReason::StoreClosed);
        assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
        assert_eq!(callbacks.load(Ordering::SeqCst), 0);
        assert!(lifetime.upgrade().is_none());
        drop(request);
        assert_eq!(Arc::strong_count(&callbacks), 1);
        assert!(fixture.driver.0.lock().unwrap().is_empty());
        if update && !teardown_host {
            // Updates retain their terminal transaction boundary without entering the callback.
            let boundary = fixture.gate.pending_boundary().unwrap();
            assert_eq!(
                fixture
                    .gate
                    .match_terminal(&boundary, |terminal| *terminal == error),
                Some(true)
            );
            assert!(fixture.gate.acknowledge_terminal(&boundary));
        }
        assert_eq!(fixture.gate.boundary(), None);
    }
}

#[test]
#[cfg(feature = "test-util")]
fn unbuilt_output_returns_original_value_without_entering_captured_handler() {
    let live = gated();
    let capture = Arc::new(());
    let lifetime = Arc::downgrade(&capture);
    let mount = OptionalMount::new(
        |state: &RootState| state.child.as_ref(),
        |state: &mut RootState| state.child.as_mut(),
        |state: &RootState| {
            state.child.as_ref().map(|()| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        |()| Child,
    );
    let (builder, site) = RootMount::new(Parent)
        .builder(
            RootState {
                child: Some(()),
                outcome: None,
            },
            Runtime::new(live.driver.clone()),
        )
        .mount_with_site(mount);
    let calls = Arc::new(AtomicUsize::new(0));
    let callback_calls = Arc::clone(&calls);
    let (builder, output) = builder.output::<Outcome>(&site, |handlers| {
        handlers.at_root(move |_, value, _| {
            let _ = &capture;
            callback_calls.fetch_add(1, Ordering::SeqCst);
            OutputFlow::Continue(value)
        });
    });
    let drops = Arc::new(AtomicUsize::new(0));
    let value = outcome(&drops, 37);
    let identity = value.identity();
    let mut delivery = Box::pin(live.context.output(&output, value));
    let Poll::Ready(Err(rca_core::OutputError::Returned { value, error })) =
        poll(delivery.as_mut())
    else {
        panic!("an unbuilt declaration has no actor to accept the output");
    };
    assert_eq!(error.reason(), rca_core::TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
    assert_eq!(value.identity(), identity);
    assert_eq!(value.value.get(), 37);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(live.calls.load(Ordering::SeqCst), 0);
    assert_eq!(live.gate.boundary(), None);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    assert!(lifetime.upgrade().is_some());
    drop(value);
    drop(delivery);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    drop(builder);
    assert!(lifetime.upgrade().is_none());
    assert_eq!(Arc::strong_count(&calls), 1);
    drop((site, output, live.store));
    live.driver.turn();
    assert!(live.driver.0.lock().unwrap().is_empty());
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(Arc::strong_count(&drops), 1);
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One destructor timeline proves the foreign declaration survives its node and preserves the output"
)]
fn dying_child_handler_is_skipped_before_its_declaration_is_destroyed() {
    /// Installs the second actor's operation after both stores are ready.
    type DropCallback = Arc<Mutex<Option<Box<dyn FnOnce() + Send>>>>;
    /// Re-enters a different actor while this installed feature is being destroyed.
    struct DroppingChild(DropCallback);
    impl Feature for DroppingChild {
        type State = ();
        type Action = ChildAction;

        fn reduce(
            &mut self,
            (): &mut (),
            _: &ChildAction,
            _: &mut FeatureContext<'_, (), ChildAction>,
        ) {
        }
    }
    impl Drop for DroppingChild {
        fn drop(&mut self) {
            let callback = self.0.lock().unwrap().take();
            if let Some(callback) = callback {
                callback();
            }
        }
    }

    let foreign_driver = Driver::default();
    let driver = Driver::default();
    let callback: DropCallback = Arc::default();
    let constructor_callback = Arc::clone(&callback);
    let declaration = Arc::new(());
    let declaration_lifetime = Arc::downgrade(&declaration);
    let mount = OptionalMount::new(
        |state: &RootState| state.child.as_ref(),
        |state: &mut RootState| state.child.as_mut(),
        |state: &RootState| {
            state.child.as_ref().map(|()| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        move |()| {
            let _ = &declaration;
            DroppingChild(Arc::clone(&constructor_callback))
        },
    );
    let (builder, foreign_site) = RootMount::new(Parent)
        .builder(
            RootState {
                child: Some(()),
                outcome: None,
            },
            Runtime::new(foreign_driver.clone()),
        )
        .mount_with_site(mount);
    let foreign = builder.build().unwrap();
    foreign_driver.finish(foreign.ready()).unwrap();

    let (builder, site) = RootMount::new(Parent)
        .builder(
            RootState {
                child: Some(()),
                outcome: None,
            },
            Runtime::new(driver.clone()),
        )
        .mount_with_site(OptionalMount::new(
            |state: &RootState| state.child.as_ref(),
            |state: &mut RootState| state.child.as_mut(),
            |state: &RootState| {
                state.child.as_ref().map(|()| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            |()| Child,
        ));
    let trace = Arc::new(Mutex::new(Vec::new()));
    let stale_calls = Arc::new(AtomicUsize::new(0));
    let stale = Arc::clone(&stale_calls);
    let observed_trace = Arc::clone(&trace);
    let observed_declaration = std::sync::Weak::clone(&declaration_lifetime);
    let (builder, output) = builder.output::<Outcome>(&site, |handlers| {
        handlers.at(&foreign_site, move |(), value, _| {
            stale.fetch_add(1, Ordering::SeqCst);
            OutputFlow::Continue(value)
        });
        handlers.at_root(move |state, value, _| {
            assert!(observed_declaration.upgrade().is_some());
            observed_trace.lock().unwrap().push("root");
            state.outcome = Some(value);
            OutputFlow::Consumed
        });
    });
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    let context = driver.finish(store.acquire(&site)).unwrap().unwrap();
    let drops = Arc::new(AtomicUsize::new(0));
    let value = outcome(&drops, 109);
    let identity = value.identity();
    let during_drop = Arc::clone(&trace);
    let live_driver = driver.clone();
    *callback.lock().unwrap() = Some(Box::new(move || {
        during_drop.lock().unwrap().push("drop-enter");
        let Ok(OutputDelivery::Consumed { task }) =
            live_driver.finish(context.output(&output, value))
        else {
            panic!("a dying foreign handler must not prevent live root delivery");
        };
        live_driver.finish(task.finish());
        during_drop.lock().unwrap().push("drop-exit");
    }));

    drop(foreign);
    foreign_driver.turn();
    assert_eq!(*trace.lock().unwrap(), ["drop-enter", "root", "drop-exit"]);
    assert_eq!(stale_calls.load(Ordering::SeqCst), 0);
    assert!(declaration_lifetime.upgrade().is_none());
    assert!(foreign_driver.0.lock().unwrap().is_empty());
    assert!(callback.lock().unwrap().is_none());
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    assert_eq!(
        driver
            .finish(store.read(|state| {
                let value = state.outcome.as_ref().unwrap();
                (value.identity(), value.value.get())
            }))
            .unwrap(),
        (identity, 109)
    );
    drop(store);
    driver.turn();
    assert!(driver.0.lock().unwrap().is_empty());
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}
