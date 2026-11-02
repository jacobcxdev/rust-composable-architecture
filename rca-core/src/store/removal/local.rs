//! Owner-local leaves for checked child ownership and parent-owned outcome transfer.
//!
//! Weak registrations keep the actor and its state independent of retained sites.
//! Local callbacks and removed values need no cross-thread ownership bounds.

pub(in crate::store) mod root;

use super::{
    closed, error,
    field::{OptionalField, OwnedField},
    stale,
};
use crate::local::{FeatureContext, RuntimeFuture, Task, TaskContext};
use crate::store::carriers::Carriers;
use crate::store::command::Command;
use crate::store::ingress::CommandIngress;
use crate::store::local::Local;
use crate::store::mounted::{MountedApply, MountedCall};
use crate::store::output::{
    OutputSource,
    local::{HandlerRegistration, Output, OutputRegistration},
};
use crate::store::scope::{ScopeAuthority, ScopeRegistry, ScopeToken};
use crate::store::task_context::{
    EnteredSend, TaskAccessError, TaskAccessReason, TaskProgress, TypedSend,
};
use crate::{ActionTask, ReduceContext};
use std::fmt;
use std::marker::PhantomData;
use std::rc::{Rc, Weak};
use std::sync::Weak as SharedWeak;

/// Borrows the still-owned child synchronously without a `Send` requirement.
type Before<T> = Box<dyn FnOnce(&mut T)>;

/// Transfers the retired child within the surviving parent's synchronous turn.
type After<S, T, A> =
    Box<dyn for<'turn> FnOnce(&mut S, RemovedChild<'turn, T>, &mut FeatureContext<'turn, S, A>)>;

/// Reuses typed mailbox admission while keeping commands owner-local.
type Ingress<R, A> = CommandIngress<Command<R, A, Local>>;

/// Names an owning child field without retaining its installed Feature or State.
pub struct RemovalSite<S, T, A> {
    /// Owns selection metadata whose actor and declaration references remain weak.
    endpoint: Rc<dyn RemovalEndpoint<S, T, A>>,
}

impl<S, T, A> Clone for RemovalSite<S, T, A> {
    fn clone(&self) -> Self {
        Self {
            endpoint: Rc::clone(&self.endpoint),
        }
    }
}

impl<S, T, A> fmt::Debug for RemovalSite<S, T, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemovalSite").finish_non_exhaustive()
    }
}

/// Owns a removed child and its synchronous parent-turn transfer authority.
///
/// The capability cannot escape its parent callback. Use `into_inner` to keep
/// plain owned data without retaining authority to author a removal output.
pub struct RemovedChild<'turn, T> {
    /// Owns the exact value returned by the declared field's extraction.
    value: T,
    /// Names the surviving parent that may author transferred output.
    parent: ScopeToken,
    /// Captures the parent's configuration and lifecycle validity for this turn.
    authority: ScopeAuthority,
    /// Retains historical metadata, not live child authority or its Feature.
    origin: crate::RemovalOrigin,
    /// Keeps transfer authority invariant in the synchronous callback's lifetime.
    turn: PhantomData<fn(&'turn ()) -> &'turn ()>,
}

impl<T> fmt::Debug for RemovedChild<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemovedChild").finish_non_exhaustive()
    }
}

impl<'turn, T> RemovedChild<'turn, T> {
    /// Pairs extracted ownership with the parent context that created it.
    fn new<S, A>(
        value: T,
        parent: ScopeToken,
        authority: ScopeAuthority,
        origin: crate::RemovalOrigin,
        _: &FeatureContext<'turn, S, A>,
    ) -> Self {
        Self {
            value,
            parent,
            authority,
            origin,
            turn: PhantomData,
        }
    }

    /// Borrows inert provenance for the actual retired child incarnation.
    pub fn origin(&self) -> &crate::RemovalOrigin {
        &self.origin
    }

    /// Returns the child, discarding its outcome-transfer authority.
    pub fn into_inner(self) -> T {
        self.value
    }

