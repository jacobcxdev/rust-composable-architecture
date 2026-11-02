//! Delivers owned outputs through the source's installed ancestor handlers.
//!
//! Admission retains the output and causal work, not handler captures. The actor
//! freezes the eligible ancestors, then transfers the value through that roster.
//! The declaration lends its handlers only for this synchronous traversal.

use std::fmt;
use std::sync::{Arc, OnceLock, Weak};

use futures_channel::oneshot;
use parking_lot::Mutex;

use super::carriers::{Carriers, Shared};
use super::command::Command;
use super::command::Provenance;
use super::ingress::Ingress;
use super::lifecycle::Lifecycle;
use super::mount::MountDeclaration;
use super::mount::MountSite;
use super::mounted::MountedWork;
use super::scope::ScopeTransitionBatch;
use super::scope::{ScopeAuthority, ScopeRegistry, ScopeToken};
use super::task_context::SharedReply;
use super::task_context::{CommitReply, TaskAccessError, TaskAccessReason, TaskProgress};
use super::transaction::Batch;
use crate::feature::Work;
use crate::{ActionTask, Effect, FeatureContext, ReduceContext, RuntimeFuture, Task};

#[cfg(feature = "local")]
pub(in crate::store) mod local;

/// Reaches the bound actor without extending its lifetime.
struct OutputBinding<R, A> {
    /// Admits requests while the store still owns its mailbox.
    ingress: Weak<Ingress<R, A>>,
    /// Checks closure independently of retained mailbox or declaration headers.
    lifecycle: Weak<Lifecycle>,
}

impl<R, A> Clone for OutputBinding<R, A> {
    fn clone(&self) -> Self {
        Self {
            ingress: Weak::clone(&self.ingress),
            lifecycle: Weak::clone(&self.lifecycle),
        }
    }
}

/// Separates the declaration's weak endpoint from its owned callbacks.
struct OutputCell<R, A, O> {
    /// Records the one actor selected when the builder installs this declaration.
    binding: OnceLock<OutputBinding<R, A>>,
    /// Records the declaration owner's first installed scope.
    owner: OnceLock<ScopeToken>,
    /// Identifies the only mounted source allowed to emit this output.
    source: Weak<dyn OutputRegistration>,
    /// Holds callbacks except while the actor owns their traversal lease.
    payload: Mutex<Option<OutputHandlers<R, A, O>>>,
    /// Identifies the request temporarily exposed by the test transaction gate.
    pending: Mutex<Option<(RequestId, Weak<OutputSlot<O>>)>>,
}

/// Drains handler captures when the declaration retires, despite retained headers.
pub(in crate::store) struct OutputOwner<R, A, O>(Arc<OutputCell<R, A, O>>);

impl<R, A, O> OutputOwner<R, A, O> {
    /// Drops callback captures after releasing the payload lock.
    fn drain_handlers(&self) {
        let payload = self.0.payload.lock().take();
        drop(payload);
    }
}

impl<R, A, O> Drop for OutputOwner<R, A, O> {
    fn drop(&mut self) {
        self.drain_handlers();
    }
}

impl<R: Send + 'static, A: Send + 'static, O: Send + 'static> MountDeclaration<R, A>
    for OutputOwner<R, A, O>
{
    fn bind(&self, ingress: &Arc<Ingress<R, A>>, lifecycle: &Arc<Lifecycle>) {
        let result = self.0.binding.set(OutputBinding {
            ingress: Arc::downgrade(ingress),
            lifecycle: Arc::downgrade(lifecycle),
        });
        assert!(result.is_ok(), "one output declaration binds one actor");
    }

    fn reconcile(
        &self,
        _: &R,
        _: &mut ScopeRegistry,
        _: &mut ScopeTransitionBatch,
        _: &mut Vec<Box<dyn super::mounted::InstalledNode + Send>>,
        parent: ScopeToken,
        _: &mut Vec<Box<dyn MountDeclaration<R, A> + Send>>,
        _: &mut super::mount::nested::Descendants<'_, R, A, Shared>,
        #[cfg(feature = "diagnostics")] _: Option<crate::diagnostics::LifecycleCall<'_>>,
    ) {
        self.0.owner.get_or_init(|| parent);
    }
}

/// Configures handlers immediately and erases only their declaration owner.
pub(super) fn attach<R: Send + 'static, A: Send + 'static, O: Send + 'static>(
    source: Weak<dyn OutputRegistration>,
    configure: impl FnOnce(&mut OutputHandlers<R, A, O>),
) -> (Box<dyn MountDeclaration<R, A> + Send>, Output<O>) {
    let (owner, output) = attach_owner(source, configure);
    (Box::new(owner), output)
}

/// Pairs one callback owner with its weak, typed output handle.
pub(in crate::store) fn attach_owner<R: Send + 'static, A: Send + 'static, O: Send + 'static>(
    source: Weak<dyn OutputRegistration>,
    configure: impl FnOnce(&mut OutputHandlers<R, A, O>),
) -> (OutputOwner<R, A, O>, Output<O>) {
    let mut handlers = OutputHandlers {
        handlers: Vec::new(),
    };
    configure(&mut handlers);
    let cell = Arc::new(OutputCell {
        binding: OnceLock::new(),
        owner: OnceLock::new(),
        source,
        payload: Mutex::new(Some(handlers)),
        pending: Mutex::new(None),
    });
    let endpoint: Arc<dyn OutputEndpoint<O>> = Arc::<OutputCell<R, A, O>>::clone(&cell);
    let output = Output::from_endpoint(Arc::downgrade(&endpoint));
    (OutputOwner(cell), output)
}

