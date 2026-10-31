//! Checks immediate predicate completion and cancellation across a real reducer boundary.

#[cfg(not(miri))]
#[path = "../src/allocation_test/child.rs"]
mod subprocess;

#[path = "support/thread_runtime.rs"]
mod support;

use rca_core::{Effect, PredicateWaitOutcome, ReduceContext, Reducer, Store};

/// Commits the requested Boolean state without spawning effect work.
struct Toggle;

impl Reducer for Toggle {
    type State = bool;
    type Action = bool;

    fn reduce(
        &mut self,
        state: &mut bool,
        action: &bool,
        _: &mut ReduceContext<'_>,
    ) -> Effect<bool> {
        *state = *action;
        Effect::none()
    }
}

// Catches skipping the registration's initial selection and waiting forever.
#[test]
fn initially_false_completes_without_an_action() {
    #[cfg(not(miri))]
    if subprocess::run_in_child("initially_false_completes_without_an_action") {
        return;
    }

    let runtime = support::ThreadRuntime::new();
    let store = Store::new(false, Toggle, runtime.runtime());
    let mut wait = store.wait_while(|state| *state);
    assert_eq!(format!("{wait:?}"), "PredicateWait { outcome: None, .. }");
    assert_eq!(
        futures_executor::block_on(&mut wait),
        PredicateWaitOutcome::Satisfied,
    );
    assert_eq!(
        format!("{wait:?}"),
        "PredicateWait { outcome: Some(Satisfied), .. }"
    );
}

// Catches subscribing only to changes after a synchronous reducer transition.
#[test]
fn synchronous_post_send_false_completes_without_another_action() {
    #[cfg(not(miri))]
    if subprocess::run_in_child("synchronous_post_send_false_completes_without_another_action") {
        return;
    }

    let runtime = support::ThreadRuntime::new();
    let store = Store::new(true, Toggle, runtime.runtime());
    assert_eq!(
        futures_executor::block_on(store.send_and_wait_while(false, |state| *state)),
        PredicateWaitOutcome::Satisfied,
    );
}

// Catches a cancellation request lost while reduction owns the commit boundary.
#[test]
fn cancellation_during_reduction_does_not_retract_commit_or_lose_causal_cleanup() {
    use std::sync::mpsc;
    use std::task::{Context, Poll, Waker};
    use std::time::Duration;

    /// Reports effect release without panicking if the test has already unwound.
    struct Retire(mpsc::Sender<()>);
    impl Drop for Retire {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }

    /// Holds reduction at an explicit gate while the caller requests cancellation.
    struct Blocked {
        entered: mpsc::Sender<()>,
        /// Disconnection also releases the gate when the test body unwinds.
        release: mpsc::Receiver<()>,
        retired: mpsc::Sender<()>,
    }
    impl Reducer for Blocked {
        type State = bool;
        type Action = ();
        fn reduce(&mut self, state: &mut bool, (): &(), _: &mut ReduceContext<'_>) -> Effect<()> {
            if self.entered.send(()).is_err() || self.release.recv().is_err() {
                return Effect::none();
            }
            *state = false;
            let guard = Retire(self.retired.clone());
            Effect::run(move |_| async move {
                let _guard = guard;
                std::future::pending::<()>().await;
            })
        }
    }

    // Bound a broken wait without changing the channel-ordered race.
    // Miri runs the same scenario directly under the surrounding job's limit.
    #[cfg(not(miri))]
    if subprocess::run_in_child(
        "cancellation_during_reduction_does_not_retract_commit_or_lose_causal_cleanup",
    ) {
        return;
    }

    // Channels drop before the runtime joins, releasing blocked reduction on unwind.
    let runtime = support::ThreadRuntime::new();
    let (entered, received) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let (retired, retirement) = mpsc::channel();
    let store = Store::new(
        true,
        Blocked {
            entered,
            release: released,
            retired,
        },
        runtime.runtime(),
    );
    let mut wait = store.send_and_wait_while((), |state| *state);
    assert_eq!(
        std::pin::Pin::new(&mut wait).poll(&mut Context::from_waker(Waker::noop())),
        Poll::Pending
    );
    // The deadline only bounds a broken test; channel handshakes order the race.
    received.recv_timeout(Duration::from_secs(5)).unwrap();
    wait.cancel();
    release.send(()).unwrap();
    assert_eq!(
        futures_executor::block_on(wait),
        PredicateWaitOutcome::Cancelled
    );
    assert!(!futures_executor::block_on(store.read(|state| *state)).unwrap());
    retirement.recv_timeout(Duration::from_secs(5)).unwrap();
    futures_executor::block_on(store.shutdown()).unwrap();
}
