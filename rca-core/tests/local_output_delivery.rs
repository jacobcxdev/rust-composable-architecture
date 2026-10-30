//! Local output ownership, routing and teardown with non-thread-safe payloads.
//!
//! `Rc` identity and drop counts distinguish returned values from replacements.
//! Explicit driver turns keep the tests independent of wall-clock timing.

#![cfg(feature = "local")]

#[cfg(all(feature = "allocation-measurement", feature = "test-util"))]
#[path = "../src/allocation_test/child.rs"]
mod allocation_test;

use std::cell::{Cell, RefCell};
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use rca_core::local::{
    Feature, FeatureContext, OptionalMount, Runtime, RuntimeDriver, RuntimeFuture, Store,
};
use rca_core::{MonotonicInstant, MountIdentity, OutputDelivery, OutputFlow};

#[cfg(all(feature = "allocation-measurement", feature = "test-util"))]
#[global_allocator]
/// Measures allocations only inside the isolated child assay.
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

/// Polls owner-thread tasks explicitly, with a clock that stays at zero.
#[derive(Clone, Default)]
struct Driver(Rc<RefCell<Vec<RuntimeFuture>>>);

impl std::fmt::Debug for Driver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LocalOutputDriver")
    }
}

impl RuntimeDriver for Driver {
    fn spawn(&self, task: RuntimeFuture) {
        self.0.borrow_mut().push(task);
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
    /// Polls each queued task once, without borrowing the queue during callbacks.
    fn turn(&self) {
        let pending = std::mem::take(&mut *self.0.borrow_mut());
        for mut future in pending {
            if poll(future.as_mut()).is_pending() {
                self.0.borrow_mut().push(future);
            }
        }
    }

    /// Completes finite work within 64 turns without allocating a fixture box.
    fn finish<F: Future>(&self, future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        for _ in 0..64 {
            if let Poll::Ready(value) = poll(future.as_mut()) {
                return value;
            }
            self.turn();
        }
        panic!("finite local output fixture did not finish");
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
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Counts wake requests without requiring local payloads to be thread-safe.
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

/// Tracks one moved `Rc` allocation without implementing `Clone` or `Debug`.
struct Outcome {
    value: Rc<Cell<usize>>,
    drops: Rc<Cell<usize>>,
}

impl Drop for Outcome {
    fn drop(&mut self) {
        self.drops.set(self.drops.get() + 1);
    }
}

/// Keeps the optional source and the value consumed by its root handler.
struct RootState {
    child: Option<Rc<Cell<usize>>>,
    outcome: Option<Outcome>,
}

/// Requests child retirement while retaining a non-thread-safe action owner.
struct RootAction(Rc<()>);
/// Fills the child mailbox with a non-thread-safe action owner.
struct ChildAction(Rc<()>);
/// Removes the child when a root action is committed.
struct Parent;
/// Supplies an installed output source with no reducer behaviour.
struct Child;

impl Feature for Parent {
    type State = RootState;
    type Action = RootAction;

    fn update(
        &mut self,
        state: &mut RootState,
        action: &RootAction,
        _: &mut FeatureContext<'_, RootState, RootAction>,
    ) {
        let _ = &action.0;
        state.child = None;
    }
}

impl Feature for Child {
    type State = Rc<Cell<usize>>;
    type Action = ChildAction;

    fn update(
        &mut self,
        _: &mut Self::State,
        action: &ChildAction,
        _: &mut FeatureContext<'_, Self::State, ChildAction>,
    ) {
        let _ = &action.0;
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One remount timeline checks handler order, transformed ownership and stale authority together"
)]
fn local_remounted_output_keeps_ancestor_order_before_transform_and_consumption() {
    /// Gives the output source a mounted ancestor below the root.
    struct Branch {
        leaf: Rc<Cell<usize>>,
    }
    /// Removes and restores the whole branch while retaining the root.
    struct State {
        branch: Option<Branch>,
    }

    for consume in [false, true] {
        let driver = Driver::default();
        let trace = Rc::new(RefCell::new(Vec::new()));
        let declarations = Rc::new(RefCell::new(Vec::new()));
        let recipe_trace = Rc::clone(&trace);
        let recipe_declarations = Rc::clone(&declarations);
        let root = rca_core::local::Reduce::new(
            |state: &mut State, present: &bool, _: &mut rca_core::ReduceContext<'_>| {
                state.branch = present.then(|| Branch {
                    leaf: Rc::new(Cell::new(0)),
                });
                rca_core::local::Effect::none()
            },
        );
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
                rca_core::local::Reduce::new(
                    |_: &mut Branch, (): &(), _: &mut rca_core::ReduceContext<'_>| {
                        rca_core::local::Effect::none()
                    },
                )
            },
        );
        let (builder, _) = Store::builder(
            State {
                branch: Some(Branch {
                    leaf: Rc::new(Cell::new(0)),
                }),
            },
            root,
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
                |_| Child,
            ));
            let root_trace = Rc::clone(&recipe_trace);
            let transform_trace = Rc::clone(&recipe_trace);
            let decision_trace = Rc::clone(&recipe_trace);
            let output = children.output::<Outcome>(&source, |handlers| {
                // Register the root first so registration order cannot pass this test.
                handlers.at_root(move |_, value, _| {
                    root_trace.borrow_mut().push((
                        "root",
                        value.value.get(),
                        Rc::as_ptr(&value.value),
                    ));
                    OutputFlow::Consumed
                });
                handlers.at(&parent, move |_, value, _| {
                    transform_trace.borrow_mut().push((
                        "transform",
                        value.value.get(),
                        Rc::as_ptr(&value.value),
                    ));
                    value.value.set(value.value.get() * 10);
                    OutputFlow::Continue(value)
                });
                handlers.at(&parent, move |_, value, _| {
                    decision_trace.borrow_mut().push((
                        "decision",
                        value.value.get(),
                        Rc::as_ptr(&value.value),
                    ));
                    if consume {
                        OutputFlow::Consumed
                    } else {
                        value.value.set(value.value.get() + 1);
                        OutputFlow::Continue(value)
                    }
                });
            });
            recipe_declarations.borrow_mut().push((source, output));
        });
        let store = builder.build().unwrap();
        driver.finish(store.ready()).unwrap();
        assert_eq!(declarations.borrow().len(), 1);

        for incarnation in 0..2 {
            let (source, output) = declarations.borrow()[incarnation].clone();
            let context = driver.finish(store.acquire(&source)).unwrap().unwrap();
            let drops = Rc::new(Cell::new(0));
            let value = Outcome {
                value: Rc::new(Cell::new(1)),
                drops: Rc::clone(&drops),
            };
            let identity = Rc::as_ptr(&value.value);
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
            assert_eq!(std::mem::take(&mut *trace.borrow_mut()), expected);
            assert_eq!(drops.get(), 1);
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
                assert_eq!(declarations.borrow().len(), 2);
                let error = driver
                    .finish(store.acquire(&source))
                    .expect_err("remounting must not revive the old nested declaration");
                assert_eq!(error.reason(), rca_core::TaskAccessReason::StoreClosed);
                assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);

                let stale_drops = Rc::new(Cell::new(0));
                let value = Outcome {
                    value: Rc::new(Cell::new(99)),
                    drops: Rc::clone(&stale_drops),
                };
                let identity = Rc::as_ptr(&value.value);
                let Err(rca_core::OutputError::Returned { value, error }) =
                    driver.finish(context.output(&output, value))
                else {
                    panic!("old authority must not deliver through the remounted branch");
                };
                assert_eq!(error.reason(), rca_core::TaskAccessReason::StoreClosed);
                assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
                assert_eq!(
                    (value.value.get(), Rc::as_ptr(&value.value)),
                    (99, identity)
                );
                assert!(trace.borrow().is_empty());
                assert_eq!(stale_drops.get(), 0);

                let fresh_output = declarations.borrow()[1].1.clone();
                let Err(rca_core::OutputError::Returned { value, error }) =
                    driver.finish(context.output(&fresh_output, value))
                else {
                    panic!("a fresh output must not revive old source authority");
                };
                assert_eq!(error.reason(), rca_core::TaskAccessReason::ScopeEnded);
                assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
                assert_eq!(
                    (value.value.get(), Rc::as_ptr(&value.value)),
                    (99, identity)
                );
                assert!(trace.borrow().is_empty());
                assert_eq!(stale_drops.get(), 0);
                drop(value);
                assert_eq!(stale_drops.get(), 1);
            }
        }

        drop(store);
        driver.turn();
        assert!(driver.0.borrow().is_empty());
    }
}

