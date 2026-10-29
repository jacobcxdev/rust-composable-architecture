//! Owned original-command failure and retirement boundaries through public APIs.
use rca_core::{
    ActionTask, Feature, FeatureContext, MountIdentity, OptionalMount, Runtime, RuntimeDriver,
    RuntimeFuture, Store, TaskAccessReason, TaskProgress, TaskSendError, TryCommandError,
};
use std::pin::Pin;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

/// Manually polls a frozen-clock actor; yielding still suspends for one turn.
#[derive(Clone, Default)]
struct Driver(Arc<Mutex<Vec<RuntimeFuture>>>);
impl std::fmt::Debug for Driver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CommandFailureDriver")
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
        panic!("bounded failure schedule progress")
    }
}
type Trace = Arc<Mutex<Vec<&'static str>>>;
/// Retains the actual send future so destruction can check terminal publication order.
type Reply = Pin<Box<dyn Future<Output = Result<ActionTask, TaskSendError<Input>>> + Send>>;
/// Shares only the receipt probe; tests remove it before ordinary final input disposal.
type Watch = Arc<Mutex<Option<Reply>>>;
/// Its destructor verifies that entered-input disposal precedes terminal reply readiness.
struct Input {
    payload: Box<usize>,
    trace: Trace,
    reply: Watch,
}
impl Drop for Input {
    fn drop(&mut self) {
        self.trace.lock().unwrap().push("input drop");
        let mut reply = self.reply.lock().unwrap();
        if let Some(reply) = reply.as_mut() {
            assert!(
                reply
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending(),
                "original C must be destroyed before terminal reply publication"
            );
        }
    }
}
/// Makes action destruction a separate failure point after mapping has consumed input.
struct Action {
    trace: Trace,
    mode: u8,
}
impl Drop for Action {
    fn drop(&mut self) {
        self.trace.lock().unwrap().push("action drop");
        assert_ne!(self.mode, 5, "action destructor panic");
    }
}
/// Traces mapper retirement independently of command and child-feature disposal.
struct MapperCapture(Trace);
impl Drop for MapperCapture {
    fn drop(&mut self) {
        self.0.lock().unwrap().push("mapper drop");
    }
}
struct Parent;
struct State {
    child: Option<usize>,
    configuration: u64,
    replacement: u64,
    /// 1: no projection, 2: selector panic, 3: mapper panic, 4: reducer panic,
    /// 5: action-drop panic, 6: selector closes Store, 7: mapper closes Store.
    /// Zero is the normal path.
    mode: Arc<AtomicU8>,
    trace: Trace,
    owner: Arc<Mutex<Option<Store<State, ParentAction>>>>,
}
enum ParentAction {
    Configure,
    Replace,
}
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
            ParentAction::Configure => state.configuration += 1,
            ParentAction::Replace => {
                state.replacement += 1;
                state.child = Some(0);
            }
        }
    }
}
struct Child(Trace);
impl Drop for Child {
    fn drop(&mut self) {
        self.0.lock().unwrap().push("feature drop");
    }
}
impl Feature for Child {
    type State = usize;
    type Action = Action;
    fn reduce(
        &mut self,
        state: &mut usize,
        action: &Action,
        _: &mut FeatureContext<'_, usize, Action>,
    ) {
        self.0.lock().unwrap().push("reduce");
        assert_ne!(action.mode, 4, "reducer panic");
        *state += 1;
    }
}
/// Applies the selected real projection failure or owner closure before mapping.
fn select(state: &mut State) -> Option<&mut usize> {
    state.trace.lock().unwrap().push("select");
    match state.mode.load(Ordering::SeqCst) {
        1 => return None,
        2 => panic!("selector panic"),
        6 => drop(state.owner.lock().unwrap().take()),
        _ => {}
    }
    state.child.as_mut()
}
type Site = rca_core::CommandSite<State, Input>;
/// Returns independent admission, trace and failure controls for one real actor.
#[expect(
    clippy::type_complexity,
    reason = "The fixture keeps independently owned Store, typed sites, trace and failure controls explicit"
)]
fn fixture(
    capacity: usize,
) -> (
    Driver,
    Store<State, ParentAction>,
    Arc<Mutex<Vec<Site>>>,
    Trace,
    Arc<AtomicU8>,
) {
    let driver = Driver::default();
    let trace = Arc::new(Mutex::new(Vec::new()));
    let mode = Arc::new(AtomicU8::new(0));
    let sites = Arc::new(Mutex::new(Vec::new()));
    let declared = Arc::clone(&sites);
    let constructor_trace = Arc::clone(&trace);
    let mapped = Arc::clone(&trace);
    let mapper_mode = Arc::clone(&mode);
    let owner = Arc::new(Mutex::new(None));
    let mapper_owner = Arc::clone(&owner);
    let descriptor = OptionalMount::new(
        |state: &State| state.child.as_ref(),
        select,
        |state: &State| {
            state.child.map(|_| MountIdentity {
                key: (),
                replacement: state.replacement,
                configuration: state.configuration,
            })
        },
        move |_| Child(Arc::clone(&constructor_trace)),
    );
    let (builder, _) = Store::feature_builder(
        State {
            child: Some(0),
            configuration: 0,
            replacement: 0,
            mode: Arc::clone(&mode),
            trace: Arc::clone(&trace),
            owner,
        },
        Parent,
        Runtime::new(driver.clone()),
    )
    .mailbox_capacity(capacity)
    .mount_with_children(descriptor, move |_, children| {
        let capture = MapperCapture(Arc::clone(&mapped));
        let mapper_mode = Arc::clone(&mapper_mode);
        let mapper_owner = Arc::clone(&mapper_owner);
        declared
            .lock()
            .unwrap()
            .push(children.command(move |input: Input| {
                capture.0.lock().unwrap().push("map");
                let mode = mapper_mode.load(Ordering::SeqCst);
                assert_ne!(mode, 3, "mapper panic");
                let trace = Arc::clone(&input.trace);
                drop(input);
                let action = Action { trace, mode };
                if mode == 7 {
                    let store = mapper_owner.lock().unwrap().take().unwrap();
                    drop(store);
                }
                action
            }));
    });
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    (driver, store, sites, trace, mode)
}

