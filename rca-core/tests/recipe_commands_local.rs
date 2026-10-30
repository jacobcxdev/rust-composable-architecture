#![cfg(feature = "local")]
//! Owner-local commands retain non-Send payloads and mapper captures.
use rca_core::local::{
    Feature, FeatureContext, OptionalMount, Runtime, RuntimeDriver, RuntimeFuture, Store,
};
use rca_core::{MountIdentity, TaskAccessReason, TaskSendError};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

/// Polls owner-local actor tasks manually against a fixed zero clock.
#[derive(Clone, Default)]
struct Driver(Rc<RefCell<Vec<RuntimeFuture>>>);
impl std::fmt::Debug for Driver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LocalCommandDriver")
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
fn local_command_driver_honours_elapsed_and_future_sleep() {
    let driver = Driver::default();
    let mut context = Context::from_waker(Waker::noop());
    let mut elapsed = driver.sleep_until(driver.now());
    assert!(elapsed.as_mut().poll(&mut context).is_ready());
    for deadline in [
        driver
            .now()
            .saturating_add(std::time::Duration::from_nanos(1)),
        rca_core::MonotonicInstant::MAX,
    ] {
        let mut future = driver.sleep_until(deadline);
        assert!(future.as_mut().poll(&mut context).is_pending());
    }
    assert!(driver.0.borrow().is_empty());
}

