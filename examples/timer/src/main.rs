//! Observes timer progress, requests cancellation and waits for actual completion.
//!
//! Run with `cargo run --manifest-path examples/timer/Cargo.toml`.
//! The executable waits for observed state, not a guessed elapsed duration.
//! The inline test checks fixed-rate ticks with virtual time.

use std::time::Duration;

use rca::{Effect, ReduceContext, Reducer, Store, TokioRuntime};

/// Separates timer control from each emitted state change.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Action {
    /// Starts an interval without replacing earlier intervals under the same key.
    Start,
    /// Advances state once for a delivered interval event.
    Tick,
    /// Requests cancellation of intervals under the timer key.
    Stop,
}

/// Groups intervals so one Stop request cancels their outstanding work.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
struct TimerId;

/// Produces fixed-rate ticks until the matching cancellation request.
struct TimerFeature {
    /// The fixed-rate spacing used by both live and virtual runtimes.
    period: Duration,
}

impl Reducer for TimerFeature {
    type State = usize;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            Action::Start => Effect::interval(self.period, || Action::Tick).cancellable(TimerId),
            Action::Tick => {
                *state += 1;
                Effect::none()
            }
            Action::Stop => Effect::cancel(TimerId),
        }
    }
}

/// Waits for three committed ticks, then joins interval cancellation.
///
/// # Errors
///
/// Returns runtime or Store failures, including observation closure before three ticks.
///
/// # Panics
///
/// Panics if the observation omits the initial zero value.
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let store = Store::new(
        0,
        TimerFeature {
            // Short enough for an interactive demonstration, not a timing assertion.
            period: Duration::from_millis(5),
        },
        TokioRuntime::current()?,
    );
    let mut ticks = store.observe(|state| *state).await?;
    assert_eq!(ticks.next().await, Some(0));

    let interval = store.send(Action::Start).await?;
    loop {
        let count = ticks
            .next()
            .await
            .ok_or("timer observation ended before three ticks")?;
        if count >= 3 {
            break;
        }
    }
    store.send(Action::Stop).await?.finish().await;
    interval.finish().await;
    store.shutdown().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use futures_executor::block_on;
    use rca::TestStore;

    use super::*;

    #[test]
    fn virtual_time_matches_fixed_rate_live_semantics() {
        let mut store = TestStore::new(
            0,
            TimerFeature {
                period: Duration::from_secs(1),
            },
        );

        block_on(async {
            let interval = store.send(Action::Start).await.expect("start timer");
            store
                .advance(Duration::from_secs(3))
                .await
                .expect("advance virtual time");

            for expected in 1..=3 {
                let _tick = store
                    .receive_with(
                        |action| *action == Action::Tick,
                        move |state| *state = expected,
                    )
                    .await
                    .expect("fixed-rate tick");
                store
                    .assert_state(move |state| assert_eq!(*state, expected))
                    .await
                    .expect("tick state");
            }

            let _stop = store.send(Action::Stop).await.expect("stop timer");
            interval.finish().await.expect("interval cleanup");
            store.finish().await.expect("clean finish");
        });
    }
}
