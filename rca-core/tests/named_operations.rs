//! Qualifies named completion against actual retained runtime work.

#[expect(
    dead_code,
    reason = "manual host support is shared across integration targets"
)]
#[path = "support/mod.rs"]
mod support;

use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};

use rca_core::{
    Effect, Feature, FeatureContext, Operation, OperationPolicy, Reduce, RuntimeFuture, Store,
    Task, TaskAccessReason, TaskProgress,
};

/// Polls one future while the test retains control over every actor and body turn.
fn poll<T>(future: Pin<&mut (impl Future<Output = T> + ?Sized)>) -> Poll<T> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}

/// Requires exactly one actor turn between a pending request and its reply.
fn ordered<T>(actor: &mut RuntimeFuture, request: impl Future<Output = T>) -> T {
    let mut request = Box::pin(request);
    assert!(poll(request.as_mut()).is_pending());
    assert!(poll(actor.as_mut()).is_pending());
    match poll(request.as_mut()) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("actor-ordered request did not receive its reply"),
    }
}

/// Counts destruction of a real body or mapper capture without panicking.
struct CountDrop(Arc<AtomicUsize>);

impl Drop for CountDrop {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// Mapping preserves named completion while cancellation avoids both user callbacks.
#[test]
fn mapping_named_work_preserves_delivery_completion_and_cancellation() {
    /// Separates the effect declaration from its mapped output.
    enum Action {
        /// Starts a named body with a retained mapper.
        Start,
        /// Records the transformed value after admission.
        Value(usize),
    }

    for cancel_before_launch in [false, true] {
        let host = support::ManualRuntime::new();
        let body_drops = Arc::new(AtomicUsize::new(0));
        let mapper_drops = Arc::new(AtomicUsize::new(0));
        let mappings = Arc::new(AtomicUsize::new(0));
        let captured_body_drops = Arc::clone(&body_drops);
        let captured_mapper_drops = Arc::clone(&mapper_drops);
        let captured_mappings = Arc::clone(&mappings);
        let reducer = Reduce::<Vec<usize>, Action, _>::new(move |state, action, _| match action {
            Action::Start => {
                let body_capture = CountDrop(Arc::clone(&captured_body_drops));
                let mapper_capture = CountDrop(Arc::clone(&captured_mapper_drops));
                let mappings = Arc::clone(&captured_mappings);
                Effect::<usize>::run(move |sender| async move {
                    let _capture = body_capture;
                    sender.send(7).await.expect("mapped action committed");
                })
                .named("mapped", OperationPolicy::Coexist)
                .map(move |value| {
                    let _ = &mapper_capture;
                    mappings.fetch_add(1, Ordering::SeqCst);
                    Action::Value(value + 10)
                })
            }
            Action::Value(value) => {
                state.push(*value);
                Effect::none()
            }
        });
        let store = Store::new(Vec::new(), reducer, host.runtime());
        let operation = store.operation("mapped");
        let mut actor = host.take_only_queued_task();
        drop(store.try_send(Action::Start).unwrap());
        assert!(poll(actor.as_mut()).is_pending());
        let mut body = host.take_only_queued_task();
        assert_eq!(
            ordered(&mut actor, operation.status())
                .unwrap()
                .active_count(),
            1
        );
        let mut captured = Box::pin(ordered(&mut actor, operation.capture()).unwrap());
        assert!(poll(captured.as_mut()).is_pending());
        assert_eq!(mappings.load(Ordering::SeqCst), 0);
        assert_eq!(body_drops.load(Ordering::SeqCst), 0);
        assert_eq!(mapper_drops.load(Ordering::SeqCst), 0);

        if cancel_before_launch {
            ordered(&mut actor, operation.cancel()).unwrap();
            assert!(poll(captured.as_mut()).is_pending());
        } else {
            assert!(poll(body.as_mut()).is_pending());
            assert_eq!(mappings.load(Ordering::SeqCst), 1);
            assert!(poll(actor.as_mut()).is_pending());
            assert_eq!(ordered(&mut actor, store.read(Clone::clone)).unwrap(), [17]);
            assert!(poll(captured.as_mut()).is_pending());
        }

        assert!(poll(body.as_mut()).is_ready());
        assert_eq!(poll(captured.as_mut()), Poll::Ready(Ok(())));
        assert_eq!(body_drops.load(Ordering::SeqCst), 1);
        assert_eq!(mapper_drops.load(Ordering::SeqCst), 1);
        assert_eq!(
            ordered(&mut actor, operation.status())
                .unwrap()
                .active_count(),
            0
        );
        if cancel_before_launch {
            assert_eq!(mappings.load(Ordering::SeqCst), 0);
            assert!(ordered(&mut actor, store.read(Vec::is_empty)).unwrap());
        }
        drop(store);
        assert!(poll(actor.as_mut()).is_ready());
    }
}

/// Cancellation keeps an unpolled body counted until its captures are destroyed.
#[test]
fn cancelled_unpolled_body_stays_counted_until_its_captures_die() {
    let host = support::ManualRuntime::new();
    let drops = Arc::new(AtomicUsize::new(0));
    let factories = Arc::new(AtomicUsize::new(0));
    let user_polls = Arc::new(AtomicUsize::new(0));
    let mut capture = Some(CountDrop(Arc::clone(&drops)));
    let factory_counter = Arc::clone(&factories);
    let poll_counter = Arc::clone(&user_polls);
    let reducer = Reduce::<(), (), _>::new(move |(), (), _| {
        let capture = capture.take().unwrap();
        let factories = Arc::clone(&factory_counter);
        let user_polls = Arc::clone(&poll_counter);
        Effect::run(move |_| {
            factories.fetch_add(1, Ordering::SeqCst);
            async move {
                let _capture = capture;
                std::future::poll_fn(move |_| {
                    user_polls.fetch_add(1, Ordering::SeqCst);
                    Poll::<()>::Pending
                })
                .await;
            }
        })
        .named("work", OperationPolicy::Coexist)
    });
    let store = Store::new((), reducer, host.runtime());
    let operation = store.operation("work");
    let mut actor = host.take_only_queued_task();
    let receipt = store.try_send(()).unwrap();
    assert!(poll(actor.as_mut()).is_pending());
    let mut body = host.take_only_queued_task();
    assert_eq!(
        ordered(&mut actor, operation.status())
            .unwrap()
            .active_count(),
        1
    );
    let mut captured = Box::pin(ordered(&mut actor, operation.capture()).unwrap());
    ordered(&mut actor, operation.cancel()).unwrap();
    assert_eq!(
        ordered(&mut actor, operation.status())
            .unwrap()
            .active_count(),
        1
    );
    assert!(poll(captured.as_mut()).is_pending());
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    assert!(poll(body.as_mut()).is_ready());
    assert_eq!(factories.load(Ordering::SeqCst), 0);
    assert_eq!(user_polls.load(Ordering::SeqCst), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(poll(captured.as_mut()), Poll::Ready(Ok(())));
    assert_eq!(
        ordered(&mut actor, operation.status())
            .unwrap()
            .active_count(),
        0
    );
    // The inert Ready allocation is deliberately retained until after completion.
    drop((body, receipt, store));
    assert!(poll(actor.as_mut()).is_ready());
}

/// Captures select existing generations, while replacement cancels without inventing work.
#[test]
fn coexist_replace_empty_declaration_and_captured_cancellation_keep_distinct_generations() {
    let host = support::ManualRuntime::new();
    let drops = Arc::new(AtomicUsize::new(0));
    let capture_drops = Arc::clone(&drops);
    let reducer = Reduce::<(), u8, _>::new(move |(), action, _| {
        if *action == 2 {
            Effect::none().named("work", OperationPolicy::Replace)
        } else {
            let capture = CountDrop(Arc::clone(&capture_drops));
            Effect::run(move |_| async move {
                let _capture = capture;
                std::future::pending::<()>().await;
            })
            .named(
                "work",
                if *action == 1 {
                    OperationPolicy::Replace
                } else {
                    OperationPolicy::Coexist
                },
            )
        }
    });
    let store = Store::new((), reducer, host.runtime());
    let operation = store.operation("work");
    let mut actor = host.take_only_queued_task();
    assert!(poll(actor.as_mut()).is_pending());
    let mut empty = Box::pin(ordered(&mut actor, operation.capture()).unwrap());
    assert_eq!(poll(empty.as_mut()), Poll::Ready(Ok(())));

    drop(store.try_send(0).unwrap());
    assert!(poll(actor.as_mut()).is_pending());
    let mut first = host.take_only_queued_task();
    let first_capture = ordered(&mut actor, operation.capture()).unwrap();
    drop(store.try_send(0).unwrap());
    assert!(poll(actor.as_mut()).is_pending());
    let mut second = host.take_only_queued_task();
    assert_eq!(
        ordered(&mut actor, operation.status())
            .unwrap()
            .active_count(),
        2
    );
    drop(first_capture.finish_cancelling_on_drop());
    assert!(poll(actor.as_mut()).is_pending());
    assert!(poll(first.as_mut()).is_ready());
    assert!(poll(second.as_mut()).is_pending());
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(
        ordered(&mut actor, operation.status())
            .unwrap()
            .active_count(),
        1
    );
    assert_eq!(
        poll(empty.as_mut()),
        Poll::Ready(Ok(())),
        "empty capture never follows later work"
    );

    let mut second_capture = Box::pin(ordered(&mut actor, operation.capture()).unwrap());
    drop(store.try_send(1).unwrap());
    assert!(poll(actor.as_mut()).is_pending());
    let mut replacement = host.take_only_queued_task();
    assert_eq!(
        ordered(&mut actor, operation.status())
            .unwrap()
            .active_count(),
        2
    );
    assert!(poll(second_capture.as_mut()).is_pending());
    assert!(poll(second.as_mut()).is_ready());
    assert_eq!(poll(second_capture.as_mut()), Poll::Ready(Ok(())));
    assert!(poll(replacement.as_mut()).is_pending());
    assert_eq!(
        ordered(&mut actor, operation.status())
            .unwrap()
            .active_count(),
        1
    );

    drop(store.try_send(2).unwrap());
    assert!(poll(actor.as_mut()).is_pending());
    assert_eq!(
        ordered(&mut actor, operation.status())
            .unwrap()
            .active_count(),
        1,
        "zero-work Replace cancels old work but contributes no phantom generation"
    );
    assert!(poll(replacement.as_mut()).is_ready());
    assert_eq!(
        ordered(&mut actor, operation.status())
            .unwrap()
            .active_count(),
        0
    );
    assert_eq!(drops.load(Ordering::SeqCst), 3);
    drop(store);
    assert!(poll(actor.as_mut()).is_ready());
}

/// Equal named and ordinary cancellation keys address distinct registry entries.
#[test]
fn equal_named_and_ordinary_keys_have_independent_authority_in_the_real_registry() {
    let host = support::ManualRuntime::new();
    let reducer = Reduce::<(), u8, _>::new(|(), action, _| match action {
        0 => Effect::run(|_| std::future::pending()).cancellable("same"),
        1 => Effect::run(|_| std::future::pending()).named("same", OperationPolicy::Coexist),
        _ => Effect::cancel("same"),
    });
    let store = Store::new((), reducer, host.runtime());
    let mut actor = host.take_only_queued_task();
    drop(store.try_send(0).unwrap());
    assert!(poll(actor.as_mut()).is_pending());
    let mut ordinary = host.take_only_queued_task();
    drop(store.try_send(1).unwrap());
    assert!(poll(actor.as_mut()).is_pending());
    let mut named = host.take_only_queued_task();
    let operation = store.operation("same");
    assert_eq!(
        ordered(&mut actor, operation.status())
            .unwrap()
            .active_count(),
        1
    );
    drop(store.try_send(2).unwrap());
    assert!(poll(actor.as_mut()).is_pending());
    assert!(poll(ordinary.as_mut()).is_ready());
    assert!(poll(named.as_mut()).is_pending());
    assert_eq!(
        ordered(&mut actor, operation.status())
            .unwrap()
            .active_count(),
        1
    );
    ordered(&mut actor, operation.cancel()).unwrap();
    assert!(poll(named.as_mut()).is_ready());
    assert_eq!(
        ordered(&mut actor, operation.status())
            .unwrap()
            .active_count(),
        0
    );
    drop(store);
    assert!(poll(actor.as_mut()).is_ready());
}

/// Matches by value while a weak marker reveals registry retention.
struct OperationKey {
    /// Defines key equality and hashing independently of the lifetime marker.
    id: u8,
    /// Lives only in the effect's original key, not the observer's equivalent key.
    _lifetime: Option<Arc<()>>,
}

impl PartialEq for OperationKey {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for OperationKey {}

impl std::hash::Hash for OperationKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::hash::Hash::hash(&self.id, state);
    }
}

/// Records payload destruction and ordering without assertions inside cleanup.
#[derive(Debug, Default)]
struct PayloadObservation {
    /// Counts completed payload destructors.
    drops: AtomicUsize,
    /// Records any ready wait observed before payload destruction finishes.
    completed_early: AtomicBool,
}

/// Holds the last caller-owned payload until a pending send is abandoned.
#[derive(Debug)]
struct PayloadBeforeCompletion {
    /// Shares both drop observations in one allocation.
    observation: Arc<PayloadObservation>,
    /// Must remain pending until this payload's destructor returns.
    wait: Arc<Mutex<rca_core::OperationWait>>,
}

impl Drop for PayloadBeforeCompletion {
    fn drop(&mut self) {
        let completed_early = {
            let mut wait = self.wait.lock().unwrap();
            poll(Pin::new(&mut *wait)).is_ready()
        };
        self.observation
            .completed_early
            .store(completed_early, Ordering::SeqCst);
        self.observation.drops.fetch_add(1, Ordering::SeqCst);
    }
}

/// A failed payload-order check must not itself panic during cleanup.
#[test]
fn abandoned_payload_probe_records_early_completion_without_panicking() {
    let host = support::ManualRuntime::new();
    let store = Store::new(
        (),
        Reduce::<(), (), _>::new(|(), (), _| Effect::none()),
        host.runtime(),
    );
    let mut actor = host.take_only_queued_task();
    let operation = store.operation("already complete");
    let wait = Arc::new(Mutex::new(
        ordered(&mut actor, operation.capture()).unwrap(),
    ));
    assert_eq!(
        poll(Pin::new(&mut *wait.lock().unwrap())),
        Poll::Ready(Ok(()))
    );
    let observation = Arc::new(PayloadObservation::default());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        drop(PayloadBeforeCompletion {
            observation: Arc::clone(&observation),
            wait: Arc::clone(&wait),
        });
    }));
    drop(store);
    assert!(poll(actor.as_mut()).is_ready());
    assert!(
        result.is_ok(),
        "payload ordering diagnostics must not panic during drop"
    );
    assert_eq!(observation.drops.load(Ordering::SeqCst), 1);
    assert!(observation.completed_early.load(Ordering::SeqCst));
}