    /// Converts the child once and authors ordinary parent-owned output work.
    ///
    /// No `Send` bound applies to the value or callbacks. Closure during conversion
    /// suppresses producer registration; later closure preserves ordinary task
    /// launch semantics but suppresses observed-closed submission and receipt.
    /// Cancellation may destroy the output without invoking `receive`.
    ///
    /// # Errors
    ///
    /// Returns the unconverted child if parent authority or output ownership rejects it.
    ///
    /// # Panics
    ///
    /// Propagates converter and callback-capture destructor panics. `receive`
    /// runs later in the parent-owned task, not during this method.
    pub fn into_output<S, A, O, C, F>(
        self,
        context: &mut FeatureContext<'turn, S, A>,
        output: &Output<O>,
        convert: C,
        receive: F,
    ) -> Result<(), RemovedOutputError<'turn, T>>
    where
        S: 'static,
        A: 'static,
        O: 'static,
        C: FnOnce(T) -> O,
        F: FnOnce(Result<crate::OutputDelivery<O>, crate::OutputError<O>>) + 'static,
    {
        let reducer = context.reducer_context();
        // The synchronous, turn-bound removal callback owns the matching parent
        // context. Its scope cannot change, but its lifecycle can still close.
        let validation = self
            .authority
            .admission()
            .and_then(|()| output.validate_transfer(self.parent, reducer.scopes()));
        if let Err(error) = validation {
            drop((convert, receive));
            return Err(RemovedOutputError {
                removed: self,
                error,
            });
        }
        let value = convert(self.value);
        if self.authority.admission().is_err() {
            drop((value, receive));
            return Ok(());
        }
        let output = output.clone();
        let authority = self.authority;
        #[cfg(feature = "test-util")]
        let origin = self.origin;
        #[cfg(not(feature = "test-util"))]
        drop(self.origin);
        context.task(Task::run(move |task: TaskContext<S, A>| async move {
            if authority.admission().is_err() {
                return;
            }
            #[cfg(feature = "test-util")]
            let result = output
                .deliver_removed(task.removal_source(), value, origin)
                .await;
            #[cfg(not(feature = "test-util"))]
            let result = output.deliver(task.removal_source(), value).await;
            if authority.admission().is_ok() {
                receive(result);
            }
        }));
        Ok(())
    }
}

/// Recovers the unconverted child when output-transfer validation rejects it.
///
/// [`std::error::Error::source`] borrows the underlying [`TaskAccessError`]
/// without consuming the removal capability.
pub struct RemovedOutputError<'turn, T> {
    /// Preserves the unconverted value and its original turn-bound authority.
    removed: RemovedChild<'turn, T>,
    /// Explains validation failure without borrowing the owned child.
    error: TaskAccessError,
}

impl<'turn, T> RemovedOutputError<'turn, T> {
    /// Returns the original removal capability and its rejection reason.
    pub fn into_parts(self) -> (RemovedChild<'turn, T>, TaskAccessError) {
        (self.removed, self.error)
    }
}

impl<T> fmt::Debug for RemovedOutputError<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemovedOutputError")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

impl<T> fmt::Display for RemovedOutputError<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(f)
    }
}

impl<T> std::error::Error for RemovedOutputError<'_, T> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

#[cfg(test)]
ordinary_test_items! {
mod error_tests {
    use super::*;
    use crate::store::scope::{ScopeIdentity, ScopeSlot};
    use std::cell::Cell;
    use std::error::Error;

    #[test]
    fn removal_error_chain_preserves_the_owned_child() {
        struct Value<'a>(&'a Cell<usize>);

        let value = Cell::new(7);
        let (_lifecycle, mut scopes) = ScopeRegistry::for_test();
        let parent = scopes.root();
        let node = ScopeRegistry::node(&mut ScopeSlot::new());
        let child = scopes.mount(parent, node, ScopeIdentity::unit());
        let failure = RemovedOutputError {
            removed: RemovedChild {
                value: Value(&value),
                origin: scopes.removal_origin(child),
                parent,
                authority: scopes.authority(parent),
                turn: PhantomData,
            },
            error: TaskAccessError::new(TaskAccessReason::ScopeEnded, TaskProgress::NotStarted),
        };
        let source = failure
            .source()
            .expect("transfer rejection retains its cause")
            .downcast_ref::<TaskAccessError>()
            .expect("the cause retains its concrete type");
        assert!(std::ptr::eq(source, &raw const failure.error));
        assert!(source.source().is_none());
        assert_eq!(failure.to_string(), failure.error.to_string());
        let (removed, error) = failure.into_parts();
        assert!(std::ptr::eq(removed.into_inner().0, &raw const value));
        assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
        assert_eq!(error.progress(), TaskProgress::NotStarted);
    }
}
}

/// Admits owning removal without requiring transferable state or callbacks.
trait RemovalEndpoint<S, T, A> {
    /// Retains callbacks and a terminal reply until actor entry or rejection.
    fn submit(
        self: Rc<Self>,
        parent: OutputSource,
        child: OutputSource,
        before: Before<T>,
        after: After<S, T, A>,
        request: TypedSend<()>,
    ) -> RuntimeFuture;
}

