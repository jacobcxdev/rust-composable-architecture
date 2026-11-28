//! Owns proxy acquisition and event delivery within one cancellable effect.
//!
//! Clients must release partial resources when acquisition is dropped and live
//! resources when the returned stream is dropped. This semantic obligation is
//! not enforced by the type system. Remote asynchronous cleanup requires another
//! owner; this module does not supply one. Config validation and server integration
//! remain external. Ready, Event, Failed and Ended are internal effect messages.

use std::{fmt, pin::Pin, sync::Arc};

use futures_core::Stream;
use futures_util::StreamExt;
use rca_core::{DependencyKey, Effect, ReduceContext, Reducer};

/// Identifies the server and local proxy endpoints.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    /// Identifies the server in client calls.
    pub server_id: String,
    /// Specifies the captured proxy host configuration.
    pub host: String,
    /// Lists Java proxy ports.
    pub java_ports: Vec<u16>,
    /// Lists Bedrock proxy ports.
    pub bedrock_ports: Vec<u16>,
}

/// Describes the local proxy session lifecycle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Phase {
    /// No session is requested.
    Stopped,
    /// Acquisition is pending.
    Starting,
    /// The session is delivering events.
    Listening,
    /// Acquisition failed; explicit Start may retry.
    Failed,
}

/// Holds configuration and observable lifecycle state, not resource handles.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct State {
    /// Supplies stable session configuration.
    pub config: Config,
    /// Records acquisition or event delivery state.
    pub phase: Phase,
    /// Retains the last acquisition error.
    pub last_error: Option<String>,
}

/// Reports a proxy event for parent policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProxyEvent {
    /// Indicates a connection that may require starting the backend.
    JoinObserved,
}

/// Reports failure to acquire a local proxy session.
///
/// Categories and payloads remain typed through the acquisition result action.
/// Display preserves debug-shaped diagnostics, including quoted, escaped payloads.
/// Fallible asynchronous shutdown is outside this local session contract.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProxyError {
    /// The local proxy could not start.
    StartFailed(String),
    /// Acquisition failed for another reported reason.
    Other(String),
}

impl fmt::Display for ProxyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StartFailed(message) => write!(f, "StartFailed({message:?})"),
            Self::Other(message) => write!(f, "Other({message:?})"),
        }
    }
}

impl std::error::Error for ProxyError {}

/// Owns a live session whose local resources are released on Drop.
pub type ProxySession = Pin<Box<dyn Stream<Item = ProxyEvent> + Send + 'static>>;

/// Owns pending acquisition, transferring its resources into the successful session.
pub type AcquireFuture =
    Pin<Box<dyn Future<Output = Result<ProxySession, ProxyError>> + Send + 'static>>;

/// Acquires cancellation-safe local sessions without detached resource ownership.
pub trait ProxyClient: Send + Sync {
    /// Acquires a session, releasing partial resources if its future is dropped.
    ///
    /// The returned stream must own the live resources and release them on Drop.
    /// Detached work or fallible asynchronous shutdown needs an independent owner;
    /// returning a stream alone does not satisfy this contract for remote resources.
    ///
    /// # Errors
    /// Returns an operational acquisition failure through the future.
    fn acquire(&self, config: Config) -> AcquireFuture;
}

/// Requires an explicitly injected proxy client.
#[derive(Debug)]
pub struct ProxyKey;

impl DependencyKey for ProxyKey {
    type Value = Arc<dyn ProxyClient>;

    /// Rejects missing application wiring.
    ///
    /// # Panics
    /// Always panics; this example supplies no real network client.
    fn live_value() -> Self::Value {
        panic!("inject ProxyKey before starting proxy work")
    }
}

/// Carries lifecycle commands and internal session messages.
#[derive(Debug, Eq, PartialEq)]
pub enum Action {
    /// Requests acquisition unless already starting or listening.
    Start,
    /// Cancels local session work and clears the error.
    Stop,
    /// Reports successful acquisition before event delivery.
    Ready,
    /// Forwards one event to parent policy.
    Event(ProxyEvent),
    /// Reports acquisition failure.
    Failed(ProxyError),
    /// Reports EOF after the session has been dropped.
    Ended,
}

/// Reduces local proxy session state with runtime-owned cancellation.
#[derive(Debug)]
pub struct Proxy;

/// Cancels acquisition and event delivery for one local proxy lifetime.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct ProxyLifetime;

impl Reducer for Proxy {
    type State = State;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut State,
        action: &Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        match action {
            Action::Start if matches!(state.phase, Phase::Stopped | Phase::Failed) => {
                state.phase = Phase::Starting;
                state.last_error = None;

                let client = Arc::clone(context.dependency::<ProxyKey>());
                let config = state.config.clone();
                Effect::run(move |sender| async move {
                    let mut session = match client.acquire(config).await {
                        Ok(session) => session,
                        Err(error) => {
                            let _ = sender.send(Action::Failed(error)).await;
                            return;
                        }
                    };
                    if sender.send(Action::Ready).await.is_err() {
                        return;
                    }

                    while let Some(event) = session.next().await {
                        if sender.send(Action::Event(event)).await.is_err() {
                            return;
                        }
                    }
                    drop(session);
                    let _ = sender.send(Action::Ended).await;
                })
                .cancellable(ProxyLifetime)
            }
            Action::Stop if state.phase != Phase::Stopped => {
                state.phase = Phase::Stopped;
                state.last_error = None;
                Effect::cancel(ProxyLifetime)
            }
            Action::Ready if state.phase == Phase::Starting => {
                state.phase = Phase::Listening;
                Effect::none()
            }
            Action::Failed(error) if state.phase == Phase::Starting => {
                state.phase = Phase::Failed;
                state.last_error = Some(error.to_string());
                Effect::none()
            }
            Action::Ended if state.phase == Phase::Listening => {
                state.phase = Phase::Stopped;
                Effect::none()
            }
            _ => Effect::none(),
        }
    }
}