#[test]
fn entered_failures_drop_original_input_before_exact_terminal_reply() {
    // 1: selector None; 2: selector panic; 3: mapper panic; 4: reducer panic;
    // 5: mapped action destructor panic; 6: selector closes Store; 7: mapper closes Store.
    for mode_value in 1..=7 {
        let (driver, store, sites, trace, mode) = fixture(rca_core::DEFAULT_MAILBOX_CAPACITY);
        let site = sites.lock().unwrap()[0].clone();
        let port = driver.finish(store.command_port(&site)).unwrap().unwrap();
        // The selector or mapper can release this actual final Store owner.
        let owner = driver
            .finish(store.read(|state| Arc::clone(&state.owner)))
            .unwrap();
        *owner.lock().unwrap() = Some(store);
        mode.store(mode_value, Ordering::SeqCst);
        trace.lock().unwrap().clear();
        let reply: Watch = Arc::new(Mutex::new(None));
        *reply.lock().unwrap() = Some(Box::pin(port.send(Input {
            payload: Box::new(0),
            trace: Arc::clone(&trace),
            reply: Arc::clone(&reply),
        })));
        assert!(
            reply
                .lock()
                .unwrap()
                .as_mut()
                .unwrap()
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| driver.turn()));
        assert_eq!(result.is_err(), matches!(mode_value, 2..=5));
        let terminal = reply.lock().unwrap().take().unwrap();
        let Err(TaskSendError::Failed { error }) = driver.finish(terminal) else {
            panic!("entered command failure cannot recover original C");
        };
        assert_eq!(error.progress(), TaskProgress::MayHaveRun);
        if mode_value == 1 {
            assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
        }
        if matches!(mode_value, 6 | 7) {
            assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
        }
        drop(owner.lock().unwrap().take());
        let trace = trace.lock().unwrap();
        assert_eq!(
            trace.iter().filter(|event| **event == "input drop").count(),
            1
        );
        assert_eq!(
            trace.iter().filter(|event| **event == "map").count(),
            usize::from(matches!(mode_value, 3..=5 | 7))
        );
        assert_eq!(
            trace.iter().filter(|event| **event == "reduce").count(),
            usize::from(matches!(mode_value, 4..=5))
        );
        if mode_value == 7 {
            assert_eq!(
                trace
                    .iter()
                    .filter(|event| **event == "action drop")
                    .count(),
                1
            );
        }
        drop(trace);
        driver.turn();
    }
}