#[test]
fn local_command_driver_yields_once_and_wakes_once() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Wake;

    /// Counts scheduling notifications without polling the actor from the waker.
    struct WakeCount(AtomicUsize);
    impl Wake for WakeCount {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    let driver = Driver::default();
    let count = Arc::new(WakeCount(AtomicUsize::new(0)));
    let waker = Waker::from(Arc::clone(&count));
    let mut context = Context::from_waker(&waker);
    let mut yield_once = driver.yield_now();
    assert!(yield_once.as_mut().poll(&mut context).is_pending());
    assert_eq!(count.0.load(Ordering::SeqCst), 1);
    assert!(yield_once.as_mut().poll(&mut context).is_ready());
    assert_eq!(count.0.load(Ordering::SeqCst), 1);
    assert!(driver.0.borrow().is_empty());
}
impl Driver {
    /// Polls a finite request and each queued task for at most 32 turns.
    fn finish<F: Future>(&self, future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        for _ in 0..32 {
            if let Poll::Ready(value) = future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
            {
                return value;
            }
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
        panic!("bounded owner-local command progress")
    }
}
/// Keeps the optional child projection separate from its installed recipe.
struct State {
    child: Option<Rc<Cell<usize>>>,
}
/// Carries the actual child gate into the root reducer that closes ordinary input.
struct Close(rca_core::InputGate, Rc<()>);
struct Parent;
impl Feature for Parent {
    type State = State;
    type Action = Close;
    fn update(
        &mut self,
        _: &mut State,
        action: &Close,
        context: &mut FeatureContext<'_, State, Close>,
    ) {
        let _ = &action.1;
        context.set_input_open(&action.0, false).unwrap();
    }
}
/// Counts only reductions that reach the installed child.
struct Child(Rc<Cell<usize>>);
impl Feature for Child {
    type State = Rc<Cell<usize>>;
    type Action = Rc<usize>;
    fn update(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _: &mut FeatureContext<'_, Self::State, Self::Action>,
    ) {
        self.0.set(self.0.get() + 1);
        state.set(state.get() + **action);
    }
}
/// Retains the original non-Send input until mapping or typed rejection.
struct Input(Rc<Cell<usize>>);
/// Uses the same installed child while bypassing its ordinary input gate.
struct Control(Rc<Cell<usize>>);

#[test]
fn local_task_debug_redacts_capture_without_starting_work() {
    use rca_core::local::Task;

    let payload = Rc::new(String::from("private-local-task-payload"));
    let weak = Rc::downgrade(&payload);
    let started = Rc::new(Cell::new(false));
    let run_started = Rc::clone(&started);
    let task = Task::<State, Close>::run(move |_| {
        run_started.set(true);
        async move {
            drop(payload);
        }
    });

    for debug in [format!("{task:?}"), format!("{task:#?}")] {
        assert!(debug.starts_with("Task"));
        assert!(debug.contains(".."));
        assert!(!debug.contains("private-local-task-payload"));
    }
    assert!(!started.get());
    assert!(weak.upgrade().is_some());
    drop(task);
    assert!(!started.get());
    assert!(weak.upgrade().is_none());
    assert_eq!(Rc::strong_count(&started), 1);
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One owner-local port lifetime checks shared capacity, input gating and original payload recovery after closure."
)]
fn local_same_child_ports_accept_live_inputs_and_return_closed_originals() {
    let driver = Driver::default();
    let sites = Rc::new(RefCell::new(Vec::new()));
    let declared = std::rc::Rc::clone(&sites);
    let maps = Rc::new(Cell::new(0));
    let mapped = std::rc::Rc::clone(&maps);
    let reductions = Rc::new(Cell::new(0));
    let reduced = std::rc::Rc::clone(&reductions);
    let factories = Rc::new(Cell::new(0));
    let made = Rc::clone(&factories);
    let descriptor = OptionalMount::new(
        |state: &State| state.child.as_ref(),
        |state: &mut State| state.child.as_mut(),
        |state: &State| {
            state.child.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| {
            made.set(made.get() + 1);
            Child(std::rc::Rc::clone(&reduced))
        },
    );
    let (builder, child_site) = Store::builder(
        State {
            child: Some(Rc::new(Cell::new(0))),
        },
        Parent,
        Runtime::new(driver.clone()),
    )
    .mailbox_capacity(1)
    .mount_with_children(descriptor, move |_, children| {
        let mapped = std::rc::Rc::clone(&mapped);
        let ordinary = children.command(move |input: Input| {
            mapped.set(mapped.get() + 1);
            Rc::new(input.0.get())
        });
        let control = children.control(|input: Control| Rc::new(input.0.get()));
        declared.borrow_mut().push((ordinary, control));
    });
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    let child = driver.finish(store.acquire(&child_site)).unwrap().unwrap();
    for debug in [format!("{child:?}"), format!("{child:#?}")] {
        assert!(debug.starts_with("TaskContext"));
        assert!(debug.contains(".."));
        assert!(!debug.contains("endpoint") && !debug.contains("Cell"));
    }
    assert_eq!((maps.get(), reductions.get()), (0, 0));
    let (ordinary_site, control_site) = sites.borrow()[0].clone();
    let absent_reductions = Rc::clone(&reductions);
    let (other_builder, absent_site) =
        Store::builder(State { child: None }, Parent, Runtime::new(driver.clone()))
            .mount_with_site(OptionalMount::new(
                |state: &State| state.child.as_ref(),
                |state: &mut State| state.child.as_mut(),
                |state: &State| {
                    state.child.as_ref().map(|_| MountIdentity {
                        key: (),
                        replacement: 0,
                        configuration: 0,
                    })
                },
                move |_| Child(Rc::clone(&absent_reductions)),
            ));
    let other = other_builder.build().unwrap();
    driver.finish(other.ready()).unwrap();
    assert!(
        driver
            .finish(other.input_gate(&absent_site))
            .unwrap()
            .is_none()
    );
    let error = driver
        .finish(other.command_port(&ordinary_site))
        .unwrap_err();
    assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
    assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
    assert_eq!((maps.get(), reductions.get()), (0, 0));
    assert_eq!(sites.borrow().len(), 1);
    driver.finish(other.shutdown()).unwrap();
    drop(other);
    let site_debug = format!("{ordinary_site:?}");
    assert!(site_debug.contains("CommandSite"));
    assert!(!site_debug.contains("Input") && !site_debug.contains("Cell"));
    let ordinary = driver
        .finish(store.command_port(&ordinary_site))
        .unwrap()
        .unwrap();
    let port_debug = format!("{ordinary:?}");
    assert!(port_debug.contains("CommandPort"));
    assert!(!port_debug.contains("Input") && !port_debug.contains("Cell"));
    let control = driver
        .finish(store.command_port(&control_site))
        .unwrap()
        .unwrap();
    let admitted = ordinary.try_send(Input(Rc::new(Cell::new(2)))).unwrap();
    let receipt_debug = format!("{admitted:?}");
    assert!(receipt_debug.contains("CommandReceipt"));
    assert!(!receipt_debug.contains("Input") && !receipt_debug.contains("Cell"));
    let full = Rc::new(Cell::new(11));
    let Err(full_error) = ordinary.try_send(Input(Rc::clone(&full))) else {
        panic!("the admitted command must occupy the one-place mailbox");
    };
    let full_debug = format!("{full_error:?}");
    assert!(full_debug.contains("Full"));
    assert!(!full_debug.contains("Input") && !full_debug.contains("Cell"));
    let full_display = full_error.to_string();
    assert!(full_display.contains("full"));
    assert!(!full_display.contains("Input") && !full_display.contains("Cell"));
    assert!(std::error::Error::source(&full_error).is_none());
    let rca_core::local::TryCommandError::Full(input) = full_error else {
        panic!("full mailbox must report Full rather than a terminal rejection");
    };
    assert!(Rc::ptr_eq(&input.0, &full));
    assert_eq!(input.0.get(), 11);
    assert_eq!((maps.get(), reductions.get()), (0, 0));
    drop(input);
    assert_eq!(Rc::strong_count(&full), 1);
    drop(driver.finish(admitted.committed()).unwrap());
    let gate = driver
        .finish(store.input_gate(&child_site))
        .unwrap()
        .unwrap();
    drop(driver.finish(store.send(Close(gate, Rc::new(())))).unwrap());
    let original = Rc::new(Cell::new(3));
    match driver.finish(ordinary.send(Input(std::rc::Rc::clone(&original)))) {
        Err(TaskSendError::Rejected { action, error }) => {
            assert!(Rc::ptr_eq(&action.0, &original));
            assert_eq!(error.reason(), TaskAccessReason::InputClosed);
        }
        _ => panic!("closed Local input must recover original Rc payload"),
    }
    drop(
        driver
            .finish(control.send(Control(Rc::new(Cell::new(5)))))
            .unwrap(),
    );
    assert_eq!(
        driver
            .finish(store.read(|state| state.child.as_ref().unwrap().get()))
            .unwrap(),
        7
    );
    assert_eq!(maps.get(), 1);
    assert_eq!(reductions.get(), 2);

    let deferred_child = store.acquire(&child_site);
    let deferred_port = store.command_port(&ordinary_site);
    let deferred_input = store.input_gate(&child_site);
    #[cfg(feature = "test-util")]
    let deferred_diagnostic = {
        let gate = rca_core::testing::local::TransactionGate::new();
        (
            gate.acquire(&store, &child_site),
            gate.command_port(&store, &ordinary_site),
        )
    };
    let closed_child = store.acquire(&child_site);
    let closed_port = store.command_port(&ordinary_site);
    let closed_input = store.input_gate(&child_site);
    #[cfg(feature = "test-util")]
    let closed_diagnostic = {
        let gate = rca_core::testing::local::TransactionGate::new();
        (
            gate.acquire(&store, &child_site),
            gate.command_port(&store, &ordinary_site),
        )
    };
    let retained_tasks = driver.0.borrow().len();
    assert!(retained_tasks > 0);
    assert_eq!(factories.get(), 1);
    drop(store);
    // The actor still owns its lifecycle here. First polling must reject its
    // closed authority without queuing acquisition or driving actor teardown.
    let mut cx = Context::from_waker(Waker::noop());
    for result in [
        std::pin::pin!(closed_child)
            .as_mut()
            .poll(&mut cx)
            .map(|result| result.unwrap_err()),
        std::pin::pin!(closed_port)
            .as_mut()
            .poll(&mut cx)
            .map(|result| result.unwrap_err()),
        std::pin::pin!(closed_input)
            .as_mut()
            .poll(&mut cx)
            .map(|result| result.unwrap_err()),
    ] {
        let Poll::Ready(error) = result else {
            panic!("closed owner must reject acquisition without an actor turn");
        };
        assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
        assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
    }
    #[cfg(feature = "test-util")]
    for result in [
        std::pin::pin!(closed_diagnostic.0)
            .as_mut()
            .poll(&mut cx)
            .map(|result| result.unwrap_err()),
        std::pin::pin!(closed_diagnostic.1)
            .as_mut()
            .poll(&mut cx)
            .map(|result| result.unwrap_err()),
    ] {
        let Poll::Ready(error) = result else {
            panic!("closed owner must reject diagnostic acquisition without an actor turn");
        };
        assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
        assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
    }
    assert_eq!(driver.0.borrow().len(), retained_tasks);
    assert_eq!((factories.get(), maps.get(), reductions.get()), (1, 1, 2));
    let original = Rc::new(Cell::new(9));
    let identity = Rc::as_ptr(&original);
    let Err(TaskSendError::Rejected { action, error }) =
        driver.finish(ordinary.send(Input(Rc::clone(&original))))
    else {
        panic!("closed recipe command must return the original Rc before mapping");
    };
    assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
    assert_eq!(Rc::as_ptr(&action.0), identity);
    assert_eq!(action.0.get(), 9);
    assert_eq!((maps.get(), reductions.get()), (1, 2));
    drop(action);
    let Err(closed_error) = ordinary.try_send(Input(Rc::clone(&original))) else {
        panic!("immediate closed submission must return the original input");
    };
    let closed_debug = format!("{closed_error:?}");
    assert!(closed_debug.contains("Rejected"));
    assert!(closed_debug.contains("StoreClosed"));
    assert!(closed_debug.contains("NotStarted"));
    assert!(!closed_debug.contains("Input") && !closed_debug.contains("Cell"));
    let closed_display = closed_error.to_string();
    assert!(closed_display.contains("StoreClosed"));
    assert!(closed_display.contains("NotStarted"));
    assert!(!closed_display.contains("Input") && !closed_display.contains("Cell"));
    let cause = std::error::Error::source(&closed_error)
        .expect("rejected local admission exposes its retained access error")
        .downcast_ref::<rca_core::TaskAccessError>()
        .expect("the cause keeps its concrete error type");
    let rca_core::local::TryCommandError::Rejected { error, .. } = &closed_error else {
        panic!("closed recipe command must report a typed rejection");
    };
    assert!(std::ptr::eq(cause, error));
    assert!(std::error::Error::source(cause).is_none());
    let borrowed = rca_core::local::TryCommandError::Full(&original);
    assert!(std::error::Error::source(&borrowed).is_none());
    let rca_core::local::TryCommandError::Rejected { command, error } = closed_error else {
        panic!("closed recipe command must report a typed rejection");
    };
    assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
    assert_eq!(Rc::as_ptr(&command.0), identity);
    assert_eq!(command.0.get(), 9);
    assert_eq!((maps.get(), reductions.get()), (1, 2));
    drop(command);
    assert_eq!(Rc::strong_count(&original), 1);
    driver.finish(std::future::poll_fn(|_| {
        if driver.0.borrow().is_empty() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }));
    // Acquisitions created while live must not retain the root until first poll.
    for error in [
        driver.finish(deferred_child).unwrap_err(),
        driver.finish(deferred_port).unwrap_err(),
        driver.finish(deferred_input).unwrap_err(),
    ] {
        assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
        assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
    }
    #[cfg(feature = "test-util")]
    for error in [
        driver.finish(deferred_diagnostic.0).unwrap_err(),
        driver.finish(deferred_diagnostic.1).unwrap_err(),
    ] {
        assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
        assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
    }
    assert_eq!((maps.get(), reductions.get()), (1, 2));
    assert!(driver.0.borrow().is_empty());
    assert_eq!(factories.get(), 1);
    assert_eq!(Rc::strong_count(&factories), 1);
    assert_eq!(Rc::strong_count(&maps), 1);
    assert_eq!(Rc::strong_count(&reductions), 1);
    assert_eq!(Rc::strong_count(&sites), 1);
    let foreign_builder =
        Store::builder(State { child: None }, Parent, Runtime::new(driver.clone()));
    let foreign = foreign_builder.build().unwrap();
    driver.finish(foreign.ready()).unwrap();
    let Err(error) = driver.finish(foreign.command_port(&ordinary_site)) else {
        panic!("the retained weak recipe site must not acquire through another Store");
    };
    assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
    assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
    assert_eq!(sites.borrow().len(), 1);
    assert_eq!((maps.get(), reductions.get()), (1, 2));
    assert_eq!(Rc::strong_count(&original), 1);
    driver.finish(foreign.shutdown()).unwrap();
    #[cfg(feature = "test-util")]
    {
        let gate = rca_core::testing::local::TransactionGate::new();
        let error = gate
            .validate_command_acquisition(&foreign, &ordinary_site)
            .unwrap_err();
        assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
        assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
        assert_eq!((factories.get(), maps.get(), reductions.get()), (1, 1, 2));
    };
    drop(foreign);
    driver.finish(std::future::poll_fn(|_| {
        if driver.0.borrow().is_empty() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }));
}

#[test]
fn recipe_port_try_send_during_real_host_abandonment_returns_original_rc() {
    use rca_core::local::{CommandPort, CommandReceipt, TryCommandError};

    type Payload = Rc<Cell<usize>>;
    type Outcome = Result<CommandReceipt<Payload>, TryCommandError<Payload>>;
    /// Attempts a real port send during actor-owned feature destruction.
    struct ClosingChild {
        port: Rc<RefCell<Option<CommandPort<Payload>>>>,
        original: Payload,
        result: Rc<RefCell<Option<Outcome>>>,
        reductions: Rc<Cell<usize>>,
    }
    impl Feature for ClosingChild {
        type State = Payload;
        type Action = Payload;
        fn update(
            &mut self,
            _: &mut Self::State,
            _: &Self::Action,
            _: &mut FeatureContext<'_, Self::State, Self::Action>,
        ) {
            self.reductions.set(self.reductions.get() + 1);
        }
    }
    impl Drop for ClosingChild {
        fn drop(&mut self) {
            let port = self.port.borrow().as_ref().unwrap().clone();
            *self.result.borrow_mut() = Some(port.try_send(Rc::clone(&self.original)));
        }
    }

    let driver = Driver::default();
    let state = Rc::new(Cell::new(4));
    let original = Rc::new(Cell::new(37));
    let weak = Rc::downgrade(&original);
    let port = Rc::new(RefCell::new(None));
    let result = Rc::new(RefCell::new(None));
    let maps = Rc::new(Cell::new(0));
    let reductions = Rc::new(Cell::new(0));
    let child_port = Rc::clone(&port);
    let child_original = Rc::clone(&original);
    let child_result = Rc::clone(&result);
    let child_reductions = Rc::clone(&reductions);
    let descriptor = OptionalMount::new(
        |state: &State| state.child.as_ref(),
        |state: &mut State| state.child.as_mut(),
        |state: &State| {
            state.child.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| ClosingChild {
            port: Rc::clone(&child_port),
            original: Rc::clone(&child_original),
            result: Rc::clone(&child_result),
            reductions: Rc::clone(&child_reductions),
        },
    );
    let declared = Rc::new(RefCell::new(None));
    let export = Rc::clone(&declared);
    let mapper_calls = Rc::clone(&maps);
    let (builder, _) = Store::builder(
        State {
            child: Some(Rc::clone(&state)),
        },
        Parent,
        Runtime::new(driver.clone()),
    )
    .mount_with_children(descriptor, move |_, children| {
        let mapper_calls = Rc::clone(&mapper_calls);
        *export.borrow_mut() = Some(children.command(move |input: Payload| {
            mapper_calls.set(mapper_calls.get() + 1);
            input
        }));
    });
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    let site = declared.borrow_mut().take().unwrap();
    *port.borrow_mut() = driver.finish(store.command_port(&site)).unwrap();
    assert!(port.borrow().is_some());

    // The actual actor future closes its receiver before dropping its owned Feature.
    let actor_tasks = std::mem::take(&mut *driver.0.borrow_mut());
    assert_eq!(actor_tasks.len(), 1);
    drop(actor_tasks);
    let Some(Err(TryCommandError::Rejected { command, error })) = result.borrow_mut().take() else {
        panic!("closed actor transport must return the unentered recipe command");
    };
    assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
    assert!(Rc::ptr_eq(&command, &original));
    assert_eq!(command.get(), 37);
    assert_eq!((maps.get(), reductions.get(), state.get()), (0, 0, 4));
    assert!(driver.0.borrow().is_empty());
    drop(command);
    drop(port);
    drop(store);
    drop(original);
    assert!(weak.upgrade().is_none());
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one local command schedule checks owned mapping, deferred child effects and completion"
)]
fn local_command_completion_waits_for_owned_child_effect_delivery() {
    use rca_core::local::Effect;

    enum Action {
        Start(Rc<usize>),
        Record(Rc<usize>),
    }
    /// Starts a deferred send only after the command commits its first action.
    struct WorkingChild {
        released: Rc<Cell<bool>>,
        starts: Rc<Cell<usize>>,
        records: Rc<RefCell<Vec<usize>>>,
    }
    impl Feature for WorkingChild {
        type State = Rc<Cell<usize>>;
        type Action = Action;

        fn update(
            &mut self,
            state: &mut Self::State,
            action: &Action,
            context: &mut FeatureContext<'_, Self::State, Action>,
        ) {
            match action {
                Action::Start(value) => {
                    let value = Rc::clone(value);
                    let released = Rc::clone(&self.released);
                    let starts = Rc::clone(&self.starts);
                    context.effect(Effect::run(move |sender| async move {
                        starts.set(starts.get() + 1);
                        std::future::poll_fn(|_| {
                            if released.get() {
                                Poll::Ready(())
                            } else {
                                Poll::Pending
                            }
                        })
                        .await;
                        sender.send(Action::Record(value)).await.unwrap();
                    }));
                }
                Action::Record(value) => {
                    self.records.borrow_mut().push(Rc::as_ptr(value).addr());
                    state.set(state.get() + **value);
                }
            }
        }
    }

    let driver = Driver::default();
    let state = Rc::new(Cell::new(0));
    let released = Rc::new(Cell::new(false));
    let starts = Rc::new(Cell::new(0));
    let records = Rc::new(RefCell::new(Vec::new()));
    let child_released = Rc::clone(&released);
    let child_starts = Rc::clone(&starts);
    let child_records = Rc::clone(&records);
    let descriptor = OptionalMount::new(
        |state: &State| state.child.as_ref(),
        |state: &mut State| state.child.as_mut(),
        |state: &State| {
            state.child.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| WorkingChild {
            released: Rc::clone(&child_released),
            starts: Rc::clone(&child_starts),
            records: Rc::clone(&child_records),
        },
    );
    let sites = Rc::new(RefCell::new(None));
    let declared = Rc::clone(&sites);
    let maps = Rc::new(Cell::new(0));
    let mapped = Rc::clone(&maps);
    let (builder, _) = Store::builder(
        State {
            child: Some(Rc::clone(&state)),
        },
        Parent,
        Runtime::new(driver.clone()),
    )
    .mount_with_children(descriptor, move |_, children| {
        let mapped = Rc::clone(&mapped);
        *declared.borrow_mut() = Some(children.command(move |value: Rc<usize>| {
            mapped.set(mapped.get() + 1);
            Action::Start(value)
        }));
    });
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    let site = sites.borrow_mut().take().unwrap();
    let port = driver.finish(store.command_port(&site)).unwrap().unwrap();
    let value = Rc::new(7);
    let identity = Rc::as_ptr(&value).addr();
    let weak = Rc::downgrade(&value);
    let task = driver.finish(port.send(value)).unwrap();
    assert_eq!((maps.get(), starts.get(), state.get()), (1, 0, 0));
    assert!(!task.is_finished());
    assert!(records.borrow().is_empty());
    assert!(weak.upgrade().is_some());
    driver.finish(std::future::poll_fn(|_| {
        if starts.get() == 1 {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }));
    assert!(!task.is_finished());
    assert_eq!(state.get(), 0);
    assert!(records.borrow().is_empty());
    released.set(true);
    driver.finish(task.finish());
    assert!(!task.is_cancelled());
    assert_eq!((maps.get(), starts.get(), state.get()), (1, 1, 7));
    assert_eq!(*records.borrow(), [identity]);
    assert!(weak.upgrade().is_none());
    let final_state = driver.finish(store.shutdown_into_state()).unwrap();
    assert_eq!(final_state.child.as_ref().unwrap().get(), 7);
    drop(final_state);
    assert_eq!(Rc::strong_count(&state), 1);
}

#[test]
fn local_command_mapper_closing_store_fails_after_entry_without_reducing() {
    struct OwnedInput {
        value: Rc<usize>,
        drops: Rc<Cell<usize>>,
    }
    impl Drop for OwnedInput {
        fn drop(&mut self) {
            self.drops.set(self.drops.get() + 1);
        }
    }

    let driver = Driver::default();
    let state = Rc::new(Cell::new(3));
    let reductions = Rc::new(Cell::new(0));
    let reduced = Rc::clone(&reductions);
    let owner = Rc::new(RefCell::new(None::<Store<State, Close>>));
    let mapper_owner = Rc::clone(&owner);
    let maps = Rc::new(Cell::new(0));
    let mapped = Rc::clone(&maps);
    let sites = Rc::new(RefCell::new(None));
    let declared = Rc::clone(&sites);
    let descriptor = OptionalMount::new(
        |state: &State| state.child.as_ref(),
        |state: &mut State| state.child.as_mut(),
        |state: &State| {
            state.child.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| Child(Rc::clone(&reduced)),
    );
    let (builder, _) = Store::builder(
        State {
            child: Some(Rc::clone(&state)),
        },
        Parent,
        Runtime::new(driver.clone()),
    )
    .mount_with_children(descriptor, move |_, children| {
        let owner = Rc::clone(&mapper_owner);
        let mapped = Rc::clone(&mapped);
        *declared.borrow_mut() = Some(children.command(move |input: OwnedInput| {
            mapped.set(mapped.get() + 1);
            let action = Rc::clone(&input.value);
            drop(input);
            let store = owner.borrow_mut().take().unwrap();
            drop(store);
            action
        }));
    });
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    let site = sites.borrow_mut().take().unwrap();
    let port = driver.finish(store.command_port(&site)).unwrap().unwrap();
    *owner.borrow_mut() = Some(store);
    let drops = Rc::new(Cell::new(0));
    let value = Rc::new(7);
    let weak = Rc::downgrade(&value);
    let send = port.send(OwnedInput {
        value,
        drops: Rc::clone(&drops),
    });
    assert_eq!((maps.get(), drops.get(), reductions.get()), (0, 0, 0));
    assert!(owner.borrow().is_some());
    assert!(weak.upgrade().is_some());

    let Err(TaskSendError::Failed { error }) = driver.finish(send) else {
        panic!("an entered command cannot return its consumed original input");
    };
    assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), rca_core::TaskProgress::MayHaveRun);
    assert_eq!((maps.get(), drops.get(), reductions.get()), (1, 1, 0));
    assert_eq!(state.get(), 3);
    assert!(owner.borrow().is_none());
    assert!(weak.upgrade().is_none());
    driver.finish(std::future::poll_fn(|_| {
        if Rc::strong_count(&owner) == 1 {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }));
    assert_eq!(Rc::strong_count(&state), 1);
    assert_eq!(drops.get(), 1);
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "Both entered panic boundaries retain one original input until its destructor checks the still-pending reply, then tear down the real actor."
)]
fn local_selector_and_mapper_panics_drop_input_before_terminal_reply() {
    type Reply = std::pin::Pin<
        Box<dyn Future<Output = Result<rca_core::ActionTask, TaskSendError<PanicInput>>>>,
    >;
    /// Checks that input disposal precedes publication of the terminal reply.
    struct PanicInput {
        value: Rc<usize>,
        drops: Rc<Cell<usize>>,
        reply: Rc<RefCell<Option<Reply>>>,
    }
    impl Drop for PanicInput {
        fn drop(&mut self) {
            assert_eq!(*self.value, 17);
            self.drops.set(self.drops.get() + 1);
            assert!(
                self.reply
                    .borrow_mut()
                    .as_mut()
                    .expect("the original receipt remains owned by its observer")
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending(),
                "input destruction must precede terminal reply publication"
            );
        }
    }

    for selector_panics in [true, false] {
        let driver = Driver::default();
        let state = Rc::new(Cell::new(3));
        let reductions = Rc::new(Cell::new(0));
        let reduced = Rc::clone(&reductions);
        let maps = Rc::new(Cell::new(0));
        let mapped = Rc::clone(&maps);
        let constructions = Rc::new(Cell::new(0));
        let constructed = Rc::clone(&constructions);
        let capture = Rc::new(());
        let capture_weak = Rc::downgrade(&capture);
        let sites = Rc::new(RefCell::new(Vec::new()));
        let declared = Rc::clone(&sites);
        let descriptor = OptionalMount::new(
            |state: &State| state.child.as_ref(),
            |state: &mut State| {
                assert_ne!(
                    state.child.as_ref().unwrap().get(),
                    usize::MAX,
                    "selector panic"
                );
                state.child.as_mut()
            },
            |state: &State| {
                state.child.as_ref().map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            move |_| {
                constructed.set(constructed.get() + 1);
                Child(Rc::clone(&reduced))
            },
        );
        let (builder, _) = Store::builder(
            State {
                child: Some(Rc::clone(&state)),
            },
            Parent,
            Runtime::new(driver.clone()),
        )
        .mount_with_children(descriptor, move |_, children| {
            let capture = Rc::clone(&capture);
            let mapped = Rc::clone(&mapped);
            declared
                .borrow_mut()
                .push(children.command(move |_input: PanicInput| -> Rc<usize> {
                    let _ = &capture;
                    mapped.set(mapped.get() + 1);
                    panic!("mapper panic");
                }));
        });
        let store = builder.build().unwrap();
        driver.finish(store.ready()).unwrap();
        let site = sites.borrow()[0].clone();
        let port = driver.finish(store.command_port(&site)).unwrap().unwrap();
        // Arm only after installation/acquisition; the public mutable selector
        // must be entered before mapping, without changing mount identity.
        let original_state = if selector_panics { usize::MAX } else { 3 };
        state.set(original_state);
        let drops = Rc::new(Cell::new(0));
        let value = Rc::new(17);
        let value_weak = Rc::downgrade(&value);
        let reply = Rc::new(RefCell::new(None::<Reply>));
        *reply.borrow_mut() = Some(Box::pin(port.send(PanicInput {
            value,
            drops: Rc::clone(&drops),
            reply: Rc::clone(&reply),
        })));
        assert!(
            reply
                .borrow_mut()
                .as_mut()
                .unwrap()
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        assert_eq!((maps.get(), drops.get(), reductions.get()), (0, 0, 0));
        let mut tasks = std::mem::take(&mut *driver.0.borrow_mut());
        assert_eq!(tasks.len(), 1, "only the installed actor is scheduled");
        let mut actor = tasks.pop().unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            actor.as_mut().poll(&mut Context::from_waker(Waker::noop()))
        }));
        drop(result.expect_err("the selected user callback must panic"));
        drop(actor);
        let terminal = reply.borrow_mut().take().unwrap();
        let Err(TaskSendError::Failed { error }) = driver.finish(terminal) else {
            panic!("entered panic must not return the original command");
        };
        assert_eq!(error.reason(), TaskAccessReason::ActorFailed);
        assert_eq!(error.progress(), rca_core::TaskProgress::MayHaveRun);
        assert_eq!(maps.get(), usize::from(!selector_panics));
        assert_eq!((drops.get(), reductions.get()), (1, 0));
        assert_eq!((state.get(), constructions.get()), (original_state, 1));
        assert_eq!(sites.borrow().len(), 1);
        assert!(value_weak.upgrade().is_none());
        drop(store);
        assert!(driver.0.borrow().is_empty());
        assert!(capture_weak.upgrade().is_none());
        assert_eq!(Rc::strong_count(&state), 1);
        assert_eq!(Rc::strong_count(&maps), 1);
        assert_eq!(Rc::strong_count(&reductions), 1);
    }
}

#[test]
fn local_projection_closing_store_prevents_command_mapping() {
    struct ProjectedState {
        child: Option<Rc<Cell<usize>>>,
        owner: Rc<RefCell<Option<Store<ProjectedState, ()>>>>,
    }
    struct ProjectedRoot;
    impl Feature for ProjectedRoot {
        type State = ProjectedState;
        type Action = ();
        fn update(
            &mut self,
            _: &mut ProjectedState,
            (): &(),
            _: &mut FeatureContext<'_, ProjectedState, ()>,
        ) {
        }
    }
    struct OwnedInput {
        value: Rc<usize>,
        drops: Rc<Cell<usize>>,
    }
    impl Drop for OwnedInput {
        fn drop(&mut self) {
            self.drops.set(self.drops.get() + 1);
        }
    }

    let driver = Driver::default();
    let state = Rc::new(Cell::new(3));
    let reductions = Rc::new(Cell::new(0));
    let reduced = Rc::clone(&reductions);
    let owner = Rc::new(RefCell::new(None));
    let maps = Rc::new(Cell::new(0));
    let mapped = Rc::clone(&maps);
    let sites = Rc::new(RefCell::new(None));
    let declared = Rc::clone(&sites);
    let descriptor = OptionalMount::new(
        |state: &ProjectedState| state.child.as_ref(),
        |state: &mut ProjectedState| {
            let store = state.owner.borrow_mut().take().expect("sole Store owner");
            drop(store);
            state.child.as_mut()
        },
        |state: &ProjectedState| {
            state.child.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| Child(Rc::clone(&reduced)),
    );
    let (builder, _) = Store::builder(
        ProjectedState {
            child: Some(Rc::clone(&state)),
            owner: Rc::clone(&owner),
        },
        ProjectedRoot,
        Runtime::new(driver.clone()),
    )
    .mount_with_children(descriptor, move |_, children| {
        let mapped = Rc::clone(&mapped);
        *declared.borrow_mut() = Some(children.command(move |input: OwnedInput| {
            mapped.set(mapped.get() + 1);
            Rc::clone(&input.value)
        }));
    });
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    let site = sites.borrow_mut().take().unwrap();
    let port = driver.finish(store.command_port(&site)).unwrap().unwrap();
    *owner.borrow_mut() = Some(store);
    let drops = Rc::new(Cell::new(0));
    let value = Rc::new(7);
    let weak = Rc::downgrade(&value);
    let send = port.send(OwnedInput {
        value,
        drops: Rc::clone(&drops),
    });
    assert_eq!((maps.get(), drops.get(), reductions.get()), (0, 0, 0));
    let Err(TaskSendError::Failed { error }) = driver.finish(send) else {
        panic!("projection entered, so the command cannot be returned as unstarted");
    };
    assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), rca_core::TaskProgress::MayHaveRun);
    assert_eq!((maps.get(), drops.get(), reductions.get()), (0, 1, 0));
    assert_eq!(state.get(), 3);
    assert!(owner.borrow().is_none());
    assert!(weak.upgrade().is_none());
    driver.finish(std::future::poll_fn(|_| {
        if Rc::strong_count(&owner) == 1 {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }));
    assert_eq!(Rc::strong_count(&state), 1);
    assert_eq!(Rc::strong_count(&maps), 1);
    assert_eq!(Rc::strong_count(&reductions), 1);
    assert_eq!(drops.get(), 1);
    assert!(driver.0.borrow().is_empty());
}

#[cfg(feature = "test-util")]
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One ownership schedule checks admission, terminal reply, and eventual payload release."
)]
fn ordinary_and_unheld_recipe_acquisitions_close_with_pending_original_input() {
    struct DropProbe(Rc<Cell<usize>>);
    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    let driver = Driver::default();
    let declared = Rc::new(RefCell::new(None));
    let sites = Rc::clone(&declared);
    let maps = Rc::new(Cell::new(0));
    let mapped = Rc::clone(&maps);
    let mapper_drops = Rc::new(Cell::new(0));
    let retained_drop_count = Rc::clone(&mapper_drops);
    let reductions = Rc::new(Cell::new(0));
    let reduced = Rc::clone(&reductions);
    let descriptor = OptionalMount::new(
        |state: &State| state.child.as_ref(),
        |state: &mut State| state.child.as_mut(),
        |state: &State| {
            state.child.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| Child(Rc::clone(&reduced)),
    );
    let (builder, child_site) = Store::builder(
        State {
            child: Some(Rc::new(Cell::new(0))),
        },
        Parent,
        Runtime::new(driver.clone()),
    )
    .mailbox_capacity(1)
    .mount_with_children(descriptor, move |_, children| {
        let mapper_probe = DropProbe(Rc::clone(&retained_drop_count));
        let mapped = Rc::clone(&mapped);
        *sites.borrow_mut() = Some(children.command(move |input: Input| {
            let _ = &mapper_probe;
            mapped.set(mapped.get() + 1);
            Rc::new(input.0.get())
        }));
    });
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    let site = declared.borrow_mut().take().unwrap();
    let gate = rca_core::testing::local::TransactionGate::new();
    assert!(gate.boundary().is_none());
    let port = driver
        .finish(gate.command_port(&store, &site))
        .unwrap()
        .unwrap();
    drop(
        driver
            .finish(port.send(Input(Rc::new(Cell::new(2)))))
            .unwrap(),
    );
    assert_eq!(maps.get(), 1);
    assert_eq!(reductions.get(), 1);
    let input_gate = driver
        .finish(store.input_gate(&child_site))
        .unwrap()
        .unwrap();
    let queued_owner = Rc::new(());
    let Ok(queued) = store.try_send(Close(input_gate.clone(), Rc::clone(&queued_owner))) else {
        panic!("the first root action must fill the empty mailbox");
    };
    assert_eq!(Rc::strong_count(&queued_owner), 2);
    let Err(full) = store.try_send(Close(input_gate, Rc::clone(&queued_owner))) else {
        panic!("the occupied one-place mailbox must reject another root action");
    };
    assert!(full.is_full());
    drop(full);
    assert_eq!(Rc::strong_count(&queued_owner), 2);
    let mut ordinary_acquisition = Box::pin(store.command_port(&site));
    assert!(
        ordinary_acquisition
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    let original = Rc::new(Cell::new(31));
    let identity = Rc::as_ptr(&original);
    let weak = Rc::downgrade(&original);
    let mut pending_send = Box::pin(port.send(Input(Rc::clone(&original))));
    assert!(
        pending_send
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    drop(original);
    let mut acquisition = Box::pin(gate.command_port(&store, &site));
    assert!(
        acquisition
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    assert_eq!(mapper_drops.get(), 0);
    drop(store);
    let Poll::Ready(Err(error)) = ordinary_acquisition
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    else {
        panic!("ordinary acquisition must reject transport closure without an actor turn");
    };
    assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
    let Poll::Ready(Err(error)) = acquisition
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    else {
        panic!("closing a full mailbox must publish the waiting acquisition's terminal reply");
    };
    assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
    let Poll::Ready(Err(TaskSendError::Rejected { action, error })) = pending_send
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    else {
        panic!("closing a full recipe mailbox must return its unentered Rc input");
    };
    assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
    assert_eq!(Rc::as_ptr(&action.0), identity);
    assert_eq!(action.0.get(), 31);
    assert_eq!((maps.get(), reductions.get()), (1, 1));
    drop(action);
    drop(pending_send);
    assert!(weak.upgrade().is_none());
    assert_eq!(
        mapper_drops.get(),
        0,
        "reply does not depend on actor teardown"
    );
    drop(ordinary_acquisition);
    drop(acquisition);
    drop(port);
    drop(queued);
    driver.finish(std::future::poll_fn(|_| {
        if mapper_drops.get() == 1 {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }));
    assert_eq!(mapper_drops.get(), 1);
    assert_eq!(Rc::strong_count(&queued_owner), 1);
    assert_eq!(maps.get(), 1);
    assert_eq!(reductions.get(), 1);
    assert!(declared.borrow().is_none());
}

#[cfg(feature = "test-util")]
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One real mounted frontier and its command frontier preserve Rc payloads and replies through inspection unwind."
)]
fn local_gate_inspections_preserve_mounted_and_command_frontiers_through_unwind() {
    type RootContext = rca_core::local::TaskContext<State, ()>;
    struct CaptureRoot(Rc<RefCell<Option<RootContext>>>);
    impl Feature for CaptureRoot {
        type State = State;
        type Action = ();

        fn update(&mut self, _: &mut State, (): &(), context: &mut FeatureContext<'_, State, ()>) {
            let captured = Rc::clone(&self.0);
            context.task(rca_core::local::Task::run(move |root| async move {
                *captured.borrow_mut() = Some(root);
            }));
        }
    }

    let driver = Driver::default();
    let captured_root = Rc::new(RefCell::new(None));
    let declared = Rc::new(RefCell::new(None));
    let sites = Rc::clone(&declared);
    let maps = Rc::new(Cell::new(0));
    let mapped = Rc::clone(&maps);
    let reductions = Rc::new(Cell::new(0));
    let reduced = Rc::clone(&reductions);
    let state = Rc::new(Cell::new(0));
    let descriptor = OptionalMount::new(
        |state: &State| state.child.as_ref(),
        |state: &mut State| state.child.as_mut(),
        |state: &State| {
            state.child.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| Child(Rc::clone(&reduced)),
    );
    let (builder, child_site) =
        rca_core::local::RootMount::new(CaptureRoot(Rc::clone(&captured_root)))
            .configuration(|state: &State| {
                u64::try_from(state.child.as_ref().unwrap().get() / 7).unwrap()
            })
            .builder(
                State {
                    child: Some(Rc::clone(&state)),
                },
                Runtime::new(driver.clone()),
            )
            .mailbox_capacity(1)
            .mount_with_children(descriptor, move |_, children| {
                let mapped = Rc::clone(&mapped);
                *sites.borrow_mut() = Some(children.command(move |input: Input| {
                    mapped.set(mapped.get() + 1);
                    Rc::new(input.0.get())
                }));
            });
    let gate = rca_core::testing::local::TransactionGate::new();
    let store = builder.transaction_gate(gate.clone()).build().unwrap();
    let mut cx = Context::from_waker(Waker::noop());
    let mut startup_wait = Box::pin(gate.wait_for_pending());
    assert!(startup_wait.as_mut().poll(&mut cx).is_pending());
    assert!(gate.reserve_root());
    assert!(!gate.reserve_root(), "the first reservation remains owned");
    gate.cancel_root_reservation();
    let mut no_boundary = Box::pin(
        gate.assert_state(|_| panic!("startup has not published a boundary"))
            .finish(),
    );
    assert!(matches!(
        no_boundary.as_mut().poll(&mut cx),
        Poll::Ready(Err(_))
    ));
    drop(no_boundary);
    driver.finish(startup_wait.as_mut());
    drop(startup_wait);
    assert_eq!(
        gate.pending_kind(),
        Some(rca_core::testing::TransactionKind::Structural)
    );
    let startup = gate.boundary().unwrap();
    drop(
        driver
            .finish(gate.release_pending_with_receipt().unwrap().finish())
            .unwrap(),
    );
    assert!(gate.acknowledge(startup));
    driver.finish(store.ready()).unwrap();
    let mut root_send = Box::pin(store.send(()));
    assert!(root_send.as_mut().poll(&mut cx).is_pending());
    driver.finish(gate.wait_for_pending());
    let root_boundary = gate.boundary().unwrap();
    drop(
        driver
            .finish(gate.release_pending_with_receipt().unwrap().finish())
            .unwrap(),
    );
    drop(driver.finish(root_send.as_mut()).unwrap());
    drop(root_send);
    let root = driver.finish(std::future::poll_fn(|_| {
        captured_root
            .borrow_mut()
            .take()
            .map_or(Poll::Pending, Poll::Ready)
    }));
    assert!(gate.acknowledge(root_boundary));
    let child = driver.finish(store.acquire(&child_site)).unwrap().unwrap();
    let site = declared.borrow_mut().take().unwrap();
    let port = driver.finish(store.command_port(&site)).unwrap().unwrap();
    assert!(gate.boundary().is_none());

    let action = Rc::new(7);
    let mut child_send = Box::pin(child.send(Rc::clone(&action)));
    assert!(child_send.as_mut().poll(&mut cx).is_pending());
    driver.finish(gate.wait_for_pending());
    let mounted_boundary = gate.boundary().unwrap();
    let selected_boundary = gate.pending_boundary().unwrap();
    assert!(!gate.reserve_root());
    assert!(!gate.acknowledge(mounted_boundary));
    assert!(!gate.acknowledge_terminal(&selected_boundary));
    assert_eq!(
        gate.match_terminal(&selected_boundary, |_| panic!(
            "live request is not terminal"
        )),
        None
    );
    assert_eq!(
        gate.pending_kind(),
        Some(rca_core::testing::TransactionKind::Action)
    );
    assert_eq!(
        gate.inspect_mounted(&root, |()| panic!("root authority has no mounted input")),
        Ok(None::<()>)
    );
    assert_eq!(gate.boundary(), Some(mounted_boundary));
    assert!(gate.has_pending());
    assert!(!gate.has_failed_match());
    assert!(child_send.as_mut().poll(&mut cx).is_pending());
    assert_eq!((maps.get(), reductions.get(), state.get()), (0, 0, 0));
    assert_eq!(Rc::strong_count(&action), 2);
    for unwind in [false, true, false] {
        let inspected = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            gate.inspect_mounted(&child, |held| {
                assert!(Rc::ptr_eq(held, &action));
                assert_eq!(**held, 7);
                assert_eq!(
                    gate.inspect_mounted(&child, |_| panic!("input is already borrowed")),
                    Ok(None::<()>)
                );
                assert!(!unwind, "mounted inspection unwind");
                **held
            })
        }));
        if unwind {
            assert_eq!(
                inspected.unwrap_err().downcast_ref::<&str>(),
                Some(&"mounted inspection unwind")
            );
        } else {
            assert_eq!(inspected.unwrap().unwrap(), Some(7));
        }
        assert_eq!(gate.boundary(), Some(mounted_boundary));
        assert!(gate.has_pending());
        assert!(!gate.has_failed_match());
        assert!(child_send.as_mut().poll(&mut cx).is_pending());
        assert_eq!((maps.get(), reductions.get(), state.get()), (0, 0, 0));
        assert_eq!(Rc::strong_count(&action), 2);
    }
    let receipt = gate
        .inspect_mounted(&child, |held| {
            assert!(Rc::ptr_eq(held, &action));
            let receipt = gate.release_pending_with_receipt().unwrap();
            assert!(!gate.release_pending(), "one decision owns the frontier");
            let tasks = std::mem::take(&mut *driver.0.borrow_mut());
            for mut task in tasks {
                if task.as_mut().poll(&mut cx).is_pending() {
                    driver.0.borrow_mut().push(task);
                }
            }
            assert!(child_send.as_mut().poll(&mut cx).is_pending());
            assert_eq!((maps.get(), reductions.get(), state.get()), (0, 0, 0));
            assert_eq!(Rc::strong_count(&action), 2);
            receipt
        })
        .unwrap()
        .unwrap();
    drop(driver.finish(receipt.finish()).unwrap());
    drop(driver.finish(child_send.as_mut()).unwrap());
    assert_eq!((maps.get(), reductions.get(), state.get()), (0, 1, 7));
    assert_eq!(Rc::strong_count(&action), 1);
    assert_eq!(gate.boundary(), Some(mounted_boundary));
    assert!(gate.acknowledge(mounted_boundary));
    assert!(gate.boundary().is_none());

    // The committed child input refreshed only root configuration. The recipe
    // and its mapper survive, but the earlier port's captured authority does not.
    let stale_port = port;
    let port = driver.finish(store.command_port(&site)).unwrap().unwrap();
    let input = Rc::new(Cell::new(11));
    let mut command_send = Box::pin(port.send(Input(Rc::clone(&input))));
    assert!(command_send.as_mut().poll(&mut cx).is_pending());
    driver.finish(gate.wait_for_pending());
    let command_boundary = gate.boundary().unwrap();
    assert_ne!(command_boundary, mounted_boundary);
    let error = gate
        .inspect_command(&stale_port, |_| {
            panic!("old root configuration must not inspect the current command")
        })
        .unwrap_err();
    assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
    assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
    assert_eq!(gate.boundary(), Some(command_boundary));
    assert!(gate.has_pending());
    assert!(!gate.has_failed_match());
    assert!(command_send.as_mut().poll(&mut cx).is_pending());
    assert_eq!((maps.get(), reductions.get(), state.get()), (0, 1, 7));
    assert_eq!(Rc::strong_count(&input), 2);
    for unwind in [false, true, false] {
        let inspected = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            gate.inspect_command(&port, |held| {
                assert!(Rc::ptr_eq(&held.0, &input));
                assert_eq!(held.0.get(), 11);
                assert!(!unwind, "command inspection unwind");
                held.0.get()
            })
        }));
        if unwind {
            assert_eq!(
                inspected.unwrap_err().downcast_ref::<&str>(),
                Some(&"command inspection unwind")
            );
        } else {
            assert_eq!(inspected.unwrap().unwrap(), Some(11));
        }
        assert_eq!(gate.boundary(), Some(command_boundary));
        assert!(gate.has_pending());
        assert!(!gate.has_failed_match());
        assert!(command_send.as_mut().poll(&mut cx).is_pending());
        assert_eq!((maps.get(), reductions.get(), state.get()), (0, 1, 7));
        assert_eq!(Rc::strong_count(&input), 2);
    }
    drop(
        driver
            .finish(gate.release_pending_with_receipt().unwrap().finish())
            .unwrap(),
    );
    drop(driver.finish(command_send.as_mut()).unwrap());
    assert_eq!((maps.get(), reductions.get(), state.get()), (1, 2, 18));
    assert_eq!(Rc::strong_count(&input), 1);
    assert_eq!(gate.boundary(), Some(command_boundary));
    assert!(gate.acknowledge(command_boundary));
    assert!(gate.boundary().is_none());
    drop((
        child_send,
        command_send,
        child,
        root,
        stale_port,
        port,
        store,
    ));
    driver.finish(std::future::poll_fn(|_| {
        if driver.0.borrow().is_empty() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }));
    assert_eq!(Rc::strong_count(&state), 1);
    assert!(captured_root.borrow().is_none());
    assert_eq!(Rc::strong_count(&captured_root), 1);
    assert_eq!(Rc::strong_count(&maps), 1);
    assert_eq!(Rc::strong_count(&reductions), 1);
}

