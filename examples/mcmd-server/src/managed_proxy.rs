//! Separates external proxy operation ownership from cancellable reply effects.
//!
//! Applications must drive and join the owner, retain pending recovery, and use
//! only one owner for each external server ID. Host abortion, panic and process
//! exit do not guarantee cleanup. Event-stream Drop does not confirm release.
//! Cancelling a reply does not undo committed state. A new Stop replaces the old
//! reply wait and requests fresh cleanup confirmation from the same owner.

use std::{fmt, pin::Pin, sync::Arc};

use async_channel::{Receiver, Sender, TrySendError};
use futures_core::Stream;
use futures_util::StreamExt;
use rca_core::{Effect, ReduceContext, Reducer};

use crate::proxy::{Config, ProxyEvent};

/// Reports external operation or bounded admission failure.
///
/// Display preserves the captured diagnostic spelling and escaped message payloads.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Error {
    /// Start failed, possibly after partial external acquisition.
    StartFailed(String),
    /// Cleanup failed; external resources may remain.
    StopFailed(String),
    /// The client reported another failure.
    Other(String),
    /// A previous session still requires confirmed cleanup.
    CleanupPending,
    /// The bounded command queue has no room.
    Full,
    /// The owner or reply channel is unavailable.
    Closed,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl std::error::Error for Error {}

/// Delivers events without owning external cleanup authority.
pub type Events = Pin<Box<dyn Stream<Item = ProxyEvent> + Send + 'static>>;

/// Completes external acquisition and transfers event delivery.
pub type StartFuture = Pin<Box<dyn Future<Output = Result<Events, Error>> + Send + 'static>>;

/// Reports completed external cleanup, not admission.
pub type StopFuture = Pin<Box<dyn Future<Output = Result<(), Error>> + Send + 'static>>;

/// Supplies operations driven independently of RCA effect cancellation.
pub trait Client: Send + Sync {
    /// Starts the fixed configuration; failure may leave partial resources.
    ///
    /// # Errors
    /// Returns the client's acquisition failure through the future.
    fn start(&self, config: Config) -> StartFuture;

    /// Idempotently stops the server, returning success only after release.
    ///
    /// # Errors
    /// Returns the client's cleanup failure through the future. The owner retains recovery.
    fn stop(&self, server_id: String) -> StopFuture;
}

/// Transfers event delivery without transferring the owner's cleanup obligation.
struct Started {
    /// Carries events from the successfully acquired session.
    events: Events,
    /// Confirms that the caller took the stream instead of abandoning a buffered reply.
    accepted: Sender<()>,
}

/// Serialises external operations and returns results to separate reply waits.
enum Request {
    /// Acquires a session and waits for its caller to accept event delivery.
    Start(Sender<Result<Started, Error>>),
    /// Confirms cleanup even if the caller abandons its reply wait.
    Stop(Sender<Result<(), Error>>),
}

/// Admits bounded requests without owning external operation futures.
#[derive(Clone)]
pub struct Control {
    /// Keeps the owner available and admits at most one queued command.
    commands: Sender<Request>,
}

impl fmt::Debug for Control {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Control").finish_non_exhaustive()
    }
}

impl Control {
    /// Prefers a ready reply, but stops waiting if the command owner disappears.
    async fn receive<T>(&self, reply: Receiver<Result<T, Error>>) -> Result<T, Error> {
        let receive = reply.recv();
        let closed = self.commands.closed();
        futures_util::pin_mut!(receive, closed);
        match futures_util::future::select(receive, closed).await {
            futures_util::future::Either::Left((result, _)) => {
                result.map_err(|_closed| Error::Closed)?
            }
            futures_util::future::Either::Right(_) => Err(Error::Closed),
        }
    }

    /// Rejects a full or closed queue without starting external work.
    fn admit(&self, request: Request) -> Result<(), Error> {
        self.commands
            .try_send(request)
            .map_err(|error| match error {
                TrySendError::Full(_) => Error::Full,
                TrySendError::Closed(_) => Error::Closed,
            })
    }

    /// Requests acquisition; dropping this wait does not cancel admitted work.
    ///
    /// # Errors
    /// Reports admission, existing cleanup obligation or client failure.
    pub async fn start(&self) -> Result<Events, Error> {
        let (reply, receive) = async_channel::bounded(1);
        self.admit(Request::Start(reply))?;
        let started = self.receive(receive).await?;
        // No await separates acknowledgement from returning the event stream.
        // A concurrent Stop may already have retired this hand-off.
        let _ = started.accepted.try_send(());
        Ok(started.events)
    }