impl<R: Send + 'static, A: Send + 'static, O: Send + 'static> OutputEndpoint<O>
    for OutputCell<R, A, O>
{
    fn validate_transfer(
        &self,
        parent: ScopeToken,
        scopes: &ScopeRegistry,
    ) -> Result<(), TaskAccessError> {
        let invalid =
            || TaskAccessError::new(TaskAccessReason::ScopeEnded, TaskProgress::NotStarted);
        let source = self
            .source
            .upgrade()
            .and_then(|source| source.freeze(scopes))
            .ok_or_else(invalid)?;
        let owner = self.owner.get().copied().ok_or_else(invalid)?;
        if source.scope != parent
            || !(owner == parent || scopes.ancestor_distance(parent, owner).is_some())
        {
            return Err(invalid());
        }
        Ok(())
    }

    fn submit(self: Arc<Self>, source: OutputSource, request: OutputRequest<O>) -> RuntimeFuture {
        let weak = Arc::downgrade(&self);
        let binding = self.binding.get().cloned();
        let registration = Weak::clone(&self.source);
        drop(self);
        Box::pin(async move {
            let Some(binding) = binding else {
                request.reject(closed());
                return;
            };
            #[cfg(all(test, feature = "test-util"))]
            ordinary_test_items! {
                super::mounted::mounted_admission_test_hook::before_lifecycle();
            }
            let Some(lifecycle) = binding.lifecycle.upgrade() else {
                request.reject(closed());
                return;
            };
            if let Err(error) = lifecycle
                .access()
                .and_then(|()| source.authority.admission())
            {
                request.reject(error);
                return;
            }
            let root = lifecycle.root_identity();
            drop(lifecycle);
            let installed = registration
                .upgrade()
                .and_then(|registration| registration.installed());
            if !source.belongs_to(installed, &root) {
                request.reject(TaskAccessError::new(
                    TaskAccessReason::ScopeEnded,
                    TaskProgress::NotStarted,
                ));
                return;
            }
            if !source.provenance.is_live() {
                request.reject(TaskAccessError::new(
                    TaskAccessReason::Cancelled,
                    TaskProgress::NotStarted,
                ));
                return;
            }
            let operation_work = match source.provenance.operations().try_retain_work() {
                Ok(work) => work,
                Err((reason, partial)) => {
                    request.reject(TaskAccessError::new(reason, TaskProgress::NotStarted));
                    drop(partial);
                    return;
                }
            };
            // The typed value and reply precede causal leases during suspended
            // bounded admission and abandoned-future destruction.
            let owned = (request, source, operation_work);
            #[cfg(all(test, feature = "test-util"))]
            ordinary_test_items! {
                super::mounted::mounted_admission_test_hook::after_retain();
            }
            if !owned.1.provenance.is_live() {
                owned.0.reject(TaskAccessError::new(
                    TaskAccessReason::Cancelled,
                    TaskProgress::NotStarted,
                ));
                drop(owned.2);
                return;
            }
            #[cfg(all(test, feature = "test-util"))]
            ordinary_test_items! {
                super::mounted::mounted_admission_test_hook::before_ingress();
            }
            let Some(ingress) = binding.ingress.upgrade() else {
                owned.0.reject(closed());
                drop(owned.2);
                return;
            };
            let (request, source, operation_work) = owned;
            let request_id = OutputSlot::identity(&request.slot);
            let call = SharedOutputCall {
                cell: weak,
                request: Some(request),
                request_id,
                source: source.scope,
                lease: None,
                roster: Vec::new(),
            };
            let owned = (call, source.provenance, source.authority, operation_work);
            if let Err(error) = ingress
                .send_map(owned, |(call, provenance, authority, operation_work)| {
                    Command::Output {
                        call: Box::new(call),
                        provenance,
                        authority,
                        operation_work,
                    }
                })
                .await
            {
                let (call, _, _, operation_work) = error.0;
                Box::new(call).reject(closed());
                drop(operation_work);
            }
        })
    }

    #[cfg(feature = "test-util")]
    fn pending_slot(&self) -> Arc<OutputSlot<O>> {
        let pending = self.pending.lock();
        // The gate has pinned this declaration's command. Its request owns the
        // published slot until inspection restores the command or retires it.
        let (_, slot) = pending.as_ref().expect("pinned output is published");
        slot.upgrade().expect("pinned output owns its slot")
    }
}

/// Restores leased callbacks before the actor continues reconciliation or unwinding.
struct OutputLease<R, A, O> {
    /// Keeps the restoration destination alive during the synchronous traversal.
    cell: Arc<OutputCell<R, A, O>>,
    /// Owns the handlers removed from the cell for exclusive callback entry.
    payload: Option<OutputHandlers<R, A, O>>,
}

impl<R, A, O> Drop for OutputLease<R, A, O> {
    fn drop(&mut self) {
        drop({
            let mut destination = self.cell.payload.lock();
            std::mem::swap(&mut *destination, &mut self.payload);
            destination
        });
        // Invariant failure or owner retirement never destroys captures under
        // the restoration guard.
        drop(self.payload.take());
    }
}

/// Carries one admitted output through freezing, handler entry and reply creation.
struct SharedOutputCall<R, A, O> {
    /// Avoids retaining callbacks while this command waits in the mailbox.
    cell: Weak<OutputCell<R, A, O>>,
    /// Owns the payload and terminal sender until entry or rejection.
    request: Option<OutputRequest<O>>,
    /// Prevents cleanup from withdrawing a different published request.
    request_id: RequestId,
    /// Fixes the emitting incarnation used to select strict ancestors.
    source: ScopeToken,
    /// Restores callbacks when the call completes or unwinds.
    lease: Option<OutputLease<R, A, O>>,
    /// Orders eligible handlers by ancestor distance, then registration index.
    roster: Vec<(usize, usize)>,
}

impl<R, A, O> Drop for SharedOutputCall<R, A, O> {
    fn drop(&mut self) {
        if let Some(cell) = self.cell.upgrade() {
            let mut pending = cell.pending.lock();
            if pending
                .as_ref()
                .is_some_and(|(current, _)| *current == self.request_id)
            {
                *pending = None;
            }
        }
    }
}

