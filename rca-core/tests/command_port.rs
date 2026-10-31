//! Narrow, owned commands use the mounted child's existing bounded actor.

#[cfg(feature = "allocation-measurement")]
#[path = "../src/allocation_test/child.rs"]
mod allocation_test;

use std::cell::Cell;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use rca_core::{
    CommandPort, CommandSite, Feature, FeatureContext, InputGate, MonotonicInstant, MountIdentity,
    MountSite, OptionalMount, RootMount, Runtime, RuntimeDriver, RuntimeFuture, Store,
    TaskAccessReason, TaskProgress, TaskSendError, TryCommandError,
};

#[cfg(feature = "allocation-measurement")]
#[global_allocator]
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

/// Manually polls a frozen-clock actor; yielding still suspends for one turn.
#[derive(Clone, Default)]
struct Driver(Arc<Mutex<Vec<RuntimeFuture>>>);

impl std::fmt::Debug for Driver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CommandDriver")
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
        let pending = std::mem::take(&mut *self.0.lock().unwrap());
        for mut future in pending {
            if poll(future.as_mut()).is_pending() {
                self.0.lock().unwrap().push(future);
            }
        }
    }

    /// Bounds progress for a finite operation rather than waiting on wall-clock time.
    fn finish<F: Future>(&self, future: F) -> F::Output {
        let mut future = Box::pin(future);
        for _ in 0..32 {
            if let Poll::Ready(value) = poll(future.as_mut()) {
                return value;
            }
            self.turn();
        }
        panic!("finite command operation did not finish");
    }
}

/// Polls once without driving other tasks; callers choose the next actor turn.
fn poll<F: Future + ?Sized>(future: Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}

/// Intentionally neither `Clone`, `Debug` nor `Sync`: the box identifies the original
/// owner returned by `Full` and `NotStarted` rejection.
struct Command {
    amount: Box<Cell<usize>>,
    drops: Arc<AtomicUsize>,
}

impl Command {
    fn new(amount: usize, drops: &Arc<AtomicUsize>) -> Self {
        Self {
            amount: Box::new(Cell::new(amount)),
            drops: Arc::clone(drops),
        }
    }

    fn identity(&self) -> usize {
        (&raw const *self.amount) as usize
    }
}

impl Drop for Command {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

/// Counts payload disposal without retaining the feature or state under test.
struct DropCount(Arc<AtomicUsize>);

impl Drop for DropCount {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[derive(Default)]
struct ChildState {
    total: usize,
}

struct RootState {
    children: [Option<ChildState>; 3],
    replacement: [u64; 3],
    configuration: [u64; 3],
    writes: [Arc<AtomicUsize>; 3],
    deny_write: [bool; 3],
    _owner: DropCount,
}

#[derive(Debug)]
enum RootAction {
    Noop,
    Gate(InputGate, bool),
    TryGate(InputGate, Arc<Mutex<Option<rca_core::TaskAccessError>>>),
    ToggleChild(usize),
    Replace(usize),
    Refresh(usize),
    DenyWrite(usize),
}

enum ChildAction {
    Add { command: Command, _owner: DropCount },
}

struct Parent;
impl Feature for Parent {
    type State = RootState;
    type Action = RootAction;

    fn update(
        &mut self,
        state: &mut RootState,
        action: &RootAction,
        context: &mut FeatureContext<'_, RootState, RootAction>,
    ) {
        match action {
            RootAction::Noop => {}
            RootAction::Gate(gate, open) => context.set_input_open(gate, *open).unwrap(),
            RootAction::TryGate(gate, result) => {
                *result.lock().unwrap() = context.set_input_open(gate, false).err();
            }
            RootAction::ToggleChild(index) => {
                state.children[*index] = state.children[*index].is_none().then(ChildState::default);
            }
            RootAction::Replace(index) => state.replacement[*index] += 1,
            RootAction::Refresh(index) => state.configuration[*index] += 1,
            RootAction::DenyWrite(index) => state.deny_write[*index] = true,
        }
    }
}

struct Child {
    _owner: DropCount,
}
impl Feature for Child {
    type State = ChildState;
    type Action = ChildAction;