    /// Requests confirmed cleanup independently of this wait's lifetime.
    ///
    /// # Errors
    /// Reports admission or retained external cleanup failure.
    pub async fn stop(&self) -> Result<(), Error> {
        let (reply, receive) = async_channel::bounded(1);
        self.admit(Request::Stop(reply))?;
        self.receive(receive).await
    }
}

/// Retains external cleanup authority after the command host finishes.
#[must_use = "pending cleanup must be retained and explicitly reconciled"]
pub struct Recovery {
    /// Retains the client needed to release resources after the host finishes.
    client: Arc<dyn Client>,
    /// Keeps the identity and original configuration of this single external owner.
    config: Config,
    /// Remains set until an idempotent stop confirms release.
    pending: bool,
    /// Preserves the last failure without scheduling an automatic retry.
    last_error: Option<Error>,
}

impl fmt::Debug for Recovery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Recovery")
            .field("pending", &self.pending)
            .field("last_error", &self.last_error)
            .finish_non_exhaustive()
    }
}

impl Recovery {
    /// Reports whether confirmed external release is still required.
    #[must_use]
    pub const fn is_pending(&self) -> bool {
        self.pending
    }

    /// Returns the exact retained client failure.
    #[must_use]
    pub const fn last_error(&self) -> Option<&Error> {
        self.last_error.as_ref()
    }

    /// Retries cleanup while retaining authority if this future is cancelled.
    ///
    /// # Errors
    /// Retains and returns the client's cleanup failure.
    pub async fn retry(&mut self) -> Result<(), Error> {
        if !self.pending {
            return Ok(());
        }

        let result = self.client.stop(self.config.server_id.clone()).await;
        self.last_error = result.as_ref().err().cloned();
        if result.is_ok() {
            self.pending = false;
        }
        result
    }
}

/// Serialises one server's external operations independently of RCA.
pub struct Owner {
    /// Receives commands in admission order until every control closes.
    commands: Receiver<Request>,
    /// Keeps partial or live resources recoverable across abandoned replies.
    recovery: Recovery,
}

impl fmt::Debug for Owner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Owner")
            .field("recovery", &self.recovery)
            .finish_non_exhaustive()
    }
}

/// Creates one bounded owner for a fixed external server configuration.
///
/// The application must independently drive `Owner::run` and join its recovery.
/// It must not create competing owners for the same external server ID.
pub fn channel(config: Config, client: Arc<dyn Client>) -> (Control, Owner) {
    // One queued operation bounds admission while another external call runs.
    let (commands, receive) = async_channel::bounded(1);
    (
        Control { commands },
        Owner {
            commands: receive,
            recovery: Recovery {
                client,
                config,
                pending: false,
                last_error: None,
            },
        },
    )
}

impl Owner {
    /// Drives operations until controls close, then returns cleanup authority.
    pub async fn run(mut self) -> Recovery {
        let mut handoff: Option<Receiver<()>> = None;
        loop {
            let next = if let Some(accepted) = &handoff {
                let acknowledgement = accepted.recv();
                let command = self.commands.recv();
                futures_util::pin_mut!(acknowledgement, command);
                match futures_util::future::select(acknowledgement, command).await {
                    futures_util::future::Either::Left((result, _)) => {
                        futures_util::future::Either::Left(result)
                    }
                    futures_util::future::Either::Right((request, _)) => {
                        futures_util::future::Either::Right(request)
                    }
                }
            } else {
                futures_util::future::Either::Right(self.commands.recv().await)
            };

            let request = match next {
                futures_util::future::Either::Left(result) => {
                    handoff = None;
                    if result.is_err() {
                        let _ = self.recovery.retry().await;
                    }
                    continue;
                }
                futures_util::future::Either::Right(Ok(request)) => request,
                futures_util::future::Either::Right(Err(_closed)) => break,
            };

            match request {
                Request::Start(reply) => {
                    if self.recovery.pending {
                        let _ = reply.send(Err(Error::CleanupPending)).await;
                        continue;
                    }

                    self.recovery.pending = true;
                    let result = self
                        .recovery
                        .client
                        .start(self.recovery.config.clone())
                        .await;

                    match result {
                        Ok(events) => {
                            let (accepted, receive) = async_channel::bounded(1);
                            let _ = reply.send(Ok(Started { events, accepted })).await;
                            // Do not retain a buffered reply after its receiver is dropped.
                            drop(reply);
                            handoff = Some(receive);
                        }
                        Err(error) => {
                            self.recovery.last_error = Some(error.clone());
                            let _ = reply.send(Err(error)).await;
                        }
                    }
                }
                Request::Stop(reply) => {
                    // Explicit cleanup supersedes any unconsumed start reply.
                    handoff = None;
                    let result = self.recovery.retry().await;
                    let _ = reply.send(result).await;
                }
            }
        }

        if self.recovery.pending && self.recovery.last_error.is_none() {
            let _ = self.recovery.retry().await;
        }
        self.recovery
    }
}

