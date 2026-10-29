//! Recipe-owned mappings into the current installed Feature.
//!
//! The actor owns each mapper through its recipe declaration. Sites retain only
//! weak declaration identity. Acquired ports retain frozen route metadata, not
//! the mapper or child Feature. Retirement drains captures even if a concurrent
//! acquisition or inspection temporarily retains the declaration header.
//!
//! Admission keeps the original command typed until queue capacity is reserved.
//! Actor dispatch selects the child before mapping, then checks authority again
//! before reduction. The mapper runs outside its lock. Panic unwinds the request
//! and its entered reply; this module does not resume a failed actor.

use super::{Binding, MountDeclaration, MountSite, closed, validate};
use crate::store::carriers::{Carriers, Shared};
use crate::store::command::{Command, Provenance};
use crate::store::command_port::input::{MappedInput, Request};
use crate::store::command_port::{
    CommandAcquisition, CommandAcquisitionFuture, CommandEndpoint, CommandPort, CommandSite,
    TryAdmission,
};
use crate::store::ingress::Ingress;
use crate::store::lifecycle::Lifecycle;
use crate::store::mounted::{InputClass, MountedApply, MountedCall, scope_ended};
use crate::store::scope::{ScopeAuthority, ScopeRegistry, ScopeToken, ScopeTransitionBatch};
use crate::store::task_context::{
    ReplyGuard, SharedReply, TaskAccessError, TaskProgress, TypedSend,
};
use crate::{ReduceContext, RuntimeFuture};
use parking_lot::Mutex;
use std::sync::{Arc, Weak};

/// Captures a command destination from an actor-owned parent declaration.
pub(in crate::store) trait Registration<R, A, X>: Send + Sync {
    /// Freezes the current incarnation and configuration during serial acquisition.
    fn freeze_dispatch(&self, scopes: &ScopeRegistry) -> Arc<dyn Dispatch<R, A, X>>;
}

/// Routes mapped input under one immutable child-authority capture.
pub(in crate::store) trait Dispatch<R, A, X>: Send + Sync {
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
    ) -> MountedApply<(), <Shared as Carriers<R, A>>::MountedWork>;
}

/// Separates actor-owned mapper captures from externally retainable declaration metadata.
struct Cell<R, A, C, X> {
    /// Identifies the original root and its weak admission endpoints.
    binding: Binding<R, A>,
    /// Refers to the parent declaration without extending its installed lifetime.
    source: Weak<dyn Registration<R, A, X>>,
    /// Is empty during callback execution and after explicit retirement.
    #[expect(
        clippy::type_complexity,
        reason = "The guarded optional callback retains its concrete command and action types without another abstraction."
    )]
    mapper: Mutex<Option<Box<dyn FnMut(C) -> X + Send>>>,
    /// Allows declared control input to bypass only the ordinary input gate.
    class: InputClass,
    #[cfg(feature = "test-util")]
    /// Publishes a weak request slot only while the actor holds that frontier.
    pending: Mutex<Option<(usize, Weak<crate::store::mounted::SendSlot<C>>)>>,
}

/// Drains the recipe mapper when its parent incarnation retires.
pub(super) struct Owner<R, A, C, X>(
    /// Keeps the header alive independently of the mapper's explicit retirement.
    Arc<Cell<R, A, C, X>>,
);

impl<R, A, C, X> Owner<R, A, C, X> {
    /// Removes the mapper before running its destructor outside the lock.
    fn drain(&self) {
        let mapper = self.0.mapper.lock().take();
        drop(mapper);
    }
}

impl<R, A, C, X> Drop for Owner<R, A, C, X> {
    fn drop(&mut self) {
        self.drain();
    }
}

