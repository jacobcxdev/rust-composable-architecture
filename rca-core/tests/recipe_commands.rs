//! Current-site command and control declarations share one installed Feature.
use rca_core::{
    Feature, FeatureContext, MountIdentity, OptionalMount, Runtime, RuntimeDriver, RuntimeFuture,
    Store, TaskAccessReason, TaskSendError,
};
use std::cell::Cell;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

/// Manually polls a frozen-clock actor; yielding still suspends for one turn.
#[derive(Clone, Default)]
struct Driver(Arc<Mutex<Vec<RuntimeFuture>>>);
impl std::fmt::Debug for Driver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RecipeDriver")
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
    assert_eq!(
        rca_core::RuntimeDriver::now(&driver),
        rca_core::MonotonicInstant::ZERO
    );
    let mut elapsed =
        rca_core::RuntimeDriver::sleep_until(&driver, rca_core::MonotonicInstant::ZERO);
    let mut context = Context::from_waker(Waker::noop());
    assert!(elapsed.as_mut().poll(&mut context).is_ready());

    for deadline in [
        rca_core::MonotonicInstant::from_elapsed(std::time::Duration::from_nanos(1)),
        rca_core::MonotonicInstant::MAX,
    ] {
        let mut future = rca_core::RuntimeDriver::sleep_until(&driver, deadline);
        assert!(future.as_mut().poll(&mut context).is_pending());
    }
    assert!(driver.0.lock().unwrap().is_empty());
}

