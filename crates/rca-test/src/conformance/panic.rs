//! Subprocess probes distinguish the original fault from forbidden later work.
//!
//! A host may propagate the original panic before the fixture resumes. The common
//! output validator therefore checks unwind evidence, not every async postcondition.

use std::future::{pending, poll_fn};
use std::pin::Pin;
use std::process::Output;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rca_core::{Effect, ReduceContext, Reducer, Runtime, Store};

/// Proves state destruction even when the runtime unwinds out of the fixture.
const STATE_DROPPED: &str = "RCA_PANIC_STATE_DROPPED";
/// Makes any reduction after the selected fault visible to the parent process.
const LATER_REDUCED: &str = "RCA_PANIC_LATER_REDUCED";

/// Selects one programming-fault boundary for shared panic conformance.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PanicScenario {
    /// Panics while reducing an admitted action.
    Reducer,
    /// Panics while evaluating an observation selector.
    Selector,
    /// Panics while polling an accepted effect future.
    Effect,
}

impl PanicScenario {
    /// Returns the stable subprocess name used in admission and panic diagnostics.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Reducer => "reducer",
            Self::Selector => "selector",
            Self::Effect => "effect",
        }
    }
}

/// Publishes destruction both to a surviving fixture and to its parent process.
struct PanicState {
    /// Lets a host that isolates task panics verify teardown without parsing output.
    dropped: Arc<AtomicBool>,
}

impl Drop for PanicState {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::Release);
        eprintln!("{STATE_DROPPED}");
    }
}

/// Creates pending work, crosses a selected fault boundary, or detects stale work.
#[derive(Clone, Copy)]
enum PanicAction {
    /// Keeps a causal task alive so panic teardown must cancel it.
    StartPending,
    /// Panics synchronously while reducing an admitted action.
    ReducerPanic,
    /// Panics from an accepted effect before its continuation can start.
    EffectPanic,
    /// Must never reduce after the fixture crosses the fault boundary.
    Later,
}

/// Owns the forbidden-work sentinel independently of state destruction.
struct PanicFeature {
    /// Is set only if a later queued action or continuation reaches the reducer.
    later_reduced: Arc<AtomicBool>,
}

impl Reducer for PanicFeature {
    type State = PanicState;
    type Action = PanicAction;

    fn reduce(
        &mut self,
        _state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            PanicAction::StartPending => Effect::run(|_sender| pending()),
            PanicAction::ReducerPanic => {
                panic!("intentional reducer panic")
            }
            PanicAction::EffectPanic => Effect::concatenate([
                Effect::run(|_sender| async move { panic!("intentional effect panic") }),
                Effect::send(PanicAction::Later),
            ]),
            PanicAction::Later => {
                self.later_reduced.store(true, Ordering::Release);
                eprintln!("{LATER_REDUCED}");
                Effect::none()
            }
        }
    }
}

/// Exercises shared reducer, selector, or effect panic teardown semantics.
///
/// Hosts that isolate task panics allow this future to check terminal tasks,
/// observations and admission. A host that propagates a task panic may unwind
/// before those checks; its subprocess must still prove the common markers.
///
/// # Panics
///
/// Intentionally triggers the selected programming fault. A violated teardown
/// assertion also panics when the host allows the fixture to resume.
#[doc(hidden)]
pub async fn run_panic_fixture(runtime: Runtime, scenario: PanicScenario) {
    let dropped = Arc::new(AtomicBool::new(false));
    let later_reduced = Arc::new(AtomicBool::new(false));
    let store = Store::new(
        PanicState {
            dropped: Arc::clone(&dropped),
        },
        PanicFeature {
            later_reduced: Arc::clone(&later_reduced),
        },
        runtime,
    );
    let mut observation = store.observe(|_| ()).await.expect("initial observation");
    assert_eq!(observation.next().await, Some(()));
    let task = store
        .send(PanicAction::StartPending)
        .await
        .expect("pending causal task");

    let later = match scenario {
        PanicScenario::Reducer => {
            let trigger = store
                .try_send(PanicAction::ReducerPanic)
                .expect("trigger admission");
            let later = store.try_send(PanicAction::Later);
            let _ = trigger.await;
            Some(later)
        }
        PanicScenario::Selector => {
            let mut registration = Box::pin(store.observe(|_| -> () {
                panic!("intentional selector panic");
            }));
            let pending = poll_once_then_return(registration.as_mut())
                .await
                .is_pending();
            let later = store.try_send(PanicAction::Later);
            if pending {
                let _ = registration.await;
            }
            Some(later)
        }
        PanicScenario::Effect => {
            let _ = store.send(PanicAction::EffectPanic).await;
            None
        }
    };
    match later {
        Some(Ok(receipt)) => {
            receipt
                .await
                .expect_err("queued post-fault action must not commit");
        }
        Some(Err(error)) => assert!(error.is_store_stopped()),
        None => {}
    }

    store.closed().await;
    task.finish().await;
    assert!(store.is_closed());
    assert!(task.is_cancelled());
    assert!(task.is_finished());
    assert_eq!(observation.next().await, None);
    store
        .send(PanicAction::Later)
        .await
        .expect_err("failed Store rejects later action");
    assert!(dropped.load(Ordering::Acquire));
    assert!(!later_reduced.load(Ordering::Acquire));
}

/// Verifies shared panic subprocess status and teardown markers.
///
/// This does not prove that every post-fault assertion in [`run_panic_fixture`]
/// ran: propagating hosts can unwind out of that future at the original fault.
///
/// # Panics
///
/// Panics if the subprocess succeeded, omitted the original fault or state-drop
/// marker, or reported a forbidden later reduction.
#[doc(hidden)]
pub fn assert_panic_output(scenario: PanicScenario, output: &Output) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "panic child unexpectedly succeeded"
    );
    assert!(
        stderr.contains(&format!("intentional {} panic", scenario.name())),
        "child stderr omitted original panic: {stderr}"
    );
    assert!(
        stderr.contains(STATE_DROPPED),
        "child stderr omitted Store drop marker: {stderr}"
    );
    assert!(
        !stderr.contains(LATER_REDUCED),
        "child reduced action after panic: {stderr}"
    );
}

/// Attempts admission once and retains readiness so completed work is not repolled.
async fn poll_once_then_return<F: Future>(mut future: Pin<&mut F>) -> std::task::Poll<F::Output> {
    poll_fn(move |context| std::task::Poll::Ready(future.as_mut().poll(context))).await
}

/// Checks the readiness contract used by the selector-panic scenario.
#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::task::Poll;

    #[test]
    fn admission_probe_preserves_immediate_and_pending_results() {
        let polls = Cell::new(0);
        let mut ready = std::pin::pin!(std::future::poll_fn(|_| {
            polls.set(polls.get() + 1);
            assert_eq!(
                polls.get(),
                1,
                "a completed registration must not be repolled"
            );
            Poll::Ready(7)
        }));
        let result = futures_executor::block_on(super::poll_once_then_return(ready.as_mut()));
        if result.is_pending() {
            let _ = futures_executor::block_on(ready);
        }
        assert_eq!(result, Poll::Ready(7));
        assert_eq!(polls.get(), 1);

        let mut pending = std::pin::pin!(std::future::pending::<usize>());
        assert_eq!(
            futures_executor::block_on(super::poll_once_then_return(pending.as_mut())),
            Poll::Pending
        );
    }
}