impl<R: Send + 'static, A: Send + 'static, C: Send + 'static, X: Send + 'static>
    MountDeclaration<R, A> for Owner<R, A, C, X>
{
    // Recipe commands inherit their installed parent's immutable binding.
    fn bind(&self, _: &Arc<Ingress<R, A>>, _: &Arc<Lifecycle>) {}
    fn reconcile(
        &self,
        _: &R,
        _: &mut ScopeRegistry,
        _: &mut ScopeTransitionBatch,
        _: &mut Vec<Box<<Shared as Carriers<R, A>>::Node>>,
        _: ScopeToken,
        _: &mut Vec<Box<<Shared as Carriers<R, A>>::Mount>>,
        _: &mut super::nested::Descendants<'_, R, A, Shared>,
        #[cfg(feature = "diagnostics")] _: Option<crate::diagnostics::LifecycleCall<'_>>,
    ) {
    }
}

/// Attaches one mapper and returns weak acquisition access for external callers.
///
/// The enclosing recipe owns the returned declaration. The site cannot keep
/// mapper captures alive after that owner retires.
pub(super) fn attach<
    R: Send + 'static,
    A: Send + 'static,
    S,
    X: Send + 'static,
    C: Send + 'static,
>(
    site: &MountSite<R, S, X, A>,
    binding: &Binding<R, A>,
    mapper: impl FnMut(C) -> X + Send + 'static,
    class: InputClass,
) -> (Owner<R, A, C, X>, CommandSite<R, C>) {
    let cell = Arc::new(Cell {
        binding: binding.clone(),
        source: site.command_registration(),
        mapper: Mutex::new(Some(Box::new(mapper) as Box<dyn FnMut(C) -> X + Send>)),
        class,
        #[cfg(feature = "test-util")]
        pending: Mutex::new(None),
    });
    let acquisition = Arc::downgrade(&cell);
    (Owner(cell), CommandSite::from_declaration(acquisition))
}