    fn update(
        &mut self,
        state: &mut ChildState,
        action: &ChildAction,
        _: &mut FeatureContext<'_, ChildState, ChildAction>,
    ) {
        let ChildAction::Add { command, .. } = action;
        state.total += command.amount.get();
    }
}

/// Counts real selection and construction separately for one of three sibling children.
fn mount<const INDEX: usize>(
    constructors: &Arc<AtomicUsize>,
    feature_drops: &Arc<AtomicUsize>,
) -> OptionalMount<RootState, Child, ()> {
    let constructors = Arc::clone(constructors);
    let feature_drops = Arc::clone(feature_drops);
    OptionalMount::new(
        |root: &RootState| root.children[INDEX].as_ref(),
        |root: &mut RootState| {
            root.writes[INDEX].fetch_add(1, Ordering::SeqCst);
            if root.deny_write[INDEX] {
                None
            } else {
                root.children[INDEX].as_mut()
            }
        },
        |root: &RootState| {
            root.children[INDEX].as_ref().map(|_| MountIdentity {
                key: (),
                replacement: root.replacement[INDEX],
                configuration: root.configuration[INDEX],
            })
        },
        move |_| {
            constructors.fetch_add(1, Ordering::SeqCst);
            Child {
                _owner: DropCount(Arc::clone(&feature_drops)),
            }
        },
    )
}

/// Owns a non-`Sync` capture in the actor and optionally panics after input consumption.
fn mapper(
    calls: &Arc<AtomicUsize>,
    mapper_drops: &Arc<AtomicUsize>,
    action_drops: &Arc<AtomicUsize>,
    panic: bool,
) -> impl FnMut(Command) -> ChildAction + Send + 'static {
    let calls = Arc::clone(calls);
    let owner = DropCount(Arc::clone(mapper_drops));
    let action_drops = Arc::clone(action_drops);
    // Cell is Send but not Sync: the mapper belongs to the actor, not the port.
    let count = Cell::new(0);
    move |command| {
        let _ = &owner;
        count.set(count.get() + 1);
        calls.fetch_add(1, Ordering::SeqCst);
        assert!(!panic, "deliberate command mapper panic");
        ChildAction::Add {
            command,
            _owner: DropCount(Arc::clone(&action_drops)),
        }
    }
}

/// Keeps weak ports and independent lifetime counters after the store can be dropped.
struct Fixture {
    driver: Driver,
    store: Store<RootState, RootAction>,
    mounts: [MountSite<RootState, ChildState, ChildAction, RootAction>; 3],
    sites: [CommandSite<RootState, Command>; 3],
    ports: [CommandPort<Command>; 3],
    maps: [Arc<AtomicUsize>; 3],
    constructors: Arc<AtomicUsize>,
    state_drops: Arc<AtomicUsize>,
    feature_drops: Arc<AtomicUsize>,
    mapper_drops: Arc<AtomicUsize>,
    action_drops: Arc<AtomicUsize>,
    writes: [Arc<AtomicUsize>; 3],
}

impl Fixture {
    /// Closes the real owner and checks actor/state cleanup despite retained weak handles.
    fn close(self) {
        drop(self.store);
        self.driver.turn();
        assert!(self.driver.0.lock().unwrap().is_empty());
        assert_eq!(self.state_drops.load(Ordering::SeqCst), 1);
    }
}

fn fixture() -> Fixture {
    fixture_with_panic(false)
}

fn fixture_with_panic(panic: bool) -> Fixture {
    fixture_with_mapping(panic, false)
}

/// Builds three sibling destinations sharing one slot; only the first can be control input.
fn fixture_with_mapping(panic: bool, control: bool) -> Fixture {
    let driver = Driver::default();
    let constructors = Arc::new(AtomicUsize::new(0));
    let state_drops = Arc::new(AtomicUsize::new(0));
    let feature_drops = Arc::new(AtomicUsize::new(0));
    let mapper_drops = Arc::new(AtomicUsize::new(0));
    let action_drops = Arc::new(AtomicUsize::new(0));
    let maps: [Arc<AtomicUsize>; 3] = std::array::from_fn(|_| Arc::new(AtomicUsize::new(0)));
    let writes: [Arc<AtomicUsize>; 3] = std::array::from_fn(|_| Arc::new(AtomicUsize::new(0)));
    let state = RootState {
        children: std::array::from_fn(|_| Some(ChildState::default())),
        replacement: [0; 3],
        configuration: [0; 3],
        writes: writes.clone(),
        deny_write: [false; 3],
        _owner: DropCount(Arc::clone(&state_drops)),
    };
    let builder = RootMount::new(Parent)
        .builder(state, Runtime::new(driver.clone()))
        .mailbox_capacity(1);
    let (builder, a, a_commands) = if control {
        builder.mount_with_control(
            mount::<0>(&constructors, &feature_drops),
            mapper(&maps[0], &mapper_drops, &action_drops, panic),
        )
    } else {
        builder.mount_with_command(
            mount::<0>(&constructors, &feature_drops),
            mapper(&maps[0], &mapper_drops, &action_drops, panic),
        )
    };
    let (builder, b, b_commands) = builder.mount_with_command(
        mount::<1>(&constructors, &feature_drops),
        mapper(&maps[1], &mapper_drops, &action_drops, panic),
    );
    let (builder, c, c_commands) = builder.mount_with_command(
        mount::<2>(&constructors, &feature_drops),
        mapper(&maps[2], &mapper_drops, &action_drops, panic),
    );
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    let sites = [a_commands, b_commands, c_commands];
    let ports = std::array::from_fn(|index| {
        driver
            .finish(store.command_port(&sites[index]))
            .unwrap()
            .unwrap()
    });
    Fixture {
        driver,
        store,
        mounts: [a, b, c],
        sites,
        ports,
        maps,
        constructors,
        state_drops,
        feature_drops,
        mapper_drops,
        action_drops,
        writes,
    }
}

/// Reads the live child through its public context rather than a fixture-side mirror.
fn total(fixture: &Fixture, index: usize) -> usize {
    let child = fixture
        .driver
        .finish(fixture.store.acquire(&fixture.mounts[index]))
        .unwrap()
        .unwrap();
    fixture
        .driver
        .finish(child.read(|state| state.total))
        .unwrap()
}

#[test]
fn declared_control_bypasses_only_input_closure_not_stale_authority() {
    let fixture = fixture_with_mapping(false, true);
    let gate = fixture
        .driver
        .finish(fixture.store.input_gate(&fixture.mounts[0]))
        .unwrap()
        .unwrap();
    let _ = fixture
        .driver
        .finish(fixture.store.send(RootAction::Gate(gate, false)))
        .unwrap();
    let drops = Arc::new(AtomicUsize::new(0));
    let _ = fixture
        .driver
        .finish(fixture.ports[0].send(Command::new(4, &drops)))
        .unwrap();
    assert_eq!(total(&fixture, 0), 4);
    let _ = fixture
        .driver
        .finish(fixture.store.send(RootAction::Replace(0)))
        .unwrap();
    let writes = fixture.writes[0].load(Ordering::SeqCst);
    let error = fixture
        .driver
        .finish(fixture.ports[0].send(Command::new(8, &drops)))
        .unwrap_err();
    let TaskSendError::Rejected { error, .. } = error else {
        panic!("stale control entered");
    };
    assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
    assert_eq!(fixture.writes[0].load(Ordering::SeqCst), writes);
    assert_eq!(fixture.maps[0].load(Ordering::SeqCst), 1);
    fixture.close();
}

enum ProbeAction {
    Start,
    Ordinary,
    Gate(InputGate, Arc<Mutex<Option<rca_core::TaskAccessError>>>),
    Export(Arc<Mutex<Option<rca_core::EffectSender<ProbeAction>>>>),
}
struct Probe(Arc<Mutex<Vec<&'static str>>>);
impl Feature for Probe {
    type State = usize;
    type Action = ProbeAction;

    fn update(
        &mut self,
        state: &mut usize,
        action: &ProbeAction,
        context: &mut FeatureContext<'_, usize, ProbeAction>,
    ) {
        match action {
            ProbeAction::Ordinary => *state += 100,
            ProbeAction::Gate(gate, result) => {
                *result.lock().unwrap() = context.set_input_open(gate, false).err();
            }
            ProbeAction::Export(export) => {
                let export = Arc::clone(export);
                context.effect(
                    rca_core::Effect::run(move |sender| async move {
                        *export.lock().unwrap() = Some(sender);
                        std::future::pending::<()>().await;
                    })
                    .cancellable("cancel-probe"),
                );
            }
            ProbeAction::Start => {
                let effects = Arc::clone(&self.0);
                context.effect(rca_core::Effect::run(move |sender| async move {
                    let error = sender.send(ProbeAction::Ordinary).await.unwrap_err();
                    assert!(error.is_input_closed());
                    assert!(!error.is_cancelled());
                    assert!(!error.is_store_stopped());
                    assert_eq!(
                        error.to_string().lines().next(),
                        Some("mounted input is closed")
                    );
                    effects
                        .lock()
                        .unwrap()
                        .push("effect refused, not cancelled");
                }));
                let tasks = Arc::clone(&self.0);
                context.task(rca_core::Task::run(move |context| async move {
                    let error = context.send(ProbeAction::Ordinary).await.unwrap_err();
                    let TaskSendError::Rejected { error, .. } = error else {
                        panic!("ordinary task entered");
                    };
                    assert_eq!(error.reason(), TaskAccessReason::InputClosed);
                    context.update(|state| *state += 1).await.unwrap();
                    assert_eq!(context.read(|state| *state).await.unwrap(), 1);
                    tasks.lock().unwrap().push("task continued");
                }));
            }
        }
    }
}

struct ProbeParent;
impl Feature for ProbeParent {
    type State = Option<usize>;
    type Action = (InputGate, bool);
    fn update(
        &mut self,
        _: &mut Self::State,
        action: &Self::Action,
        context: &mut FeatureContext<'_, Self::State, Self::Action>,
    ) {
        context.set_input_open(&action.0, action.1).unwrap();
    }
}

#[test]
fn control_work_does_not_inherit_bypass_and_input_refusal_does_not_cancel_tasks() {
    let driver = Driver::default();
    let results = Arc::new(Mutex::new(Vec::new()));
    let probe_results = Arc::clone(&results);
    let (builder, mount, commands) = RootMount::new(ProbeParent)
        .builder(Some(0), Runtime::new(driver.clone()))
        .mailbox_capacity(1)
        .mount_with_control(
            OptionalMount::new(
                |state: &Option<usize>| state.as_ref(),
                |state: &mut Option<usize>| state.as_mut(),
                |state: &Option<usize>| {
                    state.as_ref().map(|_| MountIdentity {
                        key: (),
                        replacement: 0,
                        configuration: 0,
                    })
                },
                move |_| Probe(Arc::clone(&probe_results)),
            ),
            |()| ProbeAction::Start,
        );
    let (builder, output) = builder.output::<Box<Cell<usize>>>(&mount, |handlers| {
        handlers.at_root(|state, value, _| {
            *state.as_mut().unwrap() += value.get();
            rca_core::OutputFlow::Consumed
        });
    });
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    let mut observation = driver
        .finish(store.observe_distinct(|state| *state))
        .unwrap();
    assert_eq!(driver.finish(observation.next()), Some(Some(0)));
    let gate = driver.finish(store.input_gate(&mount)).unwrap().unwrap();
    let child = driver.finish(store.acquire(&mount)).unwrap().unwrap();
    let self_gate = Arc::new(Mutex::new(None));
    let _ = driver
        .finish(child.send(ProbeAction::Gate(gate.clone(), Arc::clone(&self_gate))))
        .unwrap();
    assert_eq!(
        self_gate.lock().unwrap().unwrap().reason(),
        TaskAccessReason::ScopeEnded
    );
    let commands = driver
        .finish(store.command_port(&commands))
        .unwrap()
        .unwrap();
    let _ = driver.finish(store.send((gate.clone(), false))).unwrap();
    let task = driver.finish(commands.send(())).unwrap();
    driver.finish(task.finish());
    let mut actual = results.lock().unwrap().clone();
    actual.sort_unstable();
    assert_eq!(actual, ["effect refused, not cancelled", "task continued"]);
    assert_eq!(driver.finish(observation.next()), Some(Some(1)));
    let outcome = driver
        .finish(child.output(&output, Box::new(Cell::new(10))))
        .unwrap();
    assert!(matches!(outcome, rca_core::OutputDelivery::Consumed { .. }));
    assert_eq!(driver.finish(observation.next()), Some(Some(11)));

    // Actual causal cancellation remains a different refusal category.
    let _ = driver.finish(store.send((gate, true))).unwrap();
    let export = Arc::new(Mutex::new(None));
    let task = driver
        .finish(child.send(ProbeAction::Export(Arc::clone(&export))))
        .unwrap();
    driver.turn();
    let sender = export.lock().unwrap().take().unwrap();
    task.cancel();
    let error = driver
        .finish(sender.send(ProbeAction::Ordinary))
        .unwrap_err();
    assert!(error.is_cancelled());
    assert!(!error.is_input_closed());
    driver.finish(task.finish());
    drop(store);
    driver.turn();
    assert!(driver.0.lock().unwrap().is_empty());
}

#[test]
fn closed_input_returns_owned_direct_and_narrow_values_before_user_entry_then_reopens() {
    let fixture = fixture();
    let gate = fixture
        .driver
        .finish(fixture.store.input_gate(&fixture.mounts[0]))
        .unwrap()
        .unwrap();
    let child = fixture
        .driver
        .finish(fixture.store.acquire(&fixture.mounts[0]))
        .unwrap()
        .unwrap();
    let drops = Arc::new(AtomicUsize::new(0));
    let _ = fixture
        .driver
        .finish(fixture.ports[0].send(Command::new(2, &drops)))
        .unwrap();
    let _ = fixture
        .driver
        .finish(fixture.store.send(RootAction::Gate(gate.clone(), false)))
        .unwrap();
    let writes = fixture.writes[0].load(Ordering::SeqCst);
    let maps = fixture.maps[0].load(Ordering::SeqCst);

    let command = Command::new(9, &drops);
    let identity = command.identity();
    let error = fixture
        .driver
        .finish(fixture.ports[0].send(command))
        .unwrap_err();
    let TaskSendError::Rejected {
        action: command,
        error,
    } = error
    else {
        panic!("closed input must return the command");
    };
    assert_eq!(error.reason(), TaskAccessReason::InputClosed);
    assert_eq!(error.progress(), TaskProgress::NotStarted);
    assert_eq!(command.identity(), identity);
    let direct = Command::new(11, &drops);
    let direct_identity = direct.identity();
    let error = fixture
        .driver
        .finish(child.send(ChildAction::Add {
            command: direct,
            _owner: DropCount(Arc::clone(&fixture.action_drops)),
        }))
        .unwrap_err();
    let TaskSendError::Rejected { action, error } = error else {
        panic!("closed input must return the action");
    };
    assert_eq!(error.reason(), TaskAccessReason::InputClosed);
    let ChildAction::Add { command, .. } = action;
    assert_eq!(command.identity(), direct_identity);
    assert_eq!(fixture.writes[0].load(Ordering::SeqCst), writes);
    assert_eq!(fixture.maps[0].load(Ordering::SeqCst), maps);

    fixture
        .driver
        .finish(child.update(|state| state.total += 3))
        .unwrap();
    assert_eq!(
        fixture
            .driver
            .finish(child.read(|state| state.total))
            .unwrap(),
        5
    );
    let _ = fixture
        .driver
        .finish(fixture.store.send(RootAction::Gate(gate, true)))
        .unwrap();
    let _ = fixture
        .driver
        .finish(fixture.ports[0].send(Command::new(7, &drops)))
        .unwrap();
    assert_eq!(total(&fixture, 0), 12);
    assert_eq!(fixture.constructors.load(Ordering::SeqCst), 3);
    fixture.close();
}

#[test]
fn queued_gate_changes_use_actor_order_and_receipts_preserve_rejected_ownership() {
    let fixture = fixture();
    let gate = fixture
        .driver
        .finish(fixture.store.input_gate(&fixture.mounts[0]))
        .unwrap()
        .unwrap();
    let drops = Arc::new(AtomicUsize::new(0));
    let _ = fixture
        .driver
        .finish(fixture.store.send(RootAction::Gate(gate.clone(), false)))
        .unwrap();
    let reopen = fixture
        .store
        .try_send(RootAction::Gate(gate.clone(), true))
        .unwrap();
    // The send is constructed while closed and waits behind the queued reopen.
    let _ = fixture
        .driver
        .finish(fixture.ports[0].send(Command::new(5, &drops)))
        .unwrap();
    let _ = fixture.driver.finish(reopen.finish()).unwrap();
    let close = fixture
        .store
        .try_send(RootAction::Gate(gate, false))
        .unwrap();
    let command = Command::new(8, &drops);
    let identity = command.identity();
    let error = fixture
        .driver
        .finish(fixture.ports[0].send(command))
        .unwrap_err();
    let TaskSendError::Rejected { action, error } = error else {
        panic!("closed queued input entered");
    };
    assert_eq!(action.identity(), identity);
    assert_eq!(error.reason(), TaskAccessReason::InputClosed);
    let _ = fixture.driver.finish(close.finish()).unwrap();
    let receipt = fixture.ports[0].try_send(Command::new(9, &drops)).unwrap();
    let error = fixture.driver.finish(receipt.committed()).unwrap_err();
    assert!(
        matches!(error, TaskSendError::Rejected { error, .. } if error.reason() == TaskAccessReason::InputClosed)
    );
    assert_eq!(total(&fixture, 0), 5);
    assert_eq!(fixture.maps[0].load(Ordering::SeqCst), 1);
    fixture.close();
}

#[test]
fn foreign_and_refreshed_gates_cannot_close_current_input() {
    let fixture = fixture();
    let foreign = self::fixture();
    let foreign_gate = foreign
        .driver
        .finish(foreign.store.input_gate(&foreign.mounts[0]))
        .unwrap()
        .unwrap();
    let stale_gate = fixture
        .driver
        .finish(fixture.store.input_gate(&fixture.mounts[0]))
        .unwrap()
        .unwrap();
    let _ = fixture
        .driver
        .finish(fixture.store.send(RootAction::Refresh(0)))
        .unwrap();
    for gate in [foreign_gate.clone(), stale_gate] {
        let result = Arc::new(Mutex::new(None));
        let writes = fixture.writes[0].load(Ordering::SeqCst);
        let _ = fixture
            .driver
            .finish(
                fixture
                    .store
                    .send(RootAction::TryGate(gate, Arc::clone(&result))),
            )
            .unwrap();
        let error = result.lock().unwrap().unwrap();
        assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
        assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
        assert_eq!(fixture.writes[0].load(Ordering::SeqCst), writes);
    }
    foreign.driver.finish(foreign.store.shutdown()).unwrap();
    let acquisition = foreign
        .driver
        .finish(foreign.store.input_gate(&foreign.mounts[0]))
        .unwrap_err();
    assert_eq!(acquisition.reason(), TaskAccessReason::StoreClosed);
    assert_eq!(acquisition.progress(), TaskProgress::NotStarted);
    assert_eq!(foreign.state_drops.load(Ordering::SeqCst), 1);
    let mut foreign = Some(foreign);
    for expired in [false, true] {
        if expired {
            foreign.take().unwrap().close();
        }
        // First retain the closed lifecycle, then release its final owner.
        // The metadata-only gate must reject both without selecting our child.
        let result = Arc::new(Mutex::new(None));
        let writes = fixture.writes[0].load(Ordering::SeqCst);
        let _ = fixture
            .driver
            .finish(fixture.store.send(RootAction::TryGate(
                foreign_gate.clone(),
                Arc::clone(&result),
            )))
            .unwrap();
        let error = result.lock().unwrap().unwrap();
        assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
        assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
        assert_eq!(fixture.writes[0].load(Ordering::SeqCst), writes);
        assert_eq!(fixture.maps[0].load(Ordering::SeqCst), 0);
    }
    let current = fixture
        .driver
        .finish(fixture.store.command_port(&fixture.sites[0]))
        .unwrap()
        .unwrap();
    let drops = Arc::new(AtomicUsize::new(0));
    let _ = fixture
        .driver
        .finish(current.send(Command::new(6, &drops)))
        .unwrap();
    assert_eq!(total(&fixture, 0), 6);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    fixture.close();
}

#[test]
fn independent_owners_report_full_stale_and_admitted_without_losing_non_clone_commands() {
    let full = fixture();
    let stale = fixture();
    let live = fixture();
    let drops: [Arc<AtomicUsize>; 3] = std::array::from_fn(|_| Arc::new(AtomicUsize::new(0)));
    let commands = [
        Command::new(10, &drops[0]),
        Command::new(20, &drops[1]),
        Command::new(30, &drops[2]),
    ];
    let identities = commands.each_ref().map(Command::identity);
    let _queued = full.store.try_send(RootAction::Noop).unwrap();
    let _ = stale
        .driver
        .finish(stale.store.send(RootAction::Replace(0)))
        .unwrap();

    // Every destination is attempted before any receipt is awaited. Full cannot
    // suppress a later stale/live destination or imply all-or-nothing fan-out.
    let ports = [&full.ports[0], &stale.ports[0], &live.ports[0]];
    let attempts: Vec<_> = ports
        .into_iter()
        .zip(commands)
        .map(|(port, c)| port.try_send(c))
        .collect();
    assert_eq!(attempts.len(), 3);
    assert_eq!(live.maps[0].load(Ordering::SeqCst), 0);
    let mut attempts = attempts.into_iter();
    let full_error = attempts.next().unwrap().unwrap_err();
    let diagnostic = format!("{full_error:?}");
    assert!(diagnostic.starts_with("Full("));
    assert!(!diagnostic.contains("amount"));
    assert!(!diagnostic.contains(&identities[0].to_string()));
    assert!(full_error.to_string().contains("full"));
    assert!(std::error::Error::source(&full_error).is_none());
    let TryCommandError::Full(returned) = full_error else {
        panic!("first independently full owner must return its original command");
    };
    assert_eq!(returned.identity(), identities[0]);
    assert_eq!(returned.amount.get(), 10);
    assert_eq!(full.maps[0].load(Ordering::SeqCst), 0);
    drop(returned);
    let stale_receipt = attempts.next().unwrap().unwrap();
    let live_receipt = attempts.next().unwrap().unwrap();
    let diagnostic = format!("{live_receipt:?}");
    assert!(diagnostic.starts_with("CommandReceipt"));
    assert!(!diagnostic.contains("amount"));
    assert!(!diagnostic.contains(&identities[2].to_string()));
    let rejected = stale.driver.finish(stale_receipt.committed()).unwrap_err();
    match rejected {
        TaskSendError::Rejected { action, error } => {
            assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
            assert_eq!(error.progress(), TaskProgress::NotStarted);
            assert_eq!(action.identity(), identities[1]);
            assert_eq!(action.amount.get(), 20);
            drop(action);
        }
        TaskSendError::Failed { .. } => {
            panic!("metadata rejection must preserve command ownership")
        }
    }
    let task = live.driver.finish(live_receipt.committed()).unwrap();
    live.driver.finish(task.finish());
    assert_eq!(stale.maps[0].load(Ordering::SeqCst), 0);
    assert_eq!(live.maps[0].load(Ordering::SeqCst), 1);
    assert_eq!(full.action_drops.load(Ordering::SeqCst), 0);
    assert_eq!(stale.action_drops.load(Ordering::SeqCst), 0);
    assert_eq!(live.action_drops.load(Ordering::SeqCst), 1);
    assert_eq!(full.writes[0].load(Ordering::SeqCst), 0);
    assert_eq!(stale.writes[0].load(Ordering::SeqCst), 0);
    assert_eq!(live.writes[0].load(Ordering::SeqCst), 1);
    assert_eq!(total(&stale, 0), 0);
    assert_eq!(total(&live, 0), 30);
    assert_eq!(
        drops.each_ref().map(|count| count.load(Ordering::SeqCst)),
        [1, 1, 1]
    );
    full.close();
    stale.close();
    live.close();
}

#[test]
fn sibling_ports_share_one_root_capacity_and_dropped_receipt_does_not_retract_work() {
    let fixture = fixture();
    let drops: [Arc<AtomicUsize>; 3] = std::array::from_fn(|_| Arc::new(AtomicUsize::new(0)));
    let mut attempts = fixture
        .ports
        .iter()
        .enumerate()
        .map(|(index, port)| port.try_send(Command::new(index + 1, &drops[index])));
    let first = attempts.next().unwrap().unwrap();
    let second = attempts.next().unwrap();
    let third = attempts.next().unwrap();
    assert!(matches!(second, Err(TryCommandError::Full(_))));
    assert!(matches!(third, Err(TryCommandError::Full(_))));
    drop((second, third));
    drop(first);
    fixture.driver.turn();
    assert_eq!(total(&fixture, 0), 1);
    assert_eq!(total(&fixture, 1), 0);
    assert_eq!(total(&fixture, 2), 0);
    assert_eq!(
        fixture
            .maps
            .each_ref()
            .map(|calls| calls.load(Ordering::SeqCst)),
        [1, 0, 0]
    );
    assert_eq!(
        drops.each_ref().map(|count| count.load(Ordering::SeqCst)),
        [1, 1, 1]
    );
    fixture.close();
}

#[test]
fn refresh_and_replacement_require_fresh_ordered_acquisition() {
    let fixture = fixture();
    let _ = fixture
        .driver
        .finish(fixture.store.send(RootAction::ToggleChild(0)))
        .unwrap();
    assert!(
        fixture
            .driver
            .finish(fixture.store.input_gate(&fixture.mounts[0]))
            .unwrap()
            .is_none()
    );
    let _ = fixture
        .driver
        .finish(fixture.store.send(RootAction::ToggleChild(0)))
        .unwrap();
    let gate = fixture
        .driver
        .finish(fixture.store.input_gate(&fixture.mounts[0]))
        .unwrap()
        .unwrap();
    for open in [false, true] {
        let _ = fixture
            .driver
            .finish(fixture.store.send(RootAction::Gate(gate.clone(), open)))
            .unwrap();
    }
    assert_eq!(fixture.maps[0].load(Ordering::SeqCst), 0);
    let drops = Arc::new(AtomicUsize::new(0));
    for action in [RootAction::Refresh(0), RootAction::Replace(0)] {
        let old = fixture
            .driver
            .finish(fixture.store.command_port(&fixture.sites[0]))
            .unwrap()
            .unwrap();
        let before = fixture.constructors.load(Ordering::SeqCst);
        let refresh = matches!(action, RootAction::Refresh(_));
        let _ = fixture.driver.finish(fixture.store.send(action)).unwrap();
        assert_eq!(
            fixture.constructors.load(Ordering::SeqCst),
            before + usize::from(!refresh)
        );
        let calls = fixture.maps[0].load(Ordering::SeqCst);
        let rejected = fixture
            .driver
            .finish(old.send(Command::new(100, &drops)))
            .unwrap_err();
        match rejected {
            TaskSendError::Rejected { action, error } => {
                assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
                assert_eq!(error.progress(), TaskProgress::NotStarted);
                drop(action);
            }
            TaskSendError::Failed { .. } => panic!("stale port entered user code"),
        }
        assert_eq!(fixture.maps[0].load(Ordering::SeqCst), calls);
        let fresh = fixture
            .driver
            .finish(fixture.store.command_port(&fixture.sites[0]))
            .unwrap()
            .unwrap();
        let task = fixture
            .driver
            .finish(fresh.send(Command::new(7, &drops)))
            .unwrap();
        fixture.driver.finish(task.finish());
    }
    assert_eq!(total(&fixture, 0), 14);
    assert_eq!(drops.load(Ordering::SeqCst), 4);
    fixture.close();
}

#[test]
fn constructing_owned_send_is_lazy_and_pending_close_returns_the_original_command() {
    fn owned_send<T: Future + Send + 'static>(_: &T) {}

    let fixture = fixture();
    let drops = Arc::new(AtomicUsize::new(0));
    let lazy = {
        let cloned = fixture.ports[0].clone();
        cloned.send(Command::new(5, &drops))
    };
    owned_send(&lazy);
    assert_eq!(fixture.maps[0].load(Ordering::SeqCst), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(lazy);
    assert_eq!(drops.load(Ordering::SeqCst), 1);

    let _queued = fixture.store.try_send(RootAction::Noop).unwrap();
    let command = Command::new(9, &drops);
    let identity = command.identity();
    let mut pending = Box::pin(fixture.ports[0].send(command));
    assert!(poll(pending.as_mut()).is_pending());
    assert_eq!(fixture.maps[0].load(Ordering::SeqCst), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    drop(fixture.store);
    fixture.driver.turn();
    let failure = fixture.driver.finish(pending).unwrap_err();
    let diagnostic = format!("{failure:?}");
    assert!(diagnostic.starts_with("Rejected {"));
    assert!(diagnostic.contains("StoreClosed"));
    assert!(diagnostic.contains("NotStarted"));
    assert!(!diagnostic.contains("action:"));
    assert!(!diagnostic.contains("Command"));
    let display = failure.to_string();
    assert!(display.contains("StoreClosed"));
    assert!(display.contains("NotStarted"));
    match failure {
        TaskSendError::Rejected { action, error } => {
            assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
            assert_eq!(error.progress(), TaskProgress::NotStarted);
            assert_eq!(action.identity(), identity);
            assert_eq!(action.amount.get(), 9);
            drop(action);
        }
        TaskSendError::Failed { .. } => panic!("unadmitted command entered user code"),
    }
    assert_eq!(drops.load(Ordering::SeqCst), 2);
    assert_eq!(fixture.maps[0].load(Ordering::SeqCst), 0);
    assert!(fixture.driver.0.lock().unwrap().is_empty());
}

#[test]
fn first_send_after_last_store_drop_returns_command_before_actor_teardown() {
    let fixture = fixture();
    let drops = Arc::new(AtomicUsize::new(0));
    let command = Command::new(13, &drops);
    let identity = command.identity();
    drop(fixture.store);
    assert_eq!(fixture.state_drops.load(Ordering::SeqCst), 0);
    let mut send = Box::pin(fixture.ports[0].send(command));
    let Poll::Ready(Err(TaskSendError::Rejected { action, error })) = poll(send.as_mut()) else {
        panic!("a weak port must reject without waiting for actor teardown");
    };
    assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), TaskProgress::NotStarted);
    assert_eq!(action.identity(), identity);
    assert_eq!(action.amount.get(), 13);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.maps[0].load(Ordering::SeqCst), 0);
    assert_eq!(fixture.writes[0].load(Ordering::SeqCst), 0);
    drop(action);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    let command = Command::new(17, &drops);
    let identity = command.identity();
    let rejected = fixture.ports[0].try_send(command).unwrap_err();
    let cause = std::error::Error::source(&rejected)
        .expect("rejected admission exposes its retained access error")
        .downcast_ref::<rca_core::TaskAccessError>()
        .expect("the cause keeps its concrete error type");
    let TryCommandError::Rejected { error, .. } = &rejected else {
        panic!("a closed owner cannot report a full live mailbox");
    };
    assert!(std::ptr::eq(cause, error));
    assert!(std::error::Error::source(cause).is_none());
    let TryCommandError::Rejected { command, error } = rejected else {
        panic!("a closed owner cannot report a full live mailbox");
    };
    assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), TaskProgress::NotStarted);
    assert_eq!(command.identity(), identity);
    assert_eq!(command.amount.get(), 17);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.maps[0].load(Ordering::SeqCst), 0);
    assert_eq!(fixture.writes[0].load(Ordering::SeqCst), 0);
    assert_eq!(fixture.state_drops.load(Ordering::SeqCst), 0);
    drop(command);
    assert_eq!(drops.load(Ordering::SeqCst), 2);
    fixture.driver.turn();
    assert_eq!(fixture.state_drops.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.feature_drops.load(Ordering::SeqCst), 3);
    assert_eq!(fixture.mapper_drops.load(Ordering::SeqCst), 3);
    assert!(fixture.driver.0.lock().unwrap().is_empty());
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "isolated ownership fixture verifies mapping, shutdown and exact disposal together"
)]
fn mapper_closing_last_store_fails_without_reduction_or_returning_consumed_input() {
    struct Root;
    impl Feature for Root {
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
    struct Leaf {
        reductions: Arc<AtomicUsize>,
        _owner: DropCount,
    }
    impl Feature for Leaf {
        type State = usize;
        type Action = ChildAction;

        fn update(
            &mut self,
            state: &mut usize,
            action: &ChildAction,
            _: &mut FeatureContext<'_, usize, ChildAction>,
        ) {
            self.reductions.fetch_add(1, Ordering::SeqCst);
            let ChildAction::Add { command, .. } = action;
            *state += command.amount.get();
        }
    }

    let driver = Driver::default();
    let owner = Arc::new(Mutex::new(None::<Store<Option<usize>, ()>>));
    let maps = Arc::new(AtomicUsize::new(0));
    let reductions = Arc::new(AtomicUsize::new(0));
    let input_drops = Arc::new(AtomicUsize::new(0));
    let action_drops = Arc::new(AtomicUsize::new(0));
    let feature_drops = Arc::new(AtomicUsize::new(0));
    let mapper_drops = Arc::new(AtomicUsize::new(0));
    let child = OptionalMount::new(
        Option::as_ref,
        Option::as_mut,
        |state: &Option<usize>| {
            state.map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        {
            let reductions = Arc::clone(&reductions);
            let feature_drops = Arc::clone(&feature_drops);
            move |_| Leaf {
                reductions: Arc::clone(&reductions),
                _owner: DropCount(Arc::clone(&feature_drops)),
            }
        },
    );
    let (builder, _, commands) = RootMount::new(Root)
        .builder(Some(2), Runtime::new(driver.clone()))
        .mailbox_capacity(1)
        .mount_with_command(child, {
            let owner = Arc::clone(&owner);
            let maps = Arc::clone(&maps);
            let action_drops = Arc::clone(&action_drops);
            let mapper_owner = DropCount(Arc::clone(&mapper_drops));
            move |command: Command| {
                let _ = &mapper_owner;
                maps.fetch_add(1, Ordering::SeqCst);
                let action = ChildAction::Add {
                    command,
                    _owner: DropCount(Arc::clone(&action_drops)),
                };
                let store = owner.lock().unwrap().take().unwrap();
                drop(store);
                action
            }
        });
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    let port = driver
        .finish(store.command_port(&commands))
        .unwrap()
        .unwrap();
    *owner.lock().unwrap() = Some(store);
    let send = port.send(Command::new(7, &input_drops));
    assert_eq!(maps.load(Ordering::SeqCst), 0);
    let TaskSendError::Failed { error } = driver.finish(send).unwrap_err() else {
        panic!("a command consumed by its mapper cannot be returned");
    };
    assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), TaskProgress::MayHaveRun);
    driver.turn();
    assert_eq!(maps.load(Ordering::SeqCst), 1);
    assert_eq!(reductions.load(Ordering::SeqCst), 0);
    assert_eq!(input_drops.load(Ordering::SeqCst), 1);
    assert_eq!(action_drops.load(Ordering::SeqCst), 1);
    assert_eq!(feature_drops.load(Ordering::SeqCst), 1);
    assert_eq!(mapper_drops.load(Ordering::SeqCst), 1);
    assert!(owner.lock().unwrap().is_none());
    assert_eq!(Arc::strong_count(&owner), 1);
    assert!(driver.0.lock().unwrap().is_empty());
}

