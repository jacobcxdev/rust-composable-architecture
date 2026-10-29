#[expect(
    dead_code,
    reason = "this target only manually polls the captured actor"
)]
mod support;

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};

use rca_core::{
    ActionPath, Effect, OptionalStatePath, PredicateWaitOutcome, Reduce, RuntimeFuture,
    ScopedStore, Store,
};

fn drive<F: Future>(actor: &mut RuntimeFuture, future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    for _ in 0..8 {
        if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
            return value;
        }
        let _ = actor.as_mut().poll(&mut context);
    }
    panic!("the manually ordered operation did not complete");
}

fn child(
    store: &Store<Option<bool>, Option<bool>>,
    actor: &mut RuntimeFuture,
) -> ScopedStore<bool, bool> {
    drive(
        actor,
        store.scope_some(
            OptionalStatePath::new(Option::as_ref, Option::as_mut),
            ActionPath::new(Option::as_ref, Some),
        ),
    )
    .unwrap()
}

// Cancelling a plain wait must drop its unstarted selector without admitting it.
#[test]
fn cancelling_an_unpolled_plain_wait_releases_its_capture_once() {
    let runtime = support::ManualRuntime::new();
    let store = Store::new(
        true,
        Reduce::new(|state: &mut bool, action: &bool, _| {
            *state = *action;
            Effect::none()
        }),
        runtime.runtime(),
    );
    let mut actor = runtime.take_only_queued_task();
    let capture = Arc::new(());
    let weak_capture = Arc::downgrade(&capture);
    let mut wait = store.wait_while(move |_| {
        std::hint::black_box(&capture);
        panic!("cancelled wait must not register its selector");
    });
    wait.cancel();
    assert!(weak_capture.upgrade().is_none());
    wait.cancel();
    assert_eq!(
        drive(&mut actor, &mut wait),
        PredicateWaitOutcome::Cancelled
    );
    wait.cancel();
    assert_eq!(drive(&mut actor, wait), PredicateWaitOutcome::Cancelled);
    assert!(drive(&mut actor, store.send(false)).unwrap().is_finished());
    assert!(!drive(&mut actor, store.read(|state| *state)).unwrap());
    drive(&mut actor, store.shutdown()).unwrap();
}

// Late cancellation must not replace a consumed success with Cancelled.
#[test]
fn satisfied_predicate_wait_keeps_its_outcome_after_cancellation() {
    let runtime = support::ManualRuntime::new();
    let store = Store::new(
        false,
        Reduce::new(|_: &mut bool, (): &(), _| Effect::none()),
        runtime.runtime(),
    );
    let mut actor = runtime.take_only_queued_task();
    let mut wait = store.wait_while(|state| *state);
    assert_eq!(
        drive(&mut actor, &mut wait),
        PredicateWaitOutcome::Satisfied
    );
    wait.cancel();
    assert_eq!(drive(&mut actor, wait), PredicateWaitOutcome::Satisfied);
    assert!(!store.is_closed());
    drive(&mut actor, store.shutdown()).unwrap();
}

// A true initial selection must keep waiting, and root closure must terminate it.
#[test]
fn pending_predicate_wait_reports_root_closure_without_a_false_selection() {
    let runtime = support::ManualRuntime::new();
    let store = Store::new(
        true,
        Reduce::new(|_: &mut bool, (): &(), _| Effect::none()),
        runtime.runtime(),
    );
    let mut actor = runtime.take_only_queued_task();
    let capture = Arc::new(());
    let weak_capture = Arc::downgrade(&capture);
    let mut wait = store.wait_while(move |state| {
        std::hint::black_box(&capture);
        *state
    });
    let mut context = Context::from_waker(Waker::noop());
    assert!(Pin::new(&mut wait).poll(&mut context).is_pending());
    assert!(actor.as_mut().poll(&mut context).is_pending());
    assert!(Pin::new(&mut wait).poll(&mut context).is_pending());
    assert!(weak_capture.upgrade().is_some());
    drive(&mut actor, store.shutdown()).unwrap();
    assert_eq!(
        drive(&mut actor, &mut wait),
        PredicateWaitOutcome::StoreStopped
    );
    assert!(weak_capture.upgrade().is_none());
    wait.cancel();
    assert_eq!(drive(&mut actor, wait), PredicateWaitOutcome::StoreStopped);
}

