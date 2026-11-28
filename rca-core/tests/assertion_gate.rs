//! Checks gate ownership, re-entrant cleanup and manually ordered actor progress.

#![cfg(feature = "test-util")]

#[expect(dead_code, reason = "this target manually polls the captured actor")]
mod support;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::task::{Context, Poll, Waker};

use futures_util::FutureExt as _;
use rca_core::{
    Effect, MonotonicInstant, ReduceContext, Reducer, Runtime, RuntimeDriver, RuntimeFuture, Store,
    StoreBuildError, testing::AssertionGate,
};
use support::ManualRuntime;

/// Emits one even follow-up for the small odd inputs used by these tests.
struct Counter;

impl Reducer for Counter {
    type State = usize;
    type Action = u8;

    fn reduce(&mut self, state: &mut usize, action: &u8, _: &mut ReduceContext<'_>) -> Effect<u8> {
        *state += usize::from(*action);
        if *action % 2 == 1 {
            Effect::send(*action + 1)
        } else {
            Effect::none()
        }
    }
}

/// Builds a counter without polling the actor captured by the manual runtime.
fn build(
    runtime: &ManualRuntime,
    gate: &AssertionGate<u8>,
) -> Result<Store<usize, u8>, StoreBuildError> {
    Store::builder(0, Counter, runtime.runtime())
        .assertion_gate(gate.clone())
        .build()
}

#[test]
fn duplicate_attachment_fails_before_any_effect_or_second_actor_spawn() {
    let runtime = ManualRuntime::new();
    let gate = AssertionGate::new();
    let first = build(&runtime, &gate).unwrap();
    assert!(!gate.has_pending());
    let error = build(&runtime, &gate).expect_err("one gate cannot attach to two live actors");
    assert!(error.is_assertion_gate_attached());
    assert!(!error.is_zero_capacity());
    assert!(
        error
            .to_string()
            .starts_with("test gate is still attached or reserved by unfinished work\n")
    );
    assert!(format!("{error:?}").contains("AssertionGateAttached"));
    assert_eq!(runtime.spawn_count(), 1);
    drop(first);
    runtime.drop_queued();
}

#[test]
fn invalid_capacity_does_not_claim_gate() {
    let runtime = ManualRuntime::new();
    let gate = AssertionGate::new();
    let error = Store::builder(0, Counter, runtime.runtime())
        .assertion_gate(gate.clone())
        .mailbox_capacity(0)
        .build()
        .unwrap_err();
    assert!(error.is_zero_capacity());
    assert!(!error.is_assertion_gate_attached());
    assert_eq!(runtime.spawn_count(), 0);
    let valid = build(&runtime, &gate).expect("failed configuration must not retain attachment");
    assert_eq!(runtime.spawn_count(), 1);
    drop(valid);
    runtime.drop_queued();
}

#[test]
fn last_gate_setter_selects_one_controller_without_claiming_the_other() {
    use rca_core::testing::TransactionGate;

    for transaction_last in [false, true] {
        let runtime = ManualRuntime::new();
        let assertion = AssertionGate::new();
        let transaction = TransactionGate::new();
        let builder = Store::builder(0, Counter, runtime.runtime());
        let builder = if transaction_last {
            builder
                .assertion_gate(assertion.clone())
                .transaction_gate(transaction.clone())
        } else {
            builder
                .transaction_gate(transaction.clone())
                .assertion_gate(assertion.clone())
        };
        let selected = builder.build().unwrap();
        let assertion_only = build(&runtime, &assertion);
        let transaction_only = Store::builder(0, Counter, runtime.runtime())
            .transaction_gate(transaction)
            .build();
        let other = if transaction_last {
            assert!(transaction_only.unwrap_err().is_assertion_gate_attached());
            assertion_only.expect("the replaced assertion gate must remain available")
        } else {
            assert!(assertion_only.unwrap_err().is_assertion_gate_attached());
            transaction_only.expect("the replaced transaction gate must remain available")
        };
        assert_eq!(runtime.spawn_count(), 2);
        drop((selected, other));
        runtime.drop_queued();
    }
}

