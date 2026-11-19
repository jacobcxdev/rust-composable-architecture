use super::{Events, ordered, poll, support};
use rca_core::diagnostics::{CancellationCause, LifecycleKind};
use rca_core::{Effect, OperationPolicy, Reduce, Store};
use std::sync::{Arc, Mutex};

/// Checks one applied cancellation fact and its precommit revision.
fn assert_request(
    events: &Mutex<Events>,
    cause: CancellationCause,
    scope: Option<u64>,
    revision: u64,
) {
    let (events, count) = events.lock().unwrap().take();
    assert_eq!(count, 1);
    let event = events[0].unwrap();
    assert_eq!(
        event.kind(),
        LifecycleKind::CancellationRequested { cause, scope }
    );
    assert_eq!(event.last_committed_version(), revision);
}

#[test]
fn explicit_empty_requests_and_compiler_replacements_report_the_applied_revision() {
    let runtime = support::ManualRuntime::new();
    let events = Arc::new(Mutex::new(Events::default()));
    let sink = Arc::clone(&events);
    let reducer = Reduce::new(|(): &mut (), action: &u8, _| match action {
        0 => Effect::run(|_| std::future::pending()).named("named", OperationPolicy::Coexist),
        1 => Effect::run(|_| std::future::pending()).named("named", OperationPolicy::Replace),
        2 => Effect::cancel("ordinary"),
        3 => Effect::run(|_| std::future::pending()).cancellable("ordinary"),
        4 => Effect::run(|_| std::future::pending()).cancellable_in_flight("ordinary"),
        _ => Effect::none(),
    });
    let store = Store::builder((), reducer, runtime.runtime())
        .with_lifecycle_diagnostics(move |event| sink.lock().unwrap().record(event))
        .build()
        .unwrap();
    let mut actor = runtime.take_only_queued_task();
    assert!(poll(actor.as_mut()).is_pending());
    events.lock().unwrap().take();

    ordered(&mut actor, store.operation("empty").cancel()).unwrap();
    assert_request(&events, CancellationCause::Explicit, Some(0), 0);
    drop(ordered(&mut actor, store.send(2)).unwrap());
    assert_request(&events, CancellationCause::Explicit, Some(0), 1);
    drop(ordered(&mut actor, store.send(0)).unwrap());
    let mut first_named = runtime.take_only_queued_task();
    assert_eq!(events.lock().unwrap().length, 0);
    let mut captured = Box::pin(ordered(&mut actor, store.operation("named").capture()).unwrap());
    drop(ordered(&mut actor, store.send(1)).unwrap());
    let replacement_named = runtime.take_only_queued_task();
    assert_request(&events, CancellationCause::Replacement, Some(0), 3);
    assert!(poll(captured.as_mut()).is_pending());
    // Let the real cancelled wrapper terminate. Dropping an unpolled runtime
    // wrapper instead reports provider abandonment and stops the actor.
    assert!(poll(first_named.as_mut()).is_ready());
    drop(first_named);
    assert!(poll(captured.as_mut()).is_ready());

    drop(ordered(&mut actor, store.send(3)).unwrap());
    let first_ordinary = runtime.take_only_queued_task();
    assert_eq!(events.lock().unwrap().length, 0);
    drop(ordered(&mut actor, store.send(4)).unwrap());
    let replacement_ordinary = runtime.take_only_queued_task();
    assert_request(&events, CancellationCause::Replacement, Some(0), 5);
    // A subsequent idle control pass must not report a cancellation already
    // applied synchronously by either compiler replacement.
    assert!(poll(actor.as_mut()).is_pending());
    assert_eq!(events.lock().unwrap().length, 0);
    drop((
        first_ordinary,
        replacement_named,
        replacement_ordinary,
        store,
    ));
    assert!(poll(actor.as_mut()).is_ready());
}

#[test]
fn captured_wait_cancellation_reports_one_control_request_without_finishing_retained_work() {
    let runtime = support::ManualRuntime::new();
    let events = Arc::new(Mutex::new(Events::default()));
    let sink = Arc::clone(&events);
    let store = Store::builder(
        (),
        Reduce::new(|(): &mut (), (): &(), _| {
            Effect::run(|_| std::future::pending()).named("named", OperationPolicy::Coexist)
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
    let operation = store.operation("named");
    let cancelling = ordered(&mut actor, operation.capture()).unwrap();
    let mut completion = Box::pin(ordered(&mut actor, operation.capture()).unwrap());
    drop(cancelling.finish_cancelling_on_drop());
    assert!(poll(actor.as_mut()).is_pending());
    assert_request(&events, CancellationCause::Control, None, 1);
    assert!(poll(completion.as_mut()).is_pending());
    assert!(poll(actor.as_mut()).is_pending());
    assert_eq!(events.lock().unwrap().length, 0);
    drop(body);
    assert!(poll(completion.as_mut()).is_ready());
    drop(store);
    assert!(poll(actor.as_mut()).is_ready());
}

#[test]
fn control_cancellation_reports_a_generation_retained_only_by_an_unadmitted_command() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::Poll;

    let runtime = support::ManualRuntime::new();
    let events = Arc::new(Mutex::new(Events::default()));
    let sink = Arc::clone(&events);
    let escaped = Arc::new(Mutex::new(None));
    let escape = Arc::clone(&escaped);
    let finish = Arc::new(AtomicBool::new(false));
    let finish_body = Arc::clone(&finish);
    let store = Store::builder(
        (),
        Reduce::new(move |(): &mut (), action: &u8, _| match action {
            0 => {
                let escape = Arc::clone(&escape);
                let finish = Arc::clone(&finish_body);
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
                .named("command", OperationPolicy::Coexist)
            }
            1 => Effect::none(),
            _ => panic!("cancelled unadmitted command must not reduce"),
        }),
        runtime.runtime(),
    )
    .mailbox_capacity(1)
    .with_lifecycle_diagnostics(move |event| sink.lock().unwrap().record(event))
    .build()
    .unwrap();
    let mut actor = runtime.take_only_queued_task();
    assert!(poll(actor.as_mut()).is_pending());
    events.lock().unwrap().take();
    drop(ordered(&mut actor, store.send(0)).unwrap());
    let mut body = runtime.take_only_queued_task();
    assert!(poll(body.as_mut()).is_pending());
    let sender = escaped.lock().unwrap().take().unwrap();
    let operation = store.operation("command");
    let cancelling = ordered(&mut actor, operation.capture()).unwrap();
    let mut completion = Box::pin(ordered(&mut actor, operation.capture()).unwrap());
    drop(store.try_send(1).unwrap());
    let mut pending = Box::pin(sender.send(2));
    assert!(poll(pending.as_mut()).is_pending());
    finish.store(true, Ordering::SeqCst);
    assert!(poll(body.as_mut()).is_ready());
    assert!(poll(actor.as_mut()).is_pending());
    assert!(poll(completion.as_mut()).is_pending());
    assert_eq!(events.lock().unwrap().length, 0);

    // The runtime node has been harvested. Only the caller-owned send lease
    // retains the named generation, so a task-node-only hook misses this fact.
    drop(cancelling.finish_cancelling_on_drop());
    assert!(poll(actor.as_mut()).is_pending());
    assert_request(&events, CancellationCause::Control, None, 2);
    assert!(poll(actor.as_mut()).is_pending());
    assert_eq!(events.lock().unwrap().length, 0);
    assert!(poll(completion.as_mut()).is_pending());
    drop(pending);
    assert!(poll(completion.as_mut()).is_ready());
    drop((sender, body, store));
    assert!(poll(actor.as_mut()).is_ready());
}
