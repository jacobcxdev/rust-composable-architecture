//! Checks timer ordering, cancellation and causal task completion across runtimes.

use std::time::Duration;

use async_channel::Receiver;
use rca_core::{Effect, MonotonicInstant, ReduceContext, Reducer, Store};

use super::RuntimeFactory;

/// Short positive delay for explicit clock advances, not a host-latency limit.
const TIMER_DELAY: Duration = Duration::from_millis(5);
/// Fixed-rate tick spacing for the coalescing-tolerant progress check.
const INTERVAL_PERIOD: Duration = Duration::from_millis(5);

/// Selects one timer scenario or records its resulting action.
enum Action {
    /// Starts three timers in one transaction, giving them equal deadlines.
    Equal,
    /// Starts a repeating timer with a named cancellation identity.
    Interval,
    /// Parks a one-shot timer until the separate cancellation action arrives.
    Removable,
    /// Starts delayed work that waits for the held channel to close.
    Complete(Receiver<()>),
    /// Cancels the repeating timer and its causal work.
    CancelInterval,
    /// Cancels the one-shot timer and its causal work.
    CancelRemovable,
    /// Records which one-shot timer reached the reducer.
    Record(u8),
    /// Records one repeating-timer delivery.
    Tick,
}

/// Converts scenario actions into timers and records their output order.
struct Timers;

impl Reducer for Timers {
    type State = Vec<u8>;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            Action::Equal => Effect::merge([
                Effect::send_after(TIMER_DELAY, Action::Record(1)),
                Effect::send_after(TIMER_DELAY, Action::Record(2)),
                Effect::send_after(TIMER_DELAY, Action::Record(3)),
            ]),
            Action::Interval => {
                Effect::interval(INTERVAL_PERIOD, || Action::Tick).cancellable("interval")
            }
            Action::Removable => {
                Effect::send_at(MonotonicInstant::MAX, Action::Record(9)).cancellable("removable")
            }
            Action::Complete(release) => {
                let release = release.clone();
                Effect::run(move |sender| async move {
                    release
                        .recv()
                        .await
                        .expect_err("completion release closes without a value");
                    sender
                        .send(Action::Record(7))
                        .await
                        .expect("delayed completion output");
                })
                .delay(TIMER_DELAY)
            }
            Action::CancelInterval => Effect::cancel("interval"),
            Action::CancelRemovable => Effect::cancel("removable"),
            Action::Record(value) => {
                state.push(*value);
                Effect::none()
            }
            Action::Tick => {
                state.push(1);
                Effect::none()
            }
        }
    }
}

/// Checks source order for timers created at the same transaction time.
pub(super) async fn equal_deadline_order(factory: impl RuntimeFactory) {
    let store = Store::new(Vec::new(), Timers, factory.runtime());
    let timers = store.send(Action::Equal).await.expect("equal timers");
    factory
        .advance(TIMER_DELAY)
        .await
        .expect("advance equal timers");
    timers.finish().await;

    assert_eq!(
        store.read(Clone::clone).await.expect("timer state"),
        [1, 2, 3]
    );
    store.shutdown().await.expect("clean shutdown");
}

/// Checks repeated delivery without requiring each observation or a prompt host.
pub(super) async fn interval_progress(factory: impl RuntimeFactory) {
    let store = Store::new(Vec::new(), Timers, factory.runtime());
    let mut tick_counts = store.observe(Vec::len).await.expect("tick observation");
    assert_eq!(tick_counts.next().await, Some(0));
    let interval = store.send(Action::Interval).await.expect("interval start");
    factory.yield_probe().await;
    factory
        .advance(Duration::from_millis(17))
        .await
        .expect("advance interval");
    loop {
        let ticks = tick_counts.next().await.expect("live tick observation");
        if ticks >= 3 {
            break;
        }
    }
    store
        .send(Action::CancelInterval)
        .await
        .expect("cancel interval")
        .finish()
        .await;
    interval.finish().await;

    drop(tick_counts);
    store.shutdown().await.expect("clean shutdown");
}

/// Checks that cancellation removes a pending timer and finishes its action task.
pub(super) async fn timer_removal(factory: impl RuntimeFactory) {
    let store = Store::new(Vec::new(), Timers, factory.runtime());
    let removable = store
        .send(Action::Removable)
        .await
        .expect("removable timer");
    factory.yield_probe().await;
    assert_eq!(
        store
            .runtime_snapshot()
            .await
            .expect("registered timer snapshot")
            .timer_deadlines,
        [MonotonicInstant::MAX]
    );
    store
        .send(Action::CancelRemovable)
        .await
        .expect("cancel timer")
        .finish()
        .await;
    removable.finish().await;
    assert!(
        store
            .runtime_snapshot()
            .await
            .expect("cancelled timer snapshot")
            .timer_deadlines
            .is_empty()
    );
    factory
        .advance(TIMER_DELAY)
        .await
        .expect("advance removed timer");

    assert!(store.read(Vec::is_empty).await.expect("timer state"));
    store.shutdown().await.expect("clean shutdown");
}

/// Checks that an action task waits for its delayed output to commit.
pub(super) async fn action_task_completion(factory: impl RuntimeFactory) {
    let (release, waiting) = async_channel::bounded(1);
    let store = Store::new(Vec::new(), Timers, factory.runtime());
    let delayed = store
        .send(Action::Complete(waiting))
        .await
        .expect("completion timer");
    factory.yield_probe().await;
    assert!(!delayed.is_finished());

    drop(release);
    factory
        .advance(TIMER_DELAY)
        .await
        .expect("advance completion timer");
    delayed.finish().await;

    assert!(delayed.is_finished());
    assert_eq!(store.read(Clone::clone).await.expect("timer state"), [7]);
    store.shutdown().await.expect("clean shutdown");
}

/// Rejects a zero-period interval before any runtime work starts.
pub(super) fn zero_period_panics() {
    let panic = std::panic::catch_unwind(|| {
        let _effect = Effect::<Action>::interval(Duration::ZERO, || Action::Tick);
    });
    assert!(panic.is_err(), "zero-period interval must panic");
}

#[cfg(test)]
mod tests {
    use std::pin::pin;

    use super::{action_task_completion, timer_removal};
    use crate::conformance::{DeterministicFactory, poll_once};

    /// Resumes a caller only after its already-registered timer deadline passes.
    fn resume_after_timer_expiry<F: Future<Output = ()> + Send>(
        scenario: impl FnOnce(DeterministicFactory) -> F,
    ) {
        let factory = DeterministicFactory::new();
        let scheduler = factory.scheduler();
        let mut scenario = pin!(scenario(factory));

        assert!(poll_once(scenario.as_mut()).is_pending());
        scheduler.run_until_stalled().expect("start action commits");
        assert!(poll_once(scenario.as_mut()).is_pending());

        // The caller is suspended at the real driver's yield. Let other work and
        // time progress before polling that caller again, as a busy host can do.
        scheduler
            .advance(std::time::Duration::from_millis(10))
            .expect("timer expires while the caller is suspended");
        futures_executor::block_on(scheduler.run(scenario))
            .expect("the delayed caller completes the scenario");
    }

    #[test]
    fn timer_removal_tolerates_a_delayed_caller() {
        resume_after_timer_expiry(timer_removal);
    }

    #[test]
    fn action_task_completion_tolerates_a_delayed_caller() {
        resume_after_timer_expiry(action_task_completion);
    }
}