impl<R: Send + 'static, A: Send + 'static, O: Send + 'static> OutputCall<R, A, Shared>
    for SharedOutputCall<R, A, O>
{
    #[cfg(feature = "test-util")]
    fn attach_terminal(&mut self, terminal: super::task_context::TerminalReceipt) {
        self.request
            .as_mut()
            .expect("pending output owns request")
            .terminal = Some(terminal);
    }

    #[cfg(feature = "test-util")]
    fn declaration_anchor(&self) -> <Shared as Carriers<R, A>>::OutputAnchor {
        Weak::<OutputCell<R, A, O>>::clone(&self.cell)
    }

    #[cfg(feature = "test-util")]
    fn declaration(&self) -> DeclarationId {
        DeclarationId(self.cell.as_ptr() as usize)
    }

    #[cfg(feature = "test-util")]
    fn publish_pending(&self) {
        if let Some(cell) = self.cell.upgrade() {
            let request = self.request.as_ref().expect("pending output owns request");
            let mut pending = cell.pending.lock();
            assert!(
                pending.is_none(),
                "one frontier publishes one output per declaration"
            );
            *pending = Some((
                OutputSlot::identity(&request.slot),
                Arc::downgrade(&request.slot),
            ));
        }
    }

    fn freeze(&mut self, scopes: &ScopeRegistry) -> Result<(), TaskAccessError> {
        let error = || TaskAccessError::new(TaskAccessReason::ScopeEnded, TaskProgress::NotStarted);
        let cell = self.cell.upgrade().ok_or_else(error)?;
        // Transaction admission has validated the captured source authority.
        // No actor work can change its registration before this synchronous freeze.
        // The output owner can retire independently, so its weak cell remains checked.
        let payload = cell.payload.lock().take().ok_or_else(closed)?;
        self.lease = Some(OutputLease {
            cell,
            payload: Some(payload),
        });
        let handlers = &mut self
            .lease
            .as_mut()
            .expect("actor owns lease")
            .payload
            .as_mut()
            .expect("lease owns handlers")
            .handlers;
        self.roster = freeze_roster(handlers, scopes, self.source);
        Ok(())
    }

    fn reject(mut self: Box<Self>, error: TaskAccessError) {
        self.request
            .take()
            .expect("rejected output owns request")
            .reject(error);
    }

    fn apply(
        mut self: Box<Self>,
        root: &mut R,
        context: ReduceContext<'_>,
        before: Option<&mut super::reaction::BeforeMutation<'_, R>>,
    ) -> Result<(SharedReply, Vec<Batch<R, A, Shared>>), TaskAccessError> {
        let request = self.request.take().expect("output enters once");
        let handlers = &mut self
            .lease
            .as_mut()
            .expect("frozen output owns lease")
            .payload
            .as_mut()
            .expect("lease owns handlers")
            .handlers;
        let (reply, work) = apply_handlers(root, context, request, handlers, &self.roster, before)?;
        Ok((SharedReply::new(reply), work))
    }
}

/// Captures a handler's incarnation and authority for one actor transaction.
pub(super) struct FrozenRegistration {
    /// Identifies the mounted lifetime without retaining its state or feature.
    pub(super) scope: ScopeToken,
    /// Rejects stale configuration even when the lifetime token remains current.
    pub(super) authority: ScopeAuthority,
}

impl FrozenRegistration {
    /// Requires both the incarnation and its captured authority to remain current.
    pub(super) fn is_current(&self, scopes: &ScopeRegistry) -> bool {
        scopes.is_current(self.scope) && scopes.accepts(&self.authority)
    }
}

/// Exposes installed source metadata without retaining state or feature payloads.
///
/// Handler registrations share this declaration through trait-object upcasting.
pub(super) trait OutputRegistration: Send + Sync {
    /// Returns the currently installed actor identity and mounted incarnation.
    fn installed(&self) -> Option<(Arc<()>, ScopeToken)>;

    /// Captures authority only for a current incarnation in this actor.
    fn freeze(&self, scopes: &ScopeRegistry) -> Option<FrozenRegistration> {
        freeze_registration(self.installed()?, scopes)
    }
}

/// Captures a typed handler route while the declaration still knows child types.
pub(super) trait HandlerRegistration<R, S, X, A>: OutputRegistration {
    /// Returns a route only while the declaration remains installed and current.
    fn freeze_handler_route(
        &self,
        scopes: &ScopeRegistry,
    ) -> Option<Box<dyn HandlerRoute<R, A, S, X> + Send>>;
}

/// Selects frozen child storage and preserves its route for registered work.
pub(super) trait HandlerRoute<R, A, S, X>: Send {
    /// Borrows the incarnation and authority captured before handler traversal.
    fn registration(&self) -> &FrozenRegistration;

    /// Borrows selected storage for the private fold conformance fixtures.
    #[cfg(all(test, feature = "test-util"))]
    fn read<'a>(&self, root: &'a R) -> Option<&'a S>;

    /// Selects the original route, returning `None` if earlier handlers hid it.
    fn select<'a>(&self, root: &'a mut R) -> Option<&'a mut S>;

    /// Transfers this route into child work without copying the child state.
    fn into_work(
        self: Box<Self>,
        work: Work<Effect<X>, Task<S, X>>,
    ) -> Box<dyn MountedWork<R, A, Shared> + Send>;
}

/// Registers typed handlers while the builder still knows the root Action.
///
/// The builder creates this receiver; it cannot be constructed independently.
/// Eligible handlers run from the nearest ancestor to the root. Handlers at the
/// same ancestor run in registration order. The source itself is not an ancestor.
/// [`OutputFlow::Consumed`] stops traversal. Otherwise, the next handler receives
/// the value returned through [`OutputFlow::Continue`], without a clone.
pub struct OutputHandlers<R, A, O> {
    /// Retains callback registration order independently of ancestor distance.
    handlers: Vec<Box<dyn OutputHandler<R, A, O, Shared> + Send>>,
}

impl<R: Send + 'static, A: Send + 'static, O: Send + 'static> OutputHandlers<R, A, O> {
    /// Adds one handler at an explicitly declared ancestor.
    ///
    /// Foreign, retired, sibling and source registrations do not enter the
    /// captured roster. If an earlier handler hides an eligible route, delivery
    /// returns the continued value with a scope error instead of retargeting it.
    pub fn at<S: Send + 'static, X: Send + 'static>(
        &mut self,
        site: &MountSite<R, S, X, A>,
        handler: impl FnMut(&mut S, O, &mut FeatureContext<'_, S, X>) -> OutputFlow<O> + Send + 'static,
    ) {
        self.handlers.push(Box::new(ChildHandler {
            registration: site.handler_registration(),
            handler: Box::new(handler),
            route: None,
        }));
    }

    /// Adds a real root registration, rather than an implicit fallback.
    ///
    /// Root handlers follow all eligible child handlers. An output that no
    /// handler consumes returns [`OutputDelivery::Unhandled`].
    pub fn at_root(
        &mut self,
        handler: impl FnMut(&mut R, O, &mut FeatureContext<'_, R, A>) -> OutputFlow<O> + Send + 'static,
    ) {
        self.handlers.push(Box::new(RootHandler {
            handler: Box::new(handler),
            registration: None,
        }));
    }
}

impl<R, A, O> fmt::Debug for OutputHandlers<R, A, O> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OutputHandlers")
            .field("count", &self.handlers.len())
            .finish()
    }
}

/// Owns an actor-serial callback without requiring shared callback access.
type Handler<S, X, O> =
    Box<dyn FnMut(&mut S, O, &mut FeatureContext<'_, S, X>) -> OutputFlow<O> + Send>;

/// Shares roster traversal between root and child handlers in either carrier.
trait OutputHandler<R, A, O, C: Carriers<R, A>> {
    /// Captures an eligible route and returns its strict ancestor distance.
    fn freeze(&mut self, scopes: &ScopeRegistry, source: ScopeToken) -> Option<usize>;

    /// Borrows metadata after this handler successfully entered the frozen roster.
    fn registration(&self) -> &FrozenRegistration;

    /// Runs one callback, or returns ownership when its selected state disappeared.
    #[expect(
        clippy::type_complexity,
        reason = "Pairs typed output flow and work with the exact returned value on failure."
    )]
    fn apply(
        &mut self,
        root: &mut R,
        value: O,
        context: ReduceContext<'_>,
    ) -> Result<(OutputFlow<O>, Batch<R, A, C>), (O, TaskAccessError)>;
}

