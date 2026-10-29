//! Recipe-owned mappings into the current installed Feature.
//!
//! The owner-thread actor retains each recipe mapper. Sites keep weak declaration
//! identity, and acquired ports keep frozen route metadata. Neither owns the
//! child Feature. Retirement removes captures before their destructors run.
//!
//! Admission keeps commands typed until queue capacity is reserved. Dispatch
//! selects the child before mapping and checks authority before reduction.
//! Mapping runs outside the `RefCell` borrow. Panics propagate through the owned
//! request and reply; this module does not resume a failed actor.

use super::LocalMountSite as MountSite;
use crate::ReduceContext;
use crate::store::carriers::Carriers;
use crate::store::command::{Command, Provenance};
use crate::store::command_port::input::{MappedInput, Request};
use crate::store::command_port::local::{
    CommandAcquisition, CommandAcquisitionFuture, CommandEndpoint, CommandPort, CommandSite,
    TryAdmission,
};
use crate::store::ingress::CommandIngress;
use crate::store::lifecycle::Lifecycle;
use crate::store::local::Local;
use crate::store::local::{LocalFuture as RuntimeFuture, LocalReply};
use crate::store::mount::{Binding, MountDeclaration, closed, validate};
use crate::store::mounted::{InputClass, MountedApply, MountedCall, scope_ended};
use crate::store::scope::{ScopeAuthority, ScopeRegistry, ScopeToken, ScopeTransitionBatch};
use crate::store::task_context::{ReplyGuard, TaskAccessError, TaskProgress, TypedSend};
use std::cell::RefCell;
use std::rc::{Rc, Weak as LocalWeak};
use std::sync::{Arc, Weak};

/// Carries owner-local calls through the same root queue and admission protocol.
type Ingress<R, A> = CommandIngress<Command<R, A, Local>>;

/// Captures a command destination from an actor-owned parent declaration.
pub(in crate::store) trait Registration<R, A, X> {
    /// Freezes the current incarnation and configuration during serial acquisition.
    fn freeze_dispatch(&self, scopes: &ScopeRegistry) -> Rc<dyn Dispatch<R, A, X>>;
}

/// Routes mapped input under one immutable owner-local authority capture.
pub(in crate::store) trait Dispatch<R, A, X> {
    /// Borrows the capture checked before admission and callback entry.
    fn authority(&self) -> &ScopeAuthority;

    /// Returns weak queue access without retaining the Store owner.
    fn ingress(&self) -> Weak<Ingress<R, A>>;

    /// Identifies the child whose ordinary input gate controls this command.
    fn scope(&self) -> ScopeToken;

    /// Selects the child, maps once and returns work without publishing success.
    ///
    /// The request owner handles terminal completion. Selection can reject without
    /// consuming input. Mapping and reduction run synchronously on the actor.
    fn apply(
        &self,
        root: &mut R,
        context: ReduceContext<'_>,
        input: &mut MappedInput<'_, X>,
        before: Option<&mut crate::store::reaction::BeforeMutation<'_, R>>,
    ) -> MountedApply<(), <Local as Carriers<R, A>>::MountedWork>;
}

/// Separates actor-owned mapper captures from weak external declaration identity.
struct Cell<R, A, C, X> {
    /// Identifies the original root and its weak admission endpoints.
    binding: Binding<R, A, Local>,
    /// Refers to the parent declaration without extending its installed lifetime.
    source: LocalWeak<dyn Registration<R, A, X>>,
    /// Is empty during callback execution and after explicit retirement.
    #[expect(
        clippy::type_complexity,
        reason = "The guarded optional callback retains its concrete command and action types without another abstraction."
    )]
    mapper: RefCell<Option<Box<dyn FnMut(C) -> X>>>,
    /// Allows declared control input to bypass only the ordinary input gate.
    class: InputClass,
    #[cfg(feature = "test-util")]
    /// Publishes a weak request slot only while the actor holds that frontier.
    pending: RefCell<Option<(usize, Weak<crate::store::mounted::SendSlot<C>>)>>,
}

/// Drains the recipe mapper when its parent incarnation retires.
pub(super) struct Owner<R, A, C, X>(
    /// Retains the declaration header until actor-owned teardown completes.
    Rc<Cell<R, A, C, X>>,
);

impl<R, A, C, X> Owner<R, A, C, X> {
    /// Removes the mapper before running its destructor outside the borrow.
    fn drain(&self) {
        let mapper = self.0.mapper.borrow_mut().take();
        drop(mapper);
    }
}

