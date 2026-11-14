//! Named ownership across actor-owned timers, deferred sequences and teardown.

use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use rca_core::{
    Effect, MonotonicInstant, OperationPolicy, OperationWait, Reduce, Runtime, RuntimeDriver,
    RuntimeFuture, Store, TaskAccessReason,
};

/// Advances time only when a test asks, with every task polled by the test itself.
///
/// Advancing the clock does not wake sleepers. Callers explicitly poll the actor
/// afterwards and must take queued tasks before dropping this captured-task host.
#[derive(Clone, Default)]
struct ClockHost {
    /// Whole seconds since the test started.
    now: Arc<AtomicU64>,
    /// Captures spawned futures without running them.
    queued: Arc<Mutex<Vec<RuntimeFuture>>>,
}

impl std::fmt::Debug for ClockHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClockHost").finish_non_exhaustive()
    }
}

impl ClockHost {
    /// Gives the store a handle to this test's clock and task queue.
    fn runtime(&self) -> Runtime {
        Runtime::new(self.clone())
    }

    /// Takes the sole queued task, rejecting any unexpected spawn.
    fn take(&self) -> RuntimeFuture {
        let mut tasks = self.queued.lock().unwrap();
        assert_eq!(tasks.len(), 1);
        tasks.pop().unwrap()
    }

    /// Moves the clock forward one second without polling any future.
    fn advance(&self) {
        self.now.fetch_add(1, Ordering::SeqCst);
    }
}

impl RuntimeDriver for ClockHost {
    fn spawn(&self, task: RuntimeFuture) {
        self.queued.lock().unwrap().push(task);
    }

    fn now(&self) -> MonotonicInstant {
        MonotonicInstant::from_elapsed(Duration::from_secs(self.now.load(Ordering::SeqCst)))
    }

    fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
        let clock = self.clone();
        Box::pin(std::future::poll_fn(move |_| {
            if clock.now() >= deadline {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }))
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

/// Polls once without scheduling other work or advancing the clock.
fn poll<T>(future: Pin<&mut (impl Future<Output = T> + ?Sized)>) -> Poll<T> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}

/// Requires one actor turn to admit a request and produce its reply.
fn ordered<T>(actor: &mut RuntimeFuture, request: impl Future<Output = T>) -> T {
    let mut request = Box::pin(request);
    assert!(poll(request.as_mut()).is_pending());
    assert!(poll(actor.as_mut()).is_pending());
    match poll(request.as_mut()) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("missing reply"),
    }
}

/// Checks payload destruction before completion without asserting inside `Drop`.
#[derive(Clone, Default)]
struct Destruction {
    /// Observes the operation that must remain incomplete while its payload drops.
    wait: Arc<Mutex<Option<OperationWait>>>,
    /// Shares both observations in one allocation, separate from the wait lock.
    counts: Arc<DestructionCounts>,
}

/// Records normal drops and premature completion for assertions outside cleanup.
#[derive(Default)]
struct DestructionCounts {
    /// Counts every payload whose drop check returned.
    drops: AtomicUsize,
    /// Counts drops that found a completed wait instead of a pending one.
    early_completions: AtomicUsize,
}

/// Travels in a real timer payload or deferred factory capture.
struct Probe(Destruction);

impl std::fmt::Debug for Probe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Probe")
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        if let Some(wait) = self.0.wait.lock().unwrap().as_mut()
            && poll(Pin::new(wait)).is_ready()
        {
            self.0
                .counts
                .early_completions
                .fetch_add(1, Ordering::SeqCst);
        }
        self.0.counts.drops.fetch_add(1, Ordering::SeqCst);
    }
}

impl Destruction {
    /// Polls the installed operation capture without driving its producer.
    fn completed(&self) -> bool {
        poll(Pin::new(self.wait.lock().unwrap().as_mut().unwrap())) == Poll::Ready(Ok(()))
    }

    /// Checks exact destruction counts and ordering after the drop callbacks return.
    fn assert_drops(&self, expected: usize) {
        assert_eq!(self.counts.drops.load(Ordering::SeqCst), expected);
        assert_eq!(
            self.counts.early_completions.load(Ordering::SeqCst),
            0,
            "captured payload precedes completion"
        );
    }
}

