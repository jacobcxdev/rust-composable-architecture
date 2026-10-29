#![cfg(all(
    feature = "test-util",
    target_os = "macos",
    target_arch = "aarch64",
    not(loom)
))]
//! Publishes gated output during the first assertion-listener allocation window.
//!
//! Waiting must observe publication without releasing the pending action.
//! The caller then releases that action and checks its real causal completion.

#[path = "support/cold_listener_alloc.rs"]
mod cold_listener_alloc;
#[expect(dead_code, reason = "this test uses only manual actor polling")]
mod support;

use std::task::{Context, Poll, Waker};

use futures_util::FutureExt as _;
use rca_core::testing::AssertionGate;
use rca_core::{Effect, Reduce, Store};

#[test]
fn gate_wait_observes_publication_before_cold_listener_registration() {
    let runtime = support::ManualRuntime::new();
    let gate = AssertionGate::new();
    let reducer = Reduce::<u64, u8, _>::new(|state, action, _| {
        *state += u64::from(*action);
        if *action == 1 {
            Effect::send(2)
        } else {
            Effect::none()
        }
    });
    let store = Store::builder(0, reducer, runtime.runtime())
        .assertion_gate(gate.clone())
        .build()
        .expect("valid gated store");
    let mut actor = runtime.take_only_queued_task();
    let mut context = Context::from_waker(Waker::noop());
    let start = store.try_send(1).expect("start admitted");
    assert!(actor.as_mut().poll(&mut context).is_pending());
    let task = start
        .finish()
        .now_or_never()
        .expect("start reply")
        .expect("start committed");
    let mut effect = runtime.take_only_queued_task();
    // On macOS the first Mutex lock may allocate; warm it, not the Event.
    assert!(!gate.has_pending());
    cold_listener_alloc::prepare();
    let worker = std::thread::spawn(move || {
        let intercepted = cold_listener_alloc::await_interception();
        let mut context = Context::from_waker(Waker::noop());
        let pending = intercepted
            && effect.as_mut().poll(&mut context).is_pending()
            && actor.as_mut().poll(&mut context).is_pending();
        if pending {
            cold_listener_alloc::produced();
        }
        (actor, effect, pending)
    });
    let first_poll = {
        let mut waiting = std::pin::pin!(gate.wait_for_pending());
        cold_listener_alloc::arm();
        let result = waiting.as_mut().poll(&mut context);
        cold_listener_alloc::disarm();
        result
    };
    let (mut actor, mut effect, pending) = worker.join().expect("producer joined");
    cold_listener_alloc::assert_completed();
    assert!(pending);
    assert_eq!(first_poll, Poll::Ready(()));
    assert_eq!(gate.inspect_pending(|action| *action), Some(2));
    assert!(!task.is_finished(), "waiting did not release the action");
    let release = gate
        .release_pending_with_receipt()
        .expect("explicit release");
    assert!(actor.as_mut().poll(&mut context).is_pending());
    let released = release
        .finish()
        .now_or_never()
        .expect("release reply")
        .expect("response committed");
    assert!(effect.as_mut().poll(&mut context).is_ready());
    assert!(actor.as_mut().poll(&mut context).is_pending());
    assert!(task.is_finished());
    assert!(released.is_finished());
    assert!(!gate.has_pending());
    let mut shutdown = std::pin::pin!(store.shutdown_into_state());
    assert!(shutdown.as_mut().poll(&mut context).is_pending());
    assert!(actor.as_mut().poll(&mut context).is_ready());
    assert!(matches!(
        shutdown.as_mut().poll(&mut context),
        Poll::Ready(Ok(3))
    ));
}