impl<S: 'static, A: 'static> TaskContext<S, A> {
    /// Removes the exact direct child through its declared owning field.
    ///
    /// Validation precedes selectors and callbacks. `before` borrows the live
    /// child; `after` owns it after retirement. User mutation is not rolled back.
    ///
    /// Creating the future captures the requested incarnations but does not
    /// submit work. Its first poll starts admission. Dropping it before that poll
    /// releases its callbacks without running them; admitted work may still run.
    ///
    /// # Errors
    ///
    /// Rejects stale, foreign or closed authority, or reports entered actor failure.
    ///
    /// # Panics
    ///
    /// Panics if the internal owned-reply protocol is violated.
    pub fn remove_child<T, X, B, F>(
        &self,
        site: &RemovalSite<S, T, A>,
        child: &TaskContext<T, X>,
        before: B,
        after: F,
    ) -> impl Future<Output = Result<ActionTask, TaskAccessError>> + 'static + use<S, A, T, X, B, F>
    where
        T: 'static,
        X: 'static,
        B: FnOnce(&mut T) + 'static,
        F: for<'turn> FnOnce(&mut S, RemovedChild<'turn, T>, &mut FeatureContext<'turn, S, A>)
            + 'static,
    {
        let endpoint = Rc::clone(&site.endpoint);
        let parent = self.removal_source();
        let child = child.removal_source();
        async move {
            let (request, receive) = TypedSend::channel(());
            endpoint
                .submit(parent, child, Box::new(before), Box::new(after), request)
                .await;
            receive
                .await
                .expect("owned removal reply publishes a terminal result")
                .map_err(|error| match error {
                    crate::TaskSendError::Rejected { error, .. }
                    | crate::TaskSendError::Failed { error } => error,
                })
        }
    }
}

/// Stores one optional, keyed or stack owning path independently of live state.
struct OptionalEndpoint<R, A, S, T, X, P> {
    /// Resolves the surviving parent's current typed route during actor entry.
    parent: Weak<dyn HandlerRegistration<R, S, X, A>>,
    /// Confirms that the captured child still belongs to this exact declaration.
    child: Weak<dyn OutputRegistration>,
    /// Reaches the original actor without keeping its mailbox open.
    ingress: RemovalIngress<R, A>,
    /// Selects and extracts ownership after authority validation.
    field: P,
    /// Records the selected type without owning another value or adding bounds.
    value: PhantomData<fn() -> T>,
}

/// Separates an already-bound nested declaration from lazy root registration.
enum RemovalIngress<R, A> {
    /// Borrows the mailbox captured when the parent's recipe installed.
    Nested(SharedWeak<Ingress<R, A>>),
    /// Resolves a builder-created root site after that root binds to an actor.
    Root(Weak<root::RootRegistration<R, A>>),
}

impl<R, A> RemovalIngress<R, A> {
    /// Returns no actor for an unbuilt or disposed declaration.
    fn upgrade(&self) -> Option<std::sync::Arc<Ingress<R, A>>> {
        match self {
            Self::Nested(ingress) => ingress.upgrade(),
            Self::Root(registration) => registration.upgrade()?.ingress().upgrade(),
        }
    }
}

/// Retains an optional field selector and weak parent/child registrations.
pub(in crate::store) fn optional<R: 'static, A: 'static, S: 'static, T: 'static, X: 'static>(
    parent: Weak<dyn HandlerRegistration<R, S, X, A>>,
    child: Weak<dyn OutputRegistration>,
    ingress: SharedWeak<Ingress<R, A>>,
    field: for<'a> fn(&'a mut S) -> &'a mut Option<T>,
) -> RemovalSite<S, T, X> {
    RemovalSite {
        endpoint: Rc::new(OptionalEndpoint {
            parent,
            child,
            ingress: RemovalIngress::Nested(ingress),
            field: OptionalField(field),
            value: PhantomData,
        }),
    }
}

/// Creates a root removal site before the builder has an actor mailbox.
pub(in crate::store) fn root_optional<R: 'static, A: 'static, T: 'static>(
    parent: &Rc<root::RootRegistration<R, A>>,
    child: Weak<dyn OutputRegistration>,
    field: for<'a> fn(&'a mut R) -> &'a mut Option<T>,
) -> RemovalSite<R, T, A> {
    let ingress = RemovalIngress::Root(Rc::downgrade(parent));
    let parent: Rc<dyn HandlerRegistration<R, R, A, A>> =
        Rc::<root::RootRegistration<R, A>>::clone(parent);
    RemovalSite {
        endpoint: Rc::new(OptionalEndpoint {
            parent: Rc::downgrade(&parent),
            child,
            ingress,
            field: OptionalField(field),
            value: PhantomData,
        }),
    }
}