// Explicit cancellation must reach causal work already installed by a committed send.
#[test]
fn cancelling_an_admitted_predicate_wait_retires_its_causal_work() {
    let runtime = support::ManualRuntime::new();
    let capture = Arc::new(());
    let weak_capture = Arc::downgrade(&capture);
    let mut capture = Some(capture);
    let store = Store::new(
        0,
        Reduce::new(move |state: &mut usize, (): &(), _| {
            *state += 1;
            let capture = capture.take().unwrap();
            Effect::run(move |_| async move {
                std::future::pending::<()>().await;
                drop(capture);
            })
        }),
        runtime.runtime(),
    );
    let mut actor = runtime.take_only_queued_task();
    let mut wait = store.send_and_wait_while((), |_| true);
    let mut context = Context::from_waker(Waker::noop());
    assert!(Pin::new(&mut wait).poll(&mut context).is_pending());
    assert!(actor.as_mut().poll(&mut context).is_pending());
    assert!(Pin::new(&mut wait).poll(&mut context).is_pending());
    assert!(actor.as_mut().poll(&mut context).is_pending());
    assert!(Pin::new(&mut wait).poll(&mut context).is_pending());
    let mut effect = runtime.take_only_queued_task();
    assert!(effect.as_mut().poll(&mut context).is_pending());
    assert!(weak_capture.upgrade().is_some());

    wait.cancel();
    assert_eq!(drive(&mut actor, wait), PredicateWaitOutcome::Cancelled);
    assert!(actor.as_mut().poll(&mut context).is_pending());
    assert!(effect.as_mut().poll(&mut context).is_ready());
    assert!(weak_capture.upgrade().is_none());
    assert_eq!(drive(&mut actor, store.read(|state| *state)).unwrap(), 1);
    assert!(!store.is_closed());
    drive(&mut actor, store.shutdown()).unwrap();
}

// The owned scoped send must register after its commit and consume that false state.
#[test]
fn scoped_owned_send_observes_its_committed_false_selection() {
    let runtime = support::ManualRuntime::new();
    let store = Store::new(
        Some(true),
        Reduce::new(|state: &mut Option<bool>, action: &Option<bool>, _| {
            *state = *action;
            Effect::none()
        }),
        runtime.runtime(),
    );
    let mut actor = runtime.take_only_queued_task();
    let child = child(&store, &mut actor);
    let mut wait = child.send_and_wait_while(false, |state| *state);
    assert_eq!(
        drive(&mut actor, &mut wait),
        PredicateWaitOutcome::Satisfied
    );
    assert_eq!(
        drive(&mut actor, store.read(|state| *state)).unwrap(),
        Some(false)
    );
    wait.cancel();
    assert_eq!(drive(&mut actor, wait), PredicateWaitOutcome::Satisfied);
    drive(&mut actor, store.shutdown()).unwrap();
}

// Queued removal must reject later observation admission without invoking its selector.
#[test]
fn child_removal_rejects_queued_observation_and_later_owned_send() {
    let runtime = support::ManualRuntime::new();
    let store = Store::new(
        Some(true),
        Reduce::new(|state: &mut Option<bool>, action: &Option<bool>, _| {
            *state = *action;
            Effect::none()
        }),
        runtime.runtime(),
    );
    let mut actor = runtime.take_only_queued_task();
    let child = child(&store, &mut actor);
    let removal = store.try_send(None).unwrap();
    let selections = Arc::new(AtomicUsize::new(0));
    let selected = Arc::clone(&selections);
    let capture = Arc::new(());
    let weak_capture = Arc::downgrade(&capture);
    let mut registration = Box::pin(child.observe(move |state| {
        std::hint::black_box(&capture);
        selected.fetch_add(1, Ordering::Relaxed);
        *state
    }));
    let mut context = Context::from_waker(Waker::noop());
    assert!(registration.as_mut().poll(&mut context).is_pending());
    assert!(actor.as_mut().poll(&mut context).is_pending());
    let error = drive(&mut actor, registration).unwrap_err();
    assert!(error.is_scope_ended());
    assert!(!error.is_store_stopped());
    assert_eq!(selections.load(Ordering::Relaxed), 0);
    assert!(weak_capture.upgrade().is_none());
    assert!(drive(&mut actor, removal.finish()).unwrap().is_finished());
    assert_eq!(
        drive(
            &mut actor,
            child.send_and_wait_while(false, |_| {
                panic!("ended scoped send must not register a predicate");
            })
        ),
        PredicateWaitOutcome::ScopeEnded,
    );
    assert_eq!(drive(&mut actor, store.read(|state| *state)).unwrap(), None);
    assert!(!store.is_closed());
    drive(&mut actor, store.shutdown()).unwrap();
}

