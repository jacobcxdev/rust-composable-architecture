use super::{Events, ordered, poll, support};
use rca_core::diagnostics::{CancellationCause, LifecycleKind};
use rca_core::{
    Effect, MonotonicInstant, OperationPolicy, Reduce, Runtime, RuntimeDriver, RuntimeFuture, Store,
};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

/// Keeps future timed waits pending until cancellation or actor teardown.
///
/// The fixed zero clock completes elapsed waits; manual polling controls each transition.
#[derive(Clone, Default)]
struct ClockHost(Arc<Mutex<Vec<RuntimeFuture>>>);
impl std::fmt::Debug for ClockHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StoppedClock")
    }
}
impl RuntimeDriver for ClockHost {
    fn spawn(&self, task: RuntimeFuture) {
        self.0.lock().unwrap().push(task);
    }
    fn now(&self) -> MonotonicInstant {
        MonotonicInstant::ZERO
    }
    fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
        if deadline <= MonotonicInstant::ZERO {
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
impl ClockHost {
    /// Takes the one queued actor or body task without polling another task.
    fn take(&self) -> RuntimeFuture {
        let mut queue = self.0.lock().unwrap();
        assert_eq!(queue.len(), 1);
        queue.pop().unwrap()
    }
}

#[test]
fn clock_host_elapsed_deadline_is_ready_on_first_poll() {
    let host = ClockHost::default();
    let mut wait = RuntimeDriver::sleep_until(&host, MonotonicInstant::ZERO);
    assert!(poll(wait.as_mut()).is_ready());
}

#[test]
fn clock_host_yield_wakes_once_before_completion() {
    struct Notification(AtomicUsize);
    impl Wake for Notification {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Release);
        }
    }