/// A failed ownership check must not panic while another payload is being dropped.
#[test]
fn destruction_probe_records_early_completion_without_panicking() {
    let host = ClockHost::default();
    let store = Store::new(
        (),
        Reduce::<(), (), _>::new(|(), (), _| Effect::none()),
        host.runtime(),
    );
    let operation = store.operation("already complete");
    let mut actor = host.take();
    let destruction = Destruction::default();
    *destruction.wait.lock().unwrap() = Some(ordered(&mut actor, operation.capture()).unwrap());
    assert!(destruction.completed());

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        drop(Probe(destruction.clone()));
    }));
    drop(store);
    assert!(poll(actor.as_mut()).is_ready());
    assert!(
        result.is_ok(),
        "a probe must report early completion without panicking during drop"
    );
    assert_eq!(destruction.counts.drops.load(Ordering::SeqCst), 1);
    assert_eq!(
        destruction.counts.early_completions.load(Ordering::SeqCst),
        1,
        "the non-panicking probe must still record the failed ownership check"
    );
}

/// Cancelling a sequence destroys both the timer payload and the unstarted factory.
#[test]
fn sequence_handoff_keeps_timer_and_unstarted_suffix_captures_owned() {
    /// Separates the start request from a timer action that must never reduce.
    #[derive(Debug)]
    enum Action {
        /// Starts the three-part named sequence.
        Start,
        /// Carries the cancelled timer's payload.
        Timer(Probe),
    }
    let host = ClockHost::default();
    let destruction = Destruction::default();
    let probes = destruction.clone();
    let factories = Arc::new(AtomicUsize::new(0));
    let suffix_factories = Arc::clone(&factories);
    let reducer = Reduce::<(), Action, _>::new(move |(), action, _| match action {
        Action::Start => {
            let timer = Probe(probes.clone());
            let suffix = Probe(probes.clone());
            let factories = Arc::clone(&suffix_factories);
            Effect::concatenate([
                Effect::run(|_| async {}),
                Effect::send_after(Duration::from_secs(1), Action::Timer(timer)),
                Effect::run(move |_| {
                    factories.fetch_add(1, Ordering::SeqCst);
                    async move {
                        drop(suffix);
                    }
                }),
            ])
            .named("sequence", OperationPolicy::Coexist)
        }
        Action::Timer(probe) => {
            let _ = probe;
            panic!("cancelled timer must not reduce")
        }
    });
    let store = Store::new((), reducer, host.runtime());
    let operation = store.operation("sequence");
    let mut actor = host.take();
    drop(store.try_send(Action::Start).unwrap());
    assert!(poll(actor.as_mut()).is_pending());
    let mut first = host.take();
    *destruction.wait.lock().unwrap() = Some(ordered(&mut actor, operation.capture()).unwrap());
    assert!(poll(first.as_mut()).is_ready());
    assert!(
        !destruction.completed(),
        "remaining sequence owns the generation"
    );
    assert!(poll(actor.as_mut()).is_pending());
    assert_eq!(
        ordered(&mut actor, operation.status())
            .unwrap()
            .active_count(),
        1
    );
    ordered(&mut actor, operation.cancel()).unwrap();
    assert!(destruction.completed());
    destruction.assert_drops(2);
    assert_eq!(factories.load(Ordering::SeqCst), 0);
    drop(store);
    assert!(poll(actor.as_mut()).is_ready());
}

/// Replacing one due timer's generation also cancels a timer already popped with it.
#[test]
fn popped_due_timer_stays_owned_when_an_earlier_timer_replaces_its_generation() {
    /// Uses two timers due together to expose their ordering inside the actor.
    #[derive(Debug)]
    enum Action {
        /// Queues both timers in one named generation.
        Start,
        /// Replaces the current generation before the other timer reduces.
        Replace,
        /// Carries the second timer's payload, which must be destroyed instead.
        Cancelled(Probe),
    }
    let host = ClockHost::default();
    let destruction = Destruction::default();
    let probes = destruction.clone();
    let replaced = Arc::new(AtomicUsize::new(0));
    let replacements = Arc::clone(&replaced);
    let reducer = Reduce::<(), Action, _>::new(move |(), action, _| match action {
        Action::Start => Effect::merge([
            Effect::send_after(Duration::from_secs(1), Action::Replace),
            Effect::send_after(
                Duration::from_secs(1),
                Action::Cancelled(Probe(probes.clone())),
            ),
        ])
        .named("batch", OperationPolicy::Coexist),
        Action::Replace => {
            assert!(!probes.completed());
            replacements.fetch_add(1, Ordering::SeqCst);
            Effect::none().named("batch", OperationPolicy::Replace)
        }
        Action::Cancelled(probe) => {
            let _ = probe;
            panic!("already-popped cancelled timer must not reduce")
        }
    });
    let store = Store::new((), reducer, host.runtime());
    let operation = store.operation("batch");
    let mut actor = host.take();
    drop(store.try_send(Action::Start).unwrap());
    assert!(poll(actor.as_mut()).is_pending());
    *destruction.wait.lock().unwrap() = Some(ordered(&mut actor, operation.capture()).unwrap());
    host.advance();
    assert!(poll(actor.as_mut()).is_pending());
    assert_eq!(replaced.load(Ordering::SeqCst), 1);
    destruction.assert_drops(1);
    assert!(destruction.completed());
    assert_eq!(
        ordered(&mut actor, operation.status())
            .unwrap()
            .active_count(),
        0
    );
    drop(store);
    assert!(poll(actor.as_mut()).is_ready());
}

