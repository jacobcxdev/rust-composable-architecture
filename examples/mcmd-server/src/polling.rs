//! Polls one server with a cancellable timer and at most one pending query.
//!
//! Tick and result actions are internal messages. Cancellation rejects obsolete
//! effect results, but does not roll back remote work. Clients must own their I/O
//! resources correctly when their futures are dropped.

use std::{fmt, pin::Pin, sync::Arc, time::Duration};

use rca_core::{DependencyKey, Effect, ReduceContext, Reducer};

/// Holds query results and the polling lifecycle without task handles.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct State {
    /// Identifies the server in query calls.
    pub server_id: String,
    /// Enables periodic queries.
    pub active: bool,
    /// Records one pending query.
    pub querying: bool,
    /// Retains the last successful player count.
    pub player_count: u32,
    /// Counts consecutive failures with saturation.
    pub failures: u32,
    /// Retains the last query failure's category and diagnostic text.
    pub last_error: Option<String>,
}

/// Reports an operational query failure.
///
/// Categories and payloads remain typed through the client and result action.
/// Display preserves the captured application's debug-shaped diagnostics in
/// `State::last_error`, including quoting and escaping message payloads.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum QueryError {
    /// The server connection could not be established.
    ConnectionFailed,
    /// The server response violated the query protocol.
    Protocol(String),
    /// The query failed for another reported reason.
    Other(String),
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConnectionFailed => f.write_str("ConnectionFailed"),
            Self::Protocol(message) => write!(f, "Protocol({message:?})"),
            Self::Other(message) => write!(f, "Other({message:?})"),
        }
    }
}

impl std::error::Error for QueryError {}

/// Completes one server query with a player count or failure.
pub type QueryFuture = Pin<Box<dyn Future<Output = Result<u32, QueryError>> + Send + 'static>>;

/// Supplies runtime-independent server queries.
pub trait QueryClient: Send + Sync {
    /// Queries the identified server and reports its player count or failure.
    ///
    /// # Errors
    /// Returns a connection, protocol or other client failure through the future.
    fn poll(&self, server_id: String) -> QueryFuture;
}

/// Requires an explicitly injected query client.
#[derive(Debug)]
pub struct QueryKey;

impl DependencyKey for QueryKey {
    type Value = Arc<dyn QueryClient>;

    /// Rejects missing application wiring.
    ///
    /// # Panics
    /// Always panics; this example has no real query implementation.
    fn live_value() -> Self::Value {
        panic!("inject QueryKey before starting polling")
    }
}

/// Carries lifecycle commands and internal timer/query results.
#[derive(Debug, Eq, PartialEq)]
pub enum Action {
    /// Enables polling and requests an immediate tick.
    Start,
    /// Disables polling and cancels its outstanding work.
    Stop,
    /// Requests a query when active and not already querying.
    Tick,
    /// Reports the current query's result.
    Polled(Result<u32, QueryError>),
}

/// Reduces a server's bounded polling lifecycle.
#[derive(Debug)]
pub struct Polling;

/// Cancels the timer and queries descended from its ticks within this feature.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct PollLifetime;

/// Preserves the captured application's five-second status cadence.
const POLL_INTERVAL: Duration = Duration::from_secs(5);

impl Reducer for Polling {
    type State = State;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut State,
        action: &Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        match action {
            Action::Start if !state.active => {
                state.active = true;
                Effect::merge([
                    Effect::send(Action::Tick),
                    Effect::interval(POLL_INTERVAL, || Action::Tick),
                ])
                .cancellable(PollLifetime)
            }
            Action::Stop if state.active => {
                state.active = false;
                state.querying = false;
                Effect::cancel(PollLifetime)
            }
            Action::Tick if state.active && !state.querying => {
                state.querying = true;

                let client = Arc::clone(context.dependency::<QueryKey>());
                let server_id = state.server_id.clone();
                Effect::run(move |sender| async move {
                    let result = client.poll(server_id).await;
                    let _ = sender.send(Action::Polled(result)).await;
                })
            }
            Action::Polled(Ok(count)) if state.active && state.querying => {
                state.querying = false;
                state.player_count = *count;
                state.failures = 0;
                state.last_error = None;
                Effect::none()
            }
            Action::Polled(Err(error)) if state.active && state.querying => {
                state.querying = false;
                state.failures = state.failures.saturating_add(1);
                state.last_error = Some(error.to_string());
                Effect::none()
            }
            _ => Effect::none(),
        }
    }
}