#[test]
fn public_local_child_output_keeps_rc_identity_and_owner_local_captures() {
    let driver = Driver::default();
    let drops = Rc::new(Cell::new(0));
    let calls = Rc::new(Cell::new(0));
    let capture = Rc::new(());
    let weak_capture = Rc::downgrade(&capture);
    let mount = OptionalMount::new(
        |state: &RootState| state.child.as_ref(),
        |state: &mut RootState| state.child.as_mut(),
        |state: &RootState| {
            state.child.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        |_| Child,
    );
    let (builder, source) = Store::builder(
        RootState {
            child: Some(Rc::new(Cell::new(7))),
            outcome: None,
        },
        Parent,
        Runtime::new(driver.clone()),
    )
    .mount_with_site(mount);
    let seen = Rc::clone(&calls);
    let (builder, output) = builder.output::<Outcome>(&source, |handlers| {
        assert_eq!(format!("{handlers:?}"), "OutputHandlers { count: 0 }");
        handlers.at_root(move |state, value, _| {
            let _ = &capture;
            seen.set(seen.get() + 1);
            state.outcome = Some(value);
            OutputFlow::Consumed
        });
        assert_eq!(format!("{handlers:?}"), "OutputHandlers { count: 1 }");
        assert_eq!(calls.get(), 0);
        assert_eq!(drops.get(), 0);
        assert!(weak_capture.upgrade().is_some());
    });
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    let context = driver.finish(store.acquire(&source)).unwrap().unwrap();
    let value = Rc::new(Cell::new(41));
    let identity = Rc::as_ptr(&value);
    let value = Outcome {
        value,
        drops: Rc::clone(&drops),
    };
    assert_eq!(format!("{output:?}"), "Output { .. }");
    assert_eq!((calls.get(), drops.get()), (0, 0));
    assert!(weak_capture.upgrade().is_some());
    let result = driver.finish(context.output(&output, value));
    let delivery = result.unwrap();
    let OutputDelivery::Consumed { task } = delivery else {
        panic!("root must consume");
    };
    driver.finish(task.finish());
    assert_eq!(calls.get(), 1);
    assert_eq!(
        driver
            .finish(store.read(|state| Rc::as_ptr(&state.outcome.as_ref().unwrap().value)))
            .unwrap(),
        identity
    );
    assert_eq!(drops.get(), 0);
    drop(store);
    driver.turn();
    assert_eq!(drops.get(), 1);
    assert!(weak_capture.upgrade().is_none());
    drop(output);
}

#[cfg(feature = "test-util")]
/// Pauses transactions over the shared local fixture state.
type Gate = rca_core::testing::local::TransactionGate<RootState, RootAction>;

#[cfg(feature = "test-util")]
/// Holds a ready child, a one-slot mailbox and two distinct output declarations.
struct Gated {
    driver: Driver,
    gate: Gate,
    store: Store<RootState, RootAction>,
    context: rca_core::local::TaskContext<Rc<Cell<usize>>, ChildAction>,
    output: rca_core::local::Output<Outcome>,
    /// Has no handlers and must never match an inspection of `output`.
    other: rca_core::local::Output<Outcome>,
    calls: Rc<Cell<usize>>,
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
    mut handler: impl FnMut(&mut RootState, Outcome) -> OutputFlow<Outcome> + 'static,
) -> Gated {
    let driver = Driver::default();
    let gate = Gate::new();
    let calls = Rc::new(Cell::new(0));
    let mount = OptionalMount::new(
        |state: &RootState| state.child.as_ref(),
        |state: &mut RootState| state.child.as_mut(),
        |state: &RootState| {
            state.child.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        |_| Child,
    );
    let (builder, source) = Store::builder(
        RootState {
            child: Some(Rc::new(Cell::new(7))),
            outcome: None,
        },
        Parent,
        Runtime::new(driver.clone()),
    )
    .transaction_gate(gate.clone())
    .mailbox_capacity(1)
    .mount_with_site(mount);
    let observed = std::rc::Rc::clone(&calls);
    let (builder, output) = builder.output::<Outcome>(&source, |handlers| {
        handlers.at_root(move |state, value, _| {
            observed.set(observed.get() + 1);
            handler(state, value)
        });
    });
    let (builder, other) = builder.output::<Outcome>(&source, |_| {});
    let store = builder.build().unwrap();
    assert_eq!(
        gate.inspect_output(&output, |_| panic!("an empty gate has no output")),
        None::<()>
    );
    assert_eq!(gate.boundary(), None);
    driver.turn();
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
    let context = driver.finish(store.acquire(&source)).unwrap().unwrap();
    Gated {
        driver,
        gate,
        store,
        context,
        output,
        other,
        calls,
    }
}

/// Creates a distinct owned value whose destruction increments the supplied counter.
fn outcome(drops: &Rc<Cell<usize>>, value: usize) -> Outcome {
    Outcome {
        value: Rc::new(Cell::new(value)),
        drops: std::rc::Rc::clone(drops),
    }
}

#[test]
#[cfg(feature = "test-util")]
fn local_output_after_final_store_drop_returns_original_before_actor_teardown() {
    let capture = Rc::new(());
    let weak_capture = Rc::downgrade(&capture);
    let fixture = gated_with_handler(move |state, value| {
        let _ = &capture;
        state.outcome = Some(value);
        OutputFlow::Consumed
    });
    let drops = Rc::new(Cell::new(0));
    let value = outcome(&drops, 61);
    let identity = Rc::as_ptr(&value.value);

    drop(fixture.store);
    assert!(
        !fixture.driver.0.borrow().is_empty(),
        "the actor has not been polled after closure"
    );
    let mut delivery = Box::pin(fixture.context.output(&fixture.output, value));
    let Poll::Ready(Err(rca_core::OutputError::Returned { value, error })) =
        poll(delivery.as_mut())
    else {
        panic!("closed Local output must return its original value before actor teardown");
    };
    assert_eq!(Rc::as_ptr(&value.value), identity);
    assert_eq!(error.reason(), rca_core::TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
    assert_eq!(fixture.gate.boundary(), None);
    assert_eq!(fixture.calls.get(), 0);
    assert_eq!(drops.get(), 0);
    drop(value);
    drop(delivery);
    assert_eq!(drops.get(), 1);
    fixture.driver.turn();
    assert!(fixture.driver.0.borrow().is_empty());
    assert!(weak_capture.upgrade().is_none());
    assert_eq!(fixture.calls.get(), 0);
    assert_eq!(drops.get(), 1);
    assert_eq!(Rc::strong_count(&drops), 1);
    assert_eq!(Rc::strong_count(&fixture.calls), 1);
}

#[test]
#[cfg(feature = "test-util")]
fn admitted_local_output_returns_original_on_shutdown_or_host_abandonment() {
    for abandon_host in [false, true] {
        let capture = Rc::new(());
        let weak_capture = Rc::downgrade(&capture);
        let fixture = gated_with_handler(move |state, value| {
            let _ = &capture;
            state.outcome = Some(value);
            OutputFlow::Consumed
        });
        let drops = Rc::new(Cell::new(0));
        let value = outcome(&drops, 79);
        let identity = Rc::as_ptr(&value.value);
        let mut delivery = Box::pin(fixture.context.output(&fixture.output, value));
        assert!(poll(delivery.as_mut()).is_pending());
        assert_eq!(fixture.gate.boundary(), None);
        assert_eq!(fixture.calls.get(), 0);
        assert_eq!(drops.get(), 0);

        drop(fixture.store);
        // Admission already completed; only actor teardown can settle this reply.
        assert!(poll(delivery.as_mut()).is_pending());
        if abandon_host {
            let tasks = std::mem::take(&mut *fixture.driver.0.borrow_mut());
            assert!(!tasks.is_empty());
            drop(tasks);
        } else {
            fixture.driver.turn();
        }
        let Poll::Ready(Err(rca_core::OutputError::Returned { value, error })) =
            poll(delivery.as_mut())
        else {
            panic!("an admitted Local output must return its original value on actor shutdown");
        };
        assert_eq!(Rc::as_ptr(&value.value), identity);
        assert_eq!(value.value.get(), 79);
        assert_eq!(error.reason(), rca_core::TaskAccessReason::StoreClosed);
        assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
        assert_eq!(fixture.gate.boundary(), None);
        assert_eq!(fixture.calls.get(), 0);
        assert_eq!(drops.get(), 0);
        drop(value);
        drop(delivery);
        assert_eq!(drops.get(), 1);
        assert!(fixture.driver.0.borrow().is_empty());
        assert!(weak_capture.upgrade().is_none());
        assert_eq!(Rc::strong_count(&drops), 1);
        assert_eq!(Rc::strong_count(&fixture.calls), 1);
    }
}

#[test]
#[cfg(feature = "test-util")]
fn handler_capture_drop_reenters_closed_local_output_before_lifecycle_disposal() {
    /// Retains weak access to the source and its output declaration.
    type Reentrant = (
        rca_core::local::TaskContext<Rc<Cell<usize>>, ChildAction>,
        rca_core::local::Output<Outcome>,
    );
    /// Attempts output delivery while the actor destroys its handler captures.
    struct ReenterOnDrop {
        /// Is filled only after the store has installed the source.
        request: Rc<RefCell<Option<Reentrant>>>,
        drops: Rc<Cell<usize>>,
        observed: Rc<Cell<bool>>,
    }
    impl Drop for ReenterOnDrop {
        fn drop(&mut self) {
            let (context, output) = self
                .request
                .borrow_mut()
                .take()
                .expect("the ready fixture exports one reentrant request");
            let value = outcome(&self.drops, 73);
            let identity = Rc::as_ptr(&value.value);
            let mut delivery = Box::pin(context.output(&output, value));
            let Poll::Ready(Err(rca_core::OutputError::Returned { value, error })) =
                poll(delivery.as_mut())
            else {
                panic!("closed actor must reject output during handler-capture disposal");
            };
            assert_eq!(Rc::as_ptr(&value.value), identity);
            assert_eq!(error.reason(), rca_core::TaskAccessReason::StoreClosed);
            assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
            assert_eq!(self.drops.get(), 0);
            drop(value);
            drop(delivery);
            assert_eq!(self.drops.get(), 1);
            self.observed.set(true);
        }
    }

    let request = Rc::new(RefCell::new(None));
    let drops = Rc::new(Cell::new(0));
    let observed = Rc::new(Cell::new(false));
    let capture = ReenterOnDrop {
        request: Rc::clone(&request),
        drops: Rc::clone(&drops),
        observed: Rc::clone(&observed),
    };
    let fixture = gated_with_handler(move |_, _| {
        let _ = &capture;
        panic!("closed output must not enter the handler");
    });
    *request.borrow_mut() = Some((fixture.context.clone(), fixture.output.clone()));
    assert!(!fixture.driver.0.borrow().is_empty());
    drop(fixture.store);
    assert!(!observed.get());
    let tasks = std::mem::take(&mut *fixture.driver.0.borrow_mut());
    drop(tasks);
    assert!(observed.get());
    assert!(request.borrow().is_none());
    assert!(fixture.driver.0.borrow().is_empty());
    assert_eq!(fixture.gate.boundary(), None);
    assert_eq!(fixture.calls.get(), 0);
    assert_eq!(drops.get(), 1);
    assert_eq!(Rc::strong_count(&request), 1);
    assert_eq!(Rc::strong_count(&drops), 1);
    assert_eq!(Rc::strong_count(&observed), 1);
}

#[test]
#[cfg(feature = "test-util")]
fn unbuilt_local_output_returns_original_without_entering_captured_handler() {
    let live = gated();
    let capture = Rc::new(());
    let weak_capture = Rc::downgrade(&capture);
    let mount = OptionalMount::new(
        |state: &RootState| state.child.as_ref(),
        |state: &mut RootState| state.child.as_mut(),
        |state: &RootState| {
            state.child.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        |_| Child,
    );
    let (builder, source) = Store::builder(
        RootState {
            child: Some(Rc::new(Cell::new(7))),
            outcome: None,
        },
        Parent,
        Runtime::new(live.driver.clone()),
    )
    .mount_with_site(mount);
    let calls = Rc::new(Cell::new(0));
    let callback_calls = Rc::clone(&calls);
    let (builder, output) = builder.output::<Outcome>(&source, |handlers| {
        handlers.at_root(move |_, value, _| {
            let _ = &capture;
            callback_calls.set(callback_calls.get() + 1);
            OutputFlow::Continue(value)
        });
    });
    let drops = Rc::new(Cell::new(0));
    let value = outcome(&drops, 79);
    let identity = Rc::as_ptr(&value.value);
    let mut delivery = Box::pin(live.context.output(&output, value));
    let Poll::Ready(Err(rca_core::OutputError::Returned { value, error })) =
        poll(delivery.as_mut())
    else {
        panic!("an unbuilt Local declaration cannot accept output");
    };
    assert_eq!(Rc::as_ptr(&value.value), identity);
    assert_eq!(value.value.get(), 79);
    assert_eq!(error.reason(), rca_core::TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
    assert_eq!(calls.get(), 0);
    assert_eq!(live.calls.get(), 0);
    assert_eq!(live.gate.boundary(), None);
    assert_eq!(drops.get(), 0);
    assert!(weak_capture.upgrade().is_some());
    drop(value);
    drop(delivery);
    assert_eq!(drops.get(), 1);
    drop(builder);
    assert!(weak_capture.upgrade().is_none());
    assert_eq!(Rc::strong_count(&calls), 1);
    drop((source, output, live.store));
    live.driver.turn();
    assert!(live.driver.0.borrow().is_empty());
    assert_eq!(drops.get(), 1);
    assert_eq!(Rc::strong_count(&drops), 1);
}

#[test]
#[cfg(feature = "test-util")]
fn capacity_blocked_local_output_returns_original_when_store_closes() {
    let capture = Rc::new(());
    let weak_capture = Rc::downgrade(&capture);
    let fixture = gated_with_handler(move |state, value| {
        let _ = &capture;
        state.outcome = Some(value);
        OutputFlow::Consumed
    });
    let action_owner = Rc::new(());
    let action_identity = Rc::as_ptr(&action_owner);
    let action_weak = Rc::downgrade(&action_owner);
    let mut send = Box::pin(fixture.context.send(ChildAction(action_owner)));
    assert!(poll(send.as_mut()).is_pending());
    let drops = Rc::new(Cell::new(0));
    let value = outcome(&drops, 71);
    let identity = Rc::as_ptr(&value.value);
    let mut delivery = Box::pin(fixture.context.output(&fixture.output, value));
    assert!(poll(delivery.as_mut()).is_pending());
    assert_eq!(fixture.gate.boundary(), None);
    assert_eq!(fixture.calls.get(), 0);
    assert_eq!(drops.get(), 0);

    drop(fixture.store);
    let Poll::Ready(Err(rca_core::OutputError::Returned { value, error })) =
        poll(delivery.as_mut())
    else {
        panic!("capacity-blocked Local output must return its original value on closure");
    };
    assert_eq!(Rc::as_ptr(&value.value), identity);
    assert_eq!(error.reason(), rca_core::TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
    assert_eq!(fixture.gate.boundary(), None);
    assert_eq!(fixture.calls.get(), 0);
    assert_eq!(drops.get(), 0);
    drop(value);
    drop(delivery);
    assert_eq!(drops.get(), 1);

    fixture.driver.turn();
    let Poll::Ready(Err(rca_core::TaskSendError::Rejected { action, error })) = poll(send.as_mut())
    else {
        panic!("queued child action must reject when the actor drains");
    };
    assert_eq!(error.reason(), rca_core::TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
    assert_eq!(Rc::as_ptr(&action.0), action_identity);
    drop(action);
    drop(send);
    assert!(action_weak.upgrade().is_none());
    fixture.driver.turn();
    assert!(fixture.driver.0.borrow().is_empty());
    assert!(weak_capture.upgrade().is_none());
    assert_eq!(fixture.gate.boundary(), None);
    assert_eq!(fixture.calls.get(), 0);
    assert_eq!(drops.get(), 1);
    assert_eq!(Rc::strong_count(&drops), 1);
    assert_eq!(Rc::strong_count(&fixture.calls), 1);
}

#[test]
#[cfg(feature = "test-util")]
fn foreign_local_context_returns_original_output_without_handler_or_frontier() {
    let capture = Rc::new(());
    let weak_capture = Rc::downgrade(&capture);
    let source = gated_with_handler(move |state, value| {
        let _ = &capture;
        state.outcome = Some(value);
        OutputFlow::Consumed
    });
    let foreign = gated();
    let drops = Rc::new(Cell::new(0));
    let value = outcome(&drops, 67);
    let identity = Rc::as_ptr(&value.value);
    let result = foreign
        .driver
        .finish(foreign.context.output(&source.output, value));
    let Err(rca_core::OutputError::Returned { value, error }) = result else {
        panic!("a foreign Local context cannot enter the source declaration");
    };
    assert_eq!(Rc::as_ptr(&value.value), identity);
    assert_eq!(error.reason(), rca_core::TaskAccessReason::ScopeEnded);
    assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
    assert_eq!(source.gate.boundary(), None);
    assert_eq!(foreign.gate.boundary(), None);
    assert_eq!(source.calls.get(), 0);
    assert_eq!(foreign.calls.get(), 0);
    assert_eq!(drops.get(), 0);
    drop(value);
    assert_eq!(drops.get(), 1);
    drop(source.store);
    source.driver.turn();
    drop(foreign.store);
    foreign.driver.turn();
    assert!(source.driver.0.borrow().is_empty());
    assert!(foreign.driver.0.borrow().is_empty());
    assert!(weak_capture.upgrade().is_none());
    assert_eq!(drops.get(), 1);
    assert_eq!(Rc::strong_count(&drops), 1);
    assert_eq!(Rc::strong_count(&source.calls), 1);
    assert_eq!(Rc::strong_count(&foreign.calls), 1);
}

#[test]
#[cfg(feature = "test-util")]
#[expect(
    clippy::too_many_lines,
    reason = "one ownership timeline distinguishes declaration root, emitting scope and successful delivery"
)]
fn local_output_rejects_wrong_declaration_root_and_scope_before_admission() {
    let driver = Driver::default();
    let gate = Gate::new();
    let other_gate = Gate::new();
    let calls = Rc::new(Cell::new(0));
    let other_calls = Rc::new(Cell::new(0));
    let child = Rc::new(Cell::new(7));
    let mount = || {
        OptionalMount::new(
            |state: &RootState| state.child.as_ref(),
            |state: &mut RootState| state.child.as_mut(),
            |state: &RootState| {
                state.child.as_ref().map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            |_| Child,
        )
    };
    let (builder, source) = Store::builder(
        RootState {
            child: Some(Rc::clone(&child)),
            outcome: None,
        },
        Parent,
        Runtime::new(driver.clone()),
    )
    .transaction_gate(gate.clone())
    .mount_with_site(mount());
    // These declarations borrow the same storage but own distinct installed scopes.
    let (builder, sibling) = builder.mount_with_site(mount());
    let observed = Rc::clone(&calls);
    let (builder, output) = builder.output::<Outcome>(&source, |handlers| {
        handlers.at(&source, |_, _, _| {
            panic!("the source is not its own ancestor")
        });
        handlers.at(&sibling, |_, _, _| {
            panic!("a live sibling is not an output ancestor");
        });
        handlers.at_root(move |state, value, _| {
            observed.set(observed.get() + 1);
            state.outcome = Some(value);
            OutputFlow::Consumed
        });
    });
    let (unbuilt_builder, unbuilt_site) = Store::builder(
        RootState {
            child: Some(Rc::clone(&child)),
            outcome: None,
        },
        Parent,
        Runtime::new(driver.clone()),
    )
    .mount_with_site(mount());
    let capture = Rc::new(());
    let weak_capture = Rc::downgrade(&capture);
    let (builder, uninstalled_source) = builder.output::<Outcome>(&unbuilt_site, |handlers| {
        handlers.at_root(move |_, _, _| {
            let _ = &capture;
            panic!("a bound Local output cannot deliver from an unbuilt source");
        });
    });
    let store = builder.build().unwrap();
    let observed = Rc::clone(&other_calls);
    let (builder, wrong_root) = Store::builder(
        RootState {
            child: None,
            outcome: None,
        },
        Parent,
        Runtime::new(driver.clone()),
    )
    .transaction_gate(other_gate.clone())
    .output::<Outcome>(&source, |handlers| {
        handlers.at_root(move |state, value, _| {
            observed.set(observed.get() + 1);
            state.outcome = Some(value);
            OutputFlow::Consumed
        });
    });
    let other = builder.build().unwrap();
    driver.turn();
    for gate in [&gate, &other_gate] {
        let boundary = gate.boundary().unwrap();
        drop(
            driver
                .finish(gate.release_pending_with_receipt().unwrap().finish())
                .unwrap(),
        );
        assert!(gate.acknowledge(boundary));
    }
    driver.finish(store.ready()).unwrap();
    driver.finish(other.ready()).unwrap();
    let context = driver.finish(store.acquire(&source)).unwrap().unwrap();
    let sibling = driver.finish(store.acquire(&sibling)).unwrap().unwrap();
    let drops = Rc::new(Cell::new(0));
    let mut value = outcome(&drops, 83);
    let identity = Rc::as_ptr(&value.value);
    // The last output belongs to the live Store, while its source builder is
    // retained without ever binding or installing that source declaration.
    for (context, output) in [
        (&context, &wrong_root),
        (&sibling, &output),
        (&context, &uninstalled_source),
    ] {
        let mut delivery = Box::pin(context.output(output, value));
        let Poll::Ready(Err(rca_core::OutputError::Returned {
            value: returned,
            error,
        })) = poll(delivery.as_mut())
        else {
            panic!("a wrong declaration root or source scope rejects before publication");
        };
        assert_eq!(error.reason(), rca_core::TaskAccessReason::ScopeEnded);
        assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
        assert_eq!(Rc::as_ptr(&returned.value), identity);
        assert_eq!(returned.value.get(), 83);
        assert_eq!((calls.get(), other_calls.get(), drops.get()), (0, 0, 0));
        assert!(weak_capture.upgrade().is_some());
        for gate in [&gate, &other_gate] {
            assert_eq!(gate.pending_kind(), None);
            assert_eq!(gate.boundary(), None);
        }
        assert!(
            driver
                .finish(store.read(|state| state.outcome.is_none()))
                .unwrap()
        );
        assert!(
            driver
                .finish(other.read(|state| state.outcome.is_none()))
                .unwrap()
        );
        assert_eq!(child.get(), 7);
        value = returned;
    }
    let mut delivery = Box::pin(context.output(&output, value));
    assert!(poll(delivery.as_mut()).is_pending());
    driver.turn();
    let boundary = gate.boundary().unwrap();
    assert_eq!(
        gate.pending_kind(),
        Some(rca_core::testing::TransactionKind::Output)
    );
    drop(
        driver
            .finish(gate.release_pending_with_receipt().unwrap().finish())
            .unwrap(),
    );
    let Ok(OutputDelivery::Consumed { task }) = driver.finish(delivery) else {
        panic!("the correct source consumes the same returned allocation");
    };
    assert!(gate.acknowledge(boundary));
    driver.finish(task.finish());
    assert_eq!((calls.get(), other_calls.get(), drops.get()), (1, 0, 0));
    assert_eq!(
        driver
            .finish(store.read(|state| {
                let value = state.outcome.as_ref().unwrap();
                (Rc::as_ptr(&value.value), value.value.get())
            }))
            .unwrap(),
        (identity, 83)
    );
    assert_eq!(gate.boundary(), None);
    assert_eq!(other_gate.boundary(), None);
    drop((store, other));
    driver.turn();
    assert!(driver.0.borrow().is_empty());
    assert!(weak_capture.upgrade().is_none());
    drop(unbuilt_builder);
    assert_eq!(drops.get(), 1);
    assert_eq!(Rc::strong_count(&calls), 1);
    assert_eq!(Rc::strong_count(&other_calls), 1);
    assert_eq!(Rc::strong_count(&child), 1);
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one two-store retirement timeline checks weak registration, retained capture and exact value disposal"
)]
fn local_retired_handler_registration_is_skipped_without_releasing_its_capture() {
    let driver = Driver::default();
    let mount = || {
        OptionalMount::new(
            |state: &RootState| state.child.as_ref(),
            |state: &mut RootState| state.child.as_mut(),
            |state: &RootState| {
                state.child.as_ref().map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            |_| Child,
        )
    };
    let retired_child = Rc::new(Cell::new(11));
    let (builder, retired_site) = Store::builder(
        RootState {
            child: Some(Rc::clone(&retired_child)),
            outcome: None,
        },
        Parent,
        Runtime::new(driver.clone()),
    )
    .mount_with_site(mount());
    let retired = builder.build().unwrap();
    driver.finish(retired.ready()).unwrap();
    let retired_context = driver
        .finish(retired.acquire(&retired_site))
        .unwrap()
        .unwrap();
    assert_eq!(
        driver
            .finish(retired_context.read(|value| value.get()))
            .unwrap(),
        11
    );
    drop(retired_context);

    let (builder, source) = Store::builder(
        RootState {
            child: Some(Rc::new(Cell::new(7))),
            outcome: None,
        },
        Parent,
        Runtime::new(driver.clone()),
    )
    .mount_with_site(mount());
    let capture = Rc::new(());
    let weak_capture = Rc::downgrade(&capture);
    let stale_calls = Rc::new(Cell::new(0));
    let root_calls = Rc::new(Cell::new(0));
    let stale = Rc::clone(&stale_calls);
    let root = Rc::clone(&root_calls);
    let (builder, output) = builder.output::<Outcome>(&source, |handlers| {
        handlers.at(&retired_site, move |_, _, _| {
            let _ = &capture;
            stale.set(stale.get() + 1);
            panic!("an expired handler registration cannot receive output");
        });
        handlers.at_root(move |state, value, _| {
            root.set(root.get() + 1);
            state.outcome = Some(value);
            OutputFlow::Consumed
        });
    });
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    let context = driver.finish(store.acquire(&source)).unwrap().unwrap();
    drop(retired);
    driver.turn();
    assert_eq!(Rc::strong_count(&retired_child), 1);
    assert!(weak_capture.upgrade().is_some());
    assert_eq!((stale_calls.get(), root_calls.get()), (0, 0));

    let drops = Rc::new(Cell::new(0));
    let value = outcome(&drops, 89);
    let identity = Rc::as_ptr(&value.value);
    let Ok(OutputDelivery::Consumed { task }) = driver.finish(context.output(&output, value))
    else {
        panic!("a stale declared handler does not prevent root delivery");
    };
    driver.finish(task.finish());
    assert_eq!(
        (stale_calls.get(), root_calls.get(), drops.get()),
        (0, 1, 0)
    );
    assert!(weak_capture.upgrade().is_some());
    assert_eq!(
        driver
            .finish(store.read(|state| {
                let value = state.outcome.as_ref().unwrap();
                (
                    Rc::as_ptr(&value.value),
                    value.value.get(),
                    state.child.as_ref().unwrap().get(),
                )
            }))
            .unwrap(),
        (identity, 89, 7)
    );
    drop(store);
    driver.turn();
    assert!(driver.0.borrow().is_empty());
    assert!(weak_capture.upgrade().is_none());
    assert_eq!(
        (stale_calls.get(), root_calls.get(), drops.get()),
        (0, 1, 1)
    );
    assert_eq!(Rc::strong_count(&stale_calls), 1);
    assert_eq!(Rc::strong_count(&root_calls), 1);
    drop((output, retired_site));
    assert_eq!(drops.get(), 1);
}

#[test]
#[cfg(feature = "test-util")]
fn local_matcher_mismatch_and_panic_restore_original_rc_before_retry() {
    let fixture = gated();
    let drops = Rc::new(Cell::new(0));
    let value = outcome(&drops, 17);
    let identity = Rc::as_ptr(&value.value);
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
            .inspect_pending(|_| panic!("a Local output must not enter an action inspector")),
        None::<()>
    );
    assert_eq!(fixture.gate.boundary(), Some(boundary));
    assert!(fixture.gate.has_pending());
    assert_eq!(fixture.calls.get(), 0);
    assert_eq!(drops.get(), 0);
    assert_eq!(
        fixture
            .gate
            .inspect_output(&fixture.other, |_| panic!("wrong declaration")),
        None::<()>
    );
    assert_eq!(
        fixture
            .gate
            .inspect_output(&fixture.output, |value| value.value.get() == 99),
        Some(false)
    );
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        fixture.gate.inspect_output(&fixture.output, |value| {
            assert_eq!(Rc::as_ptr(&value.value), identity);
            assert_eq!(
                fixture.gate.inspect_output(&fixture.output, |_| panic!(
                    "recursive inspection cannot borrow the pinned value"
                )),
                None::<()>
            );
            assert_eq!(fixture.gate.output_origin(&fixture.output), None);
            panic!("local matcher unwind");
        });
    }));
    assert_eq!(
        panic.unwrap_err().downcast_ref::<&str>(),
        Some(&"local matcher unwind")
    );
    assert_eq!(fixture.gate.boundary(), Some(boundary));
    assert_eq!(
        fixture
            .gate
            .inspect_output(&fixture.output, |value| Rc::as_ptr(&value.value)),
        Some(identity)
    );
    assert_eq!(drops.get(), 0);
    assert_eq!(fixture.calls.get(), 0);
    let receipt = fixture.gate.release_pending_with_receipt().unwrap();
    drop(fixture.driver.finish(receipt.finish()).unwrap());
    assert!(matches!(
        fixture.driver.finish(delivery),
        Ok(OutputDelivery::Consumed { .. })
    ));
    assert!(fixture.gate.acknowledge(boundary));
    assert_eq!(fixture.calls.get(), 1);
    drop(fixture.store);
    fixture.driver.turn();
    assert_eq!(drops.get(), 1);
}

#[test]
#[cfg(feature = "test-util")]
fn local_shutdown_inside_matcher_restores_rc_and_reserves_attachment() {
    let fixture = gated();
    let drops = Rc::new(Cell::new(0));
    let value = outcome(&drops, 23);
    let identity = Rc::as_ptr(&value.value);
    let mut delivery = Box::pin(fixture.context.output(&fixture.output, value));
    assert!(poll(delivery.as_mut()).is_pending());
    fixture.driver.turn();
    let boundary = fixture.gate.boundary().unwrap();
    let terminal_boundary = fixture.gate.pending_boundary().unwrap();
    fixture
        .gate
        .inspect_output(&fixture.output, |value| {
            drop(fixture.store);
            fixture.driver.turn();
            assert_eq!(Rc::as_ptr(&value.value), identity);
            assert_eq!(drops.get(), 0);
            assert_eq!(fixture.gate.boundary(), Some(boundary));
            Store::builder(
                RootState {
                    child: None,
                    outcome: None,
                },
                Parent,
                Runtime::new(fixture.driver.clone()),
            )
            .transaction_gate(fixture.gate.clone())
            .build()
            .expect_err("the retiring gate cannot bind another store");
        })
        .unwrap();
    let Err(rca_core::OutputError::Returned { value, error }) = fixture.driver.finish(delivery)
    else {
        panic!("never-entered local output must return its original value");
    };
    assert_eq!(Rc::as_ptr(&value.value), identity);
    assert_eq!(error.reason(), rca_core::TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
    assert_eq!(fixture.calls.get(), 0);
    assert_eq!(drops.get(), 0);
    drop(value);
    assert_eq!(drops.get(), 1);
    fixture.driver.turn();
    assert_eq!(fixture.gate.boundary(), Some(boundary));
    assert_eq!(
        fixture
            .gate
            .match_terminal(&terminal_boundary, |terminal| *terminal == error),
        Some(true)
    );
    assert!(fixture.gate.acknowledge_terminal(&terminal_boundary));
    assert_eq!(fixture.gate.boundary(), None);
    let replacement = Store::builder(
        RootState {
            child: None,
            outcome: None,
        },
        Parent,
        Runtime::new(fixture.driver.clone()),
    )
    .transaction_gate(fixture.gate.clone())
    .build()
    .unwrap();
    drop(replacement);
    fixture.driver.turn();
}

#[test]
#[cfg(feature = "test-util")]
fn local_host_teardown_inside_matcher_rejects_output_without_losing_inspected_value() {
    let capture = Rc::new(());
    let lifetime = Rc::downgrade(&capture);
    let fixture = gated_with_handler(move |state, value| {
        let _ = &capture;
        state.outcome = Some(value);
        OutputFlow::Consumed
    });
    let drops = Rc::new(Cell::new(0));
    let value = outcome(&drops, 23);
    let identity = Rc::as_ptr(&value.value);
    let mut delivery = Box::pin(fixture.context.output(&fixture.output, value));
    assert!(poll(delivery.as_mut()).is_pending());
    fixture.driver.turn();
    let terminal_boundary = fixture.gate.pending_boundary().unwrap();
    let nested_drops = Rc::new(Cell::new(0));
    assert!(lifetime.upgrade().is_some());
    fixture
        .gate
        .inspect_output(&fixture.output, |value| {
            drop(fixture.store);
            // Host teardown drops accepted tasks rather than polling the actor.
            // Inspection owns the pending request and value, not the handlers.
            let tasks = std::mem::take(&mut *fixture.driver.0.borrow_mut());
            assert!(!tasks.is_empty());
            drop(tasks);
            assert!(fixture.driver.0.borrow().is_empty());
            assert!(lifetime.upgrade().is_none());
            assert_eq!(Rc::as_ptr(&value.value), identity);
            assert_eq!(drops.get(), 0);

            let nested_value = outcome(&nested_drops, 29);
            let nested_identity = Rc::as_ptr(&nested_value.value);
            let mut nested = Box::pin(fixture.context.output(&fixture.output, nested_value));
            let Poll::Ready(Err(rca_core::OutputError::Returned { value, error })) =
                poll(nested.as_mut())
            else {
                panic!("the expired declaration must reject after its host is gone");
            };
            assert_eq!(error.reason(), rca_core::TaskAccessReason::StoreClosed);
            assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
            assert_eq!(Rc::as_ptr(&value.value), nested_identity);
            assert_eq!(value.value.get(), 29);
            assert_eq!(fixture.calls.get(), 0);
            assert_eq!(nested_drops.get(), 0);
            drop(value);
            drop(nested);
            assert_eq!(nested_drops.get(), 1);
            assert_eq!(drops.get(), 0);
        })
        .unwrap();

    let Poll::Ready(Err(rca_core::OutputError::Returned { value, error })) =
        poll(delivery.as_mut())
    else {
        panic!("inspection retirement must return the restored original output");
    };
    assert_eq!(error.reason(), rca_core::TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
    assert_eq!(Rc::as_ptr(&value.value), identity);
    assert_eq!(value.value.get(), 23);
    assert_eq!(fixture.calls.get(), 0);
    assert_eq!(drops.get(), 0);
    assert!(lifetime.upgrade().is_none());
    assert_eq!(
        fixture
            .gate
            .match_terminal(&terminal_boundary, |terminal| *terminal == error),
        Some(true)
    );
    assert!(fixture.gate.acknowledge_terminal(&terminal_boundary));
    assert_eq!(fixture.gate.boundary(), None);
    drop(value);
    drop(delivery);
    assert_eq!(drops.get(), 1);
    assert_eq!(Rc::strong_count(&drops), 1);
    assert_eq!(Rc::strong_count(&nested_drops), 1);
    assert_eq!(Rc::strong_count(&fixture.calls), 1);
    assert!(fixture.driver.0.borrow().is_empty());
}

#[test]
#[cfg(feature = "test-util")]
fn local_queued_stale_output_returns_original_rc_without_publishing_frontier() {
    let fixture = gated();
    let mut remove = Box::pin(fixture.store.send(RootAction(Rc::new(()))));
    assert!(poll(remove.as_mut()).is_pending());
    fixture.driver.turn();
    assert_eq!(
        fixture.gate.pending_kind(),
        Some(rca_core::testing::TransactionKind::Action)
    );
    let boundary = fixture.gate.boundary().unwrap();
    let drops = Rc::new(Cell::new(0));
    let value = outcome(&drops, 29);
    let identity = Rc::as_ptr(&value.value);
    let mut delivery = Box::pin(fixture.context.output(&fixture.output, value));
    assert!(poll(delivery.as_mut()).is_pending());
    let receipt = fixture.gate.release_pending_with_receipt().unwrap();
    drop(fixture.driver.finish(receipt.finish()).unwrap());
    drop(fixture.driver.finish(remove.as_mut()).unwrap());
    drop(remove);
    assert!(fixture.gate.acknowledge(boundary));
    let Err(rca_core::OutputError::Returned { value, error }) = fixture.driver.finish(delivery)
    else {
        panic!("retired local source returns original input");
    };
    assert_eq!(Rc::as_ptr(&value.value), identity);
    assert_eq!(error.reason(), rca_core::TaskAccessReason::ScopeEnded);
    assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
    assert_eq!(fixture.gate.pending_kind(), None);
    assert_eq!(fixture.calls.get(), 0);
    assert_eq!(drops.get(), 0);
    drop(value);
    assert_eq!(drops.get(), 1);
    drop(fixture.store);
    fixture.driver.turn();
}

#[test]
#[cfg(feature = "test-util")]
fn local_consuming_handler_panic_drops_rc_once_and_reports_may_have_run() {
    let fixture = gated_with_handler(|_, _value| panic!("local handler panic"));
    let drops = Rc::new(Cell::new(0));
    let mut delivery = Box::pin(fixture.context.output(&fixture.output, outcome(&drops, 31)));
    assert!(poll(delivery.as_mut()).is_pending());
    fixture.driver.turn();
    let receipt = fixture.gate.release_pending_with_receipt().unwrap();
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fixture.driver.turn()));
    assert_eq!(
        panic.unwrap_err().downcast_ref::<&str>(),
        Some(&"local handler panic")
    );
    let Err(rca_core::OutputError::Failed { error }) = fixture.driver.finish(delivery) else {
        panic!("entered local handler cannot recover its consumed value");
    };
    assert_eq!(error.reason(), rca_core::TaskAccessReason::ActorFailed);
    assert_eq!(error.progress(), rca_core::TaskProgress::MayHaveRun);
    fixture
        .driver
        .finish(receipt.finish())
        .expect_err("the failed handler leaves a terminal receipt");
    assert_eq!(drops.get(), 1);
}

#[test]
fn local_reaction_unwind_returns_continued_output_with_original_allocation() {
    let driver = Driver::default();
    let handlers = Rc::new(Cell::new(0));
    let reactions = Rc::new(Cell::new(0));
    let drops = Rc::new(Cell::new(0));
    let capture = Rc::new(());
    let weak_capture = Rc::downgrade(&capture);
    let reacted = Rc::clone(&reactions);
    let pending_value = Rc::clone(&drops);
    let root = rca_core::local::RootMount::new(Parent).on_change(
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
            reacted.set(reacted.get() + 1);
            assert_eq!(pending_value.get(), 0);
            panic!("Local reaction after the handler returned its unhandled value");
        },
    );
    let mount = OptionalMount::new(
        |state: &RootState| state.child.as_ref(),
        |state: &mut RootState| state.child.as_mut(),
        |state: &RootState| {
            state.child.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        |_| Child,
    );
    let (builder, site) = root
        .builder(
            RootState {
                child: Some(Rc::new(Cell::new(7))),
                outcome: None,
            },
            Runtime::new(driver.clone()),
        )
        .mount_with_site(mount);
    let handled = Rc::clone(&handlers);
    let (builder, output) = builder.output::<Outcome>(&site, |handlers| {
        handlers.at_root(move |state, value, _| {
            handled.set(handled.get() + 1);
            state.child = None;
            OutputFlow::Continue(value)
        });
    });
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    let context = driver.finish(store.acquire(&site)).unwrap().unwrap();
    let value = outcome(&drops, 79);
    let identity = Rc::as_ptr(&value.value);
    let lifetime = Rc::downgrade(&value.value);
    let mut delivery = Box::pin(context.output(&output, value));
    assert!(poll(delivery.as_mut()).is_pending());
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| driver.turn()));
    assert_eq!(
        panic.unwrap_err().downcast_ref::<&str>(),
        Some(&"Local reaction after the handler returned its unhandled value")
    );
    let Poll::Ready(Err(rca_core::OutputError::Returned { value, error })) =
        poll(delivery.as_mut())
    else {
        panic!("post-handler unwind returns the retained Local output");
    };
    assert_eq!(error.reason(), rca_core::TaskAccessReason::ActorFailed);
    assert_eq!(error.progress(), rca_core::TaskProgress::MayHaveRun);
    assert_eq!(Rc::as_ptr(&value.value), identity);
    assert_eq!(value.value.get(), 79);
    assert_eq!(drops.get(), 0);
    assert_eq!((handlers.get(), reactions.get()), (1, 1));
    assert!(weak_capture.upgrade().is_none());
    assert!(poll(Box::pin(store.closed()).as_mut()).is_ready());
    driver.turn();
    assert!(driver.0.borrow().is_empty());
    drop(delivery);
    drop(context);
    drop(store);
    driver.turn();
    assert_eq!(drops.get(), 0);
    drop(value);
    assert_eq!(drops.get(), 1);
    assert!(lifetime.upgrade().is_none());
    assert_eq!((handlers.get(), reactions.get()), (1, 1));
}