/// Holds a root callback and the authority captured for its current traversal.
struct RootHandler<R, A, O> {
    /// Receives exclusive root state and the sole owned output value.
    handler: Handler<R, A, O>,
    /// Exists after the root qualifies as a strict ancestor of the source.
    registration: Option<FrozenRegistration>,
}

impl<R, A, O> OutputHandler<R, A, O, Shared> for RootHandler<R, A, O> {
    fn freeze(&mut self, scopes: &ScopeRegistry, source: ScopeToken) -> Option<usize> {
        let distance = scopes.ancestor_distance(source, scopes.root())?;
        self.registration = Some(FrozenRegistration {
            scope: scopes.root(),
            authority: scopes.authority(scopes.root()),
        });
        Some(distance)
    }

    fn registration(&self) -> &FrozenRegistration {
        self.registration
            .as_ref()
            .expect("frozen root registration")
    }

    fn apply(
        &mut self,
        root: &mut R,
        value: O,
        context: ReduceContext<'_>,
    ) -> Result<(OutputFlow<O>, Batch<R, A, Shared>), (O, TaskAccessError)> {
        let mut context = FeatureContext::new(context);
        let flow = (self.handler)(root, value, &mut context);
        Ok((flow, Batch::Root(context.into_work())))
    }
}

/// Holds a child callback separately from its weak registration and frozen route.
struct ChildHandler<R, A, S, X, O> {
    /// Does not prolong the ancestor declaration's lifetime.
    registration: Weak<dyn HandlerRegistration<R, S, X, A>>,
    /// Remains owned by this output even when its ancestor registration retires.
    handler: Handler<S, X, O>,
    /// Enters at most once per traversal, then transfers into any returned work.
    route: Option<Box<dyn HandlerRoute<R, A, S, X> + Send>>,
}

impl<R: Send + 'static, A: Send + 'static, S: Send + 'static, X: Send + 'static, O>
    OutputHandler<R, A, O, Shared> for ChildHandler<R, A, S, X, O>
{
    fn freeze(&mut self, scopes: &ScopeRegistry, source: ScopeToken) -> Option<usize> {
        self.route = None;
        let registration = self.registration.upgrade()?;
        let route = registration.freeze_handler_route(scopes)?;
        let distance = scopes.ancestor_distance(source, route.registration().scope)?;
        self.route = Some(route);
        Some(distance)
    }

    fn registration(&self) -> &FrozenRegistration {
        self.route
            .as_ref()
            .expect("frozen child registration")
            .registration()
    }

    fn apply(
        &mut self,
        root: &mut R,
        value: O,
        context: ReduceContext<'_>,
    ) -> Result<(OutputFlow<O>, Batch<R, A, Shared>), (O, TaskAccessError)> {
        let route = self.route.take().expect("captured child route enters once");
        let scope = route.registration().scope;
        let mut context = FeatureContext::new(context);
        let flow = match route.select(root) {
            Some(state) => (self.handler)(state, value, &mut context),
            None => {
                return Err((
                    value,
                    TaskAccessError::new(TaskAccessReason::ScopeEnded, TaskProgress::MayHaveRun),
                ));
            }
        };
        let work = context.into_work();
        let work = (!work.is_empty()).then(|| route.into_work(work));
        Ok((flow, Batch::Mounted { work, scope }))
    }
}

/// Erases the output type while preserving its actor-side ownership protocol.
pub(crate) trait OutputCall<R, A, C: Carriers<R, A>> {
    /// Associates test-gate terminal evidence with this request before release.
    #[cfg(feature = "test-util")]
    fn attach_terminal(&mut self, terminal: super::task_context::TerminalReceipt);

    /// Reserves declaration identity without retaining its callbacks.
    #[cfg(feature = "test-util")]
    fn declaration_anchor(&self) -> C::OutputAnchor;

    /// Names the declaration used to match this pending request.
    #[cfg(feature = "test-util")]
    fn declaration(&self) -> DeclarationId;

    /// Makes the pinned request's slot available for typed inspection.
    #[cfg(feature = "test-util")]
    fn publish_pending(&self);

    /// Leases callbacks and fixes the ancestor roster before any handler runs.
    fn freeze(&mut self, scopes: &ScopeRegistry) -> Result<(), TaskAccessError>;

    /// Returns the unentered payload with the supplied admission failure.
    fn reject(self: Box<Self>, error: TaskAccessError);

    /// Applies the frozen roster and returns its terminal reply and ordered work.
    #[expect(
        clippy::type_complexity,
        reason = "Keeps the terminal reply paired with its typed ordered work batches."
    )]
    fn apply(
        self: Box<Self>,
        root: &mut R,
        context: ReduceContext<'_>,
        before: Option<&mut super::reaction::BeforeMutation<'_, R>>,
    ) -> Result<(C::Reply, Vec<Batch<R, A, C>>), TaskAccessError>;
}

/// Names one weak, source-bound output declaration.
///
/// This handle cannot send independently of a `TaskContext`. Cloning it retains
/// neither State, handlers, their captures nor an output value.
pub struct Output<O> {
    /// Refers to a declaration without retaining its callbacks or source state.
    endpoint: Weak<dyn OutputEndpoint<O>>,
}

impl<O> Clone for Output<O> {
    fn clone(&self) -> Self {
        Self {
            endpoint: Weak::clone(&self.endpoint),
        }
    }
}

impl<O> fmt::Debug for Output<O> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Output").finish_non_exhaustive()
    }
}

impl<O> Output<O> {
    /// Wraps the weak endpoint created with its owning declaration.
    pub(super) fn from_endpoint(endpoint: Weak<dyn OutputEndpoint<O>>) -> Self {
        Self { endpoint }
    }

    /// Identifies this declaration while its weak handle reserves the allocation.
    #[cfg(feature = "test-util")]
    pub(crate) fn declaration(&self) -> DeclarationId {
        DeclarationId(self.endpoint.as_ptr().cast::<()>() as usize)
    }
}

