//! Sends owned narrow commands through an installed child's bounded actor.

pub(in crate::store) mod input;
#[cfg(feature = "local")]
pub(in crate::store) mod local;

use std::fmt;
use std::pin::Pin;
use std::sync::{Arc, Weak};

use futures_channel::oneshot;

use super::task_context::{TaskAccessError, TaskSendError, TypedSend};
use crate::{ActionTask, RuntimeFuture};

/// Holds send-only access to one installed child and its configuration.
///
/// Cloning a port does not retain its Store, State, Feature or command mapper.
/// Acquire a new port after replacement or configuration changes; an old port
/// never retargets the replacement. Siblings share their root mailbox.
pub struct CommandPort<C> {
    /// Erased admission route containing weak owner and mapper references.
    endpoint: Arc<dyn CommandEndpoint<C>>,
}

impl<C> Clone for CommandPort<C> {
    fn clone(&self) -> Self {
        Self {
            endpoint: Arc::clone(&self.endpoint),
        }
    }
}

impl<C> fmt::Debug for CommandPort<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CommandPort").finish_non_exhaustive()
    }
}

impl<C> CommandPort<C> {
    /// Wraps an already acquired route without strengthening its Store ownership.
    pub(super) fn from_endpoint(endpoint: Arc<dyn CommandEndpoint<C>>) -> Self {
        Self { endpoint }
    }
}

impl<C: Send + 'static> CommandPort<C> {
    /// Waits for bounded admission and then the actor's terminal commit result.
    ///
    /// Construction is lazy. Until first poll the returned owned future does
    /// not enqueue or map `C`. While capacity is unavailable it retains one `C`.
    ///
    /// # Errors
    ///
    /// Returns the original command when rejected before user entry. Once a
    /// selector or mapper has entered, failure does not promise `C` recovery.
    ///
    /// # Panics
    ///
    /// Panics if an internal request owner fails to publish its terminal reply.
    pub fn send(
        &self,
        command: C,
    ) -> impl Future<Output = Result<ActionTask, TaskSendError<C>>> + Send + 'static + use<C> {
        let endpoint = Arc::clone(&self.endpoint);
        async move {
            let (request, receive) = TypedSend::channel(command);
            endpoint.submit(request).await;
            receive
                .await
                .expect("owned command request publishes terminal result")
        }
    }

    /// Tries one bounded queue admission without waiting or converting `C`.
    ///
    /// Success means only queue admission. The destination may become stale
    /// before actor entry; consume the receipt to obtain that owned result.
    /// Dropping an admitted receipt does not retract work.
    ///
    /// # Errors
    ///
    /// Returns the original command on `Full` or immediate authority rejection.
    pub fn try_send(&self, command: C) -> Result<CommandReceipt<C>, TryCommandError<C>> {
        let (request, receive) = TypedSend::channel(command);
        match self.endpoint.try_submit(request) {
            Ok(()) => Ok(CommandReceipt { receive }),
            Err(TryAdmission::Full(request)) => Err(TryCommandError::Full(request.into_action())),
            Err(TryAdmission::Rejected { request, error }) => Err(TryCommandError::Rejected {
                command: request.into_action(),
                error,
            }),
        }
    }
}

/// Owns the one terminal reply for an admitted command.
///
/// This receipt is not Clone: a rejected non-Clone command has one owner.
pub struct CommandReceipt<C> {
    /// Receives either causal completion authority or the owned terminal error.
    receive: oneshot::Receiver<Result<ActionTask, TaskSendError<C>>>,
}

impl<C> fmt::Debug for CommandReceipt<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CommandReceipt").finish_non_exhaustive()
    }
}

impl<C: Send + 'static> CommandReceipt<C> {
    /// Waits for commit or the actor's owned terminal rejection/failure.
    ///
    /// The resulting `ActionTask` keeps the ordinary causal completion contract;
    /// commit does not mean all resulting work has physically finished.
    ///
    /// # Errors
    ///
    /// Returns `C` only if the actor rejected before entering user code.
    ///
    /// # Panics
    ///
    /// Panics if an internal request owner fails to publish its terminal reply.
    #[expect(
        clippy::manual_async_fn,
        reason = "the public future explicitly guarantees Send, static ownership and exact type captures"
    )]
    pub fn committed(
        self,
    ) -> impl Future<Output = Result<ActionTask, TaskSendError<C>>> + Send + 'static + use<C> {
        async move {
            self.receive
                .await
                .expect("owned command request publishes terminal result")
        }
    }
}

/// Returns ownership when immediate queue admission could not succeed.
///
/// `Rejected` exposes its retained [`TaskAccessError`] through
/// [`std::error::Error::source`]; `Full` has no underlying cause.
pub enum TryCommandError<C> {
    /// The shared bounded mailbox is currently full.
    Full(C),
    /// Authority ended before queue admission, without entering user code.
    Rejected {
        /// The unchanged caller-owned command.
        command: C,
        /// The pre-entry reason admission failed.
        error: TaskAccessError,
    },
}

impl<C> fmt::Debug for TryCommandError<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full(_) => f.debug_tuple("Full").finish_non_exhaustive(),
            Self::Rejected { error, .. } => f
                .debug_struct("Rejected")
                .field("error", error)
                .finish_non_exhaustive(),
        }
    }
}

impl<C> fmt::Display for TryCommandError<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full(_) => f.write_str("command mailbox is full"),
            Self::Rejected { error, .. } => error.fmt(f),
        }
    }
}

impl<C> std::error::Error for TryCommandError<C> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Full(_) => None,
            Self::Rejected { error, .. } => Some(error),
        }
    }
}

/// Names one narrow command declaration without retaining its owner/callbacks.
pub struct CommandSite<R, C> {
    /// Does not retain the installed declaration or its user callbacks.
    declaration: Weak<dyn CommandAcquisition<R, C>>,
}