impl<R, A, C, X> Drop for Owner<R, A, C, X> {
    fn drop(&mut self) {
        self.drain();
    }
}

impl<R: 'static, A: 'static, C: 'static, X: 'static> MountDeclaration<R, A, Local>
    for Owner<R, A, C, X>
{
    // Recipe commands inherit their installed parent's immutable binding.
    fn bind(&self, _: &Arc<Ingress<R, A>>, _: &Arc<Lifecycle>) {}
    fn reconcile(
        &self,
        _: &R,
        _: &mut ScopeRegistry,
        _: &mut ScopeTransitionBatch,
        _: &mut Vec<Box<<Local as Carriers<R, A>>::Node>>,
        _: ScopeToken,
        _: &mut Vec<Box<<Local as Carriers<R, A>>::Mount>>,
        _: &mut crate::store::mount::nested::Descendants<'_, R, A, Local>,
        #[cfg(feature = "diagnostics")] _: Option<crate::diagnostics::LifecycleCall<'_>>,
    ) {
    }
}

/// Attaches one owner-local mapper and returns weak external acquisition access.
///
/// The enclosing recipe owns the returned declaration. The site cannot keep
/// mapper captures alive after that owner retires.
pub(super) fn attach<R: 'static, A: 'static, S, X: 'static, C: 'static>(
    site: &MountSite<R, S, X, A>,
    binding: &Binding<R, A, Local>,
    mapper: impl FnMut(C) -> X + 'static,
    class: InputClass,
) -> (Owner<R, A, C, X>, CommandSite<R, C>) {
    let cell = Rc::new(Cell {
        binding: binding.clone(),
        source: site.command_registration(),
        mapper: RefCell::new(Some(Box::new(mapper) as Box<dyn FnMut(C) -> X>)),
        class,
        #[cfg(feature = "test-util")]
        pending: RefCell::new(None),
    });
    let acquisition = Rc::downgrade(&cell);
    (Owner(cell), CommandSite::from_declaration(acquisition))
}

impl<R: 'static, A: 'static, C: 'static, X: 'static> CommandAcquisition<R, C> for Cell<R, A, C, X> {
    #[cfg(feature = "test-util")]
    fn validate(&self, root: &Arc<()>) -> Result<(), TaskAccessError> {
        validate(&self.binding, root)
    }

    fn acquire(
        self: Rc<Self>,
        root: Arc<()>,
        #[cfg(feature = "test-util")] query: Option<
            crate::store::command_port::local::AcquireSubmit,
        >,
    ) -> CommandAcquisitionFuture<C> {
        let binding = self.binding.clone();
        let cell = Rc::downgrade(&self);
        drop(self);
        let prepare = move || {
            validate(&binding, &root)?;
            // Validation cannot yield or call user code on the owner thread.
            let ingress = binding
                .ingress
                .upgrade()
                .expect("validated owner-local command acquisition retains ingress");
            let (reply, receive) = ReplyGuard::channel();
            let acquisition = Acquire { cell, reply };
            Ok::<_, TaskAccessError>((ingress, acquisition, receive))
        };
        #[cfg(feature = "test-util")]
        if let Some(query) = query {
            return Box::pin(async move {
                let (ingress, acquisition, receive) = prepare()?;
                if let Some(call) = query(Box::new(acquisition)).await
                    && let Err(error) = ingress.send_map(call, Command::Acquire).await
                {
                    error.0.close();
                }
                receive
                    .await
                    .expect("owned command acquisition publishes terminal result")
            });
        }
        Box::pin(async move {
            let (ingress, acquisition, receive) = prepare()?;
            if let Err(error) = ingress
                .send_map(acquisition, |call| {
                    Command::<R, A, Local>::Acquire(Box::new(call))
                })
                .await
            {
                error.0.reply.complete(Err(closed()));
            }
            receive
                .await
                .expect("owned command acquisition publishes terminal result")
        })
    }
}

/// Resolves one weak declaration in actor order and owns its terminal reply.
struct Acquire<R, A, C, X> {
    /// Can expire while this acquisition waits behind earlier actor work.
    cell: LocalWeak<Cell<R, A, C, X>>,
    /// Publishes exactly one result, including cancellation and unwind failures.
    reply: ReplyGuard<Option<CommandPort<C>>>,
}