impl<O: Send + 'static> Output<O> {
    /// Checks source and owner ancestry before transferring a removal output.
    pub(super) fn validate_transfer(
        &self,
        parent: ScopeToken,
        scopes: &ScopeRegistry,
    ) -> Result<(), TaskAccessError> {
        self.endpoint
            .upgrade()
            .ok_or_else(closed)?
            .validate_transfer(parent, scopes)
    }

    /// Creates an owned future whose first poll checks admission and submits output.
    pub(super) fn deliver(
        &self,
        source: OutputSource,
        value: O,
    ) -> impl Future<Output = Result<OutputDelivery<O>, OutputError<O>>> + Send + 'static + use<O>
    {
        self.deliver_request(
            source,
            value,
            #[cfg(feature = "test-util")]
            None,
        )
    }

    /// Adds removal-origin evidence without changing the delivery protocol.
    #[cfg(feature = "test-util")]
    pub(super) fn deliver_removed(
        &self,
        source: OutputSource,
        value: O,
        origin: crate::RemovalOrigin,
    ) -> impl Future<Output = Result<OutputDelivery<O>, OutputError<O>>> + Send + 'static + use<O>
    {
        self.deliver_request(source, value, Some(origin))
    }

    /// Retains the value across admission and waits for its terminal delivery result.
    fn deliver_request(
        &self,
        source: OutputSource,
        value: O,
        #[cfg(feature = "test-util")] origin: Option<crate::RemovalOrigin>,
    ) -> impl Future<Output = Result<OutputDelivery<O>, OutputError<O>>> + Send + 'static + use<O>
    {
        let endpoint = Weak::clone(&self.endpoint);
        async move {
            let Some(endpoint) = endpoint.upgrade() else {
                return Err(OutputError::Returned {
                    value,
                    error: closed(),
                });
            };
            let (request, receive) = OutputRequest::channel(
                value,
                #[cfg(feature = "test-util")]
                origin,
            );
            endpoint.submit(source, request).await;
            receive
                .await
                .expect("owned output request publishes its terminal result")
        }
    }

    /// Temporarily takes the gate-pinned value and restores it through a drop guard.
    #[cfg(feature = "test-util")]
    pub(crate) fn inspect_value(&self) -> Option<ValueInspection<O>> {
        let endpoint = self.endpoint.upgrade()?;
        let slot = endpoint.pending_slot();
        drop(endpoint);
        let value = slot
            .value
            .lock()
            .take()
            .expect("pinned output owns its value");
        Some(ValueInspection {
            value: Some(value),
            slot,
        })
    }
}

/// Returns ownership when an ancestor declines an output.
pub enum OutputFlow<O> {
    /// Stops traversal after consuming the value.
    Consumed,
    /// Passes the sole owned value to the next captured ancestor.
    Continue(O),
}

impl<O> fmt::Debug for OutputFlow<O> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Consumed => f.write_str("Consumed"),
            Self::Continue(_) => f.debug_tuple("Continue").field(&"..").finish(),
        }
    }
}

/// Reports the committed traversal and its causal task receipt.
///
/// Delivery acknowledges the output transaction, not completion of the work it
/// registered. [`ActionTask::finish`] waits for that causal work separately.
pub enum OutputDelivery<O> {
    /// A handler consumed the value.
    Consumed {
        /// Work registered by the output transaction.
        task: ActionTask,
    },
    /// No captured handler consumed the value.
    Unhandled {
        /// The unchanged or explicitly continued owned value.
        value: O,
        /// Work registered by the output transaction.
        task: ActionTask,
    },
}

impl<O> fmt::Debug for OutputDelivery<O> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Consumed { task } => f.debug_struct("Consumed").field("task", task).finish(),
            Self::Unhandled { task, .. } => f
                .debug_struct("Unhandled")
                .field("task", task)
                .finish_non_exhaustive(),
        }
    }
}

/// Distinguishes owned recovery from failure after a consuming callback.
///
/// [`std::error::Error::source`] borrows the underlying [`TaskAccessError`]
/// without consuming any recoverable value.
pub enum OutputError<O> {
    /// Returns the value still owned by the request or a continuation.
    Returned {
        /// The sole value remaining after any completed continuations.
        value: O,
        /// The failure and its actual progress, which may include user entry.
        error: TaskAccessError,
    },
    /// A consuming callback may have destroyed the value.
    Failed {
        /// The failure and its conservative progress.
        error: TaskAccessError,
    },
}

impl<O> fmt::Debug for OutputError<O> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Returned { error, .. } => f
                .debug_struct("Returned")
                .field("error", error)
                .finish_non_exhaustive(),
            Self::Failed { error } => f.debug_struct("Failed").field("error", error).finish(),
        }
    }
}

impl<O> fmt::Display for OutputError<O> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Returned { error, .. } | Self::Failed { error } => error.fmt(f),
        }
    }
}

impl<O> std::error::Error for OutputError<O> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Returned { error, .. } | Self::Failed { error } => Some(error),
        }
    }
}

// Identity uses existing allocation ownership, not a counter or another Arc.
// A live Output's Weak reserves the declaration allocation. A pending command
// owns its slot strongly, and its published Weak reserves that allocation until
// conditional withdrawal. These addresses cannot be reused while compared.
/// Names a declaration while a retained weak handle prevents address reuse.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg(feature = "test-util")]
pub(crate) struct DeclarationId(usize);

/// Names a request slot while its owner or published weak handle reserves it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RequestId(usize);

/// Carries the emitting lifetime and its captured causal and admission authority.
pub(crate) struct OutputSource {
    /// Must match the installed source declaration, not merely a sibling's state.
    pub(super) scope: ScopeToken,
    /// Retains cancellation and operation ancestry for admission and registered work.
    pub(super) provenance: Provenance,
    /// Rejects stale configuration or a different actor's scope authority.
    pub(super) authority: ScopeAuthority,
}

impl OutputSource {
    /// Requires the source declaration, endpoint binding and authority to share one actor.
    fn belongs_to(&self, installed: Option<(Arc<()>, ScopeToken)>, root: &Arc<()>) -> bool {
        installed.is_some_and(|(installed_root, scope)| {
            Arc::ptr_eq(&installed_root, root)
                && self.scope == scope
                && self.authority.belongs_to(root)
        })
    }
}

/// Captures authority only when the registration belongs to the current actor.
fn freeze_registration(
    (root, scope): (Arc<()>, ScopeToken),
    scopes: &ScopeRegistry,
) -> Option<FrozenRegistration> {
    if !Arc::ptr_eq(&root, &scopes.root_identity()) || !scopes.is_current(scope) {
        return None;
    }
    Some(FrozenRegistration {
        scope,
        authority: scopes.authority(scope),
    })
}