#[cfg(feature = "test-util")]
/// Separates parent replacement from child retirement and retained root output.
struct NestedRoot {
    parent: Option<NestedParent>,
    epoch: u64,
    outcome: Option<Outcome>,
}

#[cfg(feature = "test-util")]
/// Holds the optional child whose lifetime can end without replacing its parent.
struct NestedParent(Option<Rc<Cell<usize>>>);

#[cfg(feature = "test-util")]
/// Replaces the parent and advances its identity on each root action.
struct NestedRootFeature;

#[cfg(feature = "test-util")]
/// Preserves nested state while output handlers control the test schedule.
struct NestedParentFeature;

#[cfg(feature = "test-util")]
impl Feature for NestedRootFeature {
    type State = NestedRoot;
    type Action = RootAction;
    fn update(
        &mut self,
        state: &mut NestedRoot,
        _: &RootAction,
        _: &mut FeatureContext<'_, NestedRoot, RootAction>,
    ) {
        state.epoch += 1;
        state.parent = Some(NestedParent(Some(Rc::new(Cell::new(9)))));
    }
}

#[cfg(feature = "test-util")]
impl Feature for NestedParentFeature {
    type State = NestedParent;
    type Action = u8;
    fn update(
        &mut self,
        _: &mut NestedParent,
        _: &u8,
        _: &mut FeatureContext<'_, NestedParent, u8>,
    ) {
    }
}