#[test]
fn manual_driver_yield_suspends_and_wakes_before_completion() {
    /// Counts scheduling requests without driving the actor from the waker.
    struct WakeCount(std::sync::atomic::AtomicUsize);
    impl std::task::Wake for WakeCount {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    let driver = Driver::default();
    let notifications = Arc::new(WakeCount(std::sync::atomic::AtomicUsize::new(0)));
    let waker = Waker::from(Arc::clone(&notifications));
    let mut context = Context::from_waker(&waker);
    let mut future = rca_core::RuntimeDriver::yield_now(&driver);
    assert_eq!(notifications.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(future.as_mut().poll(&mut context).is_pending());
    assert_eq!(notifications.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(future.as_mut().poll(&mut context).is_ready());
    assert_eq!(notifications.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(driver.0.lock().unwrap().is_empty());
}

impl Driver {
    /// Polls the captured queue once, outside its lock; new work waits for a later turn.
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
    /// Bounds progress for a finite operation rather than waiting on wall-clock time.
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
        panic!("bounded recipe command progress")
    }
}
struct State {
    child: Option<usize>,
    replacement: u64,
}
enum ParentAction {
    Gate(rca_core::InputGate, bool),
    Replace,
}
struct Parent;
impl Feature for Parent {
    type State = State;
    type Action = ParentAction;
    fn update(
        &mut self,
        state: &mut State,
        action: &ParentAction,
        context: &mut FeatureContext<'_, State, ParentAction>,
    ) {
        match action {
            ParentAction::Gate(gate, open) => context.set_input_open(gate, *open).unwrap(),
            ParentAction::Replace => {
                state.replacement += 1;
                state.child = Some(0);
            }
        }
    }
}
struct Child(Cell<usize>);
impl Feature for Child {
    type State = usize;
    type Action = usize;
    fn update(
        &mut self,
        state: &mut usize,
        action: &usize,
        _: &mut FeatureContext<'_, usize, usize>,
    ) {
        self.0.set(self.0.get() + 1);
        *state += action;
    }
}
/// Non-cloneable ordinary input whose box identifies recovered ownership.
struct Input(Box<Cell<usize>>);
/// A distinct owned input: bypass applies to this port, not to work it creates.
struct Control(Box<Cell<usize>>);

#[cfg(feature = "test-util")]
#[test]
fn diagnostic_recipe_acquisitions_reject_closed_admission_without_retaining_the_mapper() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let driver = Driver::default();
    let declared = Arc::new(Mutex::new(None));
    let exported = Arc::clone(&declared);
    let maps = Arc::new(AtomicUsize::new(0));
    let mapped = Arc::clone(&maps);
    let capture = Arc::new(());
    let lifetime = Arc::downgrade(&capture);
    let descriptor = OptionalMount::new(
        |state: &State| state.child.as_ref(),
        |state: &mut State| state.child.as_mut(),
        |state: &State| {
            state.child.map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        |_| Child(Cell::new(0)),
    );
    let (builder, _) = Store::builder(
        State {
            child: Some(3),
            replacement: 0,
        },
        Parent,
        Runtime::new(driver.clone()),
    )
    .mailbox_capacity(1)
    .mount_with_children(descriptor, move |_, children| {
        let capture = Arc::clone(&capture);
        let mapped = Arc::clone(&mapped);
        *exported.lock().unwrap() = Some(children.command(move |input: usize| {
            let _ = &capture;
            mapped.fetch_add(1, Ordering::SeqCst);
            input
        }));
    });
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    assert_eq!(
        driver.finish(store.read(|state| state.child)).unwrap(),
        Some(3)
    );
    let site = declared.lock().unwrap().take().unwrap();
    let gate = rca_core::testing::TransactionGate::new();
    let mut admitted = Box::pin(gate.command_port(&store, &site));
    let mut blocked = Box::pin(gate.command_port(&store, &site));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(admitted.as_mut().poll(&mut cx).is_pending());
    assert!(store.try_send(ParentAction::Replace).unwrap_err().is_full());
    assert!(blocked.as_mut().poll(&mut cx).is_pending());
    assert!(lifetime.upgrade().is_some());
    drop(store);
    driver.turn();
    for future in [&mut admitted, &mut blocked] {
        let Poll::Ready(Err(error)) = future.as_mut().poll(&mut cx) else {
            panic!("closed actor must reject admitted and capacity-blocked queries");
        };
        assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
        assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
    }
    assert_eq!(maps.load(Ordering::SeqCst), 0);
    assert!(lifetime.upgrade().is_none());
    assert!(!gate.has_pending());
    assert!(gate.boundary().is_none());
    drop((admitted, blocked));
    driver.turn();
    assert!(driver.0.lock().unwrap().is_empty());
    assert_eq!(Arc::strong_count(&maps), 1);
}

#[cfg(feature = "test-util")]
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One real actor-abandonment timeline compares held publication with and without queued sibling cleanup."
)]
fn unpublished_recipe_call_preserves_held_input_during_actor_teardown() {
    use rca_core::testing::TransactionGate;
    use rca_core::{CommandPort, TaskAccessError, TaskProgress};
    use std::sync::atomic::{AtomicUsize, Ordering};

    type Observation = Result<Option<usize>, TaskAccessError>;
    /// Inspects the held command during real actor teardown, before the gate drops.
    struct InspectParent {
        gate: TransactionGate<State, ParentAction>,
        port: Arc<Mutex<Option<CommandPort<Input>>>>,
        observed: Arc<Mutex<Option<Observation>>>,
    }
    impl Feature for InspectParent {
        type State = State;
        type Action = ParentAction;

        fn update(
            &mut self,
            state: &mut State,
            action: &ParentAction,
            context: &mut FeatureContext<'_, State, ParentAction>,
        ) {
            Parent.update(state, action, context);
        }
    }
    impl Drop for InspectParent {
        fn drop(&mut self) {
            let port = self.port.lock().unwrap().clone();
            if let Some(port) = port {
                *self.observed.lock().unwrap() =
                    Some(self.gate.inspect_command(&port, |input| input.0.get()));
            }
        }
    }

    for queue_follower in [false, true] {
        let driver = Driver::default();
        let gate = TransactionGate::new();
        let port = Arc::new(Mutex::new(None));
        let observed = Arc::new(Mutex::new(None));
        let declared = Arc::new(Mutex::new(None));
        let sites = Arc::clone(&declared);
        let maps = Arc::new(AtomicUsize::new(0));
        let mapped = Arc::clone(&maps);
        let descriptor = OptionalMount::new(
            |state: &State| state.child.as_ref(),
            |state: &mut State| state.child.as_mut(),
            |state: &State| {
                state.child.map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            |_| Child(Cell::new(0)),
        );
        let (builder, _) = Store::builder(
            State {
                child: Some(0),
                replacement: 0,
            },
            InspectParent {
                gate: gate.clone(),
                port: Arc::clone(&port),
                observed: Arc::clone(&observed),
            },
            Runtime::new(driver.clone()),
        )
        .mount_with_children(descriptor, move |_, children| {
            let mapped = Arc::clone(&mapped);
            *sites.lock().unwrap() = Some(children.command(move |input: Input| {
                mapped.fetch_add(1, Ordering::SeqCst);
                input.0.get()
            }));
        });
        let store = builder.transaction_gate(gate.clone()).build().unwrap();
        driver.finish(gate.wait_for_pending());
        let startup = gate.boundary().unwrap();
        drop(
            driver
                .finish(gate.release_pending_with_receipt().unwrap().finish())
                .unwrap(),
        );
        assert!(gate.acknowledge(startup));
        driver.finish(store.ready()).unwrap();
        let site = declared.lock().unwrap().take().unwrap();
        let acquired = driver.finish(store.command_port(&site)).unwrap().unwrap();
        *port.lock().unwrap() = Some(acquired.clone());

        let input = Input(Box::new(Cell::new(17)));
        let address = std::ptr::from_ref(&*input.0);
        let held = acquired.try_send(input).unwrap();
        driver.finish(gate.wait_for_pending());
        assert_eq!(
            gate.inspect_command(&acquired, |input| {
                assert_eq!(std::ptr::from_ref(&*input.0), address);
                input.0.get()
            })
            .unwrap(),
            Some(17)
        );
        let follower = queue_follower.then(|| {
            let input = Input(Box::new(Cell::new(23)));
            let address = std::ptr::from_ref(&*input.0);
            (address, acquired.try_send(input).unwrap())
        });

        // Drop the real actor, but retain Store and both admission receipts.
        // Its mailbox drains before the root feature inspects the held call.
        let actor_tasks = std::mem::take(&mut *driver.0.lock().unwrap());
        assert_eq!(actor_tasks.len(), 1);
        drop(actor_tasks);
        let observation = observed.lock().unwrap().take().unwrap();

        let Err(TaskSendError::Rejected { action, error }) = driver.finish(held.committed()) else {
            panic!("actor abandonment must return the held original input");
        };
        assert_eq!(std::ptr::from_ref(&*action.0), address);
        assert_eq!(action.0.get(), 17);
        assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
        assert_eq!(error.progress(), TaskProgress::NotStarted);
        drop(action);
        if let Some((address, follower)) = follower {
            let Err(TaskSendError::Rejected { action, error }) =
                driver.finish(follower.committed())
            else {
                panic!("actor abandonment must return the unpublished input");
            };
            assert_eq!(std::ptr::from_ref(&*action.0), address);
            assert_eq!(action.0.get(), 23);
            assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
            assert_eq!(error.progress(), TaskProgress::NotStarted);
        }
        assert_eq!(maps.load(Ordering::SeqCst), 0);
        assert!(driver.0.lock().unwrap().is_empty());
        drop((store, acquired, port));
        assert_eq!(observation.unwrap(), Some(17), "queued: {queue_follower}");
    }
}

#[cfg(feature = "test-util")]
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One real actor teardown checks the mapper destructor, retained frontier and original input together."
)]
fn mapper_destructor_cannot_inspect_its_retired_command_frontier() {
    use rca_core::testing::TransactionGate;
    use rca_core::{CommandPort, TaskAccessError, TaskProgress};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct OwnedInput {
        value: Box<usize>,
        drops: Arc<AtomicUsize>,
    }
    impl Drop for OwnedInput {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }
    type InspectionResult = Result<Option<()>, TaskAccessError>;
    /// Checks that retiring mapper captures cannot inspect a stale command frontier.
    struct InspectOnDrop {
        gate: TransactionGate<State, ParentAction>,
        port: Arc<Mutex<Option<CommandPort<OwnedInput>>>>,
        result: Arc<Mutex<Option<InspectionResult>>>,
        matchers: Arc<AtomicUsize>,
        drops: Arc<AtomicUsize>,
    }
    impl Drop for InspectOnDrop {
        fn drop(&mut self) {
            let port = self.port.lock().unwrap().as_ref().unwrap().clone();
            let result = self.gate.inspect_command(&port, |_| {
                self.matchers.fetch_add(1, Ordering::SeqCst);
            });
            *self.result.lock().unwrap() = Some(result);
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    for root_command in [false, true] {
        let driver = Driver::default();
        let gate = TransactionGate::new();
        let port = Arc::new(Mutex::new(None));
        let result = Arc::new(Mutex::new(None));
        let maps = Arc::new(AtomicUsize::new(0));
        let matchers = Arc::new(AtomicUsize::new(0));
        let capture_drops = Arc::new(AtomicUsize::new(0));
        let input_drops = Arc::new(AtomicUsize::new(0));
        let declared = Arc::new(Mutex::new(None));
        let sites = Arc::clone(&declared);
        let observer_gate = gate.clone();
        let observer_port = Arc::clone(&port);
        let observer_result = Arc::clone(&result);
        let observer_matchers = Arc::clone(&matchers);
        let observer_drops = Arc::clone(&capture_drops);
        let mapped = Arc::clone(&maps);
        let descriptor = OptionalMount::new(
            |state: &State| state.child.as_ref(),
            |state: &mut State| state.child.as_mut(),
            |state: &State| {
                state.child.map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            |_| Child(Cell::new(0)),
        );
        let builder = Store::builder(
            State {
                child: Some(3),
                replacement: 0,
            },
            Parent,
            Runtime::new(driver.clone()),
        );
        let builder = if root_command {
            let observer = InspectOnDrop {
                gate: observer_gate,
                port: observer_port,
                result: observer_result,
                matchers: observer_matchers,
                drops: observer_drops,
            };
            let (builder, _, site) =
                builder.mount_with_command(descriptor, move |input: OwnedInput| {
                    let _ = &observer;
                    mapped.fetch_add(1, Ordering::SeqCst);
                    *input.value
                });
            *sites.lock().unwrap() = Some(site);
            builder
        } else {
            let (builder, _) = builder.mount_with_children(descriptor, move |_, children| {
                let observer = InspectOnDrop {
                    gate: observer_gate.clone(),
                    port: Arc::clone(&observer_port),
                    result: Arc::clone(&observer_result),
                    matchers: Arc::clone(&observer_matchers),
                    drops: Arc::clone(&observer_drops),
                };
                let mapped = Arc::clone(&mapped);
                *sites.lock().unwrap() = Some(children.command(move |input: OwnedInput| {
                    let _ = &observer;
                    mapped.fetch_add(1, Ordering::SeqCst);
                    *input.value
                }));
            });
            builder
        };
        let store = builder.transaction_gate(gate.clone()).build().unwrap();
        driver.finish(gate.wait_for_pending());
        let startup = gate.boundary().unwrap();
        drop(
            driver
                .finish(gate.release_pending_with_receipt().unwrap().finish())
                .unwrap(),
        );
        assert!(gate.acknowledge(startup));
        driver.finish(store.ready()).unwrap();
        let site = declared.lock().unwrap().take().unwrap();
        let acquired = driver.finish(store.command_port(&site)).unwrap().unwrap();
        *port.lock().unwrap() = Some(acquired.clone());

        let input = OwnedInput {
            value: Box::new(37),
            drops: Arc::clone(&input_drops),
        };
        let address = std::ptr::from_ref(&*input.value);
        let mut send = Box::pin(acquired.send(input));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(send.as_mut().poll(&mut cx).is_pending());
        driver.finish(gate.wait_for_pending());
        assert_eq!(
            gate.inspect_command(&acquired, |held| {
                assert_eq!(std::ptr::from_ref(&*held.value), address);
                *held.value
            })
            .unwrap(),
            Some(37)
        );
        assert_eq!(input_drops.load(Ordering::SeqCst), 0);

        // Keep the public Store alive: only the actual runtime-owned actor is
        // abandoned. Its mapper captures drop before its lifecycle guard/binding.
        let actor_tasks = std::mem::take(&mut *driver.0.lock().unwrap());
        assert_eq!(actor_tasks.len(), 1);
        drop(actor_tasks);
        let inspection = result.lock().unwrap().take().unwrap();
        if root_command {
            assert_eq!(
                inspection.unwrap(),
                None,
                "the root mapper payload was drained"
            );
        } else {
            let error = inspection.unwrap_err();
            assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
            assert_eq!(error.progress(), TaskProgress::NotStarted);
        }
        assert_eq!(matchers.load(Ordering::SeqCst), 0);
        assert_eq!(maps.load(Ordering::SeqCst), 0);
        assert_eq!(capture_drops.load(Ordering::SeqCst), 1);
        let Poll::Ready(Err(TaskSendError::Rejected { action, error })) =
            send.as_mut().poll(&mut cx)
        else {
            panic!("abandoned actor must return its unentered original command");
        };
        assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
        assert_eq!(error.progress(), TaskProgress::NotStarted);
        assert_eq!(std::ptr::from_ref(&*action.value), address);
        assert_eq!(*action.value, 37);
        assert_eq!(input_drops.load(Ordering::SeqCst), 0);
        drop(action);
        assert_eq!(input_drops.load(Ordering::SeqCst), 1);
        assert!(driver.0.lock().unwrap().is_empty());
        drop((send, acquired, port, store));
        assert_eq!(capture_drops.load(Ordering::SeqCst), 1);
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One installed-child lifetime checks foreign ownership, shared input gating and replacement without retargeting either port."
)]
fn independent_ordinary_and_control_ports_share_one_installed_child() {
    let driver = Driver::default();
    let sites = Arc::new(Mutex::new(Vec::new()));
    let declared = std::sync::Arc::clone(&sites);
    let descriptor = OptionalMount::new(
        |state: &State| state.child.as_ref(),
        |state: &mut State| state.child.as_mut(),
        |state: &State| {
            state.child.map(|_| MountIdentity {
                key: (),
                replacement: state.replacement,
                configuration: 0,
            })
        },
        |_| Child(Cell::new(0)),
    );
    let (builder, child_site) = Store::builder(
        State {
            child: Some(0),
            replacement: 0,
        },
        Parent,
        Runtime::new(driver.clone()),
    )
    .mount_with_children(descriptor, move |_, children| {
        let ordinary = children.command(|input: Input| input.0.get());
        let control = children.control(|input: Control| input.0.get());
        declared.lock().unwrap().push((ordinary, control));
    });
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    let (ordinary_site, control_site) = sites.lock().unwrap()[0].clone();
    let foreign = Store::builder(
        State {
            child: None,
            replacement: 0,
        },
        Parent,
        Runtime::new(driver.clone()),
    )
    .build()
    .unwrap();
    driver.finish(foreign.ready()).unwrap();
    let error = driver
        .finish(foreign.command_port(&ordinary_site))
        .unwrap_err();
    assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
    assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
    assert_eq!(
        driver.finish(store.read(|state| state.child)).unwrap(),
        Some(0)
    );
    assert!(
        driver
            .finish(foreign.read(|state| state.child))
            .unwrap()
            .is_none()
    );
    driver.finish(foreign.shutdown()).unwrap();
    drop(foreign);
    let ordinary = driver
        .finish(store.command_port(&ordinary_site))
        .unwrap()
        .unwrap();
    let control = driver
        .finish(store.command_port(&control_site))
        .unwrap()
        .unwrap();
    drop(
        driver
            .finish(ordinary.send(Input(Box::new(Cell::new(2)))))
            .unwrap(),
    );
    let gate = driver
        .finish(store.input_gate(&child_site))
        .unwrap()
        .unwrap();
    drop(
        driver
            .finish(store.send(ParentAction::Gate(gate, false)))
            .unwrap(),
    );
    match driver.finish(ordinary.send(Input(Box::new(Cell::new(3))))) {
        Err(TaskSendError::Rejected { action, error }) => {
            assert_eq!(action.0.get(), 3);
            assert_eq!(error.reason(), TaskAccessReason::InputClosed);
        }
        _ => panic!("closed ordinary input must recover original payload"),
    }
    drop(
        driver
            .finish(control.send(Control(Box::new(Cell::new(5)))))
            .unwrap(),
    );
    assert_eq!(
        driver.finish(store.read(|state| state.child)).unwrap(),
        Some(7)
    );
    drop(driver.finish(store.send(ParentAction::Replace)).unwrap());
    assert!(
        matches!(driver.finish(control.send(Control(Box::new(Cell::new(9))))),
        Err(TaskSendError::Rejected { error, .. }) if error.reason() == TaskAccessReason::ScopeEnded)
    );
    let fresh_site = sites.lock().unwrap()[1].0.clone();
    let fresh = driver
        .finish(store.command_port(&fresh_site))
        .unwrap()
        .unwrap();
    drop(
        driver
            .finish(fresh.send(Input(Box::new(Cell::new(11)))))
            .unwrap(),
    );
    assert_eq!(
        driver.finish(store.read(|state| state.child)).unwrap(),
        Some(11)
    );
    driver.finish(store.shutdown()).unwrap();
}

enum WorkAction {
    Increment,
    Launch,
}
/// Records task rejection separately from the state changed by ordinary actions.
struct WorkingChild(Arc<Mutex<Vec<TaskAccessReason>>>);
impl Feature for WorkingChild {
    type State = usize;
    type Action = WorkAction;
    fn update(
        &mut self,
        state: &mut usize,
        action: &WorkAction,
        context: &mut FeatureContext<'_, usize, WorkAction>,
    ) {
        match action {
            WorkAction::Increment => *state += 1,
            WorkAction::Launch => {
                let outcomes = Arc::clone(&self.0);
                context.effect(rca_core::Effect::send(WorkAction::Increment));
                context.task(rca_core::Task::<usize, WorkAction>::run(
                    move |task| async move {
                        match task.send(WorkAction::Increment).await {
                            Err(TaskSendError::Rejected {
                                action: WorkAction::Increment,
                                error,
                            }) => outcomes.lock().unwrap().push(error.reason()),
                            _ => panic!("control-generated task send must remain ordinary input"),
                        }
                    },
                ));
            }
        }
    }
}

#[test]
fn control_bypasses_only_its_input_not_direct_child_or_generated_work() {
    let driver = Driver::default();
    let declared = Arc::new(Mutex::new(None));
    let sites = Arc::clone(&declared);
    let outcomes = Arc::new(Mutex::new(Vec::new()));
    let child_outcomes = Arc::clone(&outcomes);
    let descriptor = OptionalMount::new(
        |state: &State| state.child.as_ref(),
        |state: &mut State| state.child.as_mut(),
        |state: &State| {
            state.child.map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| WorkingChild(Arc::clone(&child_outcomes)),
    );
    let (builder, site) = Store::builder(
        State {
            child: Some(0),
            replacement: 0,
        },
        Parent,
        Runtime::new(driver.clone()),
    )
    .mount_with_children(descriptor, move |_, children| {
        *sites.lock().unwrap() = Some((
            children.command(|_: Input| WorkAction::Increment),
            children.control(|_: Control| WorkAction::Launch),
        ));
    });
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    let (ordinary_site, control_site) = declared.lock().unwrap().take().unwrap();
    let ordinary = driver
        .finish(store.command_port(&ordinary_site))
        .unwrap()
        .unwrap();
    let control = driver
        .finish(store.command_port(&control_site))
        .unwrap()
        .unwrap();
    let child = driver.finish(store.acquire(&site)).unwrap().unwrap();
    let gate = driver.finish(store.input_gate(&site)).unwrap().unwrap();
    drop(
        driver
            .finish(store.send(ParentAction::Gate(gate, false)))
            .unwrap(),
    );
    assert!(
        matches!(driver.finish(child.send(WorkAction::Increment)), Err(TaskSendError::Rejected { error, .. }) if error.reason() == TaskAccessReason::InputClosed)
    );
    assert!(
        matches!(driver.finish(ordinary.send(Input(Box::new(Cell::new(1))))), Err(TaskSendError::Rejected { error, .. }) if error.reason() == TaskAccessReason::InputClosed)
    );
    let task = driver
        .finish(control.send(Control(Box::new(Cell::new(1)))))
        .unwrap();
    driver.finish(task.finish());
    assert_eq!(*outcomes.lock().unwrap(), [TaskAccessReason::InputClosed]);
    assert_eq!(
        driver.finish(store.read(|state| state.child)).unwrap(),
        Some(0)
    );
    driver.finish(store.shutdown()).unwrap();
}

type Rows = rca_core::KeyedState<u8, usize>;
struct KeyedParent;
enum KeyedAction {
    Close(rca_core::InputGate),
    Replace,
}
impl Feature for KeyedParent {
    type State = Rows;
    type Action = KeyedAction;
    fn update(
        &mut self,
        state: &mut Rows,
        action: &KeyedAction,
        context: &mut FeatureContext<'_, Rows, KeyedAction>,
    ) {
        match action {
            KeyedAction::Close(gate) => context.set_input_open(gate, false).unwrap(),
            KeyedAction::Replace => {
                state.remove(&7);
                state.insert(7, 0);
            }
        }
    }
}

#[test]
fn keyed_current_entry_owns_independent_ports_and_same_id_replacement_does_not_retarget() {
    let driver = Driver::default();
    let sites = Arc::new(Mutex::new(Vec::new()));
    let declared = Arc::clone(&sites);
    let store = Store::builder(
        [(7, 0)].into_iter().collect::<Rows>(),
        KeyedParent,
        Runtime::new(driver.clone()),
    )
    .keyed_with_children(
        |rows| rows,
        |rows| rows,
        move |_, _| {
            let declared = Arc::clone(&declared);
            (
                rca_core::EntryMount::new(Child(Cell::new(0))),
                move |_: &usize,
                      children: &mut rca_core::MountChildren<
                    '_,
                    Rows,
                    KeyedAction,
                    usize,
                    usize,
                >| {
                    declared.lock().unwrap().push((
                        children.site().clone(),
                        children.command(|input: Input| input.0.get()),
                        children.control(|input: Control| input.0.get()),
                    ));
                },
            )
        },
    )
    .build()
    .unwrap();
    driver.finish(store.ready()).unwrap();
    let (site, ordinary_site, control_site) = sites.lock().unwrap()[0].clone();
    let ordinary = driver
        .finish(store.command_port(&ordinary_site))
        .unwrap()
        .unwrap();
    let control = driver
        .finish(store.command_port(&control_site))
        .unwrap()
        .unwrap();
    drop(
        driver
            .finish(ordinary.send(Input(Box::new(Cell::new(2)))))
            .unwrap(),
    );
    let gate = driver.finish(store.input_gate(&site)).unwrap().unwrap();
    drop(driver.finish(store.send(KeyedAction::Close(gate))).unwrap());
    assert!(
        matches!(driver.finish(ordinary.send(Input(Box::new(Cell::new(3))))), Err(TaskSendError::Rejected { error, .. }) if error.reason() == TaskAccessReason::InputClosed)
    );
    drop(
        driver
            .finish(control.send(Control(Box::new(Cell::new(5)))))
            .unwrap(),
    );
    assert_eq!(
        driver
            .finish(store.read(|rows| rows.get(&7).copied()))
            .unwrap(),
        Some(7)
    );
    drop(driver.finish(store.send(KeyedAction::Replace)).unwrap());
    assert!(
        matches!(driver.finish(control.send(Control(Box::new(Cell::new(9))))), Err(TaskSendError::Rejected { error, .. }) if error.reason() == TaskAccessReason::ScopeEnded)
    );
    let fresh_site = sites.lock().unwrap()[1].1.clone();
    let fresh = driver
        .finish(store.command_port(&fresh_site))
        .unwrap()
        .unwrap();
    drop(
        driver
            .finish(fresh.send(Input(Box::new(Cell::new(11)))))
            .unwrap(),
    );
    assert_eq!(
        driver
            .finish(store.read(|rows| rows.get(&7).copied()))
            .unwrap(),
        Some(11)
    );
    driver.finish(store.shutdown()).unwrap();
}

#[cfg(feature = "test-util")]
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One real gate callback queues acquisition before closing its owner and preserves the pending original input through teardown."
)]
fn gate_callback_closure_rejects_already_queued_recipe_acquisition() {
    // Each request kind gets a fresh Store and its own bounded diagnostic slot.
    for (root_command, mounted) in [(false, false), (false, true), (true, false)] {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct OwnedInput {
            value: Box<Cell<usize>>,
            drops: Arc<AtomicUsize>,
        }
        impl Drop for OwnedInput {
            fn drop(&mut self) {
                self.drops.fetch_add(1, Ordering::SeqCst);
            }
        }

        let driver = Driver::default();
        let declared = Arc::new(Mutex::new(None));
        let sites = Arc::clone(&declared);
        let maps = Arc::new(AtomicUsize::new(0));
        let mapped = Arc::clone(&maps);
        let capture = Arc::new(());
        let capture_lifetime = Arc::downgrade(&capture);
        let descriptor = OptionalMount::new(
            |state: &State| state.child.as_ref(),
            |state: &mut State| state.child.as_mut(),
            |state: &State| {
                state.child.map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            |_| Child(Cell::new(0)),
        );
        let builder = Store::builder(
            State {
                child: Some(3),
                replacement: 0,
            },
            Parent,
            Runtime::new(driver.clone()),
        );
        let (builder, mount_site) = if root_command {
            let (builder, mount_site, site) =
                builder.mount_with_command(descriptor, move |input: OwnedInput| {
                    let _ = &capture;
                    mapped.fetch_add(1, Ordering::SeqCst);
                    input.value.get()
                });
            *sites.lock().unwrap() = Some(site);
            (builder, mount_site)
        } else {
            builder.mount_with_children(descriptor, move |_, children| {
                let capture = Arc::clone(&capture);
                let mapped = Arc::clone(&mapped);
                *sites.lock().unwrap() = Some(children.command(move |input: OwnedInput| {
                    let _ = &capture;
                    mapped.fetch_add(1, Ordering::SeqCst);
                    input.value.get()
                }));
            })
        };
        let gate = rca_core::testing::TransactionGate::new();
        let store = builder.transaction_gate(gate.clone()).build().unwrap();
        driver.finish(gate.wait_for_pending());
        let startup = gate.boundary().unwrap();
        drop(
            driver
                .finish(gate.release_pending_with_receipt().unwrap().finish())
                .unwrap(),
        );
        assert!(gate.acknowledge(startup));
        driver.finish(store.ready()).unwrap();
        let site = declared.lock().unwrap().take().unwrap();
        let port = driver.finish(store.command_port(&site)).unwrap().unwrap();
        let input_drops = Arc::new(AtomicUsize::new(0));
        let input = OwnedInput {
            value: Box::new(Cell::new(19)),
            drops: Arc::clone(&input_drops),
        };
        let address = std::ptr::from_ref(&*input.value);
        let mut send = Box::pin(port.send(input));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(send.as_mut().poll(&mut cx).is_pending());
        driver.finish(gate.wait_for_pending());
        let boundary = gate.pending_boundary().unwrap();
        assert_eq!(input_drops.load(Ordering::SeqCst), 0);

        let queued = Arc::new(Mutex::new(None));
        let captured_queue = Arc::clone(&queued);
        let query_gate = gate.clone();
        let queries = Arc::new(AtomicUsize::new(0));
        let observed_calls = Arc::clone(&queries);
        let closed = store.closed();
        let query = gate.assert_state(move |root| {
            assert_eq!(root.child, Some(3));
            let acquisition = if mounted {
                futures_util::future::Either::Left(futures_util::FutureExt::map(
                    query_gate.acquire(&store, &mount_site),
                    |result| result.map(|_| ()),
                ))
            } else {
                futures_util::future::Either::Right(futures_util::FutureExt::map(
                    query_gate.command_port(&store, &site),
                    |result| result.map(|_| ()),
                ))
            };
            let mut acquisition = Box::pin(acquisition);
            assert!(
                acquisition
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            *captured_queue.lock().unwrap() = Some(acquisition);
            observed_calls.fetch_add(1, Ordering::SeqCst);
            // The query owns admission already; the next actor query must observe
            // actual closure, rather than returning a newly frozen command port.
            drop(store);
        });
        driver.finish(query.finish()).unwrap();
        assert_eq!(queries.load(Ordering::SeqCst), 1);
        let mut acquisition = queued.lock().unwrap().take().unwrap();
        let Poll::Ready(Err(error)) = acquisition.as_mut().poll(&mut cx) else {
            panic!("already queued acquisition must publish the owner closure");
        };
        assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
        assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
        let Err(TaskSendError::Rejected { action, error }) = driver.finish(send.as_mut()) else {
            panic!("unreleased command must return its original input after closure");
        };
        assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
        assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
        assert_eq!(std::ptr::from_ref(&*action.value), address);
        assert_eq!(action.value.get(), 19);
        assert_eq!(input_drops.load(Ordering::SeqCst), 0);
        assert_eq!(maps.load(Ordering::SeqCst), 0);
        assert!(!gate.has_pending());
        assert_eq!(
            gate.match_terminal(&boundary, |error| {
                error.reason() == TaskAccessReason::StoreClosed
                    && error.progress() == rca_core::TaskProgress::NotStarted
            }),
            Some(true)
        );
        assert!(gate.acknowledge_terminal(&boundary));
        driver.finish(closed);
        drop((action, send, acquisition, port));
        driver.turn();
        assert!(driver.0.lock().unwrap().is_empty());
        assert!(capture_lifetime.upgrade().is_none());
        assert_eq!(input_drops.load(Ordering::SeqCst), 1);
    }
}

#[cfg(feature = "test-util")]
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "Two live same-domain stores distinguish actor-root rejection and an expired query generation without retargeting the recipe."
)]
fn foreign_gate_rejects_live_recipe_acquisition_and_expired_query_admission() {
    // Each request kind gets a fresh Store and its own bounded diagnostic slot.
    for mounted in [false, true] {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let driver = Driver::default();
        let declared = Arc::new(Mutex::new(None));
        let sites = Arc::clone(&declared);
        let maps = Arc::new(AtomicUsize::new(0));
        let mapped = Arc::clone(&maps);
        let capture = Arc::new(());
        let capture_lifetime = Arc::downgrade(&capture);
        let descriptor = OptionalMount::new(
            |state: &State| state.child.as_ref(),
            |state: &mut State| state.child.as_mut(),
            |state: &State| {
                state.child.map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            |_| Child(Cell::new(0)),
        );
        let (builder, mount_site) = Store::builder(
            State {
                child: Some(5),
                replacement: 0,
            },
            Parent,
            Runtime::new(driver.clone()),
        )
        .mount_with_children(descriptor, move |_, children| {
            let capture = Arc::clone(&capture);
            let mapped = Arc::clone(&mapped);
            *sites.lock().unwrap() = Some(children.command(move |input: Input| {
                let _ = &capture;
                mapped.fetch_add(1, Ordering::SeqCst);
                input.0.get()
            }));
        });
        let store = builder.build().unwrap();
        driver.finish(store.ready()).unwrap();
        let site = declared.lock().unwrap().take().unwrap();

        let foreign_descriptor = OptionalMount::new(
            |state: &State| state.child.as_ref(),
            |state: &mut State| state.child.as_mut(),
            |state: &State| {
                state.child.map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            |_| Child(Cell::new(0)),
        );
        let (foreign_builder, _) = Store::builder(
            State {
                child: Some(23),
                replacement: 0,
            },
            Parent,
            Runtime::new(driver.clone()),
        )
        .mount_with_site(foreign_descriptor);
        let gate = rca_core::testing::TransactionGate::new();
        let foreign = foreign_builder
            .transaction_gate(gate.clone())
            .build()
            .unwrap();
        driver.finish(gate.wait_for_pending());
        let startup = gate.boundary().unwrap();
        drop(
            driver
                .finish(gate.release_pending_with_receipt().unwrap().finish())
                .unwrap(),
        );
        driver.finish(foreign.ready()).unwrap();

        let error = if mounted {
            driver
                .finish(gate.acquire(&foreign, &mount_site))
                .unwrap_err()
        } else {
            driver
                .finish(gate.command_port(&foreign, &site))
                .unwrap_err()
        };
        assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
        assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
        assert_eq!(gate.boundary(), Some(startup));
        assert!(!gate.has_pending());
        assert_eq!(maps.load(Ordering::SeqCst), 0);

        // The submitting Store matches the recipe. The diagnostic query executes
        // on the other ready actor, which must reject the immutable root mismatch.
        let acquisition = if mounted {
            futures_util::future::Either::Left(futures_util::FutureExt::map(
                gate.acquire(&store, &mount_site),
                |result| result.map(|_| ()),
            ))
        } else {
            futures_util::future::Either::Right(futures_util::FutureExt::map(
                gate.command_port(&store, &site),
                |result| result.map(|_| ()),
            ))
        };
        let mut acquisition = Box::pin(acquisition);
        let mut cx = Context::from_waker(Waker::noop());
        assert!(acquisition.as_mut().poll(&mut cx).is_pending());
        let error = driver.finish(acquisition.as_mut()).unwrap_err();
        assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
        assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
        assert_eq!(gate.boundary(), Some(startup));
        assert!(!gate.has_pending());
        assert!(!gate.has_failed_match());
        assert_eq!(maps.load(Ordering::SeqCst), 0);
        assert_eq!(
            driver.finish(store.read(|state| state.child)).unwrap(),
            Some(5)
        );

        // A second acquisition must wait outside the full diagnostic slot. An
        // acknowledged generation cannot later admit it into another boundary.
        let queries = Arc::new(AtomicUsize::new(0));
        let observed_calls = Arc::clone(&queries);
        let mut occupying = Box::pin(
            gate.assert_state(move |_| {
                observed_calls.fetch_add(1, Ordering::SeqCst);
            })
            .finish(),
        );
        assert!(occupying.as_mut().poll(&mut cx).is_pending());
        let waiting = if mounted {
            futures_util::future::Either::Left(futures_util::FutureExt::map(
                gate.acquire(&store, &mount_site),
                |result| result.map(|_| ()),
            ))
        } else {
            futures_util::future::Either::Right(futures_util::FutureExt::map(
                gate.command_port(&store, &site),
                |result| result.map(|_| ()),
            ))
        };
        let mut waiting = Box::pin(waiting);
        assert!(waiting.as_mut().poll(&mut cx).is_pending());
        assert!(gate.acknowledge(startup));
        let Poll::Ready(Err(error)) = waiting.as_mut().poll(&mut cx) else {
            panic!("retired diagnostic generation must reject the waiting acquisition");
        };
        assert_eq!(error.reason(), TaskAccessReason::PhaseDenied);
        assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
        drop(occupying);
        assert!(gate.boundary().is_none());
        assert_eq!(maps.load(Ordering::SeqCst), 0);
        assert!(capture_lifetime.upgrade().is_some());

        let port = driver.finish(store.command_port(&site)).unwrap().unwrap();
        drop(
            driver
                .finish(port.send(Input(Box::new(Cell::new(7)))))
                .unwrap(),
        );
        assert_eq!(maps.load(Ordering::SeqCst), 1);
        assert_eq!(
            driver.finish(store.read(|state| state.child)).unwrap(),
            Some(12)
        );
        assert_eq!(
            driver.finish(foreign.read(|state| state.child)).unwrap(),
            Some(23)
        );
        assert_eq!(queries.load(Ordering::SeqCst), 0);
        driver.finish(foreign.shutdown()).unwrap();
        driver.finish(store.shutdown()).unwrap();
        drop((acquisition, waiting, port, foreign, store));
        driver.turn();
        assert!(driver.0.lock().unwrap().is_empty());
        assert!(capture_lifetime.upgrade().is_none());
    }
}
