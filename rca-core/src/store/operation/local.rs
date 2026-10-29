//! Owner-local endpoint transport for the common operation request protocol.
//!
//! `Rc` keeps request futures on the owner thread without requiring transferable
//! state or actions. Keys, authority checks and captured completion metadata use
//! the same protocol as Shared operations. No handle retains Store ownership.

use std::fmt;
use std::hash::Hash;
use std::rc::Rc;
use std::sync::Weak;

use super::{OperationMetadata, OperationRequest, OperationTarget};
use crate::effect::CancellationIdentity;
use crate::effect::operation::{OperationDeclaration, OperationStatus, OperationWait};
use crate::store::command::{Command, Provenance};
use crate::store::ingress::CommandIngress;
use crate::store::lifecycle::Lifecycle;
use crate::store::local::{Local, LocalFuture};
use crate::store::scope::{ScopeAuthority, ScopeToken};
use crate::store::task_context::{ReplyGuard, TaskAccessError, TaskAccessReason, TaskProgress};

/// Observes or cancels named generations in one captured owner-local Store scope.
///
/// Requests use the Store's ordered mailbox on its host thread. This handle owns
/// neither the Store nor operation work. Dropping it does not cancel work.
#[derive(Clone)]
pub struct Operation {
    /// Owner-local identity and weak target metadata, shared between handle clones.
    endpoint: Rc<dyn OperationEndpoint>,
}

/// Erases the local Store domain without imposing `Send` on its request future.
trait OperationEndpoint {
    /// Transfers one request into deferred owner-thread mailbox admission.
    fn submit(self: Rc<Self>, request: OperationRequest) -> LocalFuture;
}

impl Operation {
    /// Captures one local Store's weak root endpoint without accessing state.
    pub(crate) fn root<S: 'static, A: 'static, K>(
        ingress: Weak<CommandIngress<Command<S, A, Local>>>,
        lifecycle: Weak<Lifecycle>,
        key: K,
    ) -> Self
    where
        K: Hash + Eq + Send + Sync + 'static,
    {
        Self {
            endpoint: Rc::new(LocalEndpoint {
                ingress,
                identity: OperationDeclaration::identity(key),
                target: OperationTarget::Root { lifecycle },
                provenance: Provenance::External { handle: None },
            }),
        }
    }

    /// Retains exact scope and producer authority without selecting local state again.
    pub(crate) fn from_frozen_identity<S: 'static, A: 'static>(
        ingress: Weak<CommandIngress<Command<S, A, Local>>>,
        provenance: Provenance,
        scope: ScopeToken,
        authority: ScopeAuthority,
        identity: CancellationIdentity,
    ) -> Self {
        Self {
            endpoint: Rc::new(LocalEndpoint {
                ingress,
                identity,
                target: OperationTarget::Frozen { scope, authority },
                provenance,
            }),
        }
    }

    /// Captures the number of incomplete generations at this request's actor position.
    ///
    /// Admission starts when the returned owner-local future is first polled.
    /// This snapshot includes cancelled generations until their concrete work is destroyed.
    ///
    /// # Errors
    ///
    /// Returns an error when lifecycle, scope or producing authority rejects access.
    ///
    /// # Panics
    ///
    /// Panics if the internal owned-reply protocol is violated.
    pub fn status(
        &self,
    ) -> impl Future<Output = Result<OperationStatus, TaskAccessError>> + 'static {
        let endpoint = Rc::clone(&self.endpoint);
        async move {
            let (reply, receive) = ReplyGuard::channel();
            endpoint.submit(OperationRequest::Status(reply)).await;
            receive
                .await
                .expect("owned reply guard publishes a terminal result")
        }
    }

    /// Captures a passive wait for exactly the currently incomplete named generations.
    ///
    /// Admission starts on first poll. The returned wait owns metadata only and
    /// continues observing concrete completion after actor access ends. It checks
    /// managed task ancestry on every poll, including when moved between tasks.
    ///
    /// # Errors
    ///
    /// Returns an error when lifecycle, scope or producing authority rejects access.
    ///
    /// # Panics
    ///
    /// Panics if the internal owned-reply protocol is violated.
    pub fn capture(
        &self,
    ) -> impl Future<Output = Result<OperationWait, TaskAccessError>> + 'static {
        let endpoint = Rc::clone(&self.endpoint);
        async move {
            let (reply, receive) = ReplyGuard::channel();
            endpoint.submit(OperationRequest::Capture(reply)).await;
            receive
                .await
                .expect("owned reply guard publishes a terminal result")
        }
    }

    /// Requests cancellation of matching authority-live generations in mailbox order.
    ///
    /// Admission starts on first poll. Success acknowledges cancellation, not
    /// destruction of retained work. Capture and await completion separately.
    ///
    /// # Errors
    ///
    /// Returns an error when lifecycle, scope or producing authority rejects access.
    ///
    /// # Panics
    ///
    /// Panics if the internal owned-reply protocol is violated.
    pub fn cancel(&self) -> impl Future<Output = Result<(), TaskAccessError>> + 'static {
        let endpoint = Rc::clone(&self.endpoint);
        async move {
            let (reply, receive) = ReplyGuard::channel();
            endpoint.submit(OperationRequest::Cancel(reply)).await;
            receive
                .await
                .expect("owned reply guard publishes a terminal result")
        }
    }
}

impl fmt::Debug for Operation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Operation").finish_non_exhaustive()
    }
}

/// Retains local request metadata without keeping the actor or work alive.
struct LocalEndpoint<S, A> {
    /// Weak mailbox endpoint paired with the target's Store authority.
    ingress: Weak<CommandIngress<Command<S, A, Local>>>,
    /// Portable named-key identity, shared without rehashing for each request.
    identity: CancellationIdentity,
    /// Root or frozen authority, rechecked at actor entry after admission.
    target: OperationTarget,
    /// Producing authority, independent of concrete work ownership.
    provenance: Provenance,
}

impl<S, A> OperationMetadata for LocalEndpoint<S, A> {
    fn identity(&self) -> &CancellationIdentity {
        &self.identity
    }

    fn target(&self) -> &OperationTarget {
        &self.target
    }

    fn provenance(&self) -> &Provenance {
        &self.provenance
    }
}

impl<S: 'static, A: 'static> OperationEndpoint for LocalEndpoint<S, A> {
    fn submit(self: Rc<Self>, request: OperationRequest) -> LocalFuture {
        let ingress = Weak::clone(&self.ingress);
        let metadata: Rc<dyn OperationMetadata> = self;
        Box::pin(async move {
            if let Err(error) = metadata.target().admission() {
                request.reject(error);
                return;
            }

            if !metadata.provenance().is_live() {
                request.reject(TaskAccessError::new(
                    TaskAccessReason::Cancelled,
                    TaskProgress::NotStarted,
                ));
                return;
            }

            // Admission checks lifecycle before this uninterrupted owner-local
            // prefix; no user callback can release the Store's ingress here.
            let ingress = ingress.upgrade().expect("open local store owns ingress");

            if let Err(async_channel::SendError((request, _))) = ingress
                .send_map((request, metadata), |(request, metadata)| {
                    Command::Operation { request, metadata }
                })
                .await
            {
                request.reject(TaskAccessError::new(
                    TaskAccessReason::StoreClosed,
                    TaskProgress::NotStarted,
                ));
            }
        })
    }
}
