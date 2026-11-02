//! Checked owned extraction from a declared direct child.
//!
//! Admission captures parent and child authority without retaining their state.
//! The actor validates both registrations before selecting the owning field.
//! Extraction retires the child; follow-up work belongs to the surviving parent.

mod field;
pub(in crate::store) mod root;
use field::{OptionalField, OwnedField};
#[cfg(feature = "local")]
pub(in crate::store) mod local;

use std::fmt;
use std::marker::PhantomData;
use std::sync::{Arc, Weak};

use super::carriers::{Carriers, Shared};
use super::command::Command;
use super::ingress::Ingress;
use super::mounted::{MountedApply, MountedCall};
use super::output::{HandlerRegistration, OutputRegistration, OutputSource};
use super::scope::{ScopeAuthority, ScopeRegistry, ScopeToken};
use super::task_context::{TaskAccessError, TaskAccessReason, TaskProgress, TypedSend};
use crate::{ActionTask, FeatureContext, ReduceContext, RuntimeFuture, TaskContext};

/// Borrows the still-owned child before extraction, with no asynchronous suspension.
type Before<T> = Box<dyn FnOnce(&mut T) + Send>;

/// Transfers the retired child within the surviving parent's synchronous turn.
type After<S, T, A> = Box<
    dyn for<'turn> FnOnce(&mut S, RemovedChild<'turn, T>, &mut FeatureContext<'turn, S, A>) + Send,
>;

/// Names a declared owning field without retaining its parent or child.
pub struct RemovalSite<S, T, A> {
    /// Owns selection metadata whose actor and declaration references remain weak.
    endpoint: Arc<dyn RemovalEndpoint<S, T, A>>,
}

impl<S, T, A> Clone for RemovalSite<S, T, A> {
    fn clone(&self) -> Self {
        Self {
            endpoint: Arc::clone(&self.endpoint),
        }
    }
}

impl<S, T, A> fmt::Debug for RemovalSite<S, T, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemovalSite").finish_non_exhaustive()
    }
}

/// Owns a removed child and its turn-bound transfer authority.
///
/// Only a checked owning removal creates this value. It cannot outlive the
/// synchronous parent callback; use `into_inner` to retain plain owned data.
pub struct RemovedChild<'turn, T> {
    /// Owns the exact value returned by the declared field's extraction.
    value: T,
    /// Retains historical metadata, not live child authority or its Feature.
    origin: crate::RemovalOrigin,
    /// Names the surviving parent that may author transferred output.
    parent: ScopeToken,
    /// Captures the parent's configuration and lifecycle validity for this turn.
    authority: ScopeAuthority,
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
        origin: crate::RemovalOrigin,
        parent: ScopeToken,
        authority: ScopeAuthority,
        _: &FeatureContext<'turn, S, A>,
    ) -> Self {
        Self {
            value,
            origin,
            parent,
            authority,
            turn: PhantomData,
        }
    }

    /// Borrows inert metadata captured from the live child before retirement.
    pub fn origin(&self) -> &crate::RemovalOrigin {
        &self.origin
    }

    /// Returns the child, discarding its outcome-transfer authority.
    pub fn into_inner(self) -> T {
        self.value
    }

    /// Converts this owned child once and authors ordinary parent-owned output work.
    ///
    /// Successful authoring is not delivery. Closure during conversion prevents
    /// producer registration and launch. Later closure retains ordinary parent
    /// task semantics: the public runtime driver may still receive its future,
    /// but observed closure suppresses output submission and `receive` entry.
    /// Cancellation may destroy the output without calling `receive`.
    ///
    /// # Errors
    ///
    /// Returns the unconverted removal if parent authority has ended,
    /// or the output is not sourced at that parent and owned by a surviving ancestor.
    ///
    /// # Panics
    ///
    /// Propagates converter and callback-capture destructor panics. `receive`
    /// runs later in the parent-owned task, not during this method.
    pub fn into_output<S, A, O, C, F>(
        self,
        context: &mut FeatureContext<'turn, S, A>,
        output: &super::output::Output<O>,
        convert: C,
        receive: F,
    ) -> Result<(), RemovedOutputError<'turn, T>>
    where
        S: Send + 'static,
        A: Send + 'static,
        O: Send + 'static,
        C: FnOnce(T) -> O,
        F: FnOnce(Result<super::output::OutputDelivery<O>, super::output::OutputError<O>>)
            + Send
            + 'static,
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
        // FnOnce invocation has completed, including its local/capture drops.
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
        context.task(crate::Task::run(
            move |task: TaskContext<S, A>| async move {
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
            },
        ));
        Ok(())
    }
}

/// Returns an unconverted owned child when transfer validation rejects it.
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

/// Admits owning removal while keeping root and child types out of the mailbox.
trait RemovalEndpoint<S, T, A>: Send + Sync {
    /// Retains callbacks and a terminal reply until actor entry or rejection.
    fn submit(
        self: Arc<Self>,
        parent: OutputSource,
        child: OutputSource,
        before: Before<T>,
        after: After<S, T, A>,
        request: TypedSend<()>,
    ) -> RuntimeFuture;
}

