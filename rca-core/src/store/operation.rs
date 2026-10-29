//! Submits ordered, metadata-only named operation requests.
//!
//! Handles share an erased endpoint, but retain only weak Store access. Requests
//! begin admission on first poll and recheck authority when the actor handles them.
//! Replies distinguish rejected access from successful observation or cancellation.
//! Captured completion waits outlive access without retaining concrete work.

use std::fmt;
use std::hash::Hash;
use std::sync::{Arc, Weak};

use super::command::{Command, Provenance};
use super::ingress::Ingress;
use super::lifecycle::Lifecycle;
use super::scope::{ScopeAuthority, ScopeToken};
use super::task_context::{ReplyGuard, TaskAccessError, TaskAccessReason, TaskProgress};
use crate::RuntimeFuture;
use crate::effect::CancellationIdentity;
use crate::effect::operation::{OperationDeclaration, OperationStatus, OperationWait};

#[cfg(feature = "local")]
pub(super) mod local;

/// Observes or cancels named generations in one captured Store scope.
///
/// Requests use the Store's ordered mailbox. This handle retains neither Store
/// ownership nor operation work. Dropping it does not request cancellation.
#[derive(Clone)]
pub struct Operation {
    /// Shared identity and weak target metadata, independent of Store ownership.
    endpoint: Arc<dyn OperationEndpoint + Send + Sync>,
}

/// Erases the Store domain while preserving transferable request futures.
trait OperationEndpoint {
    /// Transfers the request and metadata into deferred mailbox admission.
    fn submit(self: Arc<Self>, request: OperationRequest) -> RuntimeFuture;
}

/// Supplies immutable operation authority to the common actor transaction.
pub(crate) trait OperationMetadata {
    /// Borrows the named key before the actor applies its target scope.
    fn identity(&self) -> &CancellationIdentity;

    /// Borrows either dynamic root access or a captured scope and authority.
    fn target(&self) -> &OperationTarget;

    /// Borrows the producing work's authority without retaining a work lease.
    fn provenance(&self) -> &Provenance;
}

impl Operation {
    /// Captures one Store's weak root endpoint without accessing its state.
    pub(crate) fn root<S: Send + 'static, A: Send + 'static, K>(
        ingress: Weak<Ingress<S, A>>,
        lifecycle: Weak<Lifecycle>,
        key: K,
    ) -> Self
    where
        K: Hash + Eq + Send + Sync + 'static,
    {
        Self {
            endpoint: Arc::new(SharedEndpoint {
                ingress,
                identity: OperationDeclaration::identity(key),
                target: OperationTarget::Root { lifecycle },
                provenance: Provenance::External { handle: None },
            }),
        }
    }

    /// Retains the exact selected scope and producer authority without selecting again.
    pub(crate) fn from_frozen_identity<S: Send + 'static, A: Send + 'static>(
        ingress: Weak<Ingress<S, A>>,
        provenance: Provenance,
        scope: ScopeToken,
        authority: ScopeAuthority,
        identity: CancellationIdentity,
    ) -> Self {
        Self {
            endpoint: Arc::new(SharedEndpoint {
                ingress,
                identity,
                target: OperationTarget::Frozen { scope, authority },
                provenance,
            }),
        }
    }

