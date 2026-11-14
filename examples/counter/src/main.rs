//! Sends one action, reads the committed counter and shuts down the Store.
//!
//! Run with `cargo run --manifest-path examples/counter/Cargo.toml`.
//! The reducer borrows its action and returns no asynchronous work.

use rca::{Effect, ReduceContext, Reducer, Store, TokioRuntime};

/// Increments state synchronously without scheduling an effect.
struct Counter;

/// Requests a synchronous change without an asynchronous reply action.
enum Action {
    /// Advances the counter by one in this finite demonstration.
    Increment,
}

impl Reducer for Counter {
    type State = i64;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            Action::Increment => *state += 1,
        }
        Effect::none()
    }
}

/// Checks one committed increment and waits for Store shutdown.
///
/// # Errors
///
/// Returns runtime or Store access failures.
///
/// # Panics
///
/// Panics if the committed counter is not one.
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let store = Store::new(0, Counter, TokioRuntime::current()?);
    let task = store.send(Action::Increment).await?;
    task.finish().await;
    assert_eq!(store.read(|state| *state).await?, 1);
    store.shutdown().await?;
    Ok(())
}