#[test]
fn mapper_panic_fails_after_entry_without_fabricating_a_returned_command() {
    let fixture = fixture_with_panic(true);
    let drops = Arc::new(AtomicUsize::new(0));
    let receipt = fixture.ports[0].try_send(Command::new(13, &drops)).unwrap();
    let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fixture.driver.turn()));
    assert!(unwind.is_err());
    let failure = fixture.driver.finish(receipt.committed()).unwrap_err();
    let diagnostic = format!("{failure:?}");
    assert!(diagnostic.starts_with("Failed {"));
    assert!(diagnostic.contains("ActorFailed"));
    assert!(diagnostic.contains("MayHaveRun"));
    assert!(!diagnostic.contains("action:"));
    let display = failure.to_string();
    assert!(display.contains("ActorFailed"));
    assert!(display.contains("MayHaveRun"));
    match failure {
        TaskSendError::Failed { error } => {
            assert_eq!(error.reason(), TaskAccessReason::ActorFailed);
            assert_eq!(error.progress(), TaskProgress::MayHaveRun);
        }
        TaskSendError::Rejected { .. } => {
            panic!("consumed command was fabricated after mapper entry")
        }
    }
    assert_eq!(fixture.maps[0].load(Ordering::SeqCst), 1);
    assert_eq!(fixture.action_drops.load(Ordering::SeqCst), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    fixture.close();
}

