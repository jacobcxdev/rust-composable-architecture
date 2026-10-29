//! Serialises backend operations while retaining the latest requested target.
//!
//! `Started` and `Stopped` delegate actions let a parent coordinate polling.
//! Client futures must report completed external operations, not mere admission.
//! Dropping a future does not promise rollback; host shutdown and child removal
//! require an external reconciliation policy before this can become a daemon.

use std::{fmt, pin::Pin, sync::Arc};

use rca_core::{DependencyKey, Effect, ReduceContext, Reducer};

/// Describes the requested backend state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Target {
    /// Requests no running backend.
    Stopped,
    /// Requests a running backend.
    Running,
}

/// Describes the last confirmed state or the operation awaiting completion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Phase {
    /// No backend is running.
    Stopped,
    /// A start operation is pending.
    Starting,
    /// The start operation succeeded.
    Running,
    /// A stop operation is pending.
    Stopping,
    /// An operation failed; automatic reconciliation is suspended.
    Error,
}

/// Holds backend domain state without runtime task handles.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct State {
    /// Identifies the backend in client calls.
    pub server_id: String,
    /// Retains the latest requested state.
    pub target: Target,
    /// Records the current lifecycle phase.
    pub phase: Phase,
    /// Records the last operational failure.
    pub last_error: Option<String>,
}

/// Reports an operational backend failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackendError(
    /// Preserves the diagnostic supplied by the backend client.
    pub String,
);

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for BackendError {}

/// Completes one external backend operation.
pub type BackendFuture = Pin<Box<dyn Future<Output = Result<(), BackendError>> + Send + 'static>>;

/// Supplies external backend operations without tying reducers to a runtime.
pub trait BackendClient: Send + Sync {
    /// Starts the identified backend and reports completion or failure.
    ///
    /// # Errors
    /// Returns a client failure through the future, not an admission receipt.
    fn start(&self, server_id: String) -> BackendFuture;

    /// Stops the identified backend and reports completion or failure.
    ///
    /// # Errors
    /// Returns a client failure through the future. Failure does not confirm release.
    fn stop(&self, server_id: String) -> BackendFuture;
}

/// Requires an explicitly injected backend client for each Store environment.
#[derive(Debug)]
pub struct BackendKey;

impl DependencyKey for BackendKey {
    type Value = Arc<dyn BackendClient>;

    /// Rejects missing application wiring.
    ///
    /// # Panics
    /// Always panics; this example has no real backend implementation.
    fn live_value() -> Self::Value {
        panic!("inject BackendKey before starting backend work")
    }
}

/// Carries requests, internal completions and parent-facing lifecycle delegates.
///
/// Completion actions belong to effects, not untrusted external callers. Phase
/// guards do not authenticate forged completions from an earlier operation.
#[derive(Debug, Eq, PartialEq)]
pub enum Action {
    /// Requests a running backend.
    RequestStart,
    /// Requests a stopped backend.
    RequestStop,
    /// Reconciles the confirmed phase with the latest target.
    Reconcile,
    /// Reports the pending start operation's result.
    StartFinished(Result<(), BackendError>),
    /// Reports the pending stop operation's result.
    StopFinished(Result<(), BackendError>),
    /// Tells a parent that a wanted backend has started.
    Started,
    /// Tells a parent that a backend has stopped.
    Stopped,
}

/// Reduces backend lifecycle requests without overlapping external operations.
#[derive(Debug)]
pub struct Backend;

impl Reducer for Backend {
    type State = State;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut State,
        action: &Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        match action {
            Action::RequestStart => {
                state.target = Target::Running;
                Effect::send(Action::Reconcile)
            }
            Action::RequestStop => {
                state.target = Target::Stopped;
                Effect::send(Action::Reconcile)
            }
            Action::Reconcile => match (state.phase, state.target) {
                (Phase::Stopped, Target::Running) => {
                    state.phase = Phase::Starting;
                    state.last_error = None;

                    let client = Arc::clone(context.dependency::<BackendKey>());
                    let server_id = state.server_id.clone();
                    Effect::run(move |sender| async move {
                        let result = client.start(server_id).await;
                        let _ = sender.send(Action::StartFinished(result)).await;
                    })
                }
                (Phase::Running, Target::Stopped) => {
                    state.phase = Phase::Stopping;
                    state.last_error = None;

                    let client = Arc::clone(context.dependency::<BackendKey>());
                    let server_id = state.server_id.clone();
                    Effect::run(move |sender| async move {
                        let result = client.stop(server_id).await;
                        let _ = sender.send(Action::StopFinished(result)).await;
                    })
                }
                _ => Effect::none(),
            },
            Action::StartFinished(result) if state.phase == Phase::Starting => match result {
                Ok(()) => {
                    state.phase = Phase::Running;
                    state.last_error = None;
                    Effect::send(if state.target == Target::Running {
                        Action::Started
                    } else {
                        Action::Reconcile
                    })
                }
                Err(error) => {
                    state.phase = Phase::Error;
                    state.last_error = Some(error.to_string());
                    Effect::none()
                }
            },
            Action::StopFinished(result) if state.phase == Phase::Stopping => match result {
                Ok(()) => {
                    state.phase = Phase::Stopped;
                    state.last_error = None;
                    let stopped = Effect::send(Action::Stopped);
                    if state.target == Target::Running {
                        Effect::concatenate([stopped, Effect::send(Action::Reconcile)])
                    } else {
                        stopped
                    }
                }
                Err(error) => {
                    state.phase = Phase::Error;
                    state.last_error = Some(error.to_string());
                    Effect::none()
                }
            },
            _ => Effect::none(),
        }
    }
}