impl<R: Send + 'static, A: Send + 'static, C: Send + 'static, X: Send + 'static>
    CommandAcquisition<R, C> for Cell<R, A, C, X>
{
    #[cfg(feature = "test-util")]
    fn validate(&self, root: &Arc<()>) -> Result<(), TaskAccessError> {
        validate(&self.binding, root)
    }

    fn acquire(
        self: Arc<Self>,
        root: Arc<()>,
        #[cfg(feature = "test-util")] query: Option<crate::store::command_port::AcquireSubmit>,
    ) -> CommandAcquisitionFuture<C> {
        let binding = self.binding.clone();
        let cell = Arc::downgrade(&self);
        drop(self);
        let prepare = move || {
            validate(&binding, &root)?;
            #[cfg(all(test, feature = "test-util"))]
            ordinary_test_items! {
                crate::store::mounted::mounted_admission_test_hook::before_ingress();
            }
            let ingress = binding.ingress.upgrade().ok_or_else(closed)?;
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
                .send_map(acquisition, |call| Command::<R, A>::Acquire(Box::new(call)))
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
    cell: Weak<Cell<R, A, C, X>>,
    /// Publishes exactly one result, including cancellation and unwind failures.
    reply: ReplyGuard<Option<CommandPort<C>>>,
}

impl<R: Send + 'static, A: Send + 'static, C: Send + 'static, X: Send + 'static> super::AcquireCall
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
            if cell.mapper.lock().is_none() {
                return Err(scope_ended(TaskProgress::NotStarted));
            }
            // The matching actor owns both declarations. Once the mapper is
            // live, no callback or yield can retire its parent before this
            // upgrade; foreign callers can retain only the command header.
            let source = cell
                .source
                .upgrade()
                .expect("live recipe command retains its parent declaration");
            let dispatch = source.freeze_dispatch(scopes);
            Ok(Some(CommandPort::from_endpoint(Arc::new(Endpoint {
                dispatch,
                mapper: Arc::downgrade(&cell),
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
    dispatch: Arc<dyn Dispatch<R, A, X>>,
    /// Does not retain mapper captures after declaration retirement.
    mapper: Weak<Cell<R, A, C, X>>,
    /// Preserves the declaration's ordinary or control input policy.
    class: InputClass,
}

impl<R: Send + 'static, A: Send + 'static, C: Send + 'static, X: Send + 'static> CommandEndpoint<C>
    for Endpoint<R, A, C, X>
{
    fn submit(&self, request: TypedSend<C>) -> RuntimeFuture {
        let dispatch = Arc::clone(&self.dispatch);
        let mapper = Weak::clone(&self.mapper);
        let class = self.class;
        Box::pin(async move {
            if let Err(error) = dispatch.authority().admission() {
                request.reject(error);
                return;
            }
            #[cfg(all(test, feature = "test-util"))]
            ordinary_test_items! {
                crate::store::mounted::mounted_admission_test_hook::before_ingress();
            }
            let Some(ingress) = dispatch.ingress().upgrade() else {
                request.reject(closed());
                return;
            };
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
        #[cfg(all(test, feature = "test-util"))]
        ordinary_test_items! {
            crate::store::mounted::mounted_admission_test_hook::before_ingress();
        }
        let Some(ingress) = self.dispatch.ingress().upgrade() else {
            return Err(TryAdmission::Rejected {
                request,
                error: closed(),
            });
        };
        match ingress.try_send_map(
            (
                Arc::clone(&self.dispatch),
                Weak::clone(&self.mapper),
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
        if mapper.mapper.lock().is_none() {
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
            .lock()
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
    dispatch: Arc<dyn Dispatch<R, A, X>>,
    /// Resolves the callback only after actor-side authority checks.
    mapper: Weak<Cell<R, A, C, X>>,
    /// Determines whether actor admission checks the ordinary input gate.
    class: InputClass,
}

#[cfg(feature = "test-util")]
impl<R, A, C, X> Drop for Call<R, A, C, X> {
    fn drop(&mut self) {
        if let Some(mapper) = self.mapper.upgrade() {
            // An unpublished queued call can drop before the held frontier.
            // Release the matching weak slot after leaving the publication lock.
            let request = Arc::as_ptr(self.input.slot()) as usize;
            let pending = mapper.pending.lock().take_if(|(id, _)| *id == request);
            drop(pending);
        }
    }
}

impl<R: Send + 'static, A: Send + 'static, C: Send + 'static, X: Send + 'static>
    MountedCall<R, A, Shared> for Call<R, A, C, X>
{
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
        *mapper.pending.lock() = Some((Arc::as_ptr(slot) as usize, Arc::downgrade(slot)));
    }

    fn reject(mut self: Box<Self>, error: TaskAccessError) {
        self.input.reject(error);
    }

    fn apply(
        mut self: Box<Self>,
        root: &mut R,
        context: ReduceContext<'_>,
        before: Option<&mut crate::store::reaction::BeforeMutation<'_, R>>,
    ) -> MountedApply<SharedReply, <Shared as Carriers<R, A>>::MountedWork> {
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
                // Explicit draining, not this temporary header Arc, controls capture lifetime.
                let mut callback = mapper
                    .mapper
                    .lock()
                    .take()
                    .expect("serial command dispatch owns its mapper");
                let action = callback(command);
                let mut restoring = Some(callback);
                std::mem::swap(&mut *mapper.mapper.lock(), &mut restoring);
                drop(restoring);
                action
            },
            |mapping| dispatch.apply(root, context, mapping, before),
        );
        match result {
            MountedApply::Rejected(error) => MountedApply::Rejected(error),
            MountedApply::Applied { reply, work, scope } => MountedApply::Applied {
                reply: SharedReply::new(reply),
                work,
                scope,
            },
        }
    }
}

#[cfg(test)]
ordinary_test_items! {
mod tests {
    use super::*;
    use crate::store::mount::{children, nested};
    use crate::store::transaction::{self, Mutation, tests::Host};
    use crate::{Feature, FeatureContext, MountIdentity, OptionalMount};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll, Waker};

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "One retirement timeline proves the distinction between header and mapper ownership."
    )]
    fn queued_acquisition_rejects_retired_mapper_with_a_retained_header() {
        struct Root;
        impl Feature for Root {
            type State = Option<usize>;
            type Action = ();
            fn reduce(
                &mut self,
                state: &mut Option<usize>,
                (): &(),
                _: &mut FeatureContext<'_, Option<usize>, ()>,
            ) {
                *state = None;
            }
        }
        struct Child;
        impl Feature for Child {
            type State = usize;
            type Action = u8;
            fn reduce(&mut self, _: &mut usize, _: &u8, _: &mut FeatureContext<'_, usize, u8>) {}
        }
        struct DropProbe(Arc<AtomicUsize>);
        impl Drop for DropProbe {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let descriptor = OptionalMount::new(
            Option::as_ref,
            Option::as_mut,
            |state: &Option<usize>| {
                state.map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            |_| Child,
        );
        let (parent, site) = children::attach(descriptor, |_, _| {});
        let mut host = Host::<_, _, Shared>::new(Some(7), Box::new(Root));
        parent.bind(&host.ingress, &host.lifecycle);
        host.mounts.push(parent);
        drop(transaction::commit(host.parts(), Mutation::Structural));
        let parent_scope = host.mounts[0].installed_scope().unwrap();

        let calls = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        let mapped = Arc::clone(&calls);
        let probe = DropProbe(Arc::clone(&drops));
        let binding = Binding {
            ingress: Arc::downgrade(&host.ingress),
            lifecycle: Arc::downgrade(&host.lifecycle),
        };
        let (owner, command_site) = attach(
            &site,
            &binding,
            move |input: u8| {
                let _ = &probe;
                mapped.fetch_add(1, Ordering::SeqCst);
                input
            },
            InputClass::Ordinary,
        );
        // A Shared caller may be preempted with exactly this header Arc in
        // Cell::acquire or inspect_input; it must not keep mapper captures alive.
        let retained = Arc::clone(&owner.0);
        let weak = Arc::downgrade(&retained);
        owner.bind(&host.ingress, &host.lifecycle);
        host.mounts.push(Box::new(nested::NestedOwner {
            inner: owner,
            parent: parent_scope,
            depth: 1,
        }));

        let mut cx = Context::from_waker(Waker::noop());
        let mut live = Box::pin(command_site.acquire(
            host.scopes.root_identity(),
            #[cfg(feature = "test-util")]
            None,
        ));
        assert!(live.as_mut().poll(&mut cx).is_pending());
        let Command::Acquire(call) = host.receiver.try_recv().unwrap() else {
            panic!("live acquisition entered the real mailbox");
        };
        call.execute(&host.scopes, &host.lifecycle);
        assert!(matches!(
            live.as_mut().poll(&mut cx),
            Poll::Ready(Ok(Some(_)))
        ));
        drop(live);

        // Reserve the earlier actor head, freeing its mailbox slot for an
        // acquisition that must execute only after this retirement commits.
        let (reply, mut retirement) = crate::store::command::commit_channel();
        host.ingress
            .try_send(Command::Action(
                crate::store::command::ActionCommand::external((), reply),
            ))
            .unwrap_or_else(|_| panic!("retirement enters the empty mailbox"));
        let Command::Action(retire) = host.receiver.try_recv().unwrap() else {
            panic!("retirement is the earlier actor head");
        };
        let mut acquisition = Box::pin(command_site.acquire(
            host.scopes.root_identity(),
            #[cfg(feature = "test-util")]
            None,
        ));
        assert!(acquisition.as_mut().poll(&mut cx).is_pending());
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(transaction::commit(
            host.parts(),
            Mutation::Action(retire, None),
        ));
        assert!(matches!(retirement.try_recv(), Ok(Some(Ok(_)))));
        assert_eq!(host.state, None);
        assert!(!host.scopes.is_current(parent_scope));
        assert_eq!(host.mounts.len(), 1);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(weak.upgrade().is_some());
        host.lifecycle.access().unwrap();
        let Command::Acquire(call) = host.receiver.try_recv().unwrap() else {
            panic!("queued acquisition remains behind the retiring action");
        };
        call.execute(&host.scopes, &host.lifecycle);
        let Poll::Ready(Err(error)) = acquisition.as_mut().poll(&mut cx) else {
            panic!("retired mapper must reject, not return an absent installation");
        };
        assert_eq!(error.reason(), crate::TaskAccessReason::ScopeEnded);
        assert_eq!(error.progress(), TaskProgress::NotStarted);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        drop(acquisition);
        drop(retained);
        assert!(weak.upgrade().is_none());
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
}
}