#[test]
fn dropping_unpolled_actor_releases_gate_even_with_store_handle_alive() {
    let runtime = ManualRuntime::new();
    let gate = AssertionGate::new();
    let first = build(&runtime, &gate).unwrap();
    drop(runtime.take_only_queued_task());
    let second = build(&runtime, &gate).expect("actor teardown releases attachment");
    assert_eq!(runtime.spawn_count(), 2);
    drop((first, second));
    runtime.drop_queued();
}

#[test]
fn final_store_handle_drop_does_not_release_live_actor_attachment() {
    let runtime = ManualRuntime::new();
    let gate = AssertionGate::new();
    let first = build(&runtime, &gate).unwrap();
    let mut actor = runtime.take_only_queued_task();
    drop(first);
    assert!(
        build(&runtime, &gate)
            .unwrap_err()
            .is_assertion_gate_attached(),
        "actor still owns cleanup"
    );
    assert_eq!(runtime.spawn_count(), 1);
    assert!(
        actor
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_ready()
    );
    drop(actor);
    let next = build(&runtime, &gate).expect("completed actor releases attachment");
    drop(next);
    runtime.drop_queued();
}

#[test]
fn concurrent_builders_attach_exactly_one_actor() {
    let runtime = ManualRuntime::new();
    let gate = AssertionGate::new();
    let start = Barrier::new(3);
    let results = std::thread::scope(|scope| {
        let attempts: Vec<_> = (0..2)
            .map(|_| {
                scope.spawn(|| {
                    start.wait();
                    build(&runtime, &gate)
                })
            })
            .collect();
        start.wait();
        attempts
            .into_iter()
            .map(|attempt| attempt.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(results.iter().filter(|result| result.is_err()).count(), 1);
    assert!(
        results
            .iter()
            .find_map(|result| result.as_ref().err())
            .unwrap()
            .is_assertion_gate_attached()
    );
    assert_eq!(runtime.spawn_count(), 1);
    drop(results);
    runtime.drop_queued();
}

#[test]
fn rejected_attachment_preserves_pending_inspection_and_commit() {
    let runtime = ManualRuntime::new();
    let gate = AssertionGate::new();
    assert_eq!(format!("{gate:?}"), "AssertionGate { pending: false, .. }");
    let store = build(&runtime, &gate).unwrap();
    let mut actor = runtime.take_only_queued_task();
    let mut cx = Context::from_waker(Waker::noop());
    let start = store.try_send(1).unwrap();
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    let task = start.finish().now_or_never().unwrap().unwrap();
    let mut effect = runtime.take_only_queued_task();
    assert!(effect.as_mut().poll(&mut cx).is_pending());
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    assert!(
        build(&runtime, &gate)
            .unwrap_err()
            .is_assertion_gate_attached(),
        "pending command still belongs to first actor"
    );
    let release = gate
        .inspect_pending(|action| {
            assert_eq!(*action, 2);
            assert_eq!(format!("{gate:?}"), "AssertionGate { pending: true, .. }");
            assert!(
                build(&runtime, &gate)
                    .unwrap_err()
                    .is_assertion_gate_attached(),
                "inspection cannot relinquish ownership"
            );
            assert!(gate.has_pending());
            gate.release_pending_with_receipt().unwrap()
        })
        .unwrap();
    assert_eq!(runtime.spawn_count(), 2, "one actor and its effect only");
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    let _ = release.finish().now_or_never().unwrap().unwrap();
    assert!(effect.as_mut().poll(&mut cx).is_ready());
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    assert!(task.is_finished());
    assert!(!gate.has_pending());
    assert_eq!(format!("{gate:?}"), "AssertionGate { pending: false, .. }");
    let mut read = Box::pin(store.read(|state| *state));
    assert!(read.as_mut().poll(&mut cx).is_pending());
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    assert_eq!(read.now_or_never().unwrap().unwrap(), 3);
    drop(store);
    assert!(actor.as_mut().poll(&mut cx).is_ready());
}

#[test]
fn stale_inspection_cannot_replace_new_actors_pending_command() {
    let runtime = ManualRuntime::new();
    let gate = AssertionGate::new();
    let old = build(&runtime, &gate).unwrap();
    let mut actor = runtime.take_only_queued_task();
    let mut cx = Context::from_waker(Waker::noop());
    let start = old.try_send(1).unwrap();
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    let old_task = start.finish().now_or_never().unwrap().unwrap();
    let mut old_effect = runtime.take_only_queued_task();
    assert!(old_effect.as_mut().poll(&mut cx).is_pending());
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    let (new, mut actor, mut effect, task) = gate
        .inspect_pending(|action| {
            assert_eq!(*action, 2);
            drop(old);
            assert!(actor.as_mut().poll(&mut cx).is_ready());
            drop(actor);
            let new = build(&runtime, &gate)
                .expect("completed owner may be replaced during old inspection");
            let mut actor = runtime.take_only_queued_task();
            let start = new.try_send(3).unwrap();
            assert!(actor.as_mut().poll(&mut cx).is_pending());
            let task = start.finish().now_or_never().unwrap().unwrap();
            let mut effect = runtime.take_only_queued_task();
            assert!(effect.as_mut().poll(&mut cx).is_pending());
            assert!(actor.as_mut().poll(&mut cx).is_pending());
            (new, actor, effect, task)
        })
        .unwrap();
    assert!(old_task.is_cancelled());
    assert!(old_effect.as_mut().poll(&mut cx).is_ready());
    assert_eq!(gate.inspect_pending(|action| *action), Some(4));
    assert!(gate.release_pending());
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    assert!(effect.as_mut().poll(&mut cx).is_ready());
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    assert!(task.is_finished());
    let mut read = Box::pin(new.read(|state| *state));
    assert!(read.as_mut().poll(&mut cx).is_pending());
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    assert_eq!(read.now_or_never().unwrap().unwrap(), 7);
    drop(new);
    assert!(actor.as_mut().poll(&mut cx).is_ready());
}

/// Refuses spawning so unwinding must release the actor's gate attachment.
#[derive(Debug)]
struct PanicSpawn(Runtime);

impl RuntimeDriver for PanicSpawn {
    fn spawn(&self, _task: RuntimeFuture) {
        panic!("driver refused actor");
    }
    fn now(&self) -> MonotonicInstant {
        self.0.now()
    }
    fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
        self.0.clock().sleep_until(deadline)
    }
    fn yield_now(&self) -> RuntimeFuture {
        let mut yielded = false;
        Box::pin(futures_util::future::poll_fn(move |cx| {
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

#[test]
fn spawn_driver_panic_releases_actor_attachment() {
    let runtime = ManualRuntime::new();
    let gate = AssertionGate::new();
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        Store::builder(0, Counter, Runtime::new(PanicSpawn(runtime.runtime())))
            .assertion_gate(gate.clone())
            .build()
    }))
    .unwrap_err();
    assert_eq!(panic.downcast_ref::<&str>(), Some(&"driver refused actor"));
    let recovered = build(&runtime, &gate).expect("unwound actor must release ownership");
    assert_eq!(runtime.spawn_count(), 1);
    drop(recovered);
    runtime.drop_queued();
}

/// Attempts to claim the same gate while an old owner releases user data.
struct ReentrantDrop {
    /// Identifies the attachment that must remain reserved during this drop.
    gate: AssertionGate<u8>,
    /// Captures any incorrectly admitted actor for deterministic cleanup.
    runtime: Runtime,
    /// Records the exact rejection rather than treating every error as success.
    rejected: Arc<AtomicBool>,
}

impl Drop for ReentrantDrop {
    fn drop(&mut self) {
        let attempt = Store::builder(0, Counter, self.runtime.clone())
            .assertion_gate(self.gate.clone())
            .build();
        self.rejected.store(
            attempt.is_err_and(|error| error.is_assertion_gate_attached()),
            Ordering::SeqCst,
        );
    }
}

#[test]
fn user_state_destructor_cannot_rebind_before_actor_teardown_finishes() {
    let runtime = ManualRuntime::new();
    let gate = AssertionGate::new();
    let rejected = Arc::new(AtomicBool::new(false));
    let state = ReentrantDrop {
        gate: gate.clone(),
        runtime: runtime.runtime(),
        rejected: Arc::clone(&rejected),
    };
    let reducer = rca_core::Reduce::new(|_: &mut ReentrantDrop, _: &u8, _| Effect::none());
    let store = Store::builder(state, reducer, runtime.runtime())
        .assertion_gate(gate.clone())
        .build()
        .unwrap();
    drop(runtime.take_only_queued_task());
    assert!(
        rejected.load(Ordering::SeqCst),
        "attachment must outlive user state destruction"
    );
    let next = build(&runtime, &gate).expect("attachment releases after full destruction");
    drop((store, next));
    runtime.drop_queued();
}

#[test]
fn rebind_drops_stale_assertion_capture_unlocked_with_new_claim_held() {
    let runtime = ManualRuntime::new();
    let gate = AssertionGate::new();
    let previous = build(&runtime, &gate).unwrap();
    drop(runtime.take_only_queued_task());
    drop(previous);
    let rejected = Arc::new(AtomicBool::new(false));
    let capture = ReentrantDrop {
        gate: gate.clone(),
        runtime: runtime.runtime(),
        rejected: Arc::clone(&rejected),
    };
    let receipt = gate.assert_state(move |_: &usize| drop(capture));
    let store = build(&runtime, &gate).unwrap();
    assert!(
        rejected.load(Ordering::SeqCst),
        "old capture is dropped without admitting a competing owner"
    );
    assert!(
        receipt
            .finish()
            .now_or_never()
            .expect("stale assertion receipt resolves")
            .is_err()
    );
    assert_eq!(runtime.spawn_count(), 2);
    drop(store);
    runtime.drop_queued();
}

#[test]
fn assertion_queued_before_first_install_runs_at_first_paused_effect() {
    let runtime = ManualRuntime::new();
    let gate = AssertionGate::new();
    let receipt = gate.assert_state(|state: &usize| assert_eq!(*state, 1));
    let store = build(&runtime, &gate).unwrap();
    let mut actor = runtime.take_only_queued_task();
    let mut cx = Context::from_waker(Waker::noop());
    let start = store.try_send(1).unwrap();
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    let task = start.finish().now_or_never().unwrap().unwrap();
    let mut effect = runtime.take_only_queued_task();
    assert!(effect.as_mut().poll(&mut cx).is_pending());
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    receipt
        .finish()
        .now_or_never()
        .expect("pre-install assertion executes once effect pauses")
        .expect("first installation preserves queued assertion");
    assert_eq!(gate.inspect_pending(|action| *action), Some(2));
    assert!(!task.is_finished());
    drop(store);
    assert!(actor.as_mut().poll(&mut cx).is_ready());
    assert!(effect.as_mut().poll(&mut cx).is_ready());
}

#[test]
fn receipt_waker_panic_during_actor_drop_does_not_poison_gate_or_abort() {
    /// Panics at the user callback boundary during receipt cleanup.
    struct PanicWake;
    impl std::task::Wake for PanicWake {
        fn wake(self: Arc<Self>) {
            panic!("receipt wake panic");
        }
    }

    let runtime = ManualRuntime::new();
    let gate = AssertionGate::new();
    let store = build(&runtime, &gate).unwrap();
    let mut actor = runtime.take_only_queued_task();
    let mut cx = Context::from_waker(Waker::noop());
    let start = store.try_send(1).unwrap();
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    let task = start.finish().now_or_never().unwrap().unwrap();
    let mut effect = runtime.take_only_queued_task();
    assert!(effect.as_mut().poll(&mut cx).is_pending());
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    let mut receipt = Box::pin(gate.release_pending_with_receipt().unwrap().finish());
    let waker = Waker::from(Arc::new(PanicWake));
    assert!(
        receipt
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );

    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(actor)))
        .expect_err("user waker panic propagates once without a second destructor panic");
    assert_eq!(panic.downcast_ref::<&str>(), Some(&"receipt wake panic"));
    assert!(
        !gate.has_pending(),
        "cleanup removes pending command before user wake"
    );
    receipt.now_or_never().unwrap().unwrap_err();
    let next = build(&runtime, &gate).expect("unwound actor releases an unpoisoned gate");
    assert_eq!(runtime.spawn_count(), 3);
    drop((store, task, effect, next));
    runtime.drop_queued();
}

#[test]
fn actor_drop_cancels_pending_effect_and_normal_release_receipt() {
    let runtime = ManualRuntime::new();
    let gate = AssertionGate::new();
    let store = build(&runtime, &gate).unwrap();
    let closed = store.closed();
    let mut actor = runtime.take_only_queued_task();
    let mut cx = Context::from_waker(Waker::noop());
    let start = store.try_send(1).unwrap();
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    let task = start.finish().now_or_never().unwrap().unwrap();
    let mut effect = runtime.take_only_queued_task();
    assert!(effect.as_mut().poll(&mut cx).is_pending());
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    let mut receipt = Box::pin(gate.release_pending_with_receipt().unwrap().finish());
    assert!(receipt.as_mut().poll(&mut cx).is_pending());

    drop(actor);

    assert!(!gate.has_pending());
    assert!(gate.pending_diagnostics().is_none());
    receipt
        .now_or_never()
        .expect("actor drop resolves release receipt")
        .unwrap_err();
    assert!(task.is_cancelled());
    assert!(effect.as_mut().poll(&mut cx).is_ready());
    assert!(task.finish().now_or_never().is_some());
    assert!(closed.now_or_never().is_some());
    let replacement =
        build(&runtime, &gate).expect("completed cancellation releases gate ownership");
    assert_eq!(runtime.spawn_count(), 3);
    drop((store, replacement));
    runtime.drop_queued();
}

#[test]
fn stale_inspection_drop_preserves_an_active_new_inspection() {
    let runtime = ManualRuntime::new();
    let gate = AssertionGate::new();
    let old = build(&runtime, &gate).unwrap();
    let mut old_actor = runtime.take_only_queued_task();
    let mut cx = Context::from_waker(Waker::noop());
    let start = old.try_send(1).unwrap();
    assert!(old_actor.as_mut().poll(&mut cx).is_pending());
    let old_task = start.finish().now_or_never().unwrap().unwrap();
    let mut old_effect = runtime.take_only_queued_task();
    assert!(old_effect.as_mut().poll(&mut cx).is_pending());
    assert!(old_actor.as_mut().poll(&mut cx).is_pending());

    std::thread::scope(|scope| {
        let (entered, entered_rx) = std::sync::mpsc::sync_channel(0);
        let (resume, resume_rx) = std::sync::mpsc::sync_channel(0);
        let old_gate = gate.clone();
        let inspection = scope.spawn(move || {
            old_gate
                .inspect_pending(|action| {
                    assert_eq!(*action, 2);
                    entered.send(()).unwrap();
                    resume_rx.recv().unwrap();
                })
                .expect("old command remains borrowed until rendezvous");
        });
        entered_rx.recv().unwrap();
        drop(old_actor);
        assert!(old_task.is_cancelled());
        assert!(old_effect.as_mut().poll(&mut cx).is_ready());

        let new = build(&runtime, &gate).unwrap();
        let mut actor = runtime.take_only_queued_task();
        let start = new.try_send(3).unwrap();
        assert!(actor.as_mut().poll(&mut cx).is_pending());
        let task = start.finish().now_or_never().unwrap().unwrap();
        let mut effect = runtime.take_only_queued_task();
        assert!(effect.as_mut().poll(&mut cx).is_pending());
        assert!(actor.as_mut().poll(&mut cx).is_pending());

        gate.inspect_pending(|action| {
            assert_eq!(*action, 4);
            resume.send(()).unwrap();
            inspection.join().unwrap();
            assert!(
                gate.has_pending(),
                "old guard must not clear new inspection token"
            );
            assert!(
                gate.inspect_pending(|_| ()).is_none(),
                "new command remains borrowed"
            );
        })
        .expect("new actor owns an independently active inspection");

        assert_eq!(gate.inspect_pending(|action| *action), Some(4));
        let receipt = gate.release_pending_with_receipt().unwrap();
        assert!(actor.as_mut().poll(&mut cx).is_pending());
        let _ = receipt.finish().now_or_never().unwrap().unwrap();
        assert!(effect.as_mut().poll(&mut cx).is_ready());
        assert!(actor.as_mut().poll(&mut cx).is_pending());
        assert!(task.is_finished());
        assert!(old_task.finish().now_or_never().is_some());
        let mut read = Box::pin(new.read(|state| *state));
        assert!(read.as_mut().poll(&mut cx).is_pending());
        assert!(actor.as_mut().poll(&mut cx).is_pending());
        assert_eq!(read.now_or_never().unwrap().unwrap(), 7);
        drop(new);
        assert!(actor.as_mut().poll(&mut cx).is_ready());
    });
    drop(old);
    assert!(!gate.has_pending());
}