    /// Captures the number of incomplete generations at this request's actor position.
    ///
    /// Admission starts when the returned future is first polled.
    /// The result is a snapshot, not a subscription. Cancelled generations remain
    /// counted until their concrete work is destroyed.
    ///
    /// # Errors
    ///
    /// Returns [`TaskAccessError`] when lifecycle, scope or producing authority rejects access.
    ///
    /// # Panics
    ///
    /// Panics if the internal owned-reply protocol is violated.
    pub fn status(
        &self,
    ) -> impl Future<Output = Result<OperationStatus, TaskAccessError>> + Send + 'static {
        let endpoint = Arc::clone(&self.endpoint);
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
    /// continues observing concrete completion after actor access ends.
    /// Later generations are excluded. Polling the wait inside an incomplete
    /// captured ancestor returns [`crate::SelfWait`], including after task migration.
    ///
    /// # Errors
    ///
    /// Returns [`TaskAccessError`] when lifecycle, scope or producing authority rejects access.
    ///
    /// # Panics
    ///
    /// Panics if the internal owned-reply protocol is violated.
    pub fn capture(
        &self,
    ) -> impl Future<Output = Result<OperationWait, TaskAccessError>> + Send + 'static {
        let endpoint = Arc::clone(&self.endpoint);
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
    /// Returns [`TaskAccessError`] when lifecycle, scope or producing authority rejects access.
    ///
    /// # Panics
    ///
    /// Panics if the internal owned-reply protocol is violated.
    pub fn cancel(&self) -> impl Future<Output = Result<(), TaskAccessError>> + Send + 'static {
        let endpoint = Arc::clone(&self.endpoint);
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

/// Owns exactly one terminal reply for an ordered metadata-only transaction.
pub(crate) enum OperationRequest {
    /// Reports the number of generations whose work remains incomplete.
    Status(ReplyGuard<OperationStatus>),
    /// Returns a passive observer of the generations captured at actor entry.
    Capture(ReplyGuard<OperationWait>),
    /// Acknowledges cancellation requests without claiming concrete destruction.
    Cancel(ReplyGuard<()>),
}

impl OperationRequest {
    /// Publishes rejection through the request's sole reply owner.
    pub(crate) fn reject(self, error: TaskAccessError) {
        match self {
            Self::Status(reply) => reply.complete(Err(error)),
            Self::Capture(reply) => reply.complete(Err(error)),
            Self::Cancel(reply) => reply.complete(Err(error)),
        }
    }
}

/// Identifies the root or the exact scope authority captured by an operation handle.
#[derive(Clone)]
pub(crate) enum OperationTarget {
    /// Uses the actor's root scope after checking its paired lifecycle.
    Root {
        /// Weak authority paired privately with the endpoint's Store ingress.
        lifecycle: Weak<Lifecycle>,
    },
    /// Cannot retarget a replacement scope or refresh captured authority.
    Frozen {
        /// Exact scope selected when the endpoint was created.
        scope: ScopeToken,
        /// Root, scope and optional handle authority checked before admission.
        authority: ScopeAuthority,
    },
}

impl OperationTarget {
    /// Rejects closed or revoked authority without selecting state or retaining ownership.
    pub(crate) fn admission(&self) -> Result<(), TaskAccessError> {
        match self {
            Self::Root { lifecycle } => lifecycle
                .upgrade()
                .ok_or_else(|| {
                    TaskAccessError::new(TaskAccessReason::StoreClosed, TaskProgress::NotStarted)
                })?
                .access(),
            Self::Frozen { authority, .. } => authority.admission(),
        }
    }
}

/// Retains transferable request metadata without keeping the actor or work alive.
struct SharedEndpoint<S, A> {
    /// Weak mailbox endpoint, upgraded only while submitting a request.
    ingress: Weak<Ingress<S, A>>,
    /// Type-separated named key, shared without rehashing for each request.
    identity: CancellationIdentity,
    /// Root or frozen authority, also rechecked by the actor after admission.
    target: OperationTarget,
    /// Producing authority checked before queueing and at the actor boundary.
    provenance: Provenance,
}

impl<S, A> OperationMetadata for SharedEndpoint<S, A> {
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

impl<S: Send + 'static, A: Send + 'static> OperationEndpoint for SharedEndpoint<S, A> {
    fn submit(self: Arc<Self>, request: OperationRequest) -> RuntimeFuture {
        let ingress = Weak::clone(&self.ingress);
        let metadata: Arc<dyn OperationMetadata + Send + Sync> = self;
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

            #[cfg(all(test, feature = "test-util"))]
            ordinary_test_items! {
                super::mounted::mounted_admission_test_hook::before_ingress();
            }

            let Some(ingress) = ingress.upgrade() else {
                request.reject(TaskAccessError::new(
                    TaskAccessReason::StoreClosed,
                    TaskProgress::NotStarted,
                ));
                return;
            };

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

#[cfg(test)]
ordinary_test_items! {
mod tests {
    use std::sync::Arc;

    use super::{Operation, OperationRequest};
    use crate::effect::lineage::ControlWake;
    use crate::store::ingress::Ingress;
    use crate::store::lifecycle::{CloseState, Lifecycle};
    use crate::store::task_context::{ReplyGuard, TaskAccessReason, TaskProgress};

    #[test]
    fn reports_operation_metadata_layout_without_mutation() {
        macro_rules! width {
            ($type:ty) => {
                println!("{} = {}", stringify!($type), std::mem::size_of::<$type>());
            };
        }
        width!(Operation);
        width!(OperationRequest);
        width!(super::OperationTarget);
        width!(super::SharedEndpoint<(), ()>);
        width!(ReplyGuard<crate::OperationStatus>);
        width!(ReplyGuard<crate::OperationWait>);
        width!(ReplyGuard<()>);
        width!(crate::effect::CancellationIdentity);
        width!(super::Provenance);
        width!(super::ScopeToken);
        width!(super::ScopeAuthority);
        width!(std::sync::Weak<Lifecycle>);
        width!(std::sync::Weak<Ingress<(), ()>>);
        width!(Arc<dyn super::OperationMetadata + Send + Sync>);
        width!(std::rc::Rc<dyn super::OperationMetadata>);
        width!((
            OperationRequest,
            Arc<dyn super::OperationMetadata + Send + Sync>
        ));
        width!((
            OperationRequest,
            crate::effect::CancellationIdentity,
            super::OperationTarget,
            super::Provenance,
        ));
        width!(super::Command<(), ()>);
    }

    #[test]
    fn constructing_operation_futures_does_not_admit_commands() {
        let (sender, receiver) = crate::store::mailbox::bounded(
            std::num::NonZeroUsize::new(1).expect("nonzero capacity"),
        );
        let ingress = Arc::new(Ingress::<(), ()>::new(sender, ControlWake::new()));
        let lifecycle = Arc::new(Lifecycle::new());
        let operation =
            Operation::root(Arc::downgrade(&ingress), Arc::downgrade(&lifecycle), "work");
        let status = operation.status();
        let capture = operation.capture();
        let cancel = operation.cancel();
        assert!(matches!(
            receiver.try_recv(),
            Err(async_channel::TryRecvError::Empty)
        ));
        drop((status, capture, cancel));
        assert!(matches!(
            receiver.try_recv(),
            Err(async_channel::TryRecvError::Empty)
        ));
    }

    #[test]
    fn closed_endpoint_rejects_before_mailbox_admission() {
        let (sender, receiver) = crate::store::mailbox::bounded(
            std::num::NonZeroUsize::new(1).expect("nonzero capacity"),
        );
        let ingress = Arc::new(Ingress::<(), ()>::new(sender, ControlWake::new()));
        let lifecycle = Arc::new(Lifecycle::new());
        let operation =
            Operation::root(Arc::downgrade(&ingress), Arc::downgrade(&lifecycle), "work");
        lifecycle.close(CloseState::Graceful);
        let error =
            futures_executor::block_on(operation.status()).expect_err("closed root rejects");
        assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
        assert_eq!(error.progress(), TaskProgress::NotStarted);
        assert!(matches!(
            receiver.try_recv(),
            Err(async_channel::TryRecvError::Empty)
        ));
    }

    #[test]
    fn request_abandonment_reports_actor_failure_without_claiming_completion() {
        let (reply, receive) = ReplyGuard::channel();
        drop(OperationRequest::Capture(reply));
        let error = futures_executor::block_on(receive)
            .expect("guard publishes reply")
            .expect_err("abandonment is not completion");
        assert_eq!(error.reason(), TaskAccessReason::ActorFailed);
        assert_eq!(error.progress(), TaskProgress::NotStarted);
    }
}
}
