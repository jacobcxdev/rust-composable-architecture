//! Actual actor unwinds preserve entry progress, reply ownership and terminal gate debt.
//! These checks distinguish rejected input from entered work; they do not promise rollback.

#[cfg(feature = "test-util")]
#[expect(dead_code, reason = "this target uses only captured actor controls")]
mod support;

#[cfg(feature = "test-util")]
use rca_core::testing::TransactionGate;
use rca_core::{
    Effect, Feature, FeatureContext, MountIdentity, OptionalMount, OutputError, Reduce,
    ReduceContext, RootMount, Runtime, RuntimeDriver, RuntimeFuture, Store, Task, TaskAccessReason,
    TaskContext, TaskProgress, TaskSendError,
};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
#[cfg(feature = "test-util")]
use std::task::Wake;
use std::task::{Context, Poll, Waker};
#[cfg(feature = "test-util")]
use support::ManualRuntime;

#[cfg(feature = "test-util")]
/// Exports a context from a pending task so later requests use real task ownership.
struct Root(Arc<Mutex<Option<TaskContext<usize, ()>>>>);

#[cfg(feature = "test-util")]
impl Feature for Root {
    type State = usize;
    type Action = ();

    fn update(&mut self, _: &mut usize, (): &(), context: &mut FeatureContext<'_, usize, ()>) {
        let captured = Arc::clone(&self.0);
        context.task(Task::run(move |context| async move {
            *captured.lock().unwrap() = Some(context);
            std::future::pending::<()>().await;
        }));
    }
}

#[cfg(feature = "test-util")]
/// Panics after reply publication to distinguish notification failure from operation failure.
struct PanicWake;
#[cfg(feature = "test-util")]
impl Wake for PanicWake {
    fn wake(self: Arc<Self>) {
        panic!("reply wake panic");
    }
}