#[test]
fn retained_sites_and_cloned_ports_do_not_own_state_features_or_mappers() {
    let foreign = fixture();
    let fixture = fixture();
    let retained_port = fixture.ports[0].clone();
    let retained_site = fixture.sites[0].clone();
    for (diagnostic, kind) in [
        (format!("{retained_port:?}"), "CommandPort"),
        (format!("{retained_site:?}"), "CommandSite"),
    ] {
        assert!(diagnostic.starts_with(kind));
        assert!(!diagnostic.contains("endpoint"));
        assert!(!diagnostic.contains("declaration"));
    }
    drop(fixture.store);
    fixture.driver.turn();
    assert_eq!(fixture.state_drops.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.feature_drops.load(Ordering::SeqCst), 3);
    assert_eq!(fixture.mapper_drops.load(Ordering::SeqCst), 3);
    let drops = Arc::new(AtomicUsize::new(0));
    let rejected = retained_port.try_send(Command::new(1, &drops)).unwrap_err();
    let diagnostic = format!("{rejected:?}");
    assert!(diagnostic.starts_with("Rejected {"));
    assert!(diagnostic.contains("StoreClosed"));
    assert!(diagnostic.contains("NotStarted"));
    assert!(!diagnostic.contains("command:"));
    assert!(!diagnostic.contains("amount"));
    let display = rejected.to_string();
    assert!(display.contains("StoreClosed"));
    assert!(display.contains("NotStarted"));
    match rejected {
        TryCommandError::Rejected { command, error } => {
            assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
            assert_eq!(error.progress(), TaskProgress::NotStarted);
            drop(command);
        }
        TryCommandError::Full(_) => {
            panic!("closed owner cannot admit through a retained weak port")
        }
    }
    let acquisition = foreign
        .driver
        .finish(foreign.store.command_port(&retained_site))
        .unwrap_err();
    assert_eq!(acquisition.reason(), TaskAccessReason::ScopeEnded);
    assert_eq!(acquisition.progress(), TaskProgress::NotStarted);
    assert_eq!(foreign.maps[0].load(Ordering::SeqCst), 0);
    assert_eq!(total(&foreign, 0), 0);
    drop(retained_site);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert!(fixture.driver.0.lock().unwrap().is_empty());
    foreign.close();
}

