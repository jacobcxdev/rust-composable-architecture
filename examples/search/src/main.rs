//! Keeps the latest result from a simulated asynchronous search client.
//!
//! Run with `cargo run --manifest-path examples/search/Cargo.toml`.
//! This live smoke test checks the final result, not a controlled cancellation race.
//! Scheduler delays can let the first request finish before the second starts.

use std::time::Duration;

use rca::{DependencyKey, Effect, ReduceContext, Reducer, Store, TokioRuntime};

/// Supplies a simulated response without shared mutable client state.
#[derive(Clone)]
struct SearchClient;

impl SearchClient {
    /// Splits the query after illustrative latency, without network I/O.
    async fn results(&self, query: String) -> Vec<String> {
        tokio::time::sleep(Duration::from_millis(5)).await;
        query.split_whitespace().map(str::to_owned).collect()
    }
}

/// Selects the Store-owned client used when a request starts.
#[derive(DependencyKey)]
#[dependency(value = SearchClient, live = SearchClient)]
struct SearchClientKey;

/// Makes each newly started search retire earlier in-flight search work.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
struct SearchRequestId;

/// Retains the latest query and its committed owned results.
#[derive(Default)]
struct SearchState {
    /// The most recently admitted query, independent of task-owned input.
    query: String,
    /// The latest accepted response, copied from its borrowed action.
    results: Vec<String>,
}

/// Starts searches and delivers their asynchronous owned responses.
enum Action {
    /// Replaces in-flight work under the search cancellation key.
    Search(
        /// The input copied into state and the new task's owned capture.
        String,
    ),
    /// Commits results without changing the current query.
    Response(
        /// The result strings borrowed by the reducer and copied into state.
        Vec<String>,
    ),
}

/// Replaces keyed work and copies borrowed responses into owned state.
struct SearchFeature;

impl Reducer for SearchFeature {
    type State = SearchState;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            Action::Search(query) => {
                state.query.clone_from(query);
                let client = context.dependency::<SearchClientKey>().clone();
                let query = query.clone();
                Effect::run(move |sender| async move {
                    let results = client.results(query).await;
                    if let Err(error) = sender.send(Action::Response(results)).await {
                        assert!(
                            error.is_cancelled() || error.is_store_stopped(),
                            "unexpected effect delivery error: {error}"
                        );
                    }
                })
                .cancellable_in_flight(SearchRequestId)
            }
            Action::Response(results) => {
                state.results.clone_from(results);
                Effect::none()
            }
        }
    }
}

/// Exercises live task completion and observable final state.
///
/// # Errors
///
/// Returns runtime or Store failures from admission, observation, reads or shutdown.
///
/// # Panics
///
/// Panics if the observed or committed results differ from the final query.
async fn run_scenario() -> Result<(), Box<dyn std::error::Error>> {
    let store = Store::new(
        SearchState::default(),
        SearchFeature,
        TokioRuntime::current()?,
    );
    let mut result_counts = store.observe(|state| state.results.len()).await?;
    assert_eq!(result_counts.next().await, Some(0));

    let first = store.send(Action::Search("stale".to_owned())).await?;
    let second = store
        .send(Action::Search("rust architecture".to_owned()))
        .await?;
    first.finish().await;
    second.finish().await;

    assert!(first.is_finished());
    assert_eq!(result_counts.next().await, Some(2));
    assert_eq!(
        store.read(|state| state.results.clone()).await?,
        ["rust", "architecture"]
    );
    store.shutdown().await?;
    Ok(())
}

/// Runs the final-result smoke trace on Tokio's current-thread runtime.
///
/// # Errors
///
/// Returns the runtime or Store failures reported by `run_scenario`.
///
/// # Panics
///
/// Panics if the scenario's state or observation assertions fail.
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    run_scenario().await
}

#[cfg(test)]
mod tests {
    #[tokio::test(flavor = "current_thread")]
    async fn latest_search_result_is_observable() {
        super::run_scenario().await.expect("search scenario");
    }
}