    let host = ClockHost::default();
    let mut yielding = RuntimeDriver::yield_now(&host);
    let notification = Arc::new(Notification(AtomicUsize::new(0)));
    let waker = Waker::from(Arc::clone(&notification));
    assert!(
        yielding
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    assert_eq!(notification.0.load(Ordering::Acquire), 1);
    assert!(poll(yielding.as_mut()).is_ready());
}

/// Checks normal terminal ordering and the final applied revision.
fn assert_terminal(events: &Mutex<Events>, revision: u64) {
    let (events, count) = events.lock().unwrap().take();
    assert_eq!(count, 3);
    for (event, expected) in events[..count].iter().zip([
        LifecycleKind::IngressClosed,
        LifecycleKind::CancellationRequested {
            cause: CancellationCause::ActorStop,
            scope: None,
        },
        LifecycleKind::ActorStopped,
    ]) {
        let event = event.unwrap();
        assert_eq!(event.kind(), expected);
        assert_eq!(event.last_committed_version(), revision);
    }
}

#[test]
fn action_task_control_cancellation_aggregates_timer_and_runtime_nodes_without_duplicate_events() {
    let host = ClockHost::default();
    let events = Arc::new(Mutex::new(Events::default()));
    let sink = Arc::clone(&events);
    let store = Store::builder(
        (),
        Reduce::new(|(): &mut (), action: &u8, _| {
            assert_eq!(*action, 0, "cancelled timer must not deliver");
            Effect::merge([
                Effect::run(|_| std::future::pending()),
                Effect::send_after(Duration::from_secs(100), 1),
            ])
        }),
        Runtime::new(host.clone()),
    )
    .with_lifecycle_diagnostics(move |event| sink.lock().unwrap().record(event))
    .build()
    .unwrap();
    let mut actor = host.take();
    assert!(poll(actor.as_mut()).is_pending());
    events.lock().unwrap().take();
    let task = ordered(&mut actor, store.send(0)).unwrap();
    let mut body = host.take();
    task.cancel();
    assert!(poll(actor.as_mut()).is_pending());
    let (observed, count) = events.lock().unwrap().take();
    assert_eq!(count, 1);
    assert_eq!(
        observed[0].unwrap().kind(),
        LifecycleKind::CancellationRequested {
            cause: CancellationCause::Control,
            scope: None,
        }
    );
    assert_eq!(observed[0].unwrap().last_committed_version(), 1);
    assert!(
        !task.is_finished(),
        "runtime still owns its cancelled future"
    );
    task.cancel();
    assert!(poll(actor.as_mut()).is_pending());
    assert_eq!(events.lock().unwrap().length, 0);
    assert!(poll(body.as_mut()).is_ready());
    drop(body);
    // The actor still owns the registry's lineage lease until it harvests the
    // completed runtime node. A cancellation fact does not shortcut that work.
    assert!(poll(actor.as_mut()).is_pending());
    assert!(task.is_finished());
    assert_eq!(events.lock().unwrap().length, 0);
    drop(store);
    assert!(poll(actor.as_mut()).is_ready());
    assert_terminal(&events, 1);
}

#[test]
fn timed_wait_stop_and_runtime_abandonment_reach_the_normal_terminal_join() {
    for shutdown in [false, true] {
        let host = ClockHost::default();
        let events = Arc::new(Mutex::new(Events::default()));
        let sink = Arc::clone(&events);
        let store = Store::builder(
            (),
            Reduce::new(|(): &mut (), _: &u8, _| Effect::send_after(Duration::from_secs(100), 1)),
            Runtime::new(host.clone()),
        )
        .with_lifecycle_diagnostics(move |event| sink.lock().unwrap().record(event))
        .build()
        .unwrap();
        let mut actor = host.take();
        assert!(poll(actor.as_mut()).is_pending());
        events.lock().unwrap().take();
        drop(ordered(&mut actor, store.send(0)).unwrap());
        if shutdown {
            let mut request = Box::pin(store.shutdown());
            assert!(poll(request.as_mut()).is_pending());
            assert!(poll(actor.as_mut()).is_ready());
            assert!(matches!(
                poll(request.as_mut()),
                std::task::Poll::Ready(Ok(()))
            ));
        } else {
            drop(store);
            assert!(poll(actor.as_mut()).is_ready());
        }
        assert_terminal(&events, 1);
    }

    let runtime = support::ManualRuntime::new();
    let events = Arc::new(Mutex::new(Events::default()));
    let sink = Arc::clone(&events);
    let store = Store::builder(
        (),
        Reduce::new(|(): &mut (), (): &(), _| Effect::run(|_| std::future::pending())),
        runtime.runtime(),
    )
    .with_lifecycle_diagnostics(move |event| sink.lock().unwrap().record(event))
    .build()
    .unwrap();
    let mut actor = runtime.take_only_queued_task();
    assert!(poll(actor.as_mut()).is_pending());
    events.lock().unwrap().take();
    drop(ordered(&mut actor, store.send(())).unwrap());
    drop(runtime.take_only_queued_task());
    assert!(poll(actor.as_mut()).is_ready());
    assert_terminal(&events, 1);
}

#[test]
fn actor_stopped_does_not_complete_named_work_retained_by_the_runtime() {
    let runtime = support::ManualRuntime::new();
    let events = Arc::new(Mutex::new(Events::default()));
    let sink = Arc::clone(&events);
    let store = Store::builder(
        (),
        Reduce::new(|(): &mut (), (): &(), _| {
            Effect::run(|_| std::future::pending()).named("retained", OperationPolicy::Coexist)
        }),
        runtime.runtime(),
    )
    .with_lifecycle_diagnostics(move |event| sink.lock().unwrap().record(event))
    .build()
    .unwrap();
    let mut actor = runtime.take_only_queued_task();
    assert!(poll(actor.as_mut()).is_pending());
    events.lock().unwrap().take();
    drop(ordered(&mut actor, store.send(())).unwrap());
    let body = runtime.take_only_queued_task();
    let mut captured =
        Box::pin(ordered(&mut actor, store.operation("retained").capture()).unwrap());
    drop(store);
    assert!(poll(actor.as_mut()).is_ready());
    assert_terminal(&events, 1);
    assert!(poll(captured.as_mut()).is_pending());
    drop(body);
    assert!(poll(captured.as_mut()).is_ready());
}

#[test]
fn forced_actor_drop_and_reducer_panic_do_not_emit_terminal_facts_from_cleanup() {
    for panic_in_reducer in [false, true] {
        let runtime = support::ManualRuntime::new();
        let events = Arc::new(Mutex::new(Events::default()));
        let sink = Arc::clone(&events);
        let store = Store::builder(
            (),
            Reduce::new(move |(): &mut (), (): &(), _| {
                assert!(!panic_in_reducer, "reducer panic");
                Effect::none()
            }),
            runtime.runtime(),
        )
        .with_lifecycle_diagnostics(move |event| sink.lock().unwrap().record(event))
        .build()
        .unwrap();
        let mut actor = runtime.take_only_queued_task();
        assert!(poll(actor.as_mut()).is_pending());
        events.lock().unwrap().take();
        if panic_in_reducer {
            drop(store.try_send(()).unwrap());
            let error = catch_unwind(AssertUnwindSafe(|| poll(actor.as_mut()))).unwrap_err();
            assert_eq!(error.downcast_ref::<&str>(), Some(&"reducer panic"));
        }
        drop(actor);
        assert_eq!(events.lock().unwrap().length, 0);
        drop(store);
    }
}