#[cfg(feature = "test-util")]
/// Retires the child while keeping its ancestor context valid.
struct RetireNestedChild;

#[cfg(feature = "test-util")]
impl Feature for RetireNestedChild {
    type State = NestedRoot;
    type Action = RootAction;
    fn update(
        &mut self,
        state: &mut NestedRoot,
        _: &RootAction,
        _: &mut FeatureContext<'_, NestedRoot, RootAction>,
    ) {
        state.parent.as_mut().expect("live parent").0 = None;
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one queued owner-retirement schedule preserves the live ancestor and original payload"
)]
#[cfg(feature = "test-util")]
fn local_queued_output_returns_original_when_only_its_child_owner_retires() {
    let driver = Driver::default();
    let gate = rca_core::testing::local::TransactionGate::<NestedRoot, RootAction>::new();
    let outputs = Rc::new(RefCell::new(Vec::new()));
    let captures = Rc::new(RefCell::new(Vec::new()));
    let calls = Rc::new(Cell::new(0));
    let recipe_outputs = Rc::clone(&outputs);
    let recipe_captures = Rc::clone(&captures);
    let recipe_calls = Rc::clone(&calls);
    let (builder, parent_site) = Store::builder(
        NestedRoot {
            parent: Some(NestedParent(Some(Rc::new(Cell::new(7))))),
            epoch: 0,
            outcome: None,
        },
        RetireNestedChild,
        Runtime::new(driver.clone()),
    )
    .transaction_gate(gate.clone())
    .mount_with_children(
        OptionalMount::new(
            |state: &NestedRoot| state.parent.as_ref(),
            |state: &mut NestedRoot| state.parent.as_mut(),
            |state: &NestedRoot| {
                state.parent.as_ref().map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            |_| NestedParentFeature,
        ),
        move |_, children| {
            let parent = children.site().clone();
            let outputs = Rc::clone(&recipe_outputs);
            let captures = Rc::clone(&recipe_captures);
            let calls = Rc::clone(&recipe_calls);
            children.optional_with_children(
                OptionalMount::new(
                    |state: &NestedParent| state.0.as_ref(),
                    |state: &mut NestedParent| state.0.as_mut(),
                    |state: &NestedParent| {
                        state.0.as_ref().map(|_| MountIdentity {
                            key: (),
                            replacement: 0,
                            configuration: 0,
                        })
                    },
                    |_| Child,
                ),
                move |_, grandchildren| {
                    let capture = Rc::new(());
                    captures.borrow_mut().push(Rc::downgrade(&capture));
                    let calls = Rc::clone(&calls);
                    let output = grandchildren.output::<Outcome>(&parent, |handlers| {
                        handlers.at_root(move |_, value, _| {
                            let _ = &capture;
                            calls.set(calls.get() + 1);
                            drop(value);
                            OutputFlow::Consumed
                        });
                    });
                    outputs.borrow_mut().push(output);
                },
            );
        },
    );
    let store = builder.build().unwrap();
    driver.turn();
    let boundary = gate.boundary().unwrap();
    drop(
        driver
            .finish(gate.release_pending_with_receipt().unwrap().finish())
            .unwrap(),
    );
    assert!(gate.acknowledge(boundary));
    driver.finish(store.ready()).unwrap();
    let parent = driver.finish(store.acquire(&parent_site)).unwrap().unwrap();
    let output = outputs.borrow()[0].clone();
    assert!(captures.borrow()[0].upgrade().is_some());

    let mut retire = Box::pin(store.send(RootAction(Rc::new(()))));
    assert!(poll(retire.as_mut()).is_pending());
    driver.turn();
    assert_eq!(
        gate.pending_kind(),
        Some(rca_core::testing::TransactionKind::Action)
    );
    let boundary = gate.boundary().unwrap();
    let drops = Rc::new(Cell::new(0));
    let value = outcome(&drops, 47);
    let identity = Rc::as_ptr(&value.value);
    let mut delivery = Box::pin(parent.output(&output, value));
    assert!(poll(delivery.as_mut()).is_pending());
    drop(
        driver
            .finish(gate.release_pending_with_receipt().unwrap().finish())
            .unwrap(),
    );
    drop(driver.finish(retire.as_mut()).unwrap());
    drop(retire);
    assert!(gate.acknowledge(boundary));
    assert!(captures.borrow()[0].upgrade().is_none());
    driver.turn();
    assert_eq!(
        gate.pending_kind(),
        Some(rca_core::testing::TransactionKind::Output)
    );
    let output_boundary = gate.pending_boundary().unwrap();
    assert_eq!(
        gate.inspect_output(&output, |_| panic!(
            "retired declaration cannot expose its value"
        )),
        None::<()>
    );
    assert_eq!(gate.output_origin(&output), None);
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
    assert_eq!(Rc::as_ptr(&value.value), identity);
    assert_eq!(value.value.get(), 47);
    assert_eq!(
        (error.reason(), error.progress()),
        (
            rca_core::TaskAccessReason::ScopeEnded,
            rca_core::TaskProgress::NotStarted,
        )
    );
    assert_eq!(calls.get(), 0);
    assert_eq!(drops.get(), 0);
    assert_eq!(
        gate.match_terminal(&output_boundary, |terminal| *terminal == error),
        Some(true)
    );
    assert!(gate.acknowledge_terminal(&output_boundary));
    assert!(
        driver
            .finish(parent.read(|state| state.0.is_none()))
            .unwrap()
    );
    assert_eq!(gate.pending_kind(), None);
    drop(value);
    assert_eq!(drops.get(), 1);
    drop(store);
    driver.turn();
    assert!(driver.0.borrow().is_empty());
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one owned output schedule keeps ancestor removal, work disposal and terminal debt together"
)]
#[cfg(feature = "test-util")]
fn local_continuing_handler_removes_later_ancestor_route_and_returns_original_output() {
    let driver = Driver::default();
    let gate = rca_core::testing::local::TransactionGate::<NestedRoot, RootAction>::new();
    let trace = Rc::new(RefCell::new(Vec::new()));
    let operations = Rc::new(Cell::new(0));
    let mut capture = Some(Rc::new(()));
    let capture_weak = Rc::downgrade(capture.as_ref().unwrap());
    let descriptor = OptionalMount::new(
        |state: &NestedRoot| {
            state
                .parent
                .as_ref()
                .filter(|parent| parent.0.as_ref().is_some_and(|child| child.get() > 0))
        },
        |state: &mut NestedRoot| {
            state
                .parent
                .as_mut()
                .filter(|parent| parent.0.as_ref().is_some_and(|child| child.get() > 0))
        },
        |state: &NestedRoot| {
            state.parent.as_ref().and_then(|parent| {
                parent
                    .0
                    .as_ref()
                    .filter(|child| child.get() > 0)
                    .map(|_| MountIdentity {
                        key: (),
                        replacement: state.epoch,
                        configuration: 0,
                    })
            })
        },
        |_| NestedParentFeature,
    );
    let sources = Rc::new(RefCell::new(Vec::new()));
    let outputs = Rc::new(RefCell::new(Vec::new()));
    let recipe_sources = Rc::clone(&sources);
    let recipe_outputs = Rc::clone(&outputs);
    let callbacks = Rc::clone(&trace);
    let operation_calls = Rc::clone(&operations);
    let (builder, _parent) = Store::builder(
        NestedRoot {
            parent: Some(NestedParent(Some(Rc::new(Cell::new(7))))),
            epoch: 0,
            outcome: None,
        },
        NestedRootFeature,
        Runtime::new(driver.clone()),
    )
    .transaction_gate(gate.clone())
    .mount_with_children(descriptor, move |_, children| {
        let parent = children.site().clone();
        let source = children.optional(OptionalMount::new(
            |state: &NestedParent| state.0.as_ref(),
            |state: &mut NestedParent| state.0.as_mut(),
            |state: &NestedParent| {
                state.0.as_ref().map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            |_| Child,
        ));
        let first = Rc::clone(&callbacks);
        let later = Rc::clone(&callbacks);
        let root = Rc::clone(&callbacks);
        let calls = Rc::clone(&operation_calls);
        let mut capture = capture.take();
        let output = children.output::<Outcome>(&source, |handlers| {
            handlers.at(&parent, move |state, value, context| {
                first.borrow_mut().push("remove parent");
                state.0.as_ref().unwrap().set(0);
                let capture = capture.take().expect("one entered output");
                let calls = Rc::clone(&calls);
                context.effect(rca_core::local::Effect::run(move |_| {
                    calls.set(calls.get() + 1);
                    async move {
                        drop(capture);
                    }
                }));
                OutputFlow::Continue(value)
            });
            handlers.at(&parent, move |_, value, _| {
                later.borrow_mut().push("later parent");
                OutputFlow::Continue(value)
            });
            handlers.at_root(move |state, value, _| {
                root.borrow_mut().push("root");
                state.outcome = Some(value);
                OutputFlow::Consumed
            });
        });
        recipe_sources.borrow_mut().push(source);
        recipe_outputs.borrow_mut().push(output);
    });
    let store = builder.build().unwrap();
    driver.turn();
    let initial = gate.boundary().unwrap();
    drop(
        driver
            .finish(gate.release_pending_with_receipt().unwrap().finish())
            .unwrap(),
    );
    assert!(gate.acknowledge(initial));
    driver.finish(store.ready()).unwrap();
    let context = driver
        .finish(store.acquire(&sources.borrow()[0]))
        .unwrap()
        .unwrap();
    let drops = Rc::new(Cell::new(0));
    let value = outcome(&drops, 53);
    let identity = Rc::as_ptr(&value.value);
    let mut delivery = Box::pin(context.output(&outputs.borrow()[0], value));
    assert!(poll(delivery.as_mut()).is_pending());
    driver.turn();
    let boundary = gate.pending_boundary().unwrap();
    assert_eq!(
        gate.pending_kind(),
        Some(rca_core::testing::TransactionKind::Output)
    );
    assert!(trace.borrow().is_empty());
    let receipt = gate.release_pending_with_receipt().unwrap();
    driver
        .finish(receipt.finish())
        .expect_err("the entered output has a terminal route error");
    let Err(rca_core::OutputError::Returned { value, error }) = driver.finish(delivery) else {
        panic!("the continued output must retain its original owner");
    };
    assert_eq!(error.reason(), rca_core::TaskAccessReason::ScopeEnded);
    assert_eq!(error.progress(), rca_core::TaskProgress::MayHaveRun);
    assert_eq!(Rc::as_ptr(&value.value), identity);
    assert_eq!(value.value.get(), 53);
    assert_eq!(*trace.borrow(), ["remove parent"]);
    assert_eq!(operations.get(), 0);
    assert!(capture_weak.upgrade().is_none());
    assert_eq!(drops.get(), 0);
    assert_eq!(
        gate.match_terminal(&boundary, |terminal| *terminal == error),
        Some(true)
    );
    assert!(!gate.has_pending());
    assert!(gate.boundary().is_some());
    assert!(gate.acknowledge_terminal(&boundary));
    assert_eq!(gate.boundary(), None);
    drop(value);
    assert_eq!(drops.get(), 1);
    drop(store);
    driver.turn();
    assert_eq!(drops.get(), 1);
    assert_eq!(operations.get(), 0);
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One Local replacement sequence preserves capture draining before successor installation"
)]
#[cfg(feature = "test-util")]
fn local_parent_replacement_drains_captures_before_successor_with_weak_outputs_retained() {
    let driver = Driver::default();
    let gate = rca_core::testing::local::TransactionGate::<NestedRoot, RootAction>::new();
    let captures = Rc::new(RefCell::new(Vec::<std::rc::Weak<()>>::new()));
    let checked = std::rc::Rc::clone(&captures);
    let descriptor = OptionalMount::new(
        |state: &NestedRoot| state.parent.as_ref(),
        |state: &mut NestedRoot| state.parent.as_mut(),
        |state| {
            state.parent.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: state.epoch,
                configuration: 0,
            })
        },
        move |_| {
            assert!(
                checked
                    .borrow()
                    .iter()
                    .all(|capture| capture.upgrade().is_none()),
                "retired handler captures must drop before the successor constructor"
            );
            NestedParentFeature
        },
    );
    let sources = Rc::new(RefCell::new(Vec::new()));
    let outputs = Rc::new(RefCell::new(Vec::new()));
    let recipe_sources = std::rc::Rc::clone(&sources);
    let recipe_outputs = std::rc::Rc::clone(&outputs);
    let recipe_captures = std::rc::Rc::clone(&captures);
    let (builder, _parent) = Store::builder(
        NestedRoot {
            parent: Some(NestedParent(Some(Rc::new(Cell::new(7))))),
            epoch: 0,
            outcome: None,
        },
        NestedRootFeature,
        Runtime::new(driver.clone()),
    )
    .transaction_gate(gate.clone())
    .mailbox_capacity(1)
    .mount_with_children(descriptor, move |_, children| {
        let parent = children.site().clone();
        let source = children.optional(OptionalMount::new(
            |state: &NestedParent| state.0.as_ref(),
            |state: &mut NestedParent| state.0.as_mut(),
            |state| {
                state.0.as_ref().map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            |_| Child,
        ));
        let capture = Rc::new(());
        recipe_captures.borrow_mut().push(Rc::downgrade(&capture));
        let output = children.output::<Outcome>(&source, |handlers| {
            handlers.at(&parent, move |_, value, _| {
                let _ = &capture;
                OutputFlow::Continue(value)
            });
            handlers.at_root(|state, value, _| {
                state.outcome = Some(value);
                OutputFlow::Consumed
            });
        });
        recipe_sources.borrow_mut().push(source);
        recipe_outputs.borrow_mut().push(output);
    });
    let store = builder.build().unwrap();
    driver.turn();
    let boundary = gate.boundary().unwrap();
    drop(
        driver
            .finish(gate.release_pending_with_receipt().unwrap().finish())
            .unwrap(),
    );
    assert!(gate.acknowledge(boundary));
    driver.finish(store.ready()).unwrap();
    let source = sources.borrow()[0].clone();
    let output = outputs.borrow()[0].clone();
    let context = driver.finish(store.acquire(&source)).unwrap().unwrap();
    let mut replace = Box::pin(store.send(RootAction(Rc::new(()))));
    assert!(poll(replace.as_mut()).is_pending());
    driver.turn();
    let boundary = gate.boundary().unwrap();
    assert_eq!(
        gate.pending_kind(),
        Some(rca_core::testing::TransactionKind::Action)
    );
    let drops = Rc::new(Cell::new(0));
    let value = outcome(&drops, 47);
    let identity = Rc::as_ptr(&value.value);
    let mut delivery = Box::pin(context.output(&output, value));
    assert!(poll(delivery.as_mut()).is_pending());
    drop(
        driver
            .finish(gate.release_pending_with_receipt().unwrap().finish())
            .unwrap(),
    );
    drop(driver.finish(replace.as_mut()).unwrap());
    drop(replace);
    assert!(gate.acknowledge(boundary));
    let Err(rca_core::OutputError::Returned { value, error }) = driver.finish(delivery) else {
        panic!("retired nested output must return the original input");
    };
    assert_eq!(Rc::as_ptr(&value.value), identity);
    assert_eq!(error.reason(), rca_core::TaskAccessReason::ScopeEnded);
    assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
    assert_eq!(gate.pending_kind(), None);
    assert_eq!(outputs.borrow().len(), 2);
    assert!(captures.borrow()[0].upgrade().is_none());
    assert!(captures.borrow()[1].upgrade().is_some());
    assert_eq!(drops.get(), 0);
    drop(value);
    assert_eq!(drops.get(), 1);
    drop(store);
    driver.turn();
    assert!(
        captures
            .borrow()
            .iter()
            .all(|capture| capture.upgrade().is_none())
    );
    assert_eq!(
        outputs.borrow().len(),
        2,
        "weak outputs remain retained through teardown"
    );
    drop(output);
}