// A buffered take is not callback admission: closure still wins before invocation.
#[test]
fn root_closure_suppresses_a_callback_with_an_already_buffered_take_reply() {
    let runtime = support::ManualRuntime::new();
    let store = Store::new(
        0,
        Reduce::new(|state: &mut usize, (): &(), _| {
            *state += 1;
            Effect::none()
        }),
        runtime.runtime(),
    );
    let mut actor = runtime.take_only_queued_task();
    let observation = drive(&mut actor, store.observe(|state| *state)).unwrap();
    assert!(drive(&mut actor, store.send(())).unwrap().is_finished());
    let mut callback = Box::pin(observation.on_change(|_| {
        panic!("root closure must suppress a callback not yet admitted");
    }));
    let mut context = Context::from_waker(Waker::noop());
    // The first poll discards the initial value and submits a take for version 1.
    assert!(callback.as_mut().poll(&mut context).is_pending());
    assert!(actor.as_mut().poll(&mut context).is_pending());
    // The take reply is now buffered, but the callback has not resumed.
    let state = drive(&mut actor, store.clone().shutdown_into_state()).unwrap();
    assert_eq!(state, 1);
    assert!(!drive(&mut actor, callback));
}

// Cancelling admitted registration must skip its selector and release its capture.
#[test]
fn cancelling_observation_before_actor_acceptance_skips_selection() {
    let runtime = support::ManualRuntime::new();
    let store = Store::new(
        7,
        Reduce::new(|_: &mut usize, (): &(), _| Effect::none()),
        runtime.runtime(),
    );
    let mut actor = runtime.take_only_queued_task();
    let capture = Arc::new(());
    let weak_capture = Arc::downgrade(&capture);
    let mut registration = Box::pin(store.observe(move |_| {
        std::hint::black_box(&capture);
        panic!("cancelled observation must not run its selector");
    }));
    let mut context = Context::from_waker(Waker::noop());
    assert!(registration.as_mut().poll(&mut context).is_pending());
    drop(registration);
    assert!(weak_capture.upgrade().is_some());
    assert!(actor.as_mut().poll(&mut context).is_pending());
    assert!(weak_capture.upgrade().is_none());
    let mut values = drive(&mut actor, store.observe(|state| *state)).unwrap();
    assert_eq!(drive(&mut actor, values.next()), Some(7));
    drive(&mut actor, store.shutdown()).unwrap();
}

// A registered predicate wait must retain child identity, not root Store authority.
#[test]
fn pending_scoped_predicate_wait_does_not_keep_root_authority_alive() {
    let runtime = support::ManualRuntime::new();
    let store = Store::new(
        Some(true),
        Reduce::new(|_: &mut Option<bool>, _: &Option<bool>, _| Effect::none()),
        runtime.runtime(),
    );
    let mut actor = runtime.take_only_queued_task();
    let child = child(&store, &mut actor);
    let mut wait = child.wait_while(|state| *state);
    let mut context = Context::from_waker(Waker::noop());
    assert!(Pin::new(&mut wait).poll(&mut context).is_pending());
    assert!(actor.as_mut().poll(&mut context).is_pending());
    assert!(Pin::new(&mut wait).poll(&mut context).is_pending());
    drop((child, store));
    assert!(actor.as_mut().poll(&mut context).is_ready());
    drop(actor);
    assert_eq!(
        Pin::new(&mut wait).poll(&mut context),
        Poll::Ready(PredicateWaitOutcome::StoreStopped),
    );
}