impl<S: Send + 'static, A: Send + 'static> TaskContext<S, A> {
    /// Removes exactly the captured direct child through its declared owning field.
    ///
    /// Validation precedes every selector and callback. `before` borrows the live
    /// child; `after` receives its owned value after retirement. Mutation is not
    /// rolled back if user code panics.
    ///
    /// Creating the future captures the requested incarnations but does not
    /// submit work. Its first poll starts admission. Dropping it before that poll
    /// releases its callbacks without running them; admitted work may still run.
    ///
    /// # Errors
    ///
    /// Returns rejected authority before entry, or an entered actor failure.
    ///
    /// # Panics
    ///
    /// Panics if the internal owned reply protocol is violated.
    pub fn remove_child<T, X, B, F>(
        &self,
        site: &RemovalSite<S, T, A>,
        child: &TaskContext<T, X>,
        before: B,
        after: F,
    ) -> impl Future<Output = Result<ActionTask, TaskAccessError>> + Send + 'static + use<S, A, T, X, B, F>
    where
        T: Send + 'static,
        B: FnOnce(&mut T) + Send + 'static,
        F: for<'turn> FnOnce(&mut S, RemovedChild<'turn, T>, &mut FeatureContext<'turn, S, A>)
            + Send
            + 'static,
    {
        let endpoint = Arc::clone(&site.endpoint);
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
                    super::task_context::TaskSendError::Rejected { error, .. }
                    | super::task_context::TaskSendError::Failed { error } => error,
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
    Nested(Weak<Ingress<R, A>>),
    /// Resolves a builder-created root site after that root binds to an actor.
    Root(Weak<root::RootRegistration<R, A>>),
}

impl<R, A> RemovalIngress<R, A> {
    /// Returns no actor for an unbuilt or disposed declaration.
    fn upgrade(&self) -> Option<Arc<Ingress<R, A>>> {
        match self {
            Self::Nested(ingress) => ingress.upgrade(),
            Self::Root(registration) => registration.upgrade()?.ingress().upgrade(),
        }
    }
}

/// Retains an optional field selector and weak parent/child registrations.
pub(in crate::store) fn optional<
    R: Send + 'static,
    A: Send + 'static,
    S: Send + 'static,
    T: Send + 'static,
    X: Send + 'static,
>(
    parent: Weak<dyn HandlerRegistration<R, S, X, A>>,
    child: Weak<dyn OutputRegistration>,
    ingress: Weak<Ingress<R, A>>,
    field: for<'a> fn(&'a mut S) -> &'a mut Option<T>,
) -> RemovalSite<S, T, X> {
    RemovalSite {
        endpoint: Arc::new(OptionalEndpoint {
            parent,
            child,
            ingress: RemovalIngress::Nested(ingress),
            field: OptionalField(field),
            value: PhantomData,
        }),
    }
}