/// Repeated interval ticks keep one generation until either cancellation path runs.
#[test]
fn interval_renewal_retains_one_generation_until_factory_destruction() {
    for cancel_on_drop in [false, true] {
        let host = ClockHost::default();
        let destruction = Destruction::default();
        let probes = destruction.clone();
        let mut capture = Some(Probe(probes));
        let reducer = Reduce::<usize, bool, _>::new(move |state, start, _| {
            if *start {
                let probe = capture.take().unwrap();
                Effect::interval(Duration::from_secs(1), move || {
                    let _ = &probe;
                    false
                })
                .named("interval", OperationPolicy::Coexist)
            } else {
                *state += 1;
                Effect::none()
            }
        });
        let store = Store::new(0, reducer, host.runtime());
        let operation = store.operation("interval");
        let mut actor = host.take();
        drop(store.try_send(true).unwrap());
        assert!(poll(actor.as_mut()).is_pending());
        *destruction.wait.lock().unwrap() = Some(ordered(&mut actor, operation.capture()).unwrap());
        for tick in 1..=3 {
            host.advance();
            assert!(poll(actor.as_mut()).is_pending());
            assert!(!destruction.completed());
            assert_eq!(
                ordered(&mut actor, store.read(|state| *state)).unwrap(),
                tick
            );
            assert_eq!(
                ordered(&mut actor, operation.status())
                    .unwrap()
                    .active_count(),
                1
            );
        }
        if cancel_on_drop {
            let captured = ordered(&mut actor, operation.capture()).unwrap();
            drop(captured.finish_cancelling_on_drop());
            destruction.assert_drops(0);
            assert!(!destruction.completed());
            assert!(poll(actor.as_mut()).is_pending());
        } else {
            ordered(&mut actor, operation.cancel()).unwrap();
        }
        destruction.assert_drops(1);
        assert!(destruction.completed());
        assert_eq!(
            ordered(&mut actor, operation.status())
                .unwrap()
                .active_count(),
            0
        );
        host.advance();
        assert!(poll(actor.as_mut()).is_pending());
        assert_eq!(ordered(&mut actor, store.read(|state| *state)).unwrap(), 3);
        destruction.assert_drops(1);
        drop(store);
        assert!(poll(actor.as_mut()).is_ready());
    }
}

/// Dropping the actor rejects an admitted status request and destroys its timer.
#[test]
fn host_teardown_rejects_admitted_operation_metadata_and_releases_named_timer() {
    /// Carries a timer payload while another request fills the mailbox.
    #[derive(Debug)]
    enum Action {
        /// Starts one named timer.
        Start,
        /// Records any unexpected timer reduction before teardown.
        Timer(Probe),
    }
    let host = ClockHost::default();
    let destruction = Destruction::default();
    let probes = destruction.clone();
    let timer_calls = Arc::new(AtomicUsize::new(0));
    let reducer_calls = Arc::clone(&timer_calls);
    let reducer = Reduce::<(), Action, _>::new(move |(), action, _| match action {
        Action::Start => {
            Effect::send_after(Duration::from_secs(1), Action::Timer(Probe(probes.clone())))
                .named("queued metadata", OperationPolicy::Coexist)
        }
        Action::Timer(probe) => {
            let _ = probe;
            reducer_calls.fetch_add(1, Ordering::SeqCst);
            Effect::none()
        }
    });
    let store = Store::builder((), reducer, host.runtime())
        .mailbox_capacity(1)
        .build()
        .unwrap();
    let operation = store.operation("queued metadata");
    let mut actor = host.take();
    let task = ordered(&mut actor, store.send(Action::Start)).unwrap();
    *destruction.wait.lock().unwrap() = Some(ordered(&mut actor, operation.capture()).unwrap());
    assert!(!destruction.completed());
    assert!(!task.is_finished());

    let mut status = Box::pin(operation.status());
    assert!(poll(status.as_mut()).is_pending());
    assert!(store.try_send(Action::Start).unwrap_err().is_full());
    assert_eq!(timer_calls.load(Ordering::SeqCst), 0);
    destruction.assert_drops(0);
    // Drop the actual accepted actor future without another poll or panic.
    drop(actor);
    let Poll::Ready(Err(error)) = poll(status.as_mut()) else {
        panic!("host teardown must reject admitted operation metadata");
    };
    assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
    assert_eq!(timer_calls.load(Ordering::SeqCst), 0);
    destruction.assert_drops(1);
    assert!(destruction.completed());
    assert!(task.is_finished());
    assert!(host.queued.lock().unwrap().is_empty());
    drop(status);
    drop(operation);
    drop(task);
    drop(store);
    assert_eq!(Arc::strong_count(&timer_calls), 1);
}