/// Orders eligible handlers by distance, preserving registration order within each ancestor.
fn freeze_roster<R, A, O, C: Carriers<R, A>, H: OutputHandler<R, A, O, C> + ?Sized>(
    handlers: &mut [Box<H>],
    scopes: &ScopeRegistry,
    source: ScopeToken,
) -> Vec<(usize, usize)> {
    let mut roster = Vec::new();
    for (index, handler) in handlers.iter_mut().enumerate() {
        if let Some(distance) = handler.freeze(scopes, source) {
            roster.push((distance, index));
        }
    }
    roster.sort_unstable();
    roster
}

/// Traverses frozen handlers while preserving the sole output owner and failure progress.
#[expect(
    clippy::type_complexity,
    reason = "Keeps the output reply paired with the ordered handler work batches."
)]
fn apply_handlers<R, A, O, C: Carriers<R, A>, H: OutputHandler<R, A, O, C> + ?Sized>(
    root: &mut R,
    context: ReduceContext<'_>,
    request: OutputRequest<O>,
    handlers: &mut [Box<H>],
    roster: &[(usize, usize)],
    before: Option<&mut super::reaction::BeforeMutation<'_, R>>,
) -> Result<(OutputReply<O>, Vec<Batch<R, A, C>>), TaskAccessError> {
    let (value, mut reply) = request.enter();
    // Context/work drop before the terminal reply if a callback unwinds.
    let mut context = context;
    let mut work = Vec::new();
    let mut remaining = Some(value);
    if let Some(capture) = before {
        reply.mark_entered();
        if !capture(root, context.scopes()) {
            let error =
                TaskAccessError::new(TaskAccessReason::StoreClosed, TaskProgress::MayHaveRun);
            reply.returned(remaining.take().expect("entered output"), error);
            return Err(error);
        }
    }
    // Native registrations stay current until post-handler reconciliation.
    // A handler can remove a state path; each captured route checks selection.
    for &(_, index) in roster {
        let handler = &mut handlers[index];
        let scope = handler.registration().scope;
        reply.mark_entered();
        match handler.apply(
            root,
            remaining.take().expect("continuation owns output"),
            context.reborrow().in_scope(scope),
        ) {
            Ok((flow, batch)) => {
                // Empty handlers need no work-list allocation or registration pass.
                if !batch.is_empty() {
                    work.push(batch);
                }
                match flow {
                    OutputFlow::Consumed => break,
                    OutputFlow::Continue(value) => remaining = Some(value),
                }
            }
            Err((value, error)) => {
                reply.returned(value, error);
                return Err(error);
            }
        }
    }
    if let Some(value) = remaining {
        reply.unhandled(value);
    }
    Ok((reply, work))
}

/// Exposes only the request pinned by the transaction gate's inspection guard.
#[cfg(feature = "test-util")]
pub(crate) trait OutputInspection<O> {
    /// Identifies the declaration without inspecting its payload type.
    fn declaration(&self) -> DeclarationId;

    /// Copies removal evidence while the gate owns the pending command.
    fn inspect_origin(&self) -> Option<crate::RemovalOrigin>;

    /// Takes and restores the sole value while retirement waits for the gate guard.
    fn inspect_value(&self) -> Option<ValueInspection<O>>;
}

#[cfg(feature = "test-util")]
impl<O: Send + 'static> OutputInspection<O> for Output<O> {
    fn declaration(&self) -> DeclarationId {
        self.declaration()
    }

    fn inspect_origin(&self) -> Option<crate::RemovalOrigin> {
        self.endpoint.upgrade()?.pending_slot().origin.clone()
    }

    fn inspect_value(&self) -> Option<ValueInspection<O>> {
        self.inspect_value()
    }
}

/// Accepts typed requests through a weak declaration-bound endpoint.
pub(super) trait OutputEndpoint<O>: Send + Sync {
    /// Checks whether removal may transfer output from this source and owner.
    fn validate_transfer(
        &self,
        parent: ScopeToken,
        scopes: &ScopeRegistry,
    ) -> Result<(), TaskAccessError>;

    /// Releases the temporary endpoint owner before asynchronous mailbox admission.
    fn submit(self: Arc<Self>, source: OutputSource, request: OutputRequest<O>) -> RuntimeFuture;

    /// Borrows the slot pinned by the test gate, not an arbitrary queued request.
    #[cfg(feature = "test-util")]
    fn pending_slot(&self) -> Arc<OutputSlot<O>>;
}

/// Gives admission and typed inspection one stable, restorable payload location.
pub(crate) struct OutputSlot<O> {
    /// Is empty only while inspection or the entered handler owns the value.
    value: Mutex<Option<O>>,
    /// Records the removed child responsible for this output when tests request it.
    #[cfg(feature = "test-util")]
    origin: Option<crate::RemovalOrigin>,
}

impl<O> OutputSlot<O> {
    /// Uses the retained allocation address instead of allocating a separate identifier.
    pub(super) fn identity(slot: &Arc<Self>) -> RequestId {
        RequestId(Arc::as_ptr(slot) as usize)
    }
}

/// Restores the value before the enclosing pending-inspection guard releases its command.
#[cfg(feature = "test-util")]
pub(crate) struct ValueInspection<O> {
    /// Owns the temporarily borrowed payload until this guard restores it.
    value: Option<O>,
    /// Keeps the restoration destination alive independently of declaration retirement.
    slot: Arc<OutputSlot<O>>,
}

#[cfg(feature = "test-util")]
impl<O> std::ops::Deref for ValueInspection<O> {
    type Target = O;
    fn deref(&self) -> &O {
        self.value.as_ref().expect("inspection owns its one output")
    }
}

#[cfg(feature = "test-util")]
impl<O> Drop for ValueInspection<O> {
    fn drop(&mut self) {
        let displaced = {
            let mut destination = self.slot.value.lock();
            std::mem::replace(&mut *destination, self.value.take())
        };
        // Both owners remain outside the lock if this invariant is violated.
        assert!(displaced.is_none(), "inspected output slot was occupied");
    }
}

/// Publishes exactly one owned delivery result to its awaiting caller.
type OutputSender<O> = oneshot::Sender<Result<OutputDelivery<O>, OutputError<O>>>;

/// Owns an unentered output and reports abandonment if admission never completes.
pub(super) struct OutputRequest<O> {
    /// Retains the payload, including while a test inspector temporarily borrows it.
    pub(super) slot: Arc<OutputSlot<O>>,
    /// Transfers into the reply at entry, preventing duplicate terminal publication.
    sender: Option<OutputSender<O>>,
    /// Records terminal evidence before waking the typed receiver.
    #[cfg(feature = "test-util")]
    terminal: Option<super::task_context::TerminalReceipt>,
}