impl<R: 'static, A: 'static, C: 'static, X: 'static> crate::store::mount::AcquireCall
    for Acquire<R, A, C, X>
{
    fn execute(self: Box<Self>, scopes: &ScopeRegistry, lifecycle: &Lifecycle) {
        let result = (|| {
            lifecycle.access()?;
            let cell = self
                .cell
                .upgrade()
                .ok_or_else(|| scope_ended(TaskProgress::NotStarted))?;
            validate(&cell.binding, &lifecycle.root_identity())?;
            // At this acquisition boundary, the actor roster owns the parent.
            // Retirement runs serially; no callback or yield can drain the
            // mapper or drop that parent here.
            let source = cell
                .source
                .upgrade()
                .expect("live owner-local command retains its parent declaration");
            let dispatch = source.freeze_dispatch(scopes);
            Ok(Some(CommandPort::from_endpoint(Rc::new(Endpoint {
                dispatch,
                mapper: Rc::downgrade(&cell),
                class: cell.class,
            }))))
        })();
        self.reply.complete(result);
    }

    fn reject(self: Box<Self>, error: TaskAccessError) {
        self.reply.complete(Err(error));
    }
}

/// Captures routing authority while leaving mapper ownership with the recipe.
struct Endpoint<R, A, C, X> {
    /// Keeps this port tied to the acquired incarnation and configuration.
    dispatch: Rc<dyn Dispatch<R, A, X>>,
    /// Does not retain mapper captures after declaration retirement.
    mapper: LocalWeak<Cell<R, A, C, X>>,
    /// Preserves the declaration's ordinary or control input policy.
    class: InputClass,
}

impl<R: 'static, A: 'static, C: 'static, X: 'static> CommandEndpoint<C> for Endpoint<R, A, C, X> {
    fn submit(&self, request: TypedSend<C>) -> RuntimeFuture {
        let dispatch = Rc::clone(&self.dispatch);
        let mapper = LocalWeak::clone(&self.mapper);
        let class = self.class;
        Box::pin(async move {
            if let Err(error) = dispatch.authority().admission() {
                request.reject(error);
                return;
            }
            // No callback or yield separates admission from this owner-local
            // upgrade; the live Store still owns its ingress.
            let ingress = dispatch
                .ingress()
                .upgrade()
                .expect("admitted owner-local command retains ingress");
            let authority = dispatch.authority().clone();
            if let Err(error) = ingress
                .send_map(
                    (dispatch, mapper, request, class, authority),
                    |(dispatch, mapper, input, class, authority)| Command::Mounted {
                        call: Box::new(Call {
                            dispatch,
                            mapper,
                            input: Request::new(input),
                            class,
                        }),
                        provenance: Provenance::External { handle: None },
                        authority,
                        operation_work: crate::effect::operation::OperationWork::default(),
                    },
                )
                .await
            {
                error.0.2.reject(closed());
            }
        })
    }

    fn try_submit(&self, request: TypedSend<C>) -> Result<(), TryAdmission<C>> {
        if let Err(error) = self.dispatch.authority().admission() {
            return Err(TryAdmission::Rejected { request, error });
        }
        // Owner-local admission and ingress acquisition form one uninterrupted prefix.
        let ingress = self
            .dispatch
            .ingress()
            .upgrade()
            .expect("admitted owner-local command retains ingress");
        match ingress.try_send_map(
            (
                Rc::clone(&self.dispatch),
                LocalWeak::clone(&self.mapper),
                request,
                self.class,
                self.dispatch.authority().clone(),
            ),
            |(dispatch, mapper, input, class, authority)| Command::Mounted {
                call: Box::new(Call {
                    dispatch,
                    mapper,
                    input: Request::new(input),
                    class,
                }),
                provenance: Provenance::External { handle: None },
                authority,
                operation_work: crate::effect::operation::OperationWork::default(),
            },
        ) {
            Ok(()) => Ok(()),
            Err(async_channel::TrySendError::Full((_, _, request, _, _))) => {
                Err(TryAdmission::Full(request))
            }
            Err(async_channel::TrySendError::Closed((_, _, request, _, _))) => {
                Err(TryAdmission::Rejected {
                    request,
                    error: closed(),
                })
            }
        }
    }