/// Captures the keyed entry's stamp so extraction cannot retarget a reinsertion.
pub(in crate::store) fn keyed<R, A, S, T, X, K>(
    parent: Weak<dyn HandlerRegistration<R, S, X, A>>,
    child: Weak<dyn OutputRegistration>,
    ingress: SharedWeak<Ingress<R, A>>,
    write: for<'a> fn(&'a mut S) -> &'a mut crate::KeyedState<K, T>,
    key: Rc<K>,
    stamp: crate::composition::EntryStamp,
) -> RemovalSite<S, T, X>
where
    R: 'static,
    A: 'static,
    S: 'static,
    T: 'static,
    X: 'static,
    K: Eq + std::hash::Hash + 'static,
{
    RemovalSite {
        endpoint: Rc::new(OptionalEndpoint {
            parent,
            child,
            ingress: RemovalIngress::Nested(ingress),
            field: super::field::KeyedField { write, key, stamp },
            value: PhantomData,
        }),
    }
}

/// Captures a stack entry whose owning field permits only live last-entry removal.
pub(in crate::store) fn stack<R, A, S, T, X>(
    parent: Weak<dyn HandlerRegistration<R, S, X, A>>,
    child: Weak<dyn OutputRegistration>,
    ingress: SharedWeak<Ingress<R, A>>,
    write: for<'a> fn(&'a mut S) -> &'a mut crate::StackState<T>,
    key: Rc<crate::StackId>,
    stamp: crate::composition::EntryStamp,
) -> RemovalSite<S, T, X>
where
    R: 'static,
    A: 'static,
    S: 'static,
    T: 'static,
    X: 'static,
{
    RemovalSite {
        endpoint: Rc::new(OptionalEndpoint {
            parent,
            child,
            ingress: RemovalIngress::Nested(ingress),
            field: super::field::StackField { write, key, stamp },
            value: PhantomData,
        }),
    }
}

/// Owns one queued extraction, preserving callback-before-reply destruction order.
struct OptionalCall<R, A, S, T, X, P> {
    /// Retains the owning selector, not parent or child state.
    endpoint: Rc<OptionalEndpoint<R, A, S, T, X, P>>,
    /// Fixes the surviving parent incarnation supplied by the caller.
    parent: ScopeToken,
    /// Fixes the direct child incarnation to retire.
    child: ScopeToken,
    /// Rejects a reconfigured child even if its lifetime token remains unchanged.
    child_authority: ScopeAuthority,
    /// Runs only while the selected child remains owned by its field.
    before: Before<T>,
    /// Receives ownership only after extraction and child retirement.
    after: After<S, T, X>,
    /// Publishes rejection or transfers its terminal sender on entry.
    request: TypedSend<()>,
}

/// Drops unentered callbacks before publishing an abandoned entered reply.
///
/// Field destruction order preserves this receipt contract during unwinding.
struct EnteredRemoval<S, T, A> {
    /// Remains present until the owning field invokes the live-child callback.
    before: Option<Before<T>>,
    /// Remains present until the parent receives the removed value.
    after: Option<After<S, T, A>>,
    /// Must outlive both callback captures when execution fails.
    reply: EnteredSend<()>,
}

impl<S, T, A> EnteredRemoval<S, T, A> {
    /// Disposes unused captures before publishing a failure after possible mutation.
    fn fail(self, reason: TaskAccessReason) -> TaskAccessError {
        let Self {
            before,
            after,
            reply,
        } = self;
        drop((before, after));
        let error = error(reason, TaskProgress::MayHaveRun);
        reply.fail(error);
        error
    }
}