/// Closing the actor cannot finish a body still held by the runtime host.
#[test]
fn actor_closure_does_not_finish_a_retained_unpolled_runtime_body() {
    for drop_runtime_body in [false, true] {
        let host = ClockHost::default();
        let destruction = Destruction::default();
        let mut probe = Some(Probe(destruction.clone()));
        let reducer = Reduce::<(), (), _>::new(move |(), (), _| {
            let probe = probe.take().unwrap();
            Effect::run(move |_| async move {
                let _probe = probe;
                std::future::pending::<()>().await;
            })
            .named("shutdown", OperationPolicy::Coexist)
        });
        let store = Store::new((), reducer, host.runtime());
        let operation = store.operation("shutdown");
        let mut actor = host.take();
        drop(store.try_send(()).unwrap());
        assert!(poll(actor.as_mut()).is_pending());
        let mut body = host.take();
        *destruction.wait.lock().unwrap() = Some(ordered(&mut actor, operation.capture()).unwrap());
        drop(store);
        assert!(poll(actor.as_mut()).is_ready());
        assert!(!destruction.completed());
        let mut status = Box::pin(operation.status());
        let Poll::Ready(Err(error)) = poll(status.as_mut()) else {
            panic!("closed metadata endpoint admitted work")
        };
        assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
        if drop_runtime_body {
            drop(body);
        } else {
            assert!(poll(body.as_mut()).is_ready());
            assert!(
                destruction.completed(),
                "retained inert Ready wrapper is not work"
            );
        }
        assert!(destruction.completed());
        destruction.assert_drops(1);
    }
}

/// Final store-owner release destroys a due timer held behind the assertion gate.
#[cfg(feature = "test-util")]
#[test]
fn final_owner_drop_releases_named_cancellable_timer_behind_assertion_gate() {
    /// Keeps a real timer payload in the gate without allowing it to reduce.
    #[derive(Debug)]
    enum Action {
        /// Starts one cancellable named timer.
        Start,
        /// Holds the payload until gate teardown.
        Timer(Probe),
    }
    let host = ClockHost::default();
    let destruction = Destruction::default();
    let probes = destruction.clone();
    let reducer = Reduce::<(), Action, _>::new(move |(), action, _| match action {
        Action::Start => {
            Effect::send_after(Duration::from_secs(1), Action::Timer(Probe(probes.clone())))
                .cancellable("timer")
                .named("gated timer", OperationPolicy::Coexist)
        }
        Action::Timer(probe) => {
            let _ = probe;
            panic!("unreleased gated timer must not reduce")
        }
    });
    let gate = rca_core::testing::AssertionGate::new();
    let store = Store::builder((), reducer, host.runtime())
        .assertion_gate(gate.clone())
        .build()
        .unwrap();
    let operation = store.operation("gated timer");
    let mut actor = host.take();
    let task = ordered(&mut actor, store.send(Action::Start)).unwrap();
    *destruction.wait.lock().unwrap() = Some(ordered(&mut actor, operation.capture()).unwrap());
    host.advance();
    assert!(poll(actor.as_mut()).is_pending());
    assert_eq!(
        gate.inspect_pending(|action| matches!(action, Action::Timer(_))),
        Some(true)
    );
    assert!(!destruction.completed());
    assert!(!task.is_finished());
    let mut closed = Box::pin(store.closed());

    drop(store);
    assert!(poll(actor.as_mut()).is_ready());
    assert!(poll(closed.as_mut()).is_ready());
    assert!(!gate.has_pending());
    destruction.assert_drops(1);
    assert!(destruction.completed());
    assert!(task.is_cancelled());
    assert!(poll(Box::pin(task.finish()).as_mut()).is_ready());
    assert!(host.queued.lock().unwrap().is_empty());
}