/// Describes requested work separately from confirmed external release.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Phase {
    /// External cleanup has completed, or no start has been requested.
    #[default]
    Stopped,
    /// External acquisition is pending.
    Starting,
    /// Events are being delivered.
    Listening,
    /// External release is pending.
    Stopping,
    /// Work failed; explicit Stop reconciles possible external resources.
    Failed,
}

/// Holds observable state; external ownership remains with the host.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct State {
    /// Records requested and confirmed lifecycle transitions.
    pub phase: Phase,
    /// Preserves the latest debug-shaped diagnostic.
    pub last_error: Option<String>,
}

/// Carries commands and trusted internal effect completions.
#[derive(Debug, Eq, PartialEq)]
pub enum Action {
    /// Requests acquisition from a stopped state.
    Start,
    /// Requests completed external cleanup, including explicit retry.
    ///
    /// Replaces an earlier reply wait without cancelling its admitted external work.
    Stop,
    /// Reports acquisition before forwarding events.
    Ready,
    /// Forwards an event to parent policy.
    Event(ProxyEvent),
    /// Retains an exact operation failure.
    Failed(Error),
    /// Requests cleanup after event EOF.
    Ended,
    /// Confirms completed external cleanup.
    Stopped,
}

/// Connects cancellable RCA reply effects to an independently driven owner.
#[derive(Debug)]
pub struct ManagedProxy {
    /// Routes reply effects to the independently driven external owner.
    control: Control,
}

impl ManagedProxy {
    /// Binds this reducer to one fixed external server owner.
    #[must_use]
    pub fn new(control: Control) -> Self {
        Self { control }
    }

    /// Cancels event delivery before awaiting independently owned external cleanup.
    fn stop(&self, state: &mut State) -> Effect<Action> {
        state.phase = Phase::Stopping;
        state.last_error = None;

        let control = self.control.clone();
        Effect::concatenate([
            Effect::cancel(EventLifetime),
            Effect::run(move |sender| async move {
                let action = match control.stop().await {
                    Ok(()) => Action::Stopped,
                    Err(error) => Action::Failed(error),
                };
                let _ = sender.send(action).await;
            }),
        ])
        .cancellable_in_flight(StopReplyLifetime)
        .without_inherited_cancellation()
    }
}

/// Cancels acquisition replies and event delivery, but never external cleanup.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct EventLifetime;

/// Replaces obsolete cleanup replies without cancelling independently owned operations.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct StopReplyLifetime;

impl Reducer for ManagedProxy {
    type State = State;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut State,
        action: &Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        match action {
            Action::Start if state.phase == Phase::Stopped => {
                state.phase = Phase::Starting;
                state.last_error = None;

                let control = self.control.clone();
                Effect::run(move |sender| async move {
                    let mut events = match control.start().await {
                        Ok(events) => events,
                        Err(error) => {
                            let _ = sender.send(Action::Failed(error)).await;
                            return;
                        }
                    };
                    if sender.send(Action::Ready).await.is_err() {
                        return;
                    }

                    while let Some(event) = events.next().await {
                        if sender.send(Action::Event(event)).await.is_err() {
                            return;
                        }
                    }
                    drop(events);
                    let _ = sender.send(Action::Ended).await;
                })
                .cancellable(EventLifetime)
            }
            Action::Stop
                if matches!(
                    state.phase,
                    Phase::Starting | Phase::Listening | Phase::Stopping | Phase::Failed
                ) =>
            {
                self.stop(state)
            }
            Action::Ended if state.phase == Phase::Listening => self.stop(state),
            Action::Ready if state.phase == Phase::Starting => {
                state.phase = Phase::Listening;
                Effect::none()
            }
            Action::Failed(error) if matches!(state.phase, Phase::Starting | Phase::Stopping) => {
                state.phase = Phase::Failed;
                state.last_error = Some(error.to_string());
                Effect::none()
            }
            Action::Stopped if state.phase == Phase::Stopping => {
                state.phase = Phase::Stopped;
                Effect::none()
            }
            _ => Effect::none(),
        }
    }
}