#[test]
fn selector_none_is_after_entry_but_before_mapping_and_drops_command_once() {
    let fixture = fixture();
    let child = fixture
        .driver
        .finish(fixture.store.acquire(&fixture.mounts[0]))
        .unwrap()
        .unwrap();
    let _ = fixture
        .driver
        .finish(fixture.store.send(RootAction::DenyWrite(0)))
        .unwrap();
    let drops = Arc::new(AtomicUsize::new(0));
    match fixture
        .driver
        .finish(fixture.ports[0].send(Command::new(2, &drops)))
        .unwrap_err()
    {
        TaskSendError::Failed { error } => {
            assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
            assert_eq!(error.progress(), TaskProgress::MayHaveRun);
        }
        TaskSendError::Rejected { .. } => panic!("selector was already entered"),
    }
    assert_eq!(fixture.writes[0].load(Ordering::SeqCst), 1);
    assert_eq!(fixture.maps[0].load(Ordering::SeqCst), 0);
    assert_eq!(fixture.action_drops.load(Ordering::SeqCst), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    let failure = fixture
        .driver
        .finish(child.send(ChildAction::Add {
            command: Command::new(3, &drops),
            _owner: DropCount(Arc::clone(&fixture.action_drops)),
        }))
        .unwrap_err();
    let TaskSendError::Failed { error } = failure else {
        panic!("direct child action was already entered before projection failed");
    };
    assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
    assert_eq!(error.progress(), TaskProgress::MayHaveRun);
    assert_eq!(fixture.writes[0].load(Ordering::SeqCst), 2);
    assert_eq!(fixture.maps[0].load(Ordering::SeqCst), 0);
    assert_eq!(fixture.action_drops.load(Ordering::SeqCst), 1);
    assert_eq!(drops.load(Ordering::SeqCst), 2);
    assert_eq!(
        fixture
            .driver
            .finish(child.read(|state| state.total))
            .unwrap(),
        0
    );
    fixture.close();
}

#[test]
fn handles_and_errors_do_not_require_command_clone_debug_or_sync() {
    fn send_sync<T: Send + Sync>() {}
    fn clone_debug<T: Clone + std::fmt::Debug>() {}
    fn send<T: Send>() {}
    fn error<T: std::error::Error>() {}
    send_sync::<CommandPort<Command>>();
    clone_debug::<CommandPort<Command>>();
    clone_debug::<CommandSite<RootState, Command>>();
    send::<rca_core::CommandReceipt<Command>>();
    error::<TryCommandError<Command>>();
    // Even unconstructible payloads add no bounds to handle Clone/Debug.
    clone_debug::<CommandPort<std::rc::Rc<()>>>();
    clone_debug::<CommandSite<std::rc::Rc<()>, std::rc::Rc<()>>>();
    let payload = std::cell::Cell::new(0);
    let borrowed = TryCommandError::Full(&payload);
    assert!(std::error::Error::source(&borrowed).is_none());
}

#[cfg(feature = "allocation-measurement")]
/// Checks command phases in the dedicated allocation host.
///
/// # Panics
/// Panics if command behaviour or allocation counts change.
pub fn command_port_phase_costs() {
    /// Measures only the supplied operation; payload preparation stays outside the window.
    fn measure<T>(run: impl FnOnce() -> T) -> (T, (u64, u64)) {
        let before = dhat::HeapStats::get();
        let value = run();
        let after = dhat::HeapStats::get();
        (
            value,
            (
                after.total_blocks - before.total_blocks,
                after.total_bytes - before.total_bytes,
            ),
        )
    }
    if allocation_test::run_in_child("command_port_phase_costs") {
        return;
    }

    let profiler = dhat::Profiler::builder().testing().build();
    let fixture = fixture();
    let drops = Arc::new(AtomicUsize::new(0));
    let ((port, site), clones) = measure(|| (fixture.ports[0].clone(), fixture.sites[0].clone()));
    assert_eq!(clones, (0, 0));
    let command = Command::new(1, &drops);
    let (lazy, lazy_cost) = measure(|| port.send(command));
    let lazy_bytes = std::mem::size_of_val(&lazy);
    assert_eq!(lazy_cost, (0, 0));
    drop(lazy);
    let (acquire, acquire_creation) = measure(|| fixture.store.command_port(&site));
    assert_eq!(acquire_creation, (0, 0));
    drop(acquire);

    let queued = fixture.store.try_send(RootAction::Noop).unwrap();
    let command = Command::new(2, &drops);
    let (full, full_cost) = measure(|| port.try_send(command));
    assert!(matches!(full, Err(TryCommandError::Full(_))));
    assert_eq!(
        full_cost.0, 1,
        "Full creates one reply channel; immutable Arc ancestry clones allocate nothing"
    );
    drop(full);
    let _ = fixture.driver.finish(queued.finish()).unwrap();
    let mut samples = Vec::new();
    for value in [3, 4] {
        let command = Command::new(value, &drops);
        let (receipt, admission) = measure(|| port.try_send(command));
        let receipt = receipt.unwrap();
        assert_eq!(
            admission.0,
            full_cost.0 + if cfg!(feature = "test-util") { 2 } else { 1 },
            "admission adds one erased mapped-call Box and, with test-util, one typed inspection Arc"
        );
        // Includes the explicit headless Driver's Vec requeue allocation. This
        // boundary is not labelled as a production-only actor allocation count.
        let ((), driven) = measure(|| fixture.driver.turn());
        let task = fixture.driver.finish(receipt.committed()).unwrap();
        fixture.driver.finish(task.finish());
        samples.push((admission, driven));
    }
    assert_eq!(samples[0].0, samples[1].0, "same warmed admission envelope");
    assert_eq!(total(&fixture, 0), 7);
    eprintln!(
        "command port blocks/requested bytes: clones={clones:?}; lazy send={lazy_cost:?}; lazy acquisition={acquire_creation:?}; Full={full_cost:?}; cold/warm admitted+driven={samples:?}; inline port/site/receipt/send-future={}/{}/{}/{lazy_bytes}",
        std::mem::size_of::<CommandPort<Command>>(),
        std::mem::size_of::<CommandSite<RootState, Command>>(),
        std::mem::size_of::<rca_core::CommandReceipt<Command>>()
    );
    fixture.close();
    drop((port, site));
    drop(profiler);
}

/// Joins a fixed accepted set, excluding work admitted later or on replacement children.
mod finite_fanout {
    use super::*;
    use rca_core::{ActionTask, CommandReceipt, Task};

    /// Owns one input and a real channel wait whose task lifetime is counted separately.
    struct Work {
        command: Command,
        release: async_channel::Receiver<()>,
        started: Arc<AtomicUsize>,
        dropped: Arc<AtomicUsize>,
    }

    /// Releases one accepted task without owning its actor or command payload.
    struct Control {
        release: async_channel::Sender<()>,
        started: Arc<AtomicUsize>,
        dropped: Arc<AtomicUsize>,
        command_drops: Arc<AtomicUsize>,
    }

    /// Constructs one distinct command and release channel for each selected recipient.
    fn work(value: usize, constructors: &AtomicUsize) -> (Work, Control) {
        constructors.fetch_add(1, Ordering::SeqCst);
        let (send, receive) = async_channel::bounded(1);
        let started = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicUsize::new(0));
        let command_drops = Arc::new(AtomicUsize::new(0));
        (
            Work {
                command: Command::new(value, &command_drops),
                release: receive,
                started: Arc::clone(&started),
                dropped: Arc::clone(&dropped),
            },
            Control {
                release: send,
                started,
                dropped,
                command_drops,
            },
        )
    }

    struct Worker;
    impl Feature for Worker {
        type State = usize;
        type Action = Work;

        fn update(
            &mut self,
            state: &mut usize,
            action: &Work,
            context: &mut FeatureContext<'_, usize, Work>,
        ) {
            *state += action.command.amount.get();
            let release = action.release.clone();
            let started = Arc::clone(&action.started);
            let owner = DropCount(Arc::clone(&action.dropped));
            context.task(Task::run(move |context| async move {
                let _owner = owner;
                started.fetch_add(1, Ordering::SeqCst);
                release.recv().await.unwrap();
                context.update(|state| *state += 100).await.unwrap();
            }));
        }
    }

    struct Owner;
    impl Feature for Owner {
        type State = (Option<usize>, u64);
        type Action = ();

        fn update(
            &mut self,
            state: &mut Self::State,
            (): &(),
            _: &mut FeatureContext<'_, Self::State, ()>,
        ) {
            state.0 = Some(0);
            state.1 += 1;
        }
    }

    struct Recipient {
        store: Store<(Option<usize>, u64), ()>,
        site: CommandSite<(Option<usize>, u64), Work>,
        port: CommandPort<Work>,
    }

    fn recipient(driver: &Driver) -> Recipient {
        let (builder, _, site) = RootMount::new(Owner)
            .builder((Some(0), 0), Runtime::new(driver.clone()))
            .mailbox_capacity(1)
            .mount_with_command(
                OptionalMount::new(
                    |state: &(Option<usize>, u64)| state.0.as_ref(),
                    |state: &mut (Option<usize>, u64)| state.0.as_mut(),
                    |state: &(Option<usize>, u64)| {
                        state.0.as_ref().map(|_| MountIdentity {
                            key: (),
                            replacement: state.1,
                            configuration: 0,
                        })
                    },
                    |_| Worker,
                ),
                |command: Work| command,
            );
        let store = builder.build().unwrap();
        driver.finish(store.ready()).unwrap();
        let port = driver.finish(store.command_port(&site)).unwrap().unwrap();
        Recipient { store, site, port }
    }

    /// Captures a finite list and constructs one owned command per recipient.
    /// Full-mailbox and stale-owner recovery are checked above; these recipients have capacity.
    fn admit(
        ports: &[CommandPort<Work>],
        constructors: &AtomicUsize,
    ) -> Vec<(CommandReceipt<Work>, Control)> {
        ports
            .iter()
            .enumerate()
            .map(|(index, port)| {
                let (command, control) = work(index + 1, constructors);
                (port.try_send(command).unwrap(), control)
            })
            .collect()
    }

    /// Waits only for the already captured causal tasks, never later owner activity.
    async fn finish_captured(tasks: &[ActionTask]) {
        for task in tasks {
            task.finish().await;
        }
    }

    #[test]
    fn empty_group_constructs_nothing_and_finishes_without_work() {
        let constructors = AtomicUsize::new(0);
        let attempts = admit(&[], &constructors);
        assert!(attempts.is_empty());
        assert_eq!(constructors.load(Ordering::SeqCst), 0);
        let mut finish = Box::pin(finish_captured(&[]));
        assert!(poll(finish.as_mut()).is_ready());
    }

    #[test]
    fn captured_completion_joins_cancelled_work_not_later_or_replacement_work() {
        let driver = Driver::default();
        let first = recipient(&driver);
        let second = recipient(&driver);
        let ports = [first.port.clone(), second.port.clone()];
        let constructors = AtomicUsize::new(0);
        let attempts = admit(&ports, &constructors);
        assert_eq!(constructors.load(Ordering::SeqCst), 2);
        let (captured, controls): (Vec<_>, Vec<_>) = attempts
            .into_iter()
            .map(|(receipt, control)| (driver.finish(receipt.committed()).unwrap(), control))
            .unzip();
        driver.turn();
        assert_eq!(
            controls
                .iter()
                .map(|control| control.started.load(Ordering::SeqCst))
                .collect::<Vec<_>>(),
            [1, 1],
        );
        let mut finish = Box::pin(finish_captured(&captured));
        assert!(poll(finish.as_mut()).is_pending());

        // Complete one captured member normally, including its actor update.
        controls[0].release.try_send(()).unwrap();
        driver.finish(captured[0].finish());
        assert_eq!(
            driver.finish(first.store.read(|state| state.0)).unwrap(),
            Some(101)
        );
        assert_eq!(controls[0].dropped.load(Ordering::SeqCst), 1);
        assert!(poll(finish.as_mut()).is_pending());

        // Later work on the surviving owner and on a fresh replacement is not
        // added to this captured set. Neither is cancelled with its old member.
        let (command, unrelated_control) = work(7, &constructors);
        let unrelated = driver.finish(second.port.send(command)).unwrap();
        let _ = driver.finish(first.store.send(())).unwrap();
        let replacement_port = driver
            .finish(first.store.command_port(&first.site))
            .unwrap()
            .unwrap();
        let (command, replacement_control) = work(11, &constructors);
        let replacement = driver.finish(replacement_port.send(command)).unwrap();
        driver.turn();
        assert_eq!(unrelated_control.started.load(Ordering::SeqCst), 1);
        assert_eq!(replacement_control.started.load(Ordering::SeqCst), 1);
        assert!(poll(finish.as_mut()).is_pending());

        captured[1].cancel();
        assert!(captured[1].is_cancelled());
        assert!(!captured[1].is_finished());
        assert_eq!(controls[1].dropped.load(Ordering::SeqCst), 0);
        assert!(poll(finish.as_mut()).is_pending());
        driver.finish(finish);
        assert_eq!(controls[1].dropped.load(Ordering::SeqCst), 1);
        assert!(!captured[0].is_cancelled());
        assert!(captured.iter().all(ActionTask::is_finished));

        for (task, control) in [
            (&unrelated, &unrelated_control),
            (&replacement, &replacement_control),
        ] {
            assert!(!task.is_finished());
            assert!(!task.is_cancelled());
            assert_eq!(control.dropped.load(Ordering::SeqCst), 0);
            let mut pending = Box::pin(task.finish());
            assert!(poll(pending.as_mut()).is_pending());
        }
        assert_eq!(
            driver.finish(first.store.read(|state| state.0)).unwrap(),
            Some(11)
        );
        assert_eq!(
            driver.finish(second.store.read(|state| state.0)).unwrap(),
            Some(9)
        );
        for control in controls
            .iter()
            .chain([&unrelated_control, &replacement_control])
        {
            assert_eq!(control.command_drops.load(Ordering::SeqCst), 1);
        }

        // Explicit cleanup is outside the captured group's completion.
        unrelated.cancel();
        replacement.cancel();
        driver.finish(finish_captured(&[unrelated, replacement]));
        assert_eq!(unrelated_control.dropped.load(Ordering::SeqCst), 1);
        assert_eq!(replacement_control.dropped.load(Ordering::SeqCst), 1);
        drop((first, second));
        driver.turn();
        assert!(driver.0.lock().unwrap().is_empty());
    }
}
