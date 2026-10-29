//! Preserves old enum-case handle retirement without macros or wall-clock scheduling.

#[expect(
    dead_code,
    reason = "this target uses only the captured actor host control"
)]
mod support;

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::task::{Context, Poll, Waker};

use rca_core::{ActionPath, CaseStatePath, Effect, Reduce, ReducerExt, RuntimeFuture, Store};
use support::ManualRuntime;

/// Keeps the payload unchanged when the active enum case is entered again.
enum Choice {
    Active(u8),
    Idle,
}

/// Separates child input from leaving and re-entering the active case.
enum Action {
    Child(u8),
    Leave,
    Reenter,
}

/// Polls synchronous actor work within a fixed bound, without an executor or clock.
///
/// # Panics
///
/// Panics if the operation needs more than sixteen actor polls.
fn drive<F: Future>(actor: &mut RuntimeFuture, future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    // Every operation here is synchronous actor work; bound scheduling, not time.
    for _ in 0..16 {
        if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return output;
        }
        let _ = actor.as_mut().poll(&mut context);
    }
    panic!("enum-case operation exceeded its bounded actor schedule");
}

// Reusing presence identity must not revive an old handle's user-code authority.
// Treating current case presence as renewed authority invokes the counted closures.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One case re-entry sequence preserves retired-handle selector and callback rejection"
)]
fn retired_case_handle_skips_selectors_embedders_and_callbacks_after_reentry() {
    let runtime = ManualRuntime::new();
    let reducer = Reduce::new(|state: &mut Choice, action: &Action, _| {
        match action {
            Action::Leave => *state = Choice::Idle,
            Action::Reenter => *state = Choice::Active(7),
            Action::Child(_) => {}
        }
        Effect::none()
    })
    .if_case(
        CaseStatePath::new(
            |state: &Choice| match state {
                Choice::Active(value) => Some(value),
                Choice::Idle => None,
            },
            |state: &mut Choice| match state {
                Choice::Active(value) => Some(value),
                Choice::Idle => None,
            },
        ),
        ActionPath::new(
            |action: &Action| match action {
                Action::Child(value) => Some(value),
                Action::Leave | Action::Reenter => None,
            },
            Action::Child,
        ),
        Reduce::new(|state: &mut u8, value: &u8, _| {
            *state = *value;
            Effect::none()
        }),
    );
    let store = Store::new(Choice::Active(7), reducer, runtime.runtime());
    let mut actor = runtime.take_only_queued_task();
    let selections = Arc::new(AtomicUsize::new(0));
    let embeddings = Arc::new(AtomicUsize::new(0));
    let callbacks = Arc::new(AtomicUsize::new(0));
    let old = drive(
        &mut actor,
        store.scope_case(
            CaseStatePath::new(
                {
                    let selections = Arc::clone(&selections);
                    move |state: &Choice| {
                        selections.fetch_add(1, Ordering::Relaxed);
                        match state {
                            Choice::Active(value) => Some(value),
                            Choice::Idle => None,
                        }
                    }
                },
                |state: &mut Choice| match state {
                    Choice::Active(value) => Some(value),
                    Choice::Idle => None,
                },
            ),
            ActionPath::new(
                |action: &Action| match action {
                    Action::Child(value) => Some(value),
                    Action::Leave | Action::Reenter => None,
                },
                {
                    let embeddings = Arc::clone(&embeddings);
                    move |value| {
                        embeddings.fetch_add(1, Ordering::Relaxed);
                        Action::Child(value)
                    }
                },
            ),
        ),
    )
    .unwrap();
    drop(drive(&mut actor, store.send(Action::Leave)).unwrap());
    drop(drive(&mut actor, store.send(Action::Reenter)).unwrap());
    selections.store(0, Ordering::Relaxed);

    assert!(
        drive(&mut actor, old.send(99))
            .unwrap_err()
            .is_scope_ended()
    );
    let read = drive(
        &mut actor,
        old.read({
            let callbacks = Arc::clone(&callbacks);
            move |value| {
                callbacks.fetch_add(1, Ordering::Relaxed);
                *value
            }
        }),
    );
    assert!(read.unwrap_err().is_scope_ended());
    let observation = drive(
        &mut actor,
        old.observe({
            let callbacks = Arc::clone(&callbacks);
            move |value| {
                callbacks.fetch_add(1, Ordering::Relaxed);
                *value
            }
        }),
    );
    assert!(observation.err().unwrap().is_scope_ended());
    assert_eq!(selections.load(Ordering::Relaxed), 0);
    assert_eq!(embeddings.load(Ordering::Relaxed), 0);
    assert_eq!(callbacks.load(Ordering::Relaxed), 0);
    assert_eq!(
        drive(
            &mut actor,
            store.read(|state| match state {
                Choice::Active(value) => Some(*value),
                Choice::Idle => None,
            })
        )
        .unwrap(),
        Some(7)
    );

    // A fresh handle works; rejecting the old handle is not root shutdown.
    let fresh = drive(
        &mut actor,
        store.scope_case(
            CaseStatePath::new(
                |state: &Choice| match state {
                    Choice::Active(value) => Some(value),
                    Choice::Idle => None,
                },
                |state: &mut Choice| match state {
                    Choice::Active(value) => Some(value),
                    Choice::Idle => None,
                },
            ),
            ActionPath::new(
                |action: &Action| match action {
                    Action::Child(value) => Some(value),
                    Action::Leave | Action::Reenter => None,
                },
                Action::Child,
            ),
        ),
    )
    .unwrap();
    drop(drive(&mut actor, fresh.send(8)).unwrap());
    assert_eq!(drive(&mut actor, fresh.read(|value| *value)).unwrap(), 8);
    drive(&mut actor, store.shutdown()).unwrap();
}