impl<R, C> Clone for CommandSite<R, C> {
    fn clone(&self) -> Self {
        Self {
            declaration: Weak::clone(&self.declaration),
        }
    }
}

impl<R, C> fmt::Debug for CommandSite<R, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CommandSite").finish_non_exhaustive()
    }
}

impl<R, C> CommandSite<R, C> {
    /// Checks declaration ownership without queuing or inspecting a command.
    #[cfg(feature = "test-util")]
    pub(super) fn validate(&self, root: &Arc<()>) -> Result<(), TaskAccessError> {
        self.declaration
            .upgrade()
            .ok_or_else(|| crate::store::mounted::scope_ended(crate::TaskProgress::NotStarted))?
            .validate(root)
    }

    /// Records the actor-owned declaration's weak identity.
    pub(super) fn from_declaration(declaration: Weak<dyn CommandAcquisition<R, C>>) -> Self {
        Self { declaration }
    }
}

impl<R: 'static, C: Send + 'static> CommandSite<R, C> {
    /// Defers declaration upgrade and actor acquisition until the first poll.
    ///
    /// The root token identifies the owner; it does not retain Store state.
    pub(super) fn acquire(
        &self,
        root: Arc<()>,
        #[cfg(feature = "test-util")] query: Option<AcquireSubmit>,
    ) -> impl Future<Output = Result<Option<CommandPort<C>>, TaskAccessError>> + Send + 'static + use<R, C>
    {
        let declaration = Weak::clone(&self.declaration);
        async move {
            let declaration = declaration.upgrade().ok_or_else(|| {
                crate::store::mounted::scope_ended(crate::TaskProgress::NotStarted)
            })?;
            declaration
                .acquire(
                    root,
                    #[cfg(feature = "test-util")]
                    query,
                )
                .await
        }
    }
}

/// Erases the root/Feature pairing while keeping each owned command typed.
///
/// Implementations hold weak Store and mapper ownership. State and callbacks
/// remain actor-owned; admission must reject before using expired authority.
pub(super) trait CommandEndpoint<C>: Send + Sync {
    /// Borrows a matching pending input without entering its mapper.
    #[cfg(feature = "test-util")]
    fn inspect_input(
        &self,
        _: super::mounted::MountedInputIdentity,
        _: &super::scope::ScopeAuthority,
    ) -> Result<Option<super::mounted::SendInspection<C>>, TaskAccessError>;

    /// Waits for capacity, then transfers or rejects the terminal request owner.
    fn submit(&self, request: TypedSend<C>) -> RuntimeFuture;

    /// Attempts admission once, returning the untouched request on failure.
    fn try_submit(&self, request: TypedSend<C>) -> Result<(), TryAdmission<C>>;
}

#[cfg(feature = "test-util")]
impl<C> super::mounted::MountedInputInspection<C> for CommandPort<C> {
    fn inspect_input(
        &self,
        identity: super::mounted::MountedInputIdentity,
        authority: &super::scope::ScopeAuthority,
    ) -> Result<Option<super::mounted::SendInspection<C>>, TaskAccessError> {
        self.endpoint.inspect_input(identity, authority)
    }
}

/// Keeps the original typed request until immediate admission succeeds.
pub(super) enum TryAdmission<C> {
    /// The request has not entered the currently full mailbox.
    Full(TypedSend<C>),
    /// Owner or captured scope authority rejected this unentered request.
    Rejected {
        /// Still owns the input and its terminal reply sender.
        request: TypedSend<C>,
        /// Distinguishes root closure from expired captured authority.
        error: TaskAccessError,
    },
}

/// Erases an actor-ordered acquisition while preserving the typed result.
pub(super) type CommandAcquisitionFuture<C> =
    Pin<Box<dyn Future<Output = Result<Option<CommandPort<C>>, TaskAccessError>> + Send>>;

/// Acquires a route from a declaration owned by one root's actor.
pub(super) trait CommandAcquisition<R, C>: Send + Sync {
    /// Rejects a foreign root or retired declaration before actor admission.
    #[cfg(feature = "test-util")]
    fn validate(&self, root: &Arc<()>) -> Result<(), TaskAccessError>;

    /// Resolves the current installed capture in actor order, if present.
    fn acquire(
        self: Arc<Self>,
        root: Arc<()>,
        #[cfg(feature = "test-util")] query: Option<AcquireSubmit>,
    ) -> CommandAcquisitionFuture<C>;
}

/// Serialises a mutable actor-owned mapper behind a shareable declaration.
pub(super) trait CommandMapper<C, A>: Send + Sync {
    /// Consumes one command only after the destination's user-entry boundary.
    fn map(&self, command: C) -> A;

    /// Publishes weak inspection access to the held request, not its payload.
    #[cfg(feature = "test-util")]
    fn publish(&self, slot: &Arc<super::mounted::SendSlot<C>>);

    /// Temporarily lends the input only when the request identity still matches.
    #[cfg(feature = "test-util")]
    fn inspect(&self, request: usize) -> Option<super::mounted::SendInspection<C>>;

    /// Removes only the matching inspection registration during request cleanup.
    #[cfg(feature = "test-util")]
    fn clear(&self, request: usize);
}

/// Routes a diagnostic acquisition, returning its owner if it was not admitted.
#[cfg(feature = "test-util")]
pub(super) type AcquireSubmit = Box<
    dyn FnOnce(
            Box<dyn crate::store::mount::AcquireCall + Send>,
        ) -> Pin<
            Box<
                dyn Future<Output = Option<Box<dyn crate::store::mount::AcquireCall + Send>>>
                    + Send,
            >,
        > + Send,
>;