impl<R: 'static, A: 'static, S: 'static, T: 'static, X: 'static, P: OwnedField<S, T> + 'static>
    RemovalEndpoint<S, T, X> for OptionalEndpoint<R, A, S, T, X, P>
{
    fn submit(
        self: Rc<Self>,
        parent: OutputSource,
        child: OutputSource,
        before: Before<T>,
        after: After<S, T, X>,
        request: TypedSend<()>,
    ) -> RuntimeFuture {
        Box::pin(async move {
            let call = Box::new(OptionalCall {
                endpoint: Rc::clone(&self),
                parent: parent.scope,
                child: child.scope,
                child_authority: child.authority,
                before,
                after,
                request,
            });
            if let Err(error) = parent.authority.admission() {
                call.reject(error);
                return;
            }
            if !parent.provenance.is_live() {
                call.reject(error(TaskAccessReason::Cancelled, TaskProgress::NotStarted));
                return;
            }
            // The independently retained site may belong to a disposed Store,
            // even when the supplied parent and child are still live.
            let Some(ingress) = self.ingress.upgrade() else {
                call.reject(closed());
                return;
            };
            let work = match parent.provenance.operations().try_retain_work() {
                Ok(work) => work,
                Err((reason, partial)) => {
                    call.reject(error(reason, TaskProgress::NotStarted));
                    drop(partial);
                    return;
                }
            };
            if let Err(async_channel::SendError((call, _, _, work))) = ingress
                .send_map(
                    (call, parent.provenance, parent.authority, work),
                    |(call, provenance, authority, operation_work)| Command::Mounted {
                        call,
                        provenance,
                        authority,
                        operation_work,
                    },
                )
                .await
            {
                call.reject(closed());
                drop(work);
            }
        })
    }
}

impl<R: 'static, A: 'static, S: 'static, T: 'static, X: 'static, P: OwnedField<S, T> + 'static>
    MountedCall<R, A, Local> for OptionalCall<R, A, S, T, X, P>
{
    #[cfg(feature = "test-util")]
    fn attach_terminal(&mut self, terminal: crate::store::task_context::TerminalReceipt) {
        self.request.attach_terminal(terminal);
    }

    #[cfg(feature = "test-util")]
    fn is_update(&self) -> bool {
        true
    }

    fn input_rejection(&self, scopes: &ScopeRegistry) -> Option<TaskAccessError> {
        let valid = || {
            if !scopes.accepts(&self.child_authority)
                || scopes.parent(self.child) != Some(self.parent)
            {
                return None;
            }
            let parent = self.endpoint.parent.upgrade()?.freeze(scopes)?;
            let child = self.endpoint.child.upgrade()?.freeze(scopes)?;
            (parent.scope == self.parent && child.scope == self.child).then_some(())
        };
        valid().is_none().then(stale)
    }

    fn apply(
        self: Box<Self>,
        root: &mut R,
        mut context: ReduceContext<'_>,
        before: Option<&mut crate::store::reaction::BeforeMutation<'_, R>>,
    ) -> MountedApply<<Local as Carriers<R, A>>::Reply, <Local as Carriers<R, A>>::MountedWork>
    {
        let before_mutation = before;
        let Self {
            endpoint,
            parent,
            child,
            before,
            after,
            request,
            ..
        } = *self;
        // Input validation established this installed parent; no user code runs
        // before its route is frozen in the same owner-local transaction.
        let route = endpoint
            .parent
            .upgrade()
            .and_then(|registration| registration.freeze_handler_route(context.scopes()))
            .expect("validated local removal owns its parent route");
        let ((), reply) = request.enter();
        let mut entered = EnteredRemoval {
            before: Some(before),
            after: Some(after),
            reply,
        };
        let authority = route.registration().authority.clone();
        if before_mutation.is_some_and(|capture| !capture(root, context.scopes())) {
            return MountedApply::Rejected(entered.fail(TaskAccessReason::StoreClosed));
        }
        let Some(state) = route.select(root) else {
            return MountedApply::Rejected(entered.fail(TaskAccessReason::ScopeEnded));
        };
        if authority.admission().is_err() {
            return MountedApply::Rejected(entered.fail(TaskAccessReason::StoreClosed));
        }
        let origin = context.scopes().removal_origin(child);
        let value = match super::take_field(
            state,
            &endpoint.field,
            &mut context,
            child,
            &authority,
            entered
                .before
                .take()
                .expect("entered removal owns before callback"),
        ) {
            Ok(value) => value,
            Err(reason) => return MountedApply::Rejected(entered.fail(reason)),
        };
        let mut feature = FeatureContext::new(context.in_scope(parent));
        let removed = RemovedChild::new(value, parent, authority, origin, &feature);
        entered
            .after
            .take()
            .expect("entered removal owns after callback")(state, removed, &mut feature);
        let work = feature.into_work();
        MountedApply::Applied {
            reply: crate::store::local::LocalReply::new(entered.reply),
            // Plain extraction has no follow-up work to box or register.
            work: (!work.is_empty()).then(|| route.into_work(work)),
            scope: parent,
        }
    }

    fn reject(self: Box<Self>, error: TaskAccessError) {
        let Self {
            before,
            after,
            request,
            ..
        } = *self;
        drop((before, after));
        request.reject(error);
    }
}