#[test]
fn configuration_retains_mapper_but_replacement_retires_queued_acquisition_and_capture() {
    let (driver, store, sites, trace, _) = fixture(rca_core::DEFAULT_MAILBOX_CAPACITY);
    let site = sites.lock().unwrap()[0].clone();
    let old = driver.finish(store.command_port(&site)).unwrap().unwrap();
    drop(driver.finish(store.send(ParentAction::Configure)).unwrap());
    let reply = Arc::new(Mutex::new(None));
    match driver.finish(old.send(Input {
        payload: Box::new(0),
        trace: Arc::clone(&trace),
        reply,
    })) {
        Err(TaskSendError::Rejected { action, error }) => {
            assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
            drop(action);
        }
        _ => panic!("configuration must invalidate previously acquired authority"),
    }
    assert_eq!(sites.lock().unwrap().len(), 1);
    assert!(!trace.lock().unwrap().contains(&"map"));
    assert!(!trace.lock().unwrap().contains(&"mapper drop"));
    let fresh = driver.finish(store.command_port(&site)).unwrap().unwrap();
    drop(
        driver
            .finish(fresh.send(Input {
                payload: Box::new(0),
                trace: Arc::clone(&trace),
                reply: Arc::new(Mutex::new(None)),
            }))
            .unwrap(),
    );
    let mut replace = Box::pin(store.send(ParentAction::Replace));
    assert!(
        replace
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    let mut acquisition = Box::pin(store.command_port(&site));
    assert!(
        acquisition
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    drop(driver.finish(replace).unwrap());
    assert!(
        matches!(driver.finish(acquisition), Err(error) if error.reason() == TaskAccessReason::ScopeEnded)
    );
    assert_eq!(sites.lock().unwrap().len(), 2);
    assert_eq!(
        trace
            .lock()
            .unwrap()
            .iter()
            .filter(|event| **event == "mapper drop")
            .count(),
        1
    );
    assert_eq!(
        trace
            .lock()
            .unwrap()
            .iter()
            .filter(|event| **event == "feature drop")
            .count(),
        1
    );
    drop((old, fresh));
    assert_eq!(
        trace
            .lock()
            .unwrap()
            .iter()
            .filter(|event| **event == "mapper drop")
            .count(),
        1
    );
    driver.finish(store.shutdown()).unwrap();
}

#[test]
fn recipe_port_rejects_original_box_after_final_store_close_before_admission() {
    let (driver, store, sites, trace, _) = fixture(1);
    let site = sites.lock().unwrap()[0].clone();
    let port = driver.finish(store.command_port(&site)).unwrap().unwrap();
    let payload = Box::new(17);
    let identity = (&raw const *payload) as usize;
    drop(store);

    let Err(TaskSendError::Rejected { action, error }) = driver.finish(port.send(Input {
        payload,
        trace: Arc::clone(&trace),
        reply: Arc::new(Mutex::new(None)),
    })) else {
        panic!("closed recipe port must return its original input before mapping");
    };
    assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), TaskProgress::NotStarted);
    assert_eq!((&raw const *action.payload) as usize, identity);
    assert_eq!(*action.payload, 17);
    assert!(!trace.lock().unwrap().contains(&"map"));
    assert!(!trace.lock().unwrap().contains(&"reduce"));
    drop(action);
    driver.turn();
    let trace = trace.lock().unwrap();
    assert_eq!(
        trace.iter().filter(|event| **event == "input drop").count(),
        1
    );
    assert_eq!(
        trace
            .iter()
            .filter(|event| **event == "mapper drop")
            .count(),
        1
    );
    assert!(driver.0.lock().unwrap().is_empty());
}

