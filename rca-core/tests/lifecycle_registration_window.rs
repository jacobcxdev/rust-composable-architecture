#![cfg(all(target_os = "macos", target_arch = "aarch64", not(loom)))]
//! Forces final-owner closure into the first lifecycle-listener allocation window.
//!
//! The allocation hook controls ordering, not elapsed time. The actor retains its
//! state until the caller polls teardown after the closure notification.

#[path = "../src/allocation_test/child.rs"]
mod child;
#[path = "support/cold_listener_alloc.rs"]
mod cold_listener_alloc;
#[expect(dead_code, reason = "this test uses only manual actor polling")]
mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};

use futures_util::FutureExt as _;
use rca_core::{
    Effect, MountIdentity, OptionalMount, Reduce, Store, TaskAccessReason, TaskProgress,
};

#[test]
fn closed_observes_last_owner_loss_before_cold_listener_registration() {
    if child::run_in_child("closed_observes_last_owner_loss_before_cold_listener_registration") {
        return;
    }

    let runtime = support::ManualRuntime::new();
    let state = Arc::new(());
    let reducer = Reduce::<Arc<()>, (), _>::new(|_, (), _| Effect::none());
    let store = Store::new(Arc::clone(&state), reducer, runtime.runtime());
    let mut actor = runtime.take_only_queued_task();
    let mut context = Context::from_waker(Waker::noop());
    assert!(actor.as_mut().poll(&mut context).is_pending());
    assert!(!store.is_closed());
    assert_eq!(Arc::strong_count(&state), 2);
    let waiting = store.closed();
    let after_teardown = store.closed();
    cold_listener_alloc::prepare();
    let worker = std::thread::spawn(move || {
        let intercepted = cold_listener_alloc::await_interception();
        drop(store);
        if intercepted {
            cold_listener_alloc::produced();
        }
        intercepted
    });
    let first_poll = {
        let mut waiting = std::pin::pin!(waiting);
        cold_listener_alloc::arm();
        let result = waiting.as_mut().poll(&mut context);
        cold_listener_alloc::disarm();
        result
    };
    let intercepted = worker.join().expect("final Store owner joined");
    cold_listener_alloc::assert_completed();
    assert!(intercepted);
    assert_eq!(
        first_poll,
        Poll::Ready(()),
        "last-owner closure before registration must not lose its notification"
    );
    assert_eq!(
        Arc::strong_count(&state),
        2,
        "authority ended before the actor released its state"
    );
    assert!(actor.as_mut().poll(&mut context).is_ready());
    assert_eq!(Arc::strong_count(&state), 1);
    assert_eq!(runtime.spawn_count(), 1);
    assert!(after_teardown.now_or_never().is_some());
}

#[test]
fn readiness_observes_last_owner_loss_before_cold_listener_registration() {
    if child::run_in_child("readiness_observes_last_owner_loss_before_cold_listener_registration") {
        return;
    }

    let runtime = support::ManualRuntime::new();
    let state = Arc::new(());
    let constructions = Arc::new(AtomicUsize::new(0));
    let constructed = Arc::clone(&constructions);
    let mount = OptionalMount::new(
        Option::as_ref,
        Option::as_mut,
        |root: &Option<Arc<()>>| {
            root.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| {
            constructed.fetch_add(1, Ordering::Relaxed);
            Reduce::<Arc<()>, (), _>::new(|_, (), _| Effect::none())
        },
    );
    let reducer = Reduce::<Option<Arc<()>>, (), _>::new(|_, (), _| Effect::none());
    let store = Store::builder(Some(Arc::clone(&state)), reducer, runtime.runtime())
        .mount(mount)
        .build()
        .unwrap();
    let mut actor = runtime.take_only_queued_task();
    let waiting = store.ready();
    let after_teardown = store.ready();
    let mut context = Context::from_waker(Waker::noop());

    // The unpolled actor has not installed its declared child. Readiness must
    // recheck closure when the first listener registers after owner loss.
    cold_listener_alloc::prepare();
    let worker = std::thread::spawn(move || {
        let intercepted = cold_listener_alloc::await_interception();
        drop(store);
        if intercepted {
            cold_listener_alloc::produced();
        }
        intercepted
    });
    let first_poll = {
        let mut waiting = std::pin::pin!(waiting);
        cold_listener_alloc::arm();
        let result = waiting.as_mut().poll(&mut context);
        cold_listener_alloc::disarm();
        result
    };
    let intercepted = worker.join().expect("final Store owner joined");
    cold_listener_alloc::assert_completed();
    assert!(intercepted);
    let Poll::Ready(Err(error)) = first_poll else {
        panic!("readiness must observe closure before listener registration");
    };
    assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), TaskProgress::NotStarted);
    assert_eq!(constructions.load(Ordering::Relaxed), 0);
    assert_eq!(Arc::strong_count(&state), 2);

    assert!(actor.as_mut().poll(&mut context).is_ready());
    assert_eq!(constructions.load(Ordering::Relaxed), 0);
    assert_eq!(Arc::strong_count(&state), 1);
    assert_eq!(runtime.spawn_count(), 1);
    let error = after_teardown.now_or_never().unwrap().unwrap_err();
    assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), TaskProgress::NotStarted);
}