impl<O> OutputRequest<O> {
    /// Allocates the payload slot and pairs its request with one terminal receiver.
    #[expect(
        clippy::type_complexity,
        reason = "Returns the owned request and its typed terminal delivery receiver together."
    )]
    pub(super) fn channel(
        value: O,
        #[cfg(feature = "test-util")] origin: Option<crate::RemovalOrigin>,
    ) -> (
        Self,
        oneshot::Receiver<Result<OutputDelivery<O>, OutputError<O>>>,
    ) {
        let (sender, receive) = oneshot::channel();
        (
            Self {
                slot: Arc::new(OutputSlot {
                    value: Mutex::new(Some(value)),
                    #[cfg(feature = "test-util")]
                    origin,
                }),
                sender: Some(sender),
                #[cfg(feature = "test-util")]
                terminal: None,
            },
            receive,
        )
    }

    /// Returns the unentered value without invoking a handler.
    pub(super) fn reject(self, error: TaskAccessError) {
        let (value, reply) = self.enter();
        reply.returned(value, error);
    }

    /// Moves the restored payload and sender out before this request drops.
    pub(super) fn enter(mut self) -> (O, OutputReply<O>) {
        let value = self.slot.value.lock().take();
        let value = value.expect("actor owns restored output value");
        let reply = OutputReply {
            sender: self.sender.take(),
            value: None,
            progress: TaskProgress::NotStarted,
            #[cfg(feature = "test-util")]
            terminal: self.terminal.take(),
        };
        (value, reply)
    }
}

impl<O> Drop for OutputRequest<O> {
    fn drop(&mut self) {
        if let Some(sender) = self.sender.take() {
            // Inspection restores the value before releasing the pending request.
            let value = self
                .slot
                .value
                .lock()
                .take()
                .expect("pending output value was restored before request drop");
            let error =
                TaskAccessError::new(TaskAccessReason::ActorFailed, TaskProgress::NotStarted);
            #[cfg(feature = "test-util")]
            if let Some(terminal) = &self.terminal {
                terminal.record(Err(error));
            }
            let _ = sender.send(Err(OutputError::Returned { value, error }));
        }
    }
}

/// Tracks handler entry and returns retained output if the actor later fails.
pub(super) struct OutputReply<O> {
    /// Is consumed by completion, explicit rejection or abandonment, exactly once.
    sender: Option<OutputSender<O>>,
    /// Holds an unconsumed continuation after the handler traversal finishes.
    value: Option<O>,
    /// Distinguishes admission failure, possible mutation and committed revision.
    progress: TaskProgress,
    /// Records the same terminal result exposed by the typed receiver.
    #[cfg(feature = "test-util")]
    terminal: Option<super::task_context::TerminalReceipt>,
}

impl<O> OutputReply<O> {
    /// Records potential user-code entry before calling a capture or handler.
    pub(super) fn mark_entered(&mut self) {
        self.progress = TaskProgress::MayHaveRun;
    }

    /// Retains the final continuation while reconciliation and commit still can fail.
    pub(super) fn unhandled(&mut self, value: O) {
        self.value = Some(value);
    }

    /// Publishes a recoverable value and its exact failure progress.
    pub(super) fn returned(mut self, value: O, error: TaskAccessError) {
        #[cfg(feature = "test-util")]
        if let Some(terminal) = &self.terminal {
            terminal.record(Err(error));
        }
        let sender = self
            .sender
            .take()
            .expect("terminal output owns reply sender");
        let _ = sender.send(Err(OutputError::Returned { value, error }));
    }
}

impl<O> CommitReply for OutputReply<O> {
    fn mark_committed(&mut self, revision: u64) {
        self.progress = TaskProgress::Committed { revision };
    }

    fn complete(mut self, task: ActionTask) {
        #[cfg(feature = "test-util")]
        if let Some(terminal) = &self.terminal {
            terminal.record(Ok(()));
        }
        let result = match self.value.take() {
            Some(value) => OutputDelivery::Unhandled { value, task },
            None => OutputDelivery::Consumed { task },
        };
        let sender = self
            .sender
            .take()
            .expect("terminal output owns reply sender");
        let _ = sender.send(Ok(result));
    }
}

impl<O> Drop for OutputReply<O> {
    fn drop(&mut self) {
        if let Some(sender) = self.sender.take() {
            let error = TaskAccessError::new(TaskAccessReason::ActorFailed, self.progress);
            #[cfg(feature = "test-util")]
            if let Some(terminal) = &self.terminal {
                terminal.record(Err(error));
            }
            let result = match self.value.take() {
                Some(value) => Err(OutputError::Returned { value, error }),
                None => Err(OutputError::Failed { error }),
            };
            let _ = sender.send(result);
        }
    }
}

/// Reports closure before the output could enter user code.
fn closed() -> TaskAccessError {
    TaskAccessError::new(TaskAccessReason::StoreClosed, TaskProgress::NotStarted)
}