/// Counts actor notifications without polling or retaining the actor itself.
#[derive(Default)]
struct WakeCount(AtomicUsize);

impl Wake for WakeCount {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// Abandoning a full send destroys its payload, wakes cleanup and releases its key.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One abandoned-send sequence preserves payload reclamation, actor wake and named-key reuse"
)]
fn abandoning_full_caller_owned_send_wakes_idle_actor_and_reclaims_named_key() {
    /// Fills the mailbox before attempting a caller-owned effect send.
    #[derive(Debug)]
    enum Action {
        /// Starts named work that exports its sender.
        Start,
        /// Holds the sole mailbox slot until the actor consumes it.
        Filler,
        /// Carries a payload that must be destroyed without entering reduction.
        NeverAdmitted(PayloadBeforeCompletion),
    }
    let host = support::ManualRuntime::new();
    let finish = Arc::new(AtomicBool::new(false));
    let escaped = Arc::new(Mutex::new(None));
    let lifetime = Arc::new(());
    let key_lifetime = Arc::downgrade(&lifetime);
    let mut key = Some(OperationKey {
        id: 1,
        _lifetime: Some(lifetime),
    });
    let root_finish = Arc::clone(&finish);
    let root_escape = Arc::clone(&escaped);
    let reducer = Reduce::<(), Action, _>::new(move |(), action, _| match action {
        Action::Start => {
            let finish = Arc::clone(&root_finish);
            let escape = Arc::clone(&root_escape);
            Effect::run(move |sender| async move {
                *escape.lock().unwrap() = Some(sender);
                std::future::poll_fn(move |_| {
                    if finish.load(Ordering::SeqCst) {
                        Poll::Ready(())
                    } else {
                        Poll::Pending
                    }
                })
                .await;
            })
            .named(key.take().unwrap(), OperationPolicy::Coexist)
        }
        Action::Filler => Effect::none(),
        Action::NeverAdmitted(payload) => {
            let _ = &payload.observation;
            panic!("abandoned caller-owned request must not enter reduction")
        }
    });
    let store = Store::builder((), reducer, host.runtime())
        .mailbox_capacity(1)
        .build()
        .unwrap();
    let operation = store.operation(OperationKey {
        id: 1,
        _lifetime: None,
    });
    // Diagnostics neither require a Debug key nor start its operation.
    assert_eq!(format!("{operation:?}"), "Operation { .. }");
    assert_eq!(host.spawn_count(), 1);
    assert!(escaped.lock().unwrap().is_none());
    let mut actor = host.take_only_queued_task();
    drop(store.try_send(Action::Start).unwrap());
    assert!(poll(actor.as_mut()).is_pending());
    let mut body = host.take_only_queued_task();
    assert!(poll(body.as_mut()).is_pending());
    let sender = escaped.lock().unwrap().take().unwrap();
    let wait = Arc::new(Mutex::new(
        ordered(&mut actor, operation.capture()).unwrap(),
    ));
    let observation = Arc::new(PayloadObservation::default());

    drop(store.try_send(Action::Filler).unwrap());
    let mut pending = Box::pin(sender.send(Action::NeverAdmitted(PayloadBeforeCompletion {
        observation: Arc::clone(&observation),
        wait: Arc::clone(&wait),
    })));
    assert!(
        poll(pending.as_mut()).is_pending(),
        "capacity-one mailbox remains full"
    );
    finish.store(true, Ordering::SeqCst);
    assert!(poll(body.as_mut()).is_ready());

    // Consume the producer's completion wake and the filler, then leave the
    // full-send future unpolled even though capacity is now available.
    let notifications = Arc::new(WakeCount::default());
    let waker = Waker::from(Arc::clone(&notifications));
    assert!(
        actor
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    notifications.0.store(0, Ordering::SeqCst);
    let before_drop = poll(Pin::new(&mut *wait.lock().unwrap()));
    assert!(before_drop.is_pending());
    assert_eq!(observation.drops.load(Ordering::SeqCst), 0);

    drop(pending);
    drop(sender);
    assert_eq!(observation.drops.load(Ordering::SeqCst), 1);
    assert!(
        !observation.completed_early.load(Ordering::SeqCst),
        "payload must be destroyed before its final completion lease"
    );
    let after_drop = poll(Pin::new(&mut *wait.lock().unwrap()));
    assert_eq!(after_drop, Poll::Ready(Ok(())));
    assert!(
        notifications.0.load(Ordering::SeqCst) > 0,
        "off-actor final release must wake the idle actor"
    );
    assert!(
        key_lifetime.upgrade().is_some(),
        "registry still owns terminal metadata before control processing"
    );
    assert!(poll(actor.as_mut()).is_pending());
    assert!(
        key_lifetime.upgrade().is_none(),
        "control wake reclaims the key without another command/status request"
    );
    drop(store);
    assert!(poll(actor.as_mut()).is_ready());
}

/// Requires immediate rejection with the exact reason and no actor-side progress.
fn assert_operation_rejected<T>(
    future: impl Future<Output = Result<T, rca_core::TaskAccessError>>,
    reason: TaskAccessReason,
) {
    let mut future = std::pin::pin!(future);
    let Poll::Ready(Err(error)) = poll(future.as_mut()) else {
        panic!("operation must reject without an actor turn");
    };
    assert_eq!(error.reason(), reason);
    assert_eq!(error.progress(), TaskProgress::NotStarted);
}

/// Stale producer authority and a closed full mailbox reject every operation method.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one live named target distinguishes stale producer authority from blocked transport closure"
)]
fn escaped_operation_rejects_completed_producer_and_blocked_requests_close_without_actor() {
    /// Keeps named work alive while a separate task exports its operation endpoint.
    struct EndpointFeature {
        /// Receives the endpoint with the exporting task's authority.
        exported: Arc<Mutex<Option<Operation>>>,
        /// Counts destruction of the still-live named body.
        drops: Arc<AtomicUsize>,
        /// Detects commands that incorrectly reduce after store closure.
        reductions: Arc<AtomicUsize>,
    }

    impl Feature for EndpointFeature {
        type State = ();
        type Action = u8;

        fn reduce(&mut self, (): &mut (), action: &u8, context: &mut FeatureContext<'_, (), u8>) {
            match action {
                0 => {
                    let capture = CountDrop(Arc::clone(&self.drops));
                    context.task(Task::run_named(
                        "work",
                        OperationPolicy::Coexist,
                        move |_| async move {
                            let _capture = capture;
                            std::future::pending::<()>().await;
                        },
                    ));
                }
                1 => {
                    let exported = Arc::clone(&self.exported);
                    context.task(Task::run(move |context| async move {
                        *exported.lock().unwrap() = Some(context.operation("work"));
                    }));
                }
                _ => {
                    self.reductions.fetch_add(1, Ordering::SeqCst);
                }
            }
        }
    }

    let host = support::ManualRuntime::new();
    let exported = Arc::new(Mutex::new(None));
    let drops = Arc::new(AtomicUsize::new(0));
    let reductions = Arc::new(AtomicUsize::new(0));
    let store = Store::feature_builder(
        (),
        EndpointFeature {
            exported: Arc::clone(&exported),
            drops: Arc::clone(&drops),
            reductions: Arc::clone(&reductions),
        },
        host.runtime(),
    )
    .mailbox_capacity(1)
    .build()
    .unwrap();
    let mut actor = host.take_only_queued_task();
    let mut ready = std::pin::pin!(store.ready());
    assert!(matches!(poll(ready.as_mut()), Poll::Ready(Ok(()))));
    let target = store.operation("work");
    let named = ordered(&mut actor, store.send(0)).unwrap();
    let mut body = host.take_only_queued_task();
    assert!(poll(body.as_mut()).is_pending());
    let mut wait = Box::pin(ordered(&mut actor, target.capture()).unwrap());
    let producer = ordered(&mut actor, store.send(1)).unwrap();
    let mut producer_body = host.take_only_queued_task();
    assert!(poll(producer_body.as_mut()).is_ready());
    assert!(poll(actor.as_mut()).is_pending());
    assert!(producer.is_finished());
    let escaped = exported.lock().unwrap().take().unwrap();
    assert_eq!(
        ordered(&mut actor, target.status()).unwrap().active_count(),
        1
    );
    assert_operation_rejected(escaped.status(), TaskAccessReason::Cancelled);
    assert_operation_rejected(escaped.capture(), TaskAccessReason::Cancelled);
    assert_operation_rejected(escaped.cancel(), TaskAccessReason::Cancelled);
    assert!(!named.is_cancelled());
    assert!(!named.is_finished());
    assert!(poll(wait.as_mut()).is_pending());
    assert_eq!(drops.load(Ordering::SeqCst), 0);

    let mut filler = Box::pin(store.try_send(2).unwrap().finish());
    assert!(store.try_send(2).unwrap_err().is_full());
    assert!(poll(filler.as_mut()).is_pending());
    let mut status = Box::pin(target.status());
    let mut capture = Box::pin(target.capture());
    let mut cancel = Box::pin(target.cancel());
    assert!(poll(status.as_mut()).is_pending());
    assert!(poll(capture.as_mut()).is_pending());
    assert!(poll(cancel.as_mut()).is_pending());
    drop(store);
    assert_operation_rejected(status.as_mut(), TaskAccessReason::StoreClosed);
    assert_operation_rejected(capture.as_mut(), TaskAccessReason::StoreClosed);
    assert_operation_rejected(cancel.as_mut(), TaskAccessReason::StoreClosed);
    assert_eq!(reductions.load(Ordering::SeqCst), 0);
    assert!(poll(wait.as_mut()).is_pending());
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    assert!(poll(actor.as_mut()).is_ready());
    assert!(poll(body.as_mut()).is_ready());
    assert_eq!(poll(wait.as_mut()), Poll::Ready(Ok(())));
    assert!(named.is_finished());
    assert!(matches!(poll(filler.as_mut()), Poll::Ready(Err(_))));
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    drop((
        status,
        capture,
        cancel,
        escaped,
        target,
        producer_body,
        body,
        actor,
    ));
    assert_eq!(Arc::strong_count(&exported), 1);
    assert_eq!(Arc::strong_count(&drops), 1);
    assert_eq!(Arc::strong_count(&reductions), 1);
}

