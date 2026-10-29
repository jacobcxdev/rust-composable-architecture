//! Owner-local endpoints and callbacks for the common typed output traversal.
//!
//! `Rc` and `RefCell` keep callback ownership on the actor's thread. The shared
//! traversal still supplies ordering, value recovery and terminal reply semantics.
//! No callback runs while the declaration payload is borrowed.

use super::{
    Arc, Batch, Command, FrozenRegistration, Lifecycle, MountDeclaration, MountedWork, OnceLock,
    OutputCall, OutputDelivery, OutputError, OutputFlow, OutputHandler, OutputRequest, OutputSlot,
    OutputSource, ReduceContext, RequestId, ScopeRegistry, ScopeToken, ScopeTransitionBatch,
    TaskAccessError, TaskAccessReason, TaskProgress, Weak, Work, apply_handlers, closed, fmt,
    freeze_registration, freeze_roster,
};
#[cfg(feature = "test-util")]
use super::{DeclarationId, OutputInspection, ValueInspection};
use crate::store::ingress::CommandIngress;
use crate::store::local::{FeatureContext, Local, LocalFuture, LocalReply};
use crate::store::mount::local::LocalMountSite;
use std::cell::RefCell;
use std::rc::{Rc, Weak as LocalWeak};

/// Reaches the actor without extending the lifetime of its mailbox or lifecycle.
struct Binding<R, A> {
    /// Admits owner-local commands through the actor's existing mailbox.
    ingress: Weak<CommandIngress<Command<R, A, Local>>>,
    /// Checks closure before any user callback can run.
    lifecycle: Weak<Lifecycle>,
}

impl<R, A> Clone for Binding<R, A> {
    fn clone(&self) -> Self {
        Self {
            ingress: Weak::clone(&self.ingress),
            lifecycle: Weak::clone(&self.lifecycle),
        }
    }
}

/// Separates the weak Local endpoint from the declaration's callback ownership.
struct Cell<R, A, O> {
    /// Records the one actor selected when the builder installs this declaration.
    binding: OnceLock<Binding<R, A>>,
    /// Records the declaration owner's first installed scope.
    owner: OnceLock<ScopeToken>,
    /// Identifies the only mounted source allowed to emit this output.
    source: LocalWeak<dyn OutputRegistration>,
    /// Holds callbacks except while the actor owns their traversal lease.
    payload: RefCell<Option<OutputHandlers<R, A, O>>>,
    /// Identifies the request temporarily exposed by the test transaction gate.
    pending: RefCell<Option<(RequestId, Weak<OutputSlot<O>>)>>,
}

/// Drains callback captures when this owner-local declaration retires.
pub(in crate::store) struct OutputOwner<R, A, O>(Rc<Cell<R, A, O>>);

impl<R, A, O> OutputOwner<R, A, O> {
    /// Releases the payload borrow before dropping captures that can re-enter output.
    fn drain_handlers(&self) {
        let payload = { self.0.payload.borrow_mut().take() };
        drop(payload);
    }
}

impl<R, A, O> Drop for OutputOwner<R, A, O> {
    fn drop(&mut self) {
        self.drain_handlers();
    }
}

impl<R: 'static, A: 'static, O: 'static> MountDeclaration<R, A, Local> for OutputOwner<R, A, O> {
    fn bind(
        &self,
        ingress: &Arc<CommandIngress<Command<R, A, Local>>>,
        lifecycle: &Arc<Lifecycle>,
    ) {
        let result = self.0.binding.set(Binding {
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
        _: &mut Vec<Box<dyn super::super::mounted::InstalledNode>>,
        parent: ScopeToken,
        _: &mut Vec<Box<dyn MountDeclaration<R, A, Local>>>,
        _: &mut crate::store::mount::nested::Descendants<'_, R, A, Local>,
        #[cfg(feature = "diagnostics")] _: Option<crate::diagnostics::LifecycleCall<'_>>,
    ) {
        let _ = self.0.owner.set(parent);
    }
}

/// Configures handlers immediately and erases only their declaration owner.
pub(in crate::store) fn attach<R: 'static, A: 'static, O: 'static>(
    source: LocalWeak<dyn OutputRegistration>,
    configure: impl FnOnce(&mut OutputHandlers<R, A, O>),
) -> (Box<dyn MountDeclaration<R, A, Local>>, Output<O>) {
    let (owner, output) = attach_owner(source, configure);
    (Box::new(owner), output)
}