#[cfg(test)]
ordinary_test_items! {
mod tests {
    use super::*;
    use crate::store::mount::children;
    use crate::store::transaction::{self, Mutation, tests::Host};
    use crate::{Feature, MountIdentity, OptionalMount};
    use std::future::Future;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll, Waker};

    #[test]
    fn output_error_chain_preserves_the_owned_value() {
        use std::cell::Cell;
        use std::error::Error;

        // The payload is borrowed, non-Send and non-Debug.
        struct Value<'a>(&'a Cell<usize>);

        let state = Cell::new(7);
        let failures = [
            OutputError::Returned {
                value: Value(&state),
                error: TaskAccessError::new(TaskAccessReason::InputClosed, TaskProgress::NotStarted),
            },
            OutputError::Failed {
                error: TaskAccessError::new(
                    TaskAccessReason::ActorFailed,
                    TaskProgress::Committed { revision: 9 },
                ),
            },
        ];

        for failure in &failures {
            let expected = match failure {
                OutputError::Returned { error, .. } | OutputError::Failed { error } => error,
            };
            let source = failure
                .source()
                .expect("output failures retain their cause")
                .downcast_ref::<TaskAccessError>()
                .expect("the cause retains its concrete type");
            assert!(std::ptr::eq(source, expected));
            assert!(source.source().is_none());
            assert_eq!(failure.to_string(), expected.to_string());
        }

        let [returned, _failed] = failures;
        let OutputError::Returned { value, error } = returned else {
            panic!("cause inspection preserves value ownership");
        };
        assert!(std::ptr::eq(value.0, &raw const state));
        assert_eq!(error.reason(), TaskAccessReason::InputClosed);
        assert_eq!(error.progress(), TaskProgress::NotStarted);
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "One retirement timeline distinguishes source authority, header lifetime and handler ownership."
    )]
    fn retired_output_handlers_reject_with_a_retained_shared_header() {
        struct Root;

        impl Feature for Root {
            type State = (Option<usize>, Option<usize>);
            type Action = ();

            fn update(
                &mut self,
                state: &mut Self::State,
                (): &(),
                _: &mut FeatureContext<'_, Self::State, ()>,
            ) {
                state.1 = None;
            }
        }

        struct Child;

        impl Feature for Child {
            type State = usize;
            type Action = ();

            fn update(&mut self, _: &mut usize, (): &(), _: &mut FeatureContext<'_, usize, ()>) {}
        }

        struct DropProbe(Arc<AtomicUsize>);

        impl Drop for DropProbe {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let (source_owner, source_site) = children::attach(
            OptionalMount::new(
                |state: &(Option<usize>, Option<usize>)| state.0.as_ref(),
                |state: &mut (Option<usize>, Option<usize>)| state.0.as_mut(),
                |state: &(Option<usize>, Option<usize>)| {
                    state.0.map(|_| MountIdentity {
                        key: (),
                        replacement: 0,
                        configuration: 0,
                    })
                },
                |_| Child,
            ),
            |_, _| {},
        );
        let handlers = Arc::new(AtomicUsize::new(0));
        let handler_drops = Arc::new(AtomicUsize::new(0));
        let recipe_calls = Arc::clone(&handlers);
        let recipe_drops = Arc::clone(&handler_drops);
        let recipe_source = source_site.clone();
        let output_slot = Arc::new(Mutex::new(None));
        let recipe_output = Arc::clone(&output_slot);
        let (parent_owner, _) = children::attach(
            OptionalMount::new(
                |state: &(Option<usize>, Option<usize>)| state.1.as_ref(),
                |state: &mut (Option<usize>, Option<usize>)| state.1.as_mut(),
                |state: &(Option<usize>, Option<usize>)| {
                    state.1.map(|_| MountIdentity {
                        key: (),
                        replacement: 0,
                        configuration: 0,
                    })
                },
                |_| Child,
            ),
            move |_, children| {
                let calls = Arc::clone(&recipe_calls);
                let capture = DropProbe(Arc::clone(&recipe_drops));
                let output = children.output(&recipe_source, |handlers| {
                    handlers.at_root(move |_, value: Box<(usize, DropProbe)>, _| {
                        let _ = &capture;
                        calls.fetch_add(1, Ordering::SeqCst);
                        OutputFlow::Continue(value)
                    });
                });
                // A different Shared delivery may pause after deliver_request
                // upgrades its endpoint, before submit drops that Arc. This pin
                // models that legal preemption, not a second handler owner.
                let retained = output.endpoint.upgrade().unwrap();
                *recipe_output.lock() = Some((output, retained));
            },
        );
        let mut host = Host::<_, _, Shared>::new((Some(7), Some(11)), Box::new(Root));
        source_owner.bind(&host.ingress, &host.lifecycle);
        parent_owner.bind(&host.ingress, &host.lifecycle);
        host.mounts.extend([source_owner, parent_owner]);
        drop(transaction::commit(host.parts(), Mutation::Structural));
        let source_scope = host.mounts[0].installed_scope().unwrap();
        let parent_scope = host.mounts[1].installed_scope().unwrap();

        let (output, retained) = output_slot.lock().take().unwrap();
        let weak = Arc::downgrade(&retained);
        assert_eq!(handler_drops.load(Ordering::SeqCst), 0);

        let mut cx = Context::from_waker(Waker::noop());
        let mut acquisition = Box::pin(source_site.acquire(host.scopes.root_identity()));
        assert!(acquisition.as_mut().poll(&mut cx).is_pending());
        let Command::Acquire(call) = host.receiver.try_recv().unwrap() else {
            panic!("source acquisition entered the real mailbox");
        };
        call.execute(&host.scopes, &host.lifecycle);
        let Poll::Ready(Ok(Some(source))) = acquisition.as_mut().poll(&mut cx) else {
            panic!("source A is installed independently of owner B");
        };
        drop(acquisition);

        let (turn, mut retirement) = host.action(());
        drop(turn);
        assert!(matches!(retirement.try_recv(), Ok(Some(Ok(_)))));
        assert_eq!(host.state, (Some(7), None));
        assert!(host.scopes.is_current(source_scope));
        assert!(!host.scopes.is_current(parent_scope));
        host.lifecycle.access().unwrap();
        assert_eq!(host.mounts.len(), 2);
        assert_eq!(handler_drops.load(Ordering::SeqCst), 1);
        assert!(weak.upgrade().is_some());

        let value_drops = Arc::new(AtomicUsize::new(0));
        let value = Box::new((19, DropProbe(Arc::clone(&value_drops))));
        let identity = std::ptr::from_ref(value.as_ref());
        let mut delivery = Box::pin(source.output(&output, value));
        assert!(delivery.as_mut().poll(&mut cx).is_pending());
        let Command::Output {
            call,
            provenance,
            authority,
            operation_work,
        } = host.receiver.try_recv().unwrap()
        else {
            panic!("live source admits output despite independently retired handlers");
        };
        assert!(host.parts().rejection(&provenance, &authority).is_none());
        let revision = host.version;
        drop(transaction::commit(
            host.parts(),
            Mutation::Output(call, provenance, authority),
        ));
        drop(operation_work);
        let Poll::Ready(Err(OutputError::Returned { value, error })) =
            delivery.as_mut().poll(&mut cx)
        else {
            panic!("drained handlers return the original unentered value");
        };
        assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
        assert_eq!(error.progress(), TaskProgress::NotStarted);
        assert_eq!(std::ptr::from_ref(value.as_ref()), identity);
        assert_eq!(value.0, 19);
        assert_eq!(value_drops.load(Ordering::SeqCst), 0);
        assert_eq!(handlers.load(Ordering::SeqCst), 0);
        assert_eq!(host.version, revision);
        assert_eq!(host.state, (Some(7), None));
        assert!(host.receiver.try_recv().is_err());
        drop(value);
        drop(delivery);
        drop(source);
        drop(retained);
        assert!(weak.upgrade().is_none());
        assert_eq!(value_drops.load(Ordering::SeqCst), 1);
        assert_eq!(handler_drops.load(Ordering::SeqCst), 1);
    }
}
}