/// Runtime clock callbacks can close the final owner before operation admission.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "keeps the runtime callback, all three admitted operation requests and terminal cleanup in one public schedule"
)]
fn operation_actor_entry_rechecks_root_owner_after_runtime_callback() {
    /// Drops the final store owner on the next clock query.
    struct CloseOnClock {
        /// Lets the test drive the actor without starting an executor thread.
        queued: Arc<Mutex<Vec<RuntimeFuture>>>,
        /// Holds the sole store owner only after the request is admitted.
        owner: Arc<Mutex<Option<Store<(), ()>>>>,
        /// Identifies the exact clock query that performs closure.
        calls: Arc<AtomicUsize>,
    }

    impl std::fmt::Debug for CloseOnClock {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("CloseOnClock").finish_non_exhaustive()
        }
    }

    impl rca_core::RuntimeDriver for CloseOnClock {
        fn spawn(&self, task: RuntimeFuture) {
            self.queued.lock().unwrap().push(task);
        }

        fn now(&self) -> rca_core::MonotonicInstant {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let owner = self.owner.lock().unwrap().take();
            drop(owner);
            rca_core::MonotonicInstant::ZERO
        }

        fn sleep_until(&self, deadline: rca_core::MonotonicInstant) -> RuntimeFuture {
            Box::pin(async move {
                if deadline > rca_core::MonotonicInstant::ZERO {
                    std::future::pending::<()>().await;
                }
            })
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
    /// Exercises all operation commands through the same final-owner schedule.
    enum Request {
        /// Reads the active generation count.
        Status,
        /// Captures current generations for completion observation.
        Capture,
        /// Requests cancellation of current generations.
        Cancel,
    }

    for method in [Request::Status, Request::Capture, Request::Cancel] {
        let queued = Arc::new(Mutex::new(Vec::new()));
        let owner = Arc::new(Mutex::new(None));
        let calls = Arc::new(AtomicUsize::new(0));
        let reductions = Arc::new(AtomicUsize::new(0));
        let reduced = Arc::clone(&reductions);
        let drops = Arc::new(AtomicUsize::new(0));
        let capture = CountDrop(Arc::clone(&drops));
        let store = Store::new(
            (),
            Reduce::new(move |(): &mut (), (): &(), _| {
                let _ = &capture;
                reduced.fetch_add(1, Ordering::SeqCst);
                Effect::none()
            }),
            rca_core::Runtime::new(CloseOnClock {
                queued: Arc::clone(&queued),
                owner: Arc::clone(&owner),
                calls: Arc::clone(&calls),
            }),
        );
        let operation = store.operation("work");
        let mut closed = Box::pin(store.closed());
        let mut actor = {
            let mut queued = queued.lock().unwrap();
            assert_eq!(queued.len(), 1);
            queued.pop().unwrap()
        };
        // Finish startup with no mounts, effects or timers. The next clock
        // request belongs to the already-admitted operation's transaction.
        assert!(poll(actor.as_mut()).is_pending());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(poll(closed.as_mut()).is_pending());
        let mut request = Box::pin(async {
            match method {
                Request::Status => operation.status().await.map(|_| ()),
                Request::Capture => operation.capture().await.map(|_| ()),
                Request::Cancel => operation.cancel().await,
            }
        });
        assert!(poll(request.as_mut()).is_pending());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        *owner.lock().unwrap() = Some(store);

        // Runtime::now is a real driver callback: its final-owner drop occurs
        // after dispatch's closure check but before the target's admission.
        assert!(poll(actor.as_mut()).is_ready());
        assert_operation_rejected(request.as_mut(), TaskAccessReason::StoreClosed);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(owner.lock().unwrap().is_none());
        assert_eq!(poll(closed.as_mut()), Poll::Ready(()));
        assert_eq!(reductions.load(Ordering::SeqCst), 0);
        assert!(queued.lock().unwrap().is_empty());
        drop(request);
        drop((operation, actor, closed));
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(Arc::strong_count(&drops), 1);
        assert_eq!(Arc::strong_count(&reductions), 1);
        assert_eq!(Arc::strong_count(&owner), 1);
        assert_eq!(Arc::strong_count(&calls), 1);
        assert_eq!(Arc::strong_count(&queued), 1);
    }
}