#[cfg(feature = "test-util")]
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One real actor-abandonment timeline compares held publication with and without queued sibling cleanup."
)]
fn unpublished_recipe_call_preserves_held_input_during_actor_teardown() {
    use rca_core::TaskAccessError;
    use rca_core::local::CommandPort;
    use rca_core::testing::local::TransactionGate;

    type Observation = Result<Option<usize>, TaskAccessError>;
    /// Inspects the held command while actor teardown drops its root feature.
    struct InspectParent {
        gate: TransactionGate<State, Close>,
        port: Rc<RefCell<Option<CommandPort<Input>>>>,
        observed: Rc<RefCell<Option<Observation>>>,
    }
    impl Feature for InspectParent {
        type State = State;
        type Action = Close;

        fn update(
            &mut self,
            state: &mut State,
            action: &Close,
            context: &mut FeatureContext<'_, State, Close>,
        ) {
            Parent.update(state, action, context);
        }
    }
    impl Drop for InspectParent {
        fn drop(&mut self) {
            if let Some(port) = self.port.borrow().as_ref() {
                *self.observed.borrow_mut() =
                    Some(self.gate.inspect_command(port, |input| input.0.get()));
            }
        }
    }

    for queue_follower in [false, true] {
        let driver = Driver::default();
        let gate = TransactionGate::new();
        let port = Rc::new(RefCell::new(None));
        let observed = Rc::new(RefCell::new(None));
        let declared = Rc::new(RefCell::new(None));
        let sites = Rc::clone(&declared);
        let maps = Rc::new(Cell::new(0));
        let mapped = Rc::clone(&maps);
        let reductions = Rc::new(Cell::new(0));
        let reduced = Rc::clone(&reductions);
        let descriptor = OptionalMount::new(
            |state: &State| state.child.as_ref(),
            |state: &mut State| state.child.as_mut(),
            |state: &State| {
                state.child.as_ref().map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            move |_| Child(Rc::clone(&reduced)),
        );
        let (builder, _) = Store::builder(
            State {
                child: Some(Rc::new(Cell::new(0))),
            },
            InspectParent {
                gate: gate.clone(),
                port: Rc::clone(&port),
                observed: Rc::clone(&observed),
            },
            Runtime::new(driver.clone()),
        )
        .mount_with_children(descriptor, move |_, children| {
            let mapped = Rc::clone(&mapped);
            *sites.borrow_mut() = Some(children.command(move |input: Input| {
                mapped.set(mapped.get() + 1);
                Rc::new(input.0.get())
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
        let site = declared.borrow_mut().take().unwrap();
        let acquired = driver.finish(store.command_port(&site)).unwrap().unwrap();
        *port.borrow_mut() = Some(acquired.clone());

        let input = Rc::new(Cell::new(17));
        let held = acquired.try_send(Input(Rc::clone(&input))).unwrap();
        driver.finish(gate.wait_for_pending());
        assert_eq!(
            gate.inspect_command(&acquired, |input| input.0.get())
                .unwrap(),
            Some(17)
        );
        let follower =
            queue_follower.then(|| acquired.try_send(Input(Rc::new(Cell::new(23)))).unwrap());

        // Drop the real actor, but retain Store and both admission receipts.
        // Its mailbox drains before the root feature inspects the held call.
        let actor_tasks = std::mem::take(&mut *driver.0.borrow_mut());
        assert_eq!(actor_tasks.len(), 1);
        drop(actor_tasks);
        let observation = observed.borrow_mut().take().unwrap();

        let Err(TaskSendError::Rejected { action, .. }) = driver.finish(held.committed()) else {
            panic!("actor abandonment must return the held original input");
        };
        assert!(Rc::ptr_eq(&action.0, &input));
        drop(action);
        if let Some(follower) = follower {
            let Err(TaskSendError::Rejected { action, .. }) = driver.finish(follower.committed())
            else {
                panic!("actor abandonment must return the unpublished input");
            };
            assert_eq!(action.0.get(), 23);
        }
        assert_eq!((maps.get(), reductions.get()), (0, 0));
        assert_eq!(Rc::strong_count(&input), 1);
        assert_eq!(observation.unwrap(), Some(17), "queued: {queue_follower}");
        drop((store, acquired, port));
    }
}

#[cfg(feature = "test-util")]
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One owner-thread actor teardown checks destructor inspection, the original Rc input and terminal reply."
)]
fn local_mapper_destructor_cannot_inspect_its_retired_command_frontier() {
    use rca_core::local::CommandPort;
    use rca_core::testing::local::TransactionGate;
    use rca_core::{TaskAccessError, TaskProgress};

    struct OwnedInput {
        value: Rc<Cell<usize>>,
        drops: Rc<Cell<usize>>,
    }
    impl Drop for OwnedInput {
        fn drop(&mut self) {
            self.drops.set(self.drops.get() + 1);
        }
    }
    type InspectionResult = Result<Option<()>, TaskAccessError>;
    /// Probes the retired mapper's frontier from its actual destructor.
    struct InspectOnDrop {
        gate: TransactionGate<State, Close>,
        port: Rc<RefCell<Option<CommandPort<OwnedInput>>>>,
        result: Rc<RefCell<Option<InspectionResult>>>,
        matchers: Rc<Cell<usize>>,
        drops: Rc<Cell<usize>>,
    }
    impl Drop for InspectOnDrop {
        fn drop(&mut self) {
            let port = self.port.borrow().as_ref().unwrap().clone();
            let result = self.gate.inspect_command(&port, |_| {
                self.matchers.set(self.matchers.get() + 1);
            });
            *self.result.borrow_mut() = Some(result);
            self.drops.set(self.drops.get() + 1);
        }
    }

    let driver = Driver::default();
    let gate = TransactionGate::new();
    let port = Rc::new(RefCell::new(None));
    let result = Rc::new(RefCell::new(None));
    let maps = Rc::new(Cell::new(0));
    let matchers = Rc::new(Cell::new(0));
    let capture_drops = Rc::new(Cell::new(0));
    let input_drops = Rc::new(Cell::new(0));
    let reductions = Rc::new(Cell::new(0));
    let state = Rc::new(Cell::new(3));
    let declared = Rc::new(RefCell::new(None));
    let sites = Rc::clone(&declared);
    let observer_gate = gate.clone();
    let observer_port = Rc::clone(&port);
    let observer_result = Rc::clone(&result);
    let observer_matchers = Rc::clone(&matchers);
    let observer_drops = Rc::clone(&capture_drops);
    let mapped = Rc::clone(&maps);
    let reduced = Rc::clone(&reductions);
    let descriptor = OptionalMount::new(
        |state: &State| state.child.as_ref(),
        |state: &mut State| state.child.as_mut(),
        |state: &State| {
            state.child.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| Child(Rc::clone(&reduced)),
    );
    let (builder, _) = Store::builder(
        State {
            child: Some(Rc::clone(&state)),
        },
        Parent,
        Runtime::new(driver.clone()),
    )
    .mount_with_children(descriptor, move |_, children| {
        let observer = InspectOnDrop {
            gate: observer_gate.clone(),
            port: Rc::clone(&observer_port),
            result: Rc::clone(&observer_result),
            matchers: Rc::clone(&observer_matchers),
            drops: Rc::clone(&observer_drops),
        };
        let mapped = Rc::clone(&mapped);
        *sites.borrow_mut() = Some(children.command(move |input: OwnedInput| {
            let _ = &observer;
            mapped.set(mapped.get() + 1);
            Rc::new(input.value.get())
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
    let site = declared.borrow_mut().take().unwrap();
    let acquired = driver.finish(store.command_port(&site)).unwrap().unwrap();
    *port.borrow_mut() = Some(acquired.clone());

    let input = OwnedInput {
        value: Rc::new(Cell::new(37)),
        drops: Rc::clone(&input_drops),
    };
    let address = Rc::as_ptr(&input.value);
    let weak = Rc::downgrade(&input.value);
    let mut send = Box::pin(acquired.send(input));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(send.as_mut().poll(&mut cx).is_pending());
    driver.finish(gate.wait_for_pending());
    assert_eq!(
        gate.inspect_command(&acquired, |held| {
            assert_eq!(Rc::as_ptr(&held.value), address);
            held.value.get()
        })
        .unwrap(),
        Some(37)
    );
    assert_eq!(input_drops.get(), 0);

    // Abandon the real actor on its owner thread, without dropping Store.
    // Mapper destruction runs before the actor's lifecycle guard/binding.
    let actor_tasks = std::mem::take(&mut *driver.0.borrow_mut());
    assert_eq!(actor_tasks.len(), 1);
    drop(actor_tasks);
    let error = result.borrow_mut().take().unwrap().unwrap_err();
    assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
    assert_eq!(error.progress(), TaskProgress::NotStarted);
    assert_eq!(
        (matchers.get(), maps.get(), reductions.get(), state.get()),
        (0, 0, 0, 3)
    );
    assert_eq!(capture_drops.get(), 1);
    let Poll::Ready(Err(TaskSendError::Rejected { action, error })) = send.as_mut().poll(&mut cx)
    else {
        panic!("abandoned owner-local actor must return its unentered original command");
    };
    assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), TaskProgress::NotStarted);
    assert_eq!(Rc::as_ptr(&action.value), address);
    assert_eq!(action.value.get(), 37);
    assert_eq!(input_drops.get(), 0);
    drop(action);
    assert_eq!(input_drops.get(), 1);
    assert!(weak.upgrade().is_none());
    assert!(driver.0.borrow().is_empty());
    drop((send, acquired, port, store));
    assert_eq!(capture_drops.get(), 1);
    assert_eq!(Rc::strong_count(&state), 1);
}

#[cfg(feature = "test-util")]
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One real gate callback queues acquisition before closing its owner and preserves the pending original input through teardown."
)]
fn local_gate_callback_closure_rejects_already_queued_command_acquisition() {
    // Each request kind gets a fresh Store and its own bounded diagnostic slot.
    for mounted in [false, true] {
        let driver = Driver::default();
        let declared = Rc::new(RefCell::new(None));
        let sites = Rc::clone(&declared);
        let maps = Rc::new(Cell::new(0));
        let mapped = Rc::clone(&maps);
        let reductions = Rc::new(Cell::new(0));
        let reduced = Rc::clone(&reductions);
        let state = Rc::new(Cell::new(3));
        let capture = Rc::new(());
        let capture_lifetime = Rc::downgrade(&capture);
        let descriptor = OptionalMount::new(
            |state: &State| state.child.as_ref(),
            |state: &mut State| state.child.as_mut(),
            |state: &State| {
                state.child.as_ref().map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            move |_| Child(Rc::clone(&reduced)),
        );
        let (builder, mount_site) = Store::builder(
            State {
                child: Some(Rc::clone(&state)),
            },
            Parent,
            Runtime::new(driver.clone()),
        )
        .mount_with_children(descriptor, move |_, children| {
            let capture = Rc::clone(&capture);
            let mapped = Rc::clone(&mapped);
            *sites.borrow_mut() = Some(children.command(move |input: Input| {
                let _ = &capture;
                mapped.set(mapped.get() + 1);
                Rc::new(input.0.get())
            }));
        });
        let gate = rca_core::testing::local::TransactionGate::new();
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
        let site = declared.borrow_mut().take().unwrap();
        let port = driver.finish(store.command_port(&site)).unwrap().unwrap();
        let original = Rc::new(Cell::new(19));
        let input_lifetime = Rc::downgrade(&original);
        let mut send = Box::pin(port.send(Input(Rc::clone(&original))));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(send.as_mut().poll(&mut cx).is_pending());
        driver.finish(gate.wait_for_pending());
        let boundary = gate.pending_boundary().unwrap();
        assert_eq!(Rc::strong_count(&original), 2);

        let queued = Rc::new(RefCell::new(None));
        let captured_queue = Rc::clone(&queued);
        let query_gate = gate.clone();
        let queries = Rc::new(Cell::new(0));
        let observed = Rc::clone(&queries);
        let closed = store.closed();
        let query = gate.assert_state(move |root| {
            assert_eq!(root.child.as_ref().unwrap().get(), 3);
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
            *captured_queue.borrow_mut() = Some(acquisition);
            observed.set(observed.get() + 1);
            // The acquisition has entered the diagnostic queue, but the actor has
            // not executed it. Its next query must observe this actual owner close.
            drop(store);
        });
        driver.finish(query.finish()).unwrap();
        assert_eq!(queries.get(), 1);
        let mut acquisition = queued.borrow_mut().take().unwrap();
        let Poll::Ready(Err(error)) = acquisition.as_mut().poll(&mut cx) else {
            panic!("already queued acquisition must publish closure before actor teardown");
        };
        assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
        assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
        let Err(TaskSendError::Rejected { action, error }) = driver.finish(send.as_mut()) else {
            panic!("unreleased command must return its original input after closure");
        };
        assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
        assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
        assert!(Rc::ptr_eq(&action.0, &original));
        assert_eq!(action.0.get(), 19);
        assert_eq!((maps.get(), reductions.get(), state.get()), (0, 0, 3));
        assert!(!gate.has_pending());
        let terminal_id = gate.boundary().unwrap();
        assert_eq!(gate.match_terminal(&boundary, |_| false), Some(false));
        assert!(gate.has_failed_match());
        assert_eq!(
            gate.match_terminal(&boundary, |error| {
                assert_eq!(
                    gate.match_terminal(&boundary, |_| panic!("terminal is already borrowed")),
                    None
                );
                assert!(!gate.acknowledge_terminal(&boundary));
                error.reason() == TaskAccessReason::StoreClosed
                    && error.progress() == rca_core::TaskProgress::NotStarted
            }),
            Some(true)
        );
        assert!(gate.acknowledge_terminal(&boundary));
        assert_eq!(gate.boundary(), Some(terminal_id));
        assert!(!gate.acknowledge_terminal(&boundary));
        assert_eq!(
            gate.match_terminal(&boundary, |_| panic!("terminal was already acknowledged")),
            None
        );
        assert!(gate.has_failed_match());
        driver.finish(closed);
        drop((action, send, acquisition, port, original));
        driver.finish(std::future::poll_fn(|_| {
            if driver.0.borrow().is_empty() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }));
        assert!(input_lifetime.upgrade().is_none());
        assert!(capture_lifetime.upgrade().is_none());
        assert_eq!(Rc::strong_count(&state), 1);
    }
}

#[cfg(feature = "test-util")]
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "Two live same-domain stores distinguish submitting-owner validation from actor-root validation without retargeting the recipe."
)]
fn local_foreign_gate_rejects_live_recipe_acquisition_without_retargeting() {
    // Each request kind gets a fresh Store and its own bounded diagnostic slot.
    for mounted in [false, true] {
        let driver = Driver::default();
        let declared = Rc::new(RefCell::new(None));
        let sites = Rc::clone(&declared);
        let maps = Rc::new(Cell::new(0));
        let mapped = Rc::clone(&maps);
        let reductions = Rc::new(Cell::new(0));
        let reduced = Rc::clone(&reductions);
        let state = Rc::new(Cell::new(5));
        let capture = Rc::new(());
        let capture_lifetime = Rc::downgrade(&capture);
        let descriptor = OptionalMount::new(
            |state: &State| state.child.as_ref(),
            |state: &mut State| state.child.as_mut(),
            |state: &State| {
                state.child.as_ref().map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            move |_| Child(Rc::clone(&reduced)),
        );
        let (builder, mount_site) = Store::builder(
            State {
                child: Some(Rc::clone(&state)),
            },
            Parent,
            Runtime::new(driver.clone()),
        )
        .mount_with_children(descriptor, move |_, children| {
            let capture = Rc::clone(&capture);
            let mapped = Rc::clone(&mapped);
            *sites.borrow_mut() = Some(children.command(move |input: Input| {
                let _ = &capture;
                mapped.set(mapped.get() + 1);
                Rc::new(input.0.get())
            }));
        });
        let store = builder.build().unwrap();
        driver.finish(store.ready()).unwrap();
        let site = declared.borrow_mut().take().unwrap();

        let foreign_state = Rc::new(Cell::new(23));
        let foreign_reductions = Rc::new(Cell::new(0));
        let foreign_reduced = Rc::clone(&foreign_reductions);
        let foreign_descriptor = OptionalMount::new(
            |state: &State| state.child.as_ref(),
            |state: &mut State| state.child.as_mut(),
            |state: &State| {
                state.child.as_ref().map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            move |_| Child(Rc::clone(&foreign_reduced)),
        );
        let (foreign_builder, _) = Store::builder(
            State {
                child: Some(Rc::clone(&foreign_state)),
            },
            Parent,
            Runtime::new(driver.clone()),
        )
        .mount_with_site(foreign_descriptor);
        let gate = rca_core::testing::local::TransactionGate::new();
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

        if !mounted {
            // A live foreign Store must reject the recipe before consulting
            // the gate, even when both declarations still own their payloads.
            let mut rejected = std::pin::pin!(gate.command_port(&foreign, &site));
            let Poll::Ready(Err(error)) = rejected
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
            else {
                panic!("foreign recipe acquisition must reject before actor admission");
            };
            assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
            assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
            assert_eq!(gate.boundary(), Some(startup));
            assert!(!gate.has_pending());
            assert!(!gate.has_failed_match());
            assert_eq!((maps.get(), reductions.get(), state.get()), (0, 0, 5));
            assert_eq!((foreign_reductions.get(), foreign_state.get()), (0, 23));
            assert!(capture_lifetime.upgrade().is_some());
        }

        // Prepare uses the live submitting Store and its own recipe. Only the
        // attached foreign actor can reject the different root at query execution.
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
        assert!(
            acquisition
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        let error = driver.finish(acquisition.as_mut()).unwrap_err();
        assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
        assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
        assert_eq!(gate.boundary(), Some(startup));
        assert!(!gate.has_pending());
        assert!(!gate.has_failed_match());
        assert_eq!((maps.get(), reductions.get(), state.get()), (0, 0, 5));
        assert_eq!((foreign_reductions.get(), foreign_state.get()), (0, 23));
        assert!(capture_lifetime.upgrade().is_some());

        let port = driver.finish(store.command_port(&site)).unwrap().unwrap();
        drop(
            driver
                .finish(port.send(Input(Rc::new(Cell::new(7)))))
                .unwrap(),
        );
        assert_eq!((maps.get(), reductions.get(), state.get()), (1, 1, 12));
        assert_eq!((foreign_reductions.get(), foreign_state.get()), (0, 23));
        assert!(gate.acknowledge(startup));
        driver.finish(foreign.shutdown()).unwrap();
        driver.finish(store.shutdown()).unwrap();
        drop((acquisition, port, foreign, store));
        driver.finish(std::future::poll_fn(|_| {
            if driver.0.borrow().is_empty() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }));
        assert!(capture_lifetime.upgrade().is_none());
        assert_eq!(Rc::strong_count(&state), 1);
        assert_eq!(Rc::strong_count(&foreign_state), 1);
    }
}