/// Creates a root removal site before the builder has an actor mailbox.
pub(in crate::store) fn root_optional<R: Send + 'static, A: Send + 'static, T: Send + 'static>(
    parent: &Arc<root::RootRegistration<R, A>>,
    child: Weak<dyn OutputRegistration>,
    field: for<'a> fn(&'a mut R) -> &'a mut Option<T>,
) -> RemovalSite<R, T, A> {
    let ingress = RemovalIngress::Root(Arc::downgrade(parent));
    let parent: Arc<dyn HandlerRegistration<R, R, A, A>> =
        Arc::<root::RootRegistration<R, A>>::clone(parent);
    RemovalSite {
        endpoint: Arc::new(OptionalEndpoint {
            parent: Arc::downgrade(&parent),
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
    ingress: Weak<Ingress<R, A>>,
    write: for<'a> fn(&'a mut S) -> &'a mut crate::KeyedState<K, T>,
    key: Arc<K>,
    stamp: crate::composition::EntryStamp,
) -> RemovalSite<S, T, X>
where
    R: Send + 'static,
    A: Send + 'static,
    S: Send + 'static,
    T: Send + 'static,
    X: Send + 'static,
    K: Eq + std::hash::Hash + Send + Sync + 'static,
{
    RemovalSite {
        endpoint: Arc::new(OptionalEndpoint {
            parent,
            child,
            ingress: RemovalIngress::Nested(ingress),
            field: field::KeyedField { write, key, stamp },
            value: PhantomData,
        }),
    }
}

/// Captures a stack entry whose owning field permits only live last-entry removal.
pub(in crate::store) fn stack<R, A, S, T, X>(
    parent: Weak<dyn HandlerRegistration<R, S, X, A>>,
    child: Weak<dyn OutputRegistration>,
    ingress: Weak<Ingress<R, A>>,
    write: for<'a> fn(&'a mut S) -> &'a mut crate::StackState<T>,
    key: Arc<crate::StackId>,
    stamp: crate::composition::EntryStamp,
) -> RemovalSite<S, T, X>
where
    R: Send + 'static,
    A: Send + 'static,
    S: Send + 'static,
    T: Send + 'static,
    X: Send + 'static,
{
    RemovalSite {
        endpoint: Arc::new(OptionalEndpoint {
            parent,
            child,
            ingress: RemovalIngress::Nested(ingress),
            field: field::StackField { write, key, stamp },
            value: PhantomData,
        }),
    }
}

/// Combines a rejection reason with evidence of whether user code entered.
fn error(reason: TaskAccessReason, progress: TaskProgress) -> TaskAccessError {
    TaskAccessError::new(reason, progress)
}

/// Reports invalid authority before any owning selector or callback enters.
fn stale() -> TaskAccessError {
    error(TaskAccessReason::ScopeEnded, TaskProgress::NotStarted)
}

/// Reports an unavailable actor before the removal enters user code.
fn closed() -> TaskAccessError {
    error(TaskAccessReason::StoreClosed, TaskProgress::NotStarted)
}

/// Extracts once, retires the child and refuses transfer if retirement closed the actor.
///
/// The field checks liveness around selectors and the `before` callback. This
/// final check also covers lifecycle diagnostics invoked by child retirement.
fn take_field<S, T>(
    state: &mut S,
    field: &impl OwnedField<S, T>,
    context: &mut ReduceContext<'_>,
    child: ScopeToken,
    authority: &ScopeAuthority,
    before: impl FnOnce(&mut T),
) -> Result<T, TaskAccessReason> {
    let mut before = Some(before);
    let value = field.take(
        state,
        &mut |value| before.take().expect("owning path invokes before once")(value),
        &|| authority.admission().is_ok(),
    )?;
    context.leave(child);
    if authority.admission().is_err() {
        drop(value);
        return Err(TaskAccessReason::StoreClosed);
    }
    Ok(value)
}

/// Owns one queued extraction, preserving callback-before-reply destruction order.
struct OptionalCall<R, A, S, T, X, P> {
    /// Retains the owning selector, not parent or child state.
    endpoint: Arc<OptionalEndpoint<R, A, S, T, X, P>>,
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
    reply: super::task_context::EnteredSend<()>,
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

impl<
    R: Send + 'static,
    A: Send + 'static,
    S: Send + 'static,
    T: Send + 'static,
    X: Send + 'static,
    P: OwnedField<S, T> + Send + Sync + 'static,
> RemovalEndpoint<S, T, X> for OptionalEndpoint<R, A, S, T, X, P>
{
    fn submit(
        self: Arc<Self>,
        parent: OutputSource,
        child: OutputSource,
        before: Before<T>,
        after: After<S, T, X>,
        request: TypedSend<()>,
    ) -> RuntimeFuture {
        Box::pin(async move {
            let call = Box::new(OptionalCall {
                endpoint: Arc::clone(&self),
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
            #[cfg(all(test, feature = "test-util"))]
            ordinary_test_items! {
                super::mounted::mounted_admission_test_hook::before_ingress();
            }
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

impl<
    R: Send + 'static,
    A: Send + 'static,
    S: Send + 'static,
    T: Send + 'static,
    X: Send + 'static,
    P: OwnedField<S, T> + Send + Sync + 'static,
> MountedCall<R, A, Shared> for OptionalCall<R, A, S, T, X, P>
{
    #[cfg(feature = "test-util")]
    fn attach_terminal(&mut self, terminal: super::task_context::TerminalReceipt) {
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
        before: Option<&mut super::reaction::BeforeMutation<'_, R>>,
    ) -> MountedApply<<Shared as Carriers<R, A>>::Reply, <Shared as Carriers<R, A>>::MountedWork>
    {
        let Self {
            endpoint,
            parent,
            child,
            before: child_before,
            after,
            request,
            ..
        } = *self;
        // Input validation established this installed parent. The actor owns
        // its route throughout this callback-free prefix, even if Store closes.
        let route = endpoint
            .parent
            .upgrade()
            .and_then(|registration| registration.freeze_handler_route(context.scopes()))
            .expect("validated removal owns its parent route");
        let ((), reply) = request.enter();
        let mut entered = EnteredRemoval {
            before: Some(child_before),
            after: Some(after),
            reply,
        };
        let authority = route.registration().authority.clone();
        if before.is_some_and(|capture| !capture(root, context.scopes())) {
            return MountedApply::Rejected(entered.fail(TaskAccessReason::StoreClosed));
        }
        let Some(state) = route.select(root) else {
            return MountedApply::Rejected(entered.fail(TaskAccessReason::ScopeEnded));
        };
        if authority.admission().is_err() {
            return MountedApply::Rejected(entered.fail(TaskAccessReason::StoreClosed));
        }
        let origin = context.scopes().removal_origin(child);
        let value = match take_field(
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
        let removed = RemovedChild::new(value, origin, parent, authority, &feature);
        entered
            .after
            .take()
            .expect("entered removal owns after callback")(state, removed, &mut feature);
        let work = feature.into_work();
        MountedApply::Applied {
            reply: super::task_context::SharedReply::new(entered.reply),
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