#[cfg(feature = "test-util")]
/// Faults mutation, publication or reply wake and compares caller and gate outcomes.
fn typed_update_unwind(publication: bool, wake: bool) {
    let runtime = ManualRuntime::new();
    let captured = Arc::new(Mutex::new(None));
    let gate = TransactionGate::new();
    let store = Store::builder(0, Root(Arc::clone(&captured)), runtime.runtime())
        .transaction_gate(gate.clone())
        .build()
        .unwrap();
    let mut actor = runtime.take_only_queued_task();
    let mut cx = Context::from_waker(Waker::noop());
    assert!(gate.reserve_root());
    let mut start = Box::pin(store.send(()));
    assert!(start.as_mut().poll(&mut cx).is_pending());
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    assert!(matches!(start.as_mut().poll(&mut cx), Poll::Ready(Ok(_))));
    assert!(gate.acknowledge(gate.boundary().unwrap()));
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    let mut body = runtime.take_only_queued_task();
    assert!(body.as_mut().poll(&mut cx).is_pending());
    let context = captured.lock().unwrap().take().unwrap();

    let selections = Arc::new(AtomicUsize::new(0));
    let selected = Arc::clone(&selections);
    let mut registration = Box::pin(store.observe(move |state| {
        selected.fetch_add(1, Ordering::SeqCst);
        assert!(!publication || *state != 1, "publication panic");
        *state
    }));
    assert!(registration.as_mut().poll(&mut cx).is_pending());
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    let Poll::Ready(Ok(_observation)) = registration.as_mut().poll(&mut cx) else {
        panic!("observation registered before mutation");
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let called = Arc::clone(&calls);
    let mut update = Box::pin(context.update(move |state| {
        called.fetch_add(1, Ordering::SeqCst);
        *state = 1;
        assert!(publication || wake, "update panic");
    }));
    let panic_waker = Waker::from(Arc::new(PanicWake));
    let mut request_cx = Context::from_waker(if wake { &panic_waker } else { Waker::noop() });
    assert!(update.as_mut().poll(&mut request_cx).is_pending());
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    let boundary = gate.pending_boundary().unwrap();
    let boundary_id = gate.boundary().unwrap();
    assert!(gate.release_pending());
    catch_unwind(AssertUnwindSafe(|| actor.as_mut().poll(&mut cx)))
        .expect_err("released typed update must unwind");
    drop(actor);
    assert_eq!(calls.load(Ordering::SeqCst), 1, "update must not replay");
    assert_eq!(
        selections.load(Ordering::SeqCst),
        if publication || wake { 2 } else { 1 }
    );
    if wake {
        assert!(
            matches!(update.as_mut().poll(&mut cx), Poll::Ready(Ok(_))),
            "successful publication precedes the panicking wake"
        );
        assert_eq!(
            gate.match_terminal(&boundary, |_| panic!("success is not a terminal error")),
            None
        );
        assert!(
            gate.acknowledge(boundary_id),
            "successful reply retains committed assertion debt"
        );
    } else {
        let Poll::Ready(Err(error)) = update.as_mut().poll(&mut cx) else {
            panic!("reply guard publishes the actual failure");
        };
        assert_eq!(error.reason(), TaskAccessReason::ActorFailed);
        assert_eq!(
            error.progress(),
            if publication {
                TaskProgress::Committed { revision: 2 }
            } else {
                TaskProgress::MayHaveRun
            }
        );
        assert_eq!(
            gate.match_terminal(&boundary, |terminal| *terminal == error),
            Some(true),
            "gate must retain the exact result published by the primary reply guard"
        );
        assert!(gate.acknowledge_terminal(&boundary));
    }
    assert!(gate.boundary().is_none());
    drop(body);
}

#[cfg(feature = "test-util")]
#[test]
fn gated_update_panic_retains_the_callers_actual_error() {
    typed_update_unwind(false, false);
}

#[cfg(feature = "test-util")]
#[test]
fn gated_publication_panic_retains_the_callers_committed_revision() {
    typed_update_unwind(true, false);
}

#[cfg(feature = "test-util")]
#[test]
fn successful_reply_then_panicking_wake_does_not_invent_terminal_error() {
    typed_update_unwind(false, true);
}

#[cfg(feature = "test-util")]
/// Keeps terminal debt until acknowledgement while rejected queries release their captures.
fn legacy_action_unwind(publication: bool) {
    let runtime = ManualRuntime::new();
    let gate = TransactionGate::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let called = Arc::clone(&calls);
    let reducer = rca_core::Reduce::new(move |state: &mut usize, (): &(), _| {
        called.fetch_add(1, Ordering::SeqCst);
        *state = 1;
        assert!(publication, "legacy reducer panic");
        rca_core::Effect::none()
    });
    let store = Store::builder(0, reducer, runtime.runtime())
        .transaction_gate(gate.clone())
        .build()
        .unwrap();
    let mut actor = runtime.take_only_queued_task();
    let mut cx = Context::from_waker(Waker::noop());
    let mut registration = Box::pin(store.observe(move |state| {
        assert!(!publication || *state != 1, "legacy publication panic");
        *state
    }));
    assert!(registration.as_mut().poll(&mut cx).is_pending());
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    let Poll::Ready(Ok(_observation)) = registration.as_mut().poll(&mut cx) else {
        panic!("observation registered before mutation");
    };
    let mut send = Box::pin(store.send(()));
    assert!(send.as_mut().poll(&mut cx).is_pending());
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    let boundary = gate.pending_boundary().unwrap();
    let boundary_id = gate.boundary().unwrap();
    assert!(gate.release_pending());
    catch_unwind(AssertUnwindSafe(|| actor.as_mut().poll(&mut cx)))
        .expect_err("released legacy action must unwind");
    drop(actor);
    assert!(matches!(send.as_mut().poll(&mut cx), Poll::Ready(Err(_))));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let query_drops = Arc::new(AtomicUsize::new(0));
    let capture = OwnedPayload {
        identity: Box::new(7),
        drops: Arc::clone(&query_drops),
    };
    let mut query = Box::pin(
        gate.assert_state(move |_| {
            drop(capture);
            panic!("a detached actor cannot execute a diagnostic query");
        })
        .finish(),
    );
    assert!(matches!(query.as_mut().poll(&mut cx), Poll::Ready(Err(_))));
    assert_eq!(query_drops.load(Ordering::SeqCst), 1);
    assert_eq!(gate.boundary(), Some(boundary_id));
    let replacement = || {
        Store::builder(
            0,
            Reduce::new(|_: &mut usize, (): &(), _| Effect::none()),
            runtime.runtime(),
        )
        .transaction_gate(gate.clone())
        .build()
    };
    assert_eq!(
        gate.match_terminal(&boundary, |error| {
            assert!(
                replacement().is_err(),
                "terminal inspection prevents reattachment"
            );
            assert_eq!(runtime.spawn_count(), 1);
            assert_eq!(gate.boundary(), Some(boundary_id));
            error.reason() == TaskAccessReason::ActorFailed
                && error.progress()
                    == if publication {
                        TaskProgress::Committed { revision: 1 }
                    } else {
                        TaskProgress::MayHaveRun
                    }
        }),
        Some(true),
        "legacy panic uses the actual execution phase"
    );
    assert!(gate.acknowledge_terminal(&boundary));
    assert!(gate.boundary().is_none());
    let replacement = replacement().expect("acknowledged terminal debt permits reuse");
    assert_eq!(runtime.spawn_count(), 2);
    let mut actor = runtime.take_only_queued_task();
    drop(replacement);
    assert!(actor.as_mut().poll(&mut cx).is_ready());
    assert_eq!(query_drops.load(Ordering::SeqCst), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[cfg(feature = "test-util")]
#[test]
fn gated_legacy_reducer_panic_records_actual_entry() {
    legacy_action_unwind(false);
}

#[cfg(feature = "test-util")]
#[test]
fn gated_legacy_publication_panic_records_actual_revision() {
    legacy_action_unwind(true);
}

/// Captures actor tasks and faults only explicit clock reads during preparation.
#[derive(Clone, Default)]
struct ClockPanicDriver {
    tasks: Arc<Mutex<Vec<RuntimeFuture>>>,
    panic_now: Arc<AtomicBool>,
}

impl std::fmt::Debug for ClockPanicDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ClockPanicDriver")
    }
}

impl RuntimeDriver for ClockPanicDriver {
    fn spawn(&self, task: RuntimeFuture) {
        self.tasks.lock().unwrap().push(task);
    }

    fn now(&self) -> rca_core::MonotonicInstant {
        assert!(
            !self.panic_now.load(Ordering::SeqCst),
            "structural preparation clock panic"
        );
        rca_core::MonotonicInstant::from_elapsed(std::time::Duration::ZERO)
    }

    fn sleep_until(&self, deadline: rca_core::MonotonicInstant) -> RuntimeFuture {
        // Waiting does not move the intentionally armed now() fault boundary.
        if deadline <= rca_core::MonotonicInstant::ZERO {
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
    let driver = ClockPanicDriver::default();
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
    assert!(driver.tasks.lock().unwrap().is_empty());

    driver.panic_now.store(true, Ordering::SeqCst);
    let panic = catch_unwind(AssertUnwindSafe(|| driver.now()))
        .expect_err("the armed fixture clock must still panic");
    assert_eq!(
        panic.downcast_ref::<&str>().copied(),
        Some("structural preparation clock panic")
    );
    let mut elapsed = driver.sleep_until(rca_core::MonotonicInstant::ZERO);
    assert!(elapsed.as_mut().poll(&mut context).is_ready());
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
    let driver = ClockPanicDriver::default();
    let notifications = Arc::new(WakeCount(AtomicUsize::new(0)));
    let waker = Waker::from(Arc::clone(&notifications));
    let mut context = Context::from_waker(&waker);
    let mut future = driver.yield_now();
    assert_eq!(notifications.0.load(Ordering::SeqCst), 0);
    assert!(future.as_mut().poll(&mut context).is_pending());
    assert_eq!(notifications.0.load(Ordering::SeqCst), 1);
    assert!(future.as_mut().poll(&mut context).is_ready());
    assert_eq!(notifications.0.load(Ordering::SeqCst), 1);
    assert!(driver.tasks.lock().unwrap().is_empty());
}

#[cfg(feature = "test-util")]
#[test]
fn gated_structural_preparation_panic_records_not_started() {
    let driver = ClockPanicDriver::default();
    let gate = TransactionGate::new();
    let selections = Arc::new(AtomicUsize::new(0));
    let selected = Arc::clone(&selections);
    let factories = Arc::new(AtomicUsize::new(0));
    let factory = Arc::clone(&factories);
    let root = RootMount::new(Root(Arc::new(Mutex::new(None)))).on_initial(
        move |state: &usize| {
            selected.fetch_add(1, Ordering::SeqCst);
            *state
        },
        move |_, _| {
            factory.fetch_add(1, Ordering::SeqCst);
            Task::run(|_| async {})
        },
    );
    let _store = root
        .builder(0, Runtime::new(driver.clone()))
        .transaction_gate(gate.clone())
        .build()
        .unwrap();
    let mut actor = {
        let mut tasks = driver.tasks.lock().unwrap();
        assert_eq!(tasks.len(), 1);
        tasks.pop().unwrap()
    };
    let mut cx = Context::from_waker(Waker::noop());
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    let boundary = gate.pending_boundary().unwrap();
    assert!(gate.release_pending());
    driver.panic_now.store(true, Ordering::SeqCst);
    catch_unwind(AssertUnwindSafe(|| actor.as_mut().poll(&mut cx)))
        .expect_err("structural preparation clock must unwind");
    drop(actor);
    assert_eq!(selections.load(Ordering::SeqCst), 0);
    assert_eq!(factories.load(Ordering::SeqCst), 0);
    assert_eq!(
        gate.match_terminal(&boundary, |error| {
            error.reason() == TaskAccessReason::ActorFailed
                && error.progress() == TaskProgress::NotStarted
        }),
        Some(true),
        "structural preparation fails before any user callback enters"
    );
    assert!(gate.acknowledge_terminal(&boundary));
}

/// Separately checks original allocation identity and exactly-once disposal.
struct OwnedPayload {
    identity: Box<usize>,
    drops: Arc<AtomicUsize>,
}

impl Drop for OwnedPayload {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

enum SendAction {
    Start,
    Payload(OwnedPayload),
}

/// Exports a live task context before the later payload reduction deliberately panics.
struct SendRoot {
    captured: Arc<Mutex<Option<TaskContext<usize, SendAction>>>>,
    calls: Arc<AtomicUsize>,
}

impl Feature for SendRoot {
    type State = usize;
    type Action = SendAction;

    fn update(
        &mut self,
        state: &mut usize,
        action: &SendAction,
        context: &mut FeatureContext<'_, usize, SendAction>,
    ) {
        match action {
            SendAction::Start => {
                let captured = Arc::clone(&self.captured);
                context.task(Task::run(move |context| async move {
                    *captured.lock().unwrap() = Some(context);
                    std::future::pending::<()>().await;
                }));
            }
            SendAction::Payload(payload) => {
                self.calls.fetch_add(1, Ordering::SeqCst);
                *state = *payload.identity;
                panic!("typed send reducer panic");
            }
        }
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "one schedule compares gated and ungated ownership and terminal failure phases"
)]
/// Compares pre-entry payload return with entered failure, including gated builds.
fn typed_send_unwind(preparation: bool) {
    let driver = ClockPanicDriver::default();
    let captured = Arc::new(Mutex::new(None));
    let calls = Arc::new(AtomicUsize::new(0));
    #[cfg(feature = "test-util")]
    let gate = TransactionGate::new();
    let builder = Store::builder(
        0,
        SendRoot {
            captured: Arc::clone(&captured),
            calls: Arc::clone(&calls),
        },
        Runtime::new(driver.clone()),
    );
    #[cfg(feature = "test-util")]
    let builder = builder.transaction_gate(gate.clone());
    let store = builder.build().unwrap();
    let mut actor = driver.tasks.lock().unwrap().pop().unwrap();
    let mut cx = Context::from_waker(Waker::noop());
    #[cfg(feature = "test-util")]
    assert!(gate.reserve_root());
    let mut start = Box::pin(store.send(SendAction::Start));
    assert!(start.as_mut().poll(&mut cx).is_pending());
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    let Poll::Ready(Ok(task)) = start.as_mut().poll(&mut cx) else {
        panic!("context-producing action committed");
    };
    #[cfg(feature = "test-util")]
    assert!(gate.acknowledge(gate.boundary().unwrap()));
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    let mut body = driver.tasks.lock().unwrap().pop().unwrap();
    assert!(body.as_mut().poll(&mut cx).is_pending());
    let context = captured.lock().unwrap().take().unwrap();
    assert!(!task.is_finished());

    let drops = Arc::new(AtomicUsize::new(0));
    let payload = OwnedPayload {
        identity: Box::new(7),
        drops: Arc::clone(&drops),
    };
    let identity = std::ptr::from_ref(&*payload.identity);
    let mut send = Box::pin(context.send(SendAction::Payload(payload)));
    assert!(send.as_mut().poll(&mut cx).is_pending());
    #[cfg(feature = "test-util")]
    let (boundary, boundary_id) = {
        assert!(actor.as_mut().poll(&mut cx).is_pending());
        let boundary = gate.pending_boundary().unwrap();
        let boundary_id = gate.boundary().unwrap();
        assert!(gate.release_pending());
        (boundary, boundary_id)
    };
    driver.panic_now.store(preparation, Ordering::SeqCst);
    catch_unwind(AssertUnwindSafe(|| actor.as_mut().poll(&mut cx)))
        .expect_err("admitted typed send must unwind");
    drop(actor);

    let Poll::Ready(Err(result)) = send.as_mut().poll(&mut cx) else {
        panic!("typed reply guard publishes the actual failure");
    };
    let error = match result {
        TaskSendError::Rejected {
            action: SendAction::Payload(payload),
            error,
        } => {
            assert!(preparation, "an entered action cannot be returned");
            assert_eq!(std::ptr::from_ref(&*payload.identity), identity);
            assert_eq!(drops.load(Ordering::SeqCst), 0);
            drop(payload);
            error
        }
        TaskSendError::Failed { error } => {
            assert!(!preparation, "preparation must return the original action");
            error
        }
        TaskSendError::Rejected { .. } => panic!("the returned action changed variant"),
    };
    assert_eq!(calls.load(Ordering::SeqCst), usize::from(!preparation));
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(error.reason(), TaskAccessReason::ActorFailed);
    assert_eq!(
        error.progress(),
        if preparation {
            TaskProgress::NotStarted
        } else {
            TaskProgress::MayHaveRun
        }
    );
    #[cfg(feature = "test-util")]
    let final_boundary = {
        assert!(!gate.acknowledge(boundary_id), "no mutation committed");
        assert_eq!(gate.boundary(), Some(boundary_id));
        assert_eq!(
            gate.match_terminal(&boundary, |actual| *actual == error),
            Some(true)
        );
        assert!(gate.acknowledge_terminal(&boundary));
        gate.boundary()
    };
    #[cfg(feature = "test-util")]
    assert!(final_boundary.is_none());
    drop(body);
    assert!(Box::pin(task.finish()).as_mut().poll(&mut cx).is_ready());
    assert!(task.is_finished());
    assert!(driver.tasks.lock().unwrap().is_empty());
}

#[test]
fn typed_send_preparation_panic_returns_original_action() {
    typed_send_unwind(true);
}

#[test]
fn typed_send_reducer_panic_retains_entered_failure() {
    typed_send_unwind(false);
}

#[expect(
    clippy::too_many_lines,
    reason = "one mounted output schedule compares gated and ungated ownership and terminal phases"
)]
/// Tracks the original output allocation across preparation and handler failures.
fn owned_output_unwind(preparation: bool, gated: bool) {
    let driver = ClockPanicDriver::default();
    let calls = Arc::new(AtomicUsize::new(0));
    #[cfg(feature = "test-util")]
    let gate = gated.then(TransactionGate::new);
    #[cfg(not(feature = "test-util"))]
    assert!(!gated, "transaction gates require test support");
    let root = Reduce::new(|_: &mut Option<()>, (): &(), _: &mut ReduceContext<'_>| Effect::none());
    let mount = OptionalMount::new(
        Option::as_ref,
        Option::as_mut,
        |state: &Option<()>| {
            state.as_ref().map(|()| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        |()| Reduce::new(|(): &mut (), (): &(), _: &mut ReduceContext<'_>| Effect::none()),
    );
    let builder = RootMount::new(root).builder(Some(()), Runtime::new(driver.clone()));
    #[cfg(feature = "test-util")]
    let builder = if let Some(gate) = &gate {
        builder.transaction_gate(gate.clone())
    } else {
        builder
    };
    let (builder, site) = builder.mount_with_site(mount);
    let observed = Arc::clone(&calls);
    let (builder, output) = builder.output::<OwnedPayload>(&site, |handlers| {
        handlers.at_root(move |_, value, _| {
            observed.fetch_add(1, Ordering::SeqCst);
            assert_eq!(*value.identity, 7);
            panic!("owned output handler panic");
        });
    });
    let store = builder.build().unwrap();
    let mut actor = driver.tasks.lock().unwrap().pop().unwrap();
    let mut cx = Context::from_waker(Waker::noop());
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    #[cfg(feature = "test-util")]
    if let Some(gate) = &gate {
        let boundary = gate.boundary().unwrap();
        assert!(gate.release_pending());
        assert!(actor.as_mut().poll(&mut cx).is_pending());
        assert!(gate.acknowledge(boundary));
        assert!(actor.as_mut().poll(&mut cx).is_pending());
    }
    assert!(matches!(
        Box::pin(store.ready()).as_mut().poll(&mut cx),
        Poll::Ready(Ok(()))
    ));
    let mut acquire = Box::pin(store.acquire(&site));
    assert!(acquire.as_mut().poll(&mut cx).is_pending());
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    let Poll::Ready(Ok(Some(context))) = acquire.as_mut().poll(&mut cx) else {
        panic!("installed child context is acquired through its actual mount site");
    };
    drop(acquire);

    let drops = Arc::new(AtomicUsize::new(0));
    let payload = OwnedPayload {
        identity: Box::new(7),
        drops: Arc::clone(&drops),
    };
    let identity = std::ptr::from_ref(&*payload.identity);
    let mut delivery = Box::pin(context.output(&output, payload));
    assert!(delivery.as_mut().poll(&mut cx).is_pending());
    #[cfg(feature = "test-util")]
    let terminal = gate.as_ref().map(|gate| {
        assert!(actor.as_mut().poll(&mut cx).is_pending());
        let boundary = gate.pending_boundary().unwrap();
        let boundary_id = gate.boundary().unwrap();
        assert!(gate.release_pending());
        (gate, boundary, boundary_id)
    });
    driver.panic_now.store(preparation, Ordering::SeqCst);
    catch_unwind(AssertUnwindSafe(|| actor.as_mut().poll(&mut cx)))
        .expect_err("admitted owned output must unwind");
    drop(actor);

    let Poll::Ready(Err(result)) = delivery.as_mut().poll(&mut cx) else {
        panic!("owned output reply publishes the actual failure");
    };
    let error = match result {
        OutputError::Returned { value, error } => {
            assert!(preparation, "an entered output cannot be returned");
            assert_eq!(std::ptr::from_ref(&*value.identity), identity);
            assert_eq!(drops.load(Ordering::SeqCst), 0);
            drop(value);
            error
        }
        OutputError::Failed { error } => {
            assert!(!preparation, "preparation must return the original output");
            error
        }
    };
    assert_eq!(calls.load(Ordering::SeqCst), usize::from(!preparation));
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(error.reason(), TaskAccessReason::ActorFailed);
    assert_eq!(
        error.progress(),
        if preparation {
            TaskProgress::NotStarted
        } else {
            TaskProgress::MayHaveRun
        }
    );
    #[cfg(feature = "test-util")]
    if let Some((gate, boundary, boundary_id)) = terminal {
        assert!(
            !gate.acknowledge(boundary_id),
            "no output mutation committed"
        );
        assert_eq!(gate.boundary(), Some(boundary_id));
        assert_eq!(
            gate.match_terminal(&boundary, |actual| *actual == error),
            Some(true)
        );
        assert!(gate.acknowledge_terminal(&boundary));
        assert!(gate.boundary().is_none());
    }
    assert!(driver.tasks.lock().unwrap().is_empty());
    drop(delivery);
    drop(context);
    drop(store);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn owned_output_preparation_panic_returns_original_value() {
    owned_output_unwind(true, false);
    #[cfg(feature = "test-util")]
    owned_output_unwind(true, true);
}

#[test]
fn owned_output_handler_panic_retains_entered_failure() {
    owned_output_unwind(false, cfg!(feature = "test-util"));
}