#[test]
fn recipe_port_and_acquisition_waiting_on_full_mailbox_close_without_mapping() {
    let (driver, store, sites, trace, _) = fixture(1);
    let site = sites.lock().unwrap()[0].clone();
    let port = driver.finish(store.command_port(&site)).unwrap().unwrap();
    let Ok(queued) = store.try_send(ParentAction::Configure) else {
        panic!("the root action must fill the one-place mailbox");
    };
    let Err(full) = store.try_send(ParentAction::Configure) else {
        panic!("the occupied one-place mailbox must reject another root action");
    };
    assert!(full.is_full());
    drop(full);
    let mut acquisition = Box::pin(store.command_port(&site));
    assert!(
        acquisition
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    let payload = Box::new(23);
    let identity = (&raw const *payload) as usize;
    let mut pending = Box::pin(port.send(Input {
        payload,
        trace: Arc::clone(&trace),
        reply: Arc::new(Mutex::new(None)),
    }));
    assert!(
        pending
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    assert!(!trace.lock().unwrap().contains(&"map"));
    drop(store);
    let Poll::Ready(Err(error)) = acquisition
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    else {
        panic!("ordinary acquisition must reject transport closure without an actor turn");
    };
    assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), TaskProgress::NotStarted);
    assert_eq!(sites.lock().unwrap().len(), 1);
    assert!(!trace.lock().unwrap().contains(&"mapper drop"));
    drop(acquisition);
    driver.turn();

    let Err(TaskSendError::Rejected { action, error }) = driver.finish(pending) else {
        panic!("closing a full recipe mailbox must return the unentered input");
    };
    assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), TaskProgress::NotStarted);
    assert_eq!((&raw const *action.payload) as usize, identity);
    assert_eq!(*action.payload, 23);
    assert!(!trace.lock().unwrap().contains(&"map"));
    assert!(!trace.lock().unwrap().contains(&"reduce"));
    drop(action);
    drop(queued);
    driver.turn();
    let trace = trace.lock().unwrap();
    assert_eq!(
        trace.iter().filter(|event| **event == "input drop").count(),
        1
    );
    assert_eq!(
        trace
            .iter()
            .filter(|event| **event == "mapper drop")
            .count(),
        1
    );
    assert!(driver.0.lock().unwrap().is_empty());
}

#[test]
fn recipe_port_try_send_admits_once_and_returns_full_and_closed_originals() {
    let (driver, store, sites, trace, _) = fixture(1);
    let site = sites.lock().unwrap()[0].clone();
    let port = driver.finish(store.command_port(&site)).unwrap().unwrap();
    trace.lock().unwrap().clear();

    let first = port
        .try_send(Input {
            payload: Box::new(31),
            trace: Arc::clone(&trace),
            reply: Arc::new(Mutex::new(None)),
        })
        .unwrap();
    let full_payload = Box::new(37);
    let full_identity = (&raw const *full_payload) as usize;
    let Err(TryCommandError::Full(full)) = port.try_send(Input {
        payload: full_payload,
        trace: Arc::clone(&trace),
        reply: Arc::new(Mutex::new(None)),
    }) else {
        panic!("a full recipe mailbox must return the unentered input");
    };
    assert_eq!((&raw const *full.payload) as usize, full_identity);
    assert_eq!(*full.payload, 37);
    assert!(!trace.lock().unwrap().contains(&"map"));
    drop(full);

    let task = driver.finish(first.committed()).unwrap();
    driver.finish(task.finish());
    assert!(task.is_finished());
    assert_eq!(
        driver.finish(store.read(|state| state.child)).unwrap(),
        Some(1)
    );
    let events = trace.lock().unwrap();
    assert_eq!(events.iter().filter(|event| **event == "map").count(), 1);
    assert_eq!(events.iter().filter(|event| **event == "reduce").count(), 1);
    assert_eq!(
        events
            .iter()
            .filter(|event| **event == "input drop")
            .count(),
        2
    );
    drop(events);

    drop(store);
    let closed_payload = Box::new(41);
    let closed_identity = (&raw const *closed_payload) as usize;
    let Err(TryCommandError::Rejected { command, error }) = port.try_send(Input {
        payload: closed_payload,
        trace: Arc::clone(&trace),
        reply: Arc::new(Mutex::new(None)),
    }) else {
        panic!("a closed recipe port must return the unentered input");
    };
    assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), TaskProgress::NotStarted);
    assert_eq!((&raw const *command.payload) as usize, closed_identity);
    assert_eq!(*command.payload, 41);
    drop(command);
    driver.turn();
    let events = trace.lock().unwrap();
    assert_eq!(events.iter().filter(|event| **event == "map").count(), 1);
    assert_eq!(events.iter().filter(|event| **event == "reduce").count(), 1);
    assert_eq!(
        events
            .iter()
            .filter(|event| **event == "input drop")
            .count(),
        3
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| **event == "mapper drop")
            .count(),
        1
    );
    drop(events);
    assert!(driver.0.lock().unwrap().is_empty());
}