/// Pairs one owner-local callback owner with its weak, typed output handle.
pub(in crate::store) fn attach_owner<R: 'static, A: 'static, O: 'static>(
    source: LocalWeak<dyn OutputRegistration>,
    configure: impl FnOnce(&mut OutputHandlers<R, A, O>),
) -> (OutputOwner<R, A, O>, Output<O>) {
    let mut handlers = OutputHandlers {
        handlers: Vec::new(),
    };
    configure(&mut handlers);
    let cell = Rc::new(Cell {
        binding: OnceLock::new(),
        owner: OnceLock::new(),
        source,
        payload: RefCell::new(Some(handlers)),
        pending: RefCell::new(None),
    });
    let endpoint: Rc<dyn OutputEndpoint<O>> = Rc::<Cell<R, A, O>>::clone(&cell);
    let output = Output {
        endpoint: Rc::downgrade(&endpoint),
    };
    (OutputOwner(cell), output)
}

impl<R: 'static, A: 'static, O: 'static> OutputEndpoint<O> for Cell<R, A, O> {
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

    fn submit(self: Rc<Self>, source: OutputSource, request: OutputRequest<O>) -> LocalFuture {
        let weak = Rc::downgrade(&self);
        let binding = self.binding.get().cloned();
        let registration = LocalWeak::clone(&self.source);
        drop(self);
        Box::pin(async move {
            let Some(binding) = binding else {
                request.reject(closed());
                return;
            };
            // Bound declaration owners are destroyed before the actor's lifecycle
            // guard, including re-entrant output calls from handler destructors.
            let lifecycle = binding
                .lifecycle
                .upgrade()
                .expect("bound local output owner retains actor lifecycle");
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
            let operation_work = match source.provenance.operations().try_retain_work() {
                Ok(work) => work,
                Err((reason, partial)) => {
                    request.reject(TaskAccessError::new(reason, TaskProgress::NotStarted));
                    drop(partial);
                    return;
                }
            };
            let owned = (request, source, operation_work);
            if !owned.1.provenance.is_live() {
                owned.0.reject(TaskAccessError::new(
                    TaskAccessReason::Cancelled,
                    TaskProgress::NotStarted,
                ));
                drop(owned.2);
                return;
            }
            // This owner-local admission prefix cannot yield or run user code;
            // an open Store still owns ingress until its close is observable.
            let ingress = binding
                .ingress
                .upgrade()
                .expect("open local store owns output ingress");
            let (request, source, operation_work) = owned;
            let request_id = OutputSlot::identity(&request.slot);
            let call = LocalOutputCall {
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
        let pending = self.pending.borrow();
        // The gate has pinned this declaration's command. Its request owns the
        // published slot until inspection restores the command or retires it.
        let (_, slot) = pending.as_ref().expect("pinned output is published");
        slot.upgrade().expect("pinned output owns its slot")
    }
}

/// Restores leased callbacks before reconciliation or unwinding continues.
struct Lease<R, A, O> {
    /// Keeps the restoration destination alive during synchronous handler entry.
    cell: Rc<Cell<R, A, O>>,
    /// Owns callbacks outside the declaration's dynamic borrow.
    payload: Option<OutputHandlers<R, A, O>>,
}

impl<R, A, O> Drop for Lease<R, A, O> {
    fn drop(&mut self) {
        drop({
            let mut destination = self.cell.payload.borrow_mut();
            std::mem::swap(&mut *destination, &mut self.payload);
            destination
        });
        drop(self.payload.take());
    }
}

/// Carries an admitted Local output through freezing and handler entry.
struct LocalOutputCall<R, A, O> {
    /// Avoids retaining callbacks while the command waits in the mailbox.
    cell: LocalWeak<Cell<R, A, O>>,
    /// Owns the payload and terminal sender until entry or rejection.
    request: Option<OutputRequest<O>>,
    /// Prevents cleanup from withdrawing a different published request.
    request_id: RequestId,
    /// Fixes the emitting incarnation used to select strict ancestors.
    source: ScopeToken,
    /// Restores callbacks when the call completes or unwinds.
    lease: Option<Lease<R, A, O>>,
    /// Orders eligible handlers by ancestor distance, then registration index.
    roster: Vec<(usize, usize)>,
}

impl<R, A, O> Drop for LocalOutputCall<R, A, O> {
    fn drop(&mut self) {
        if let Some(cell) = self.cell.upgrade() {
            let mut pending = cell.pending.borrow_mut();
            if pending
                .as_ref()
                .is_some_and(|(current, _)| *current == self.request_id)
            {
                *pending = None;
            }
        }
    }
}

impl<R: 'static, A: 'static, O: 'static> OutputCall<R, A, Local> for LocalOutputCall<R, A, O> {
    #[cfg(feature = "test-util")]
    fn attach_terminal(&mut self, terminal: crate::store::task_context::TerminalReceipt) {
        self.request
            .as_mut()
            .expect("pending output owns request")
            .terminal = Some(terminal);
    }

    #[cfg(feature = "test-util")]
    fn declaration_anchor(
        &self,
    ) -> <Local as crate::store::carriers::Carriers<R, A>>::OutputAnchor {
        std::rc::Weak::<Cell<R, A, O>>::clone(&self.cell)
    }

    #[cfg(feature = "test-util")]
    fn declaration(&self) -> DeclarationId {
        DeclarationId(self.cell.as_ptr() as usize)
    }

    #[cfg(feature = "test-util")]
    fn publish_pending(&self) {
        if let Some(cell) = self.cell.upgrade() {
            let request = self.request.as_ref().expect("pending output owns request");
            let mut pending = cell.pending.borrow_mut();
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
        // Local admission releases its temporary Rc before yielding. A retired
        // owner therefore cannot keep an empty cell alive for a later actor turn.
        let payload =
            { cell.payload.borrow_mut().take() }.expect("live local output owner retains handlers");
        self.lease = Some(Lease {
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
        before: Option<&mut crate::store::reaction::BeforeMutation<'_, R>>,
    ) -> Result<(LocalReply, Vec<Batch<R, A, Local>>), TaskAccessError> {
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
        Ok((LocalReply::new(reply), work))
    }
}

/// Exposes installed source metadata without retaining state or feature payloads.
pub(in crate::store) trait OutputRegistration {
    /// Returns the currently installed actor identity and mounted incarnation.
    fn installed(&self) -> Option<(Arc<()>, ScopeToken)>;

    /// Captures authority only for a current incarnation in this actor.
    fn freeze(&self, scopes: &ScopeRegistry) -> Option<FrozenRegistration> {
        freeze_registration(self.installed()?, scopes)
    }
}

/// Captures a typed Local route while the declaration still knows child types.
pub(in crate::store) trait HandlerRegistration<R, S, X, A>:
    OutputRegistration
{
    /// Returns a route only while the declaration remains installed and current.
    fn freeze_handler_route(
        &self,
        scopes: &ScopeRegistry,
    ) -> Option<Box<dyn HandlerRoute<R, A, S, X>>>;
}

/// Selects frozen child storage without requiring `Send` or `Sync`.
pub(in crate::store) trait HandlerRoute<R, A, S, X> {
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
        work: Work<crate::local::Effect<X>, crate::local::Task<S, X>>,
    ) -> Box<dyn MountedWork<R, A, Local>>;
}

/// Registers owner-local handlers before root Action and child types are erased.
///
/// Eligible handlers run from the nearest ancestor to the root. Handlers at the
/// same ancestor run in registration order. The source itself is not an ancestor.
/// [`OutputFlow::Consumed`] stops traversal. Otherwise, the next handler receives
/// the continued value without cloning it. Callbacks can capture non-`Send` values.
pub struct OutputHandlers<R, A, O> {
    /// Retains callback registration order independently of ancestor distance.
    handlers: Vec<Box<dyn OutputHandler<R, A, O, Local>>>,
}

impl<R: 'static, A: 'static, O: 'static> OutputHandlers<R, A, O> {
    /// Adds one explicit ancestor handler with no Send or Sync requirement.
    ///
    /// Foreign, retired, sibling and source registrations do not enter the
    /// captured roster. A route hidden by an earlier handler returns a scope
    /// error with the continued value instead of selecting another route.
    pub fn at<S: 'static, X: 'static>(
        &mut self,
        site: &LocalMountSite<R, S, X, A>,
        handler: impl FnMut(&mut S, O, &mut FeatureContext<'_, S, X>) -> OutputFlow<O> + 'static,
    ) {
        self.handlers.push(Box::new(ChildHandler {
            registration: site.handler_registration(),
            handler: Box::new(handler),
            route: None,
        }));
    }

    /// Adds a real root registration, not an implicit fallback.
    ///
    /// Root handlers follow all eligible child handlers. An output that no
    /// handler consumes returns [`OutputDelivery::Unhandled`].
    pub fn at_root(
        &mut self,
        handler: impl FnMut(&mut R, O, &mut FeatureContext<'_, R, A>) -> OutputFlow<O> + 'static,
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

/// Owns an actor-serial callback with no cross-thread bounds.
type Handler<S, X, O> = Box<dyn FnMut(&mut S, O, &mut FeatureContext<'_, S, X>) -> OutputFlow<O>>;

/// Holds a Local root callback and the authority captured for this traversal.
struct RootHandler<R, A, O> {
    /// Receives exclusive root state and the sole owned output value.
    handler: Handler<R, A, O>,
    /// Exists after the root qualifies as a strict ancestor of the source.
    registration: Option<FrozenRegistration>,
}

impl<R, A, O> OutputHandler<R, A, O, Local> for RootHandler<R, A, O> {
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
    ) -> Result<(OutputFlow<O>, Batch<R, A, Local>), (O, TaskAccessError)> {
        let mut context = FeatureContext::new(context);
        let flow = (self.handler)(root, value, &mut context);
        Ok((flow, Batch::Root(context.into_work())))
    }
}

/// Holds a child callback separately from its weak registration and frozen route.
struct ChildHandler<R, A, S, X, O> {
    /// Does not prolong the ancestor declaration's lifetime.
    registration: LocalWeak<dyn HandlerRegistration<R, S, X, A>>,
    /// Remains owned by this output even when its ancestor registration retires.
    handler: Handler<S, X, O>,
    /// Enters once per traversal, then transfers into any returned work.
    route: Option<Box<dyn HandlerRoute<R, A, S, X>>>,
}

impl<R: 'static, A: 'static, S: 'static, X: 'static, O> OutputHandler<R, A, O, Local>
    for ChildHandler<R, A, S, X, O>
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
    ) -> Result<(OutputFlow<O>, Batch<R, A, Local>), (O, TaskAccessError)> {
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

/// Names one weak owner-local source declaration without retaining callbacks.
///
/// A Local [`crate::local::TaskContext`] supplies the source authority for
/// delivery. Cloning this handle retains neither state, handlers nor output values.
pub struct Output<O> {
    /// Refers to the declaration without retaining its callbacks or source state.
    endpoint: LocalWeak<dyn OutputEndpoint<O>>,
}

impl<O> Clone for Output<O> {
    fn clone(&self) -> Self {
        Self {
            endpoint: LocalWeak::clone(&self.endpoint),
        }
    }
}

impl<O> fmt::Debug for Output<O> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Output").finish_non_exhaustive()
    }
}

impl<O: 'static> Output<O> {
    /// Checks source and owner ancestry before transferring a removal output.
    pub(in crate::store) fn validate_transfer(
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
    pub(in crate::store) fn deliver(
        &self,
        source: OutputSource,
        value: O,
    ) -> impl Future<Output = Result<OutputDelivery<O>, OutputError<O>>> + 'static + use<O> {
        self.deliver_request(
            source,
            value,
            #[cfg(feature = "test-util")]
            None,
        )
    }

    /// Adds removal-origin evidence without changing the delivery protocol.
    #[cfg(feature = "test-util")]
    pub(in crate::store) fn deliver_removed(
        &self,
        source: OutputSource,
        value: O,
        origin: crate::RemovalOrigin,
    ) -> impl Future<Output = Result<OutputDelivery<O>, OutputError<O>>> + 'static + use<O> {
        self.deliver_request(source, value, Some(origin))
    }

    /// Retains the value across admission and awaits its terminal delivery result.
    fn deliver_request(
        &self,
        source: OutputSource,
        value: O,
        #[cfg(feature = "test-util")] origin: Option<crate::RemovalOrigin>,
    ) -> impl Future<Output = Result<OutputDelivery<O>, OutputError<O>>> + 'static + use<O> {
        let endpoint = LocalWeak::clone(&self.endpoint);
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
}

#[cfg(feature = "test-util")]
impl<O: 'static> OutputInspection<O> for Output<O> {
    fn declaration(&self) -> DeclarationId {
        DeclarationId(self.endpoint.as_ptr().cast::<()>() as usize)
    }

    fn inspect_origin(&self) -> Option<crate::RemovalOrigin> {
        self.endpoint.upgrade()?.pending_slot().origin.clone()
    }

    fn inspect_value(&self) -> Option<ValueInspection<O>> {
        let endpoint = self.endpoint.upgrade()?;
        let slot = endpoint.pending_slot();
        drop(endpoint);
        let value = {
            slot.value
                .lock()
                .take()
                .expect("pinned output owns its value")
        };
        Some(ValueInspection {
            value: Some(value),
            slot,
        })
    }
}

/// Accepts typed requests through a weak owner-local declaration endpoint.
trait OutputEndpoint<O> {
    /// Checks whether removal may transfer output from this source and owner.
    fn validate_transfer(
        &self,
        parent: ScopeToken,
        scopes: &ScopeRegistry,
    ) -> Result<(), TaskAccessError>;

    /// Releases the temporary endpoint owner before asynchronous mailbox admission.
    fn submit(self: Rc<Self>, source: OutputSource, request: OutputRequest<O>) -> LocalFuture;

    /// Borrows the slot pinned by the test gate, not an arbitrary queued request.
    #[cfg(feature = "test-util")]
    fn pending_slot(&self) -> Arc<OutputSlot<O>>;
}