    #[cfg(feature = "test-util")]
    fn inspect_input(
        &self,
        identity: crate::store::mounted::MountedInputIdentity,
        authority: &ScopeAuthority,
    ) -> Result<Option<crate::store::mounted::SendInspection<C>>, TaskAccessError> {
        self.dispatch.authority().admission()?;
        if !self.dispatch.authority().same_root(authority) {
            return Err(scope_ended(TaskProgress::NotStarted));
        }
        let mapper = self
            .mapper
            .upgrade()
            .ok_or_else(|| scope_ended(TaskProgress::NotStarted))?;
        if mapper.mapper.borrow().is_none() {
            return Err(scope_ended(TaskProgress::NotStarted));
        }
        if self.mapper.as_ptr() as usize != identity.node {
            return Ok(None);
        }
        if !self.dispatch.authority().same_capture(authority) {
            return Err(scope_ended(TaskProgress::NotStarted));
        }
        let slot = mapper
            .pending
            .borrow()
            .as_ref()
            .filter(|(request, _)| *request == identity.request)
            .and_then(|(_, slot)| slot.upgrade());
        Ok(slot.map(Request::inspect))
    }
}

/// Owns admitted input until actor rejection or one synchronous mapping attempt.
struct Call<R, A, C, X> {
    /// Precedes route fields so payload cleanup retains the routing metadata.
    input: Request<C>,
    /// Uses the exact authority captured by the submitting port.
    dispatch: Rc<dyn Dispatch<R, A, X>>,
    /// Resolves the callback only after actor-side authority checks.
    mapper: LocalWeak<Cell<R, A, C, X>>,
    /// Determines whether actor admission checks the ordinary input gate.
    class: InputClass,
}

#[cfg(feature = "test-util")]
impl<R, A, C, X> Drop for Call<R, A, C, X> {
    fn drop(&mut self) {
        if let Some(mapper) = self.mapper.upgrade() {
            // An unpublished queued call can drop before the held frontier.
            // Release the matching weak slot after leaving the publication borrow.
            let request = Arc::as_ptr(self.input.slot()) as usize;
            let pending = mapper
                .pending
                .borrow_mut()
                .take_if(|(id, _)| *id == request);
            drop(pending);
        }
    }
}

impl<R: 'static, A: 'static, C: 'static, X: 'static> MountedCall<R, A, Local> for Call<R, A, C, X> {
    #[cfg(feature = "test-util")]
    fn attach_terminal(&mut self, terminal: crate::store::task_context::TerminalReceipt) {
        self.input.attach_terminal(terminal);
    }

    fn input_rejection(&self, scopes: &ScopeRegistry) -> Option<TaskAccessError> {
        match self.class {
            InputClass::Ordinary => scopes.input_rejection(self.dispatch.scope()),
            InputClass::Control => None,
        }
    }

    #[cfg(feature = "test-util")]
    fn input_identity(&self) -> Option<crate::store::mounted::MountedInputIdentity> {
        Some(crate::store::mounted::MountedInputIdentity {
            node: self.mapper.as_ptr() as usize,
            request: Arc::as_ptr(self.input.slot()) as usize,
        })
    }

    #[cfg(feature = "test-util")]
    fn publish_input(&self) {
        // Gate selection validated authority before this callback-free prefix.
        let mapper = self
            .mapper
            .upgrade()
            .expect("selected command owns its mapper");
        let slot = self.input.slot();
        *mapper.pending.borrow_mut() = Some((Arc::as_ptr(slot) as usize, Arc::downgrade(slot)));
    }

    fn reject(mut self: Box<Self>, error: TaskAccessError) {
        self.input.reject(error);
    }

    fn apply(
        mut self: Box<Self>,
        root: &mut R,
        context: ReduceContext<'_>,
        before: Option<&mut crate::store::reaction::BeforeMutation<'_, R>>,
    ) -> MountedApply<LocalReply, <Local as Carriers<R, A>>::MountedWork> {
        // Validation established this installed incarnation. Its actor-owned
        // declaration cannot drain during the callback-free entry prefix.
        let mapper = self
            .mapper
            .upgrade()
            .expect("validated command owns its mapper");
        let Self {
            dispatch, input, ..
        } = &mut *self;
        let result = input.apply(
            |command| {
                // Explicit draining, not this temporary header Rc, controls capture lifetime.
                let mut callback = mapper
                    .mapper
                    .borrow_mut()
                    .take()
                    .expect("serial command dispatch owns its mapper");
                let action = callback(command);
                let mut restoring = Some(callback);
                std::mem::swap(&mut *mapper.mapper.borrow_mut(), &mut restoring);
                drop(restoring);
                action
            },
            |mapping| dispatch.apply(root, context, mapping, before),
        );
        match result {
            MountedApply::Rejected(error) => MountedApply::Rejected(error),
            MountedApply::Applied { reply, work, scope } => MountedApply::Applied {
                reply: LocalReply::new(reply),
                work,
                scope,
            },
        }
    }
}