#[cfg(all(feature = "allocation-measurement", feature = "test-util"))]
#[test]
fn local_output_phase_costs() {
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
    if allocation_test::run_in_child("local_output_phase_costs") {
        return;
    }

    let profiler = dhat::Profiler::builder().testing().build();
    let fixture = gated();
    let drops = Rc::new(Cell::new(0));
    let (handle, clone_cost) = measure(|| fixture.output.clone());
    assert_eq!(clone_cost, (0, 0));
    let mut samples = Vec::new();
    for value in [20, 21] {
        let value = outcome(&drops, value);
        let (delivery, creation) = measure(|| fixture.context.output(&handle, value));
        assert_eq!(creation, (0, 0));
        let inline = std::mem::size_of_val(&delivery);
        let mut delivery = Box::pin(delivery); // Fixture storage, excluded from admission.
        let (pending, admission) = measure(|| poll(delivery.as_mut()));
        assert!(pending.is_pending());
        // Driver Vec take/requeue is included; these are not production-only costs.
        let ((), publication) = measure(|| fixture.driver.turn());
        let boundary = fixture.gate.boundary().unwrap();
        let (matched, inspection) = measure(|| fixture.gate.inspect_output(&handle, |_| true));
        assert_eq!(matched, Some(true));
        assert_eq!(inspection, (0, 0)); // Typed inspection reuses its reservation.
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
        "local output blocks/requested bytes: clone={clone_cost:?}; cold/warm (future creation,inline bytes,first-poll admission,driver publication,inspection,release,driver commit,completion)={samples:?}; weak handle bytes={}",
        std::mem::size_of::<rca_core::local::Output<Outcome>>()
    );
    drop(fixture.store);
    fixture.driver.turn();
    drop(handle);
    assert_eq!(drops.get(), 2);
    drop(profiler);
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "Two live actors distinguish a dying handler node from an already drained declaration"
)]
fn local_output_skips_foreign_handler_while_its_feature_is_being_destroyed() {
    /// Drives the other actor exactly during the installed Feature's destruction.
    struct TurnOnDrop {
        driver: Driver,
        phase: Rc<Cell<usize>>,
    }

    impl Feature for TurnOnDrop {
        type State = <Child as Feature>::State;
        type Action = <Child as Feature>::Action;

        fn update(
            &mut self,
            state: &mut Self::State,
            action: &Self::Action,
            context: &mut FeatureContext<'_, Self::State, Self::Action>,
        ) {
            Child.update(state, action, context);
        }
    }

    impl Drop for TurnOnDrop {
        fn drop(&mut self) {
            self.phase.set(self.phase.get() + 1);
            self.driver.turn();
            self.phase.set(self.phase.get() + 1);
        }
    }

    let foreign_driver = Driver::default();
    let driver = Driver::default();
    let phase = Rc::new(Cell::new(0));
    let drop_driver = driver.clone();
    let drop_phase = Rc::clone(&phase);
    let declaration_capture = Rc::new(());
    let declaration_alive = Rc::downgrade(&declaration_capture);
    let (builder, foreign_site) = Store::builder(
        RootState {
            child: Some(Rc::new(Cell::new(11))),
            outcome: None,
        },
        Parent,
        Runtime::new(foreign_driver.clone()),
    )
    .mount_with_site(OptionalMount::new(
        |state: &RootState| state.child.as_ref(),
        |state: &mut RootState| state.child.as_mut(),
        |state: &RootState| {
            state.child.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| {
            let _ = &declaration_capture;
            TurnOnDrop {
                driver: drop_driver.clone(),
                phase: Rc::clone(&drop_phase),
            }
        },
    ));
    let foreign = builder.build().unwrap();
    foreign_driver.finish(foreign.ready()).unwrap();

    let (builder, source) = Store::builder(
        RootState {
            child: Some(Rc::new(Cell::new(7))),
            outcome: None,
        },
        Parent,
        Runtime::new(driver.clone()),
    )
    .mount_with_site(OptionalMount::new(
        |state: &RootState| state.child.as_ref(),
        |state: &mut RootState| state.child.as_mut(),
        |state: &RootState| {
            state.child.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        |_| Child,
    ));
    let stale_calls = Rc::new(Cell::new(0));
    let root_calls = Rc::new(Cell::new(0));
    let observed_phase = Rc::new(Cell::new((0, false)));
    let stale = Rc::clone(&stale_calls);
    let root = Rc::clone(&root_calls);
    let observed = Rc::clone(&observed_phase);
    let in_drop = Rc::clone(&phase);
    let during_drop = std::rc::Weak::clone(&declaration_alive);
    let (builder, output) = builder.output::<Outcome>(&source, |handlers| {
        handlers.at(&foreign_site, move |_, value, _| {
            stale.set(stale.get() + 1);
            OutputFlow::Continue(value)
        });
        handlers.at_root(move |state, value, _| {
            root.set(root.get() + 1);
            observed.set((in_drop.get(), during_drop.upgrade().is_some()));
            state.outcome = Some(value);
            OutputFlow::Consumed
        });
    });
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    let context = driver.finish(store.acquire(&source)).unwrap().unwrap();
    let drops = Rc::new(Cell::new(0));
    let value = outcome(&drops, 97);
    let identity = Rc::as_ptr(&value.value);
    let mut delivery = std::pin::pin!(context.output(&output, value));
    assert!(poll(delivery.as_mut()).is_pending());
    assert_eq!((phase.get(), root_calls.get(), drops.get()), (0, 0, 0));

    // The foreign node dies before its declaration; its Drop drives only the live actor.
    drop(foreign);
    foreign_driver.turn();
    let Poll::Ready(Ok(OutputDelivery::Consumed { task })) = poll(delivery.as_mut()) else {
        panic!("the live root must consume output during foreign Feature destruction");
    };
    driver.finish(task.finish());
    assert_eq!((phase.get(), observed_phase.get()), (2, (1, true)));
    assert!(declaration_alive.upgrade().is_none());
    assert_eq!(
        (stale_calls.get(), root_calls.get(), drops.get()),
        (0, 1, 0)
    );
    assert_eq!(
        driver
            .finish(store.read(|state| {
                let value = state.outcome.as_ref().unwrap();
                (Rc::as_ptr(&value.value), value.value.get())
            }))
            .unwrap(),
        (identity, 97)
    );
    assert!(foreign_driver.0.borrow().is_empty());
    drop(store);
    driver.turn();
    assert!(driver.0.borrow().is_empty());
    assert_eq!(
        (
            phase.get(),
            stale_calls.get(),
            root_calls.get(),
            drops.get()
        ),
        (2, 0, 1, 1)
    );
    assert_eq!(Rc::strong_count(&stale_calls), 1);
    assert_eq!(Rc::strong_count(&root_calls), 1);
}
