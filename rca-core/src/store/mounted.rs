//! Typed child ownership and routing into the ordinary transaction kernel.
//!
//! The actor owns each installed Feature. Escaping contexts retain weak nodes,
//! frozen authority and path metadata, never a borrow of live state. Reduction
//! leases the Feature outside its cell guard; normal returns restore it before
//! destroying input. Unwind before restoration drops the leased value instead
//! of returning it to the slot for another reduction.
//! Shared and Local routes use the same lease and deferred-program machinery.
//!
//! Ordinary reads, updates and reductions are non-pre-emptive after entry,
//! including their remaining path selectors. Structural selection and independent
//! command mapping have separate checks before later callbacks or consumption.

use super::actor::{Launches, TimerKind, WorkGroup};
use super::carriers::{Carriers, EffectLeaf, EffectOf, Execution, IntervalOf, Shared};
use super::command::{Command, CommitError, CommitSender, Provenance};
use super::command_port::{CommandEndpoint, CommandMapper, CommandPort, TryAdmission};
use super::ingress::CommandIngress;
#[cfg(feature = "local")]
use super::local;
use super::scope::{ScopeAuthority, ScopeToken};
use super::task_context::{
    Apply, CommitReply, EnteredSend, ReadCall, ReadInput, SharedReply, TaskAccessError,
    TaskAccessReason, TaskEndpoint, TaskInput, TaskProgress, TaskRequest, TypedSend, UpdateCall,
};
use super::transaction::TransactionParts;
use crate::effect::leaves::{EffectLeaves, EffectValue, SharedEffects};
use crate::effect::lineage::LineageState;
use crate::effect::operation::OperationWork;
use crate::effect::registry::{GenerationToken, PreparedTask, TaskRegistry};
use crate::feature::{Work, WorkItem};
use crate::{ActionTask, Feature, FeatureContext, ReduceContext};
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::{Arc, Weak};

pub(in crate::store) mod erased_path;
pub(in crate::store) mod path;
use path::BorrowPath;

/// Stores borrowed selectors without owning either root or selected state.
pub(crate) struct OptionalPath<R, S> {
    /// Selects the current child for reads and structural inspection.
    read: for<'a> fn(&'a R) -> Option<&'a S>,
    /// Selects mutable state at dispatch; absence rejects the entered request.
    write: for<'a> fn(&'a mut R) -> Option<&'a mut S>,
}

impl<R, S> Copy for OptionalPath<R, S> {}
impl<R, S> Clone for OptionalPath<R, S> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<R, S> OptionalPath<R, S> {
    /// Retains selectors without invoking them or imposing state ownership bounds.
    pub(super) fn new(
        read: for<'a> fn(&'a R) -> Option<&'a S>,
        write: for<'a> fn(&'a mut R) -> Option<&'a mut S>,
    ) -> Self {
        Self { read, write }
    }
}

/// Holds the actor's child Feature and its current structural identity.
///
/// Only the serial actor leases the Feature. Callbacks may enqueue work but
/// cannot re-enter reduction while its slot is empty.
struct Installed<F> {
    /// Empty during reduction and after retirement, never borrowed across callbacks.
    feature: Option<F>,
    /// Identifies this installation, independently of the selected state's address.
    token: ScopeToken,
    /// Retires older capabilities without replacing the Feature's private cache.
    configuration: u64,
}

/// Shares installed metadata while the actor retains exclusive Feature access.
struct SharedInstalled<F: Feature> {
    /// Guards extraction and restoration, not selectors, reduction or destruction.
    node: Mutex<Installed<F>>,
    #[cfg(feature = "test-util")]
    /// Weakly publishes only the currently held diagnostic input.
    pending: Mutex<Option<(usize, PendingChildSlot<F::Action>)>>,
}

impl<F: Feature> SharedInstalled<F> {
    /// Borrows metadata briefly; callers release the guard before user code.
    fn lock(&self) -> parking_lot::MutexGuard<'_, Installed<F>> {
        self.node.lock()
    }
}

#[cfg(feature = "test-util")]
/// Lets diagnostic inspection temporarily borrow the whole pending request.
pub(in crate::store) struct InputSlot<I>(
    /// Empty only while the diagnostic gate holds its inspection guard.
    Mutex<Option<I>>,
);
#[cfg(feature = "test-util")]
/// Preserves an unentered typed action and its recovery reply during inspection.
pub(in crate::store) type SendSlot<X> = InputSlot<TypedSend<X>>;
#[cfg(feature = "test-util")]
/// Exposes effect input through the same temporary inspection protocol.
type EffectSlot<X> = InputSlot<EffectInput<X>>;
#[cfg(feature = "test-util")]
/// Keeps an effect's action and commit acknowledgement under one inspection owner.
struct EffectInput<X> {
    /// Remains owned until reduction or rejection consumes the request.
    action: X,
    /// Records entry and commit for the effect sender and diagnostic observer.
    reply: CommitSender,
}
#[cfg(feature = "test-util")]
impl<I> InputSlot<I> {
    /// Stores one request before the actor publishes or enters it.
    pub(in crate::store) fn new(input: I) -> Self {
        Self(Mutex::new(Some(input)))
    }

    /// Extracts the request after any diagnostic inspection has restored it.
    pub(in crate::store) fn take(&self) -> I {
        self.0
            .lock()
            .take()
            .expect("inspection restored the request")
    }
}
#[cfg(feature = "test-util")]
impl<X> InputSlot<TypedSend<X>> {
    /// Attaches the held transaction's observer before request entry.
    pub(in crate::store) fn attach_terminal(&self, terminal: super::task_context::TerminalReceipt) {
        self.0
            .lock()
            .as_mut()
            .expect("selected input owns request")
            .attach_terminal(terminal);
    }
}
#[cfg(feature = "test-util")]
impl<X> InputSlot<EffectInput<X>> {
    /// Observes the same terminal boundary as the effect's commit reply.
    fn attach_terminal(&self, terminal: super::task_context::TerminalReceipt) {
        self.0
            .lock()
            .as_mut()
            .expect("selected input owns request")
            .reply
            .attach_terminal(terminal);
    }
}

/// The whole request, including its original reply, leaves the slot together.
#[cfg(feature = "test-util")]
struct SlotInspection<I> {
    /// Keeps the restoration destination alive without retaining the installed Feature.
    slot: Arc<InputSlot<I>>,
    /// Moves back on Drop, including after a diagnostic matcher panics.
    request: Option<I>,
}
#[cfg(feature = "test-util")]
impl<I> SlotInspection<I> {
    /// Leases input while the gate prevents actor dispatch of that request.
    fn take(slot: Arc<InputSlot<I>>) -> Self {
        // The gate holds the unique pending command until inspection restores it.
        let request = slot.take();
        Self {
            slot,
            request: Some(request),
        }
    }
}
#[cfg(feature = "test-util")]
impl<I> Drop for SlotInspection<I> {
    fn drop(&mut self) {
        let previous = std::mem::replace(&mut *self.slot.0.lock(), self.request.take());
        // Never destroy a request or publish its reply under the slot guard.
        assert!(
            previous.is_none(),
            "inspection restores an empty request slot"
        );
    }
}
#[cfg(feature = "test-util")]
/// Preserves the different reply owners behind the common action inspection API.
enum InspectionKind<X> {
    /// A typed request can still return its original action before entry.
    Typed(SlotInspection<TypedSend<X>>),
    /// An effect request owns a commit acknowledgement rather than typed recovery.
    Effect(SlotInspection<EffectInput<X>>),
}
#[cfg(feature = "test-util")]
/// Borrows an action and restores its complete request when inspection ends.
pub(crate) struct SendInspection<X>(
    /// Owns the restoring guard for the selected request kind.
    InspectionKind<X>,
);
#[cfg(feature = "test-util")]
impl<X> SendInspection<X> {
    /// Leases a typed request already selected by the diagnostic gate.
    pub(in crate::store) fn take(slot: Arc<SendSlot<X>>) -> Self {
        Self(InspectionKind::Typed(SlotInspection::take(slot)))
    }

    /// Leases an effect request without replacing its original reply.
    fn effect(slot: Arc<EffectSlot<X>>) -> Self {
        Self(InspectionKind::Effect(SlotInspection::take(slot)))
    }

    /// Borrows the action for a matcher without consuming or cloning it.
    pub(crate) fn action(&self) -> &X {
        match &self.0 {
            InspectionKind::Typed(inspection) => inspection
                .request
                .as_ref()
                .expect("inspection owns request")
                .action(),
            InspectionKind::Effect(inspection) => {
                &inspection
                    .request
                    .as_ref()
                    .expect("inspection owns request")
                    .action
            }
        }
    }
}
#[cfg(feature = "test-util")]
/// Publishes inspection access without extending the pending command's lifetime.
enum PendingChildSlot<X> {
    /// Weak ownership of a typed request's inspection slot.
    Typed(Weak<SendSlot<X>>),
    /// Weak ownership of an effect request's inspection slot.
    Effect(Weak<EffectSlot<X>>),
}
#[cfg(feature = "test-util")]
impl<X> Clone for PendingChildSlot<X> {
    fn clone(&self) -> Self {
        match self {
            Self::Typed(slot) => Self::Typed(Weak::clone(slot)),
            Self::Effect(slot) => Self::Effect(Weak::clone(slot)),
        }
    }
}
#[cfg(feature = "test-util")]
impl<X> PendingChildSlot<X> {
    /// Leases input only while its selected command still owns the slot.
    fn inspect(self) -> Option<SendInspection<X>> {
        match self {
            Self::Typed(slot) => slot.upgrade().map(SendInspection::take),
            Self::Effect(slot) => slot.upgrade().map(SendInspection::effect),
        }
    }
}
#[cfg(feature = "test-util")]
#[derive(Clone, Copy)]
/// Matches a diagnostic capability to one actor-held input publication.
pub(crate) struct MountedInputIdentity {
    /// Identifies the installed node or mapper, not a state address.
    pub(in crate::store) node: usize,
    /// Identifies the particular request slot while its command remains owned.
    pub(in crate::store) request: usize,
}
#[cfg(feature = "test-util")]
/// Validates frozen capability authority before exposing a held action.
pub(crate) trait MountedInputInspection<X> {
    /// Returns no match for another node and rejects stale or foreign authority.
    fn inspect_input(
        &self,
        identity: MountedInputIdentity,
        authority: &ScopeAuthority,
    ) -> Result<Option<SendInspection<X>>, TaskAccessError>;
}

/// Abstracts only the short metadata guard used by the shared lease algorithm.
trait NodeCell<F> {
    /// Prevents simultaneous extraction without retaining a guard across callbacks.
    type Guard<'a>: std::ops::DerefMut<Target = Installed<F>>
    where
        Self: 'a,
        F: 'a;

    /// Borrows the installation slot for extraction or restoration.
    fn borrow_node(&self) -> Self::Guard<'_>;
}
impl<F: Feature> NodeCell<F> for SharedInstalled<F> {
    type Guard<'a>
        = parking_lot::MutexGuard<'a, Installed<F>>
    where
        F: 'a;
    fn borrow_node(&self) -> Self::Guard<'_> {
        self.node.lock()
    }
}
/// Separates request entry from the child Feature's synchronous lease.
trait ChildEntry {
    /// Owned action or borrowed mapper supplied to the reducer adapter.
    type Value;
    /// Terminal owner kept alive until input and deferred work are cleaned up.
    type Reply;
    /// Marks user entry before the first selector or reaction callback.
    fn enter(self) -> (Self::Value, Self::Reply);
    /// Publishes an entered rejection after dispatch has finished cleaning up.
    fn fail(reply: Self::Reply, error: TaskAccessError);
}
impl<X> ChildEntry for ChildInput<X> {
    type Value = X;
    type Reply = ChildReply<X>;
    fn enter(self) -> (X, ChildReply<X>) {
        ChildInput::enter(self)
    }

    fn fail(reply: ChildReply<X>, error: TaskAccessError) {
        reply.fail(error);
    }
}

/// Lends mapping while the command request retains its own terminal reply.
struct BorrowedInput<'a, X>(
    /// Invoked at most once, only after the child projection succeeds.
    &'a mut super::command_port::input::MappedInput<'a, X>,
);
impl<'a, X> ChildEntry for BorrowedInput<'a, X> {
    type Value = &'a mut super::command_port::input::MappedInput<'a, X>;
    type Reply = ();
    fn enter(self) -> (Self::Value, ()) {
        (self.0, ())
    }
    // Request::apply owns the typed reply beyond this borrowed dispatch.
    fn fail((): (), _: TaskAccessError) {}
}

/// Leases a Feature outside its guard and restores it only on normal return.
///
/// Frozen authority was checked by transaction admission. Entry precedes the
/// first callback, so later rejection cannot promise input recovery. Unwind
/// destroys the leased Feature and context before publishing terminal failure.
fn apply_child_input<R, S, F, C, I: ChildEntry>(
    root: &mut R,
    input: I,
    path: &impl BorrowPath<R, State = S>,
    context: C,
    node: &impl NodeCell<F>,
    before: impl FnOnce(&R, &mut C) -> bool,
    reduce: impl FnOnce(
        &mut F,
        &mut S,
        &mut Option<I::Value>,
        &mut I::Reply,
        &mut C,
    ) -> Result<(), TaskAccessError>,
) -> Result<(I::Reply, C), TaskAccessError> {
    let mut entered;
    // Own context after the reply guards so unwind destroys captured work
    // before terminal reply delivery, with no node guard alive.
    let mut context = context;
    let mut feature = {
        let mut node = node.borrow_node();
        node.feature
            .take()
            .expect("serial actor owns the installed Feature")
    };
    // Transaction admission already validated the route's frozen authority.
    // Enter before the first selector; unwinding never restores the lease.
    let (value, reply) = input.enter();
    entered = (Some(value), reply);
    let mut closed = false;
    let mut reduction_error = None;
    let applied = if !before(root, &mut context) {
        closed = true;
        false
    } else if let Some(state) = path.write(root) {
        let (value, reply) = &mut entered;
        let result = reduce(&mut feature, state, value, reply, &mut context);
        reduction_error = result.err();
        reduction_error.is_none()
    } else {
        false
    };
    let mut restoring = Some(feature);
    drop({
        let mut node = node.borrow_node();
        // Actor serialization keeps the slot empty. Swap cannot drop user
        // data under the guard, including on the selector-None path.
        std::mem::swap(&mut node.feature, &mut restoring);
        node
    });
    drop(restoring);

    // Keep the reply guard live while input destruction can still unwind.
    // Rejection must acknowledge cleanup just as successful reduction does.
    drop(entered.0.take());
    if !applied {
        let error = if let Some(error) = reduction_error {
            error
        } else if closed {
            TaskAccessError::new(TaskAccessReason::StoreClosed, TaskProgress::MayHaveRun)
        } else {
            scope_ended(TaskProgress::MayHaveRun)
        };
        drop(context);
        let (_, reply) = entered;
        I::fail(reply, error);
        return Err(error);
    }

    let (_, reply) = entered;
    Ok((reply, context))
}

/// Adapts an owned child action to the shared lease and unwind protocol.
fn apply_child<R, S, X, F, C>(
    root: &mut R,
    input: ChildInput<X>,
    path: &impl BorrowPath<R, State = S>,
    context: C,
    node: &impl NodeCell<F>,
    before: impl FnOnce(&R, &mut C) -> bool,
    reduce: fn(&mut F, &mut S, &X, &mut C),
) -> Result<(ChildReply<X>, C), TaskAccessError> {
    apply_child_input(
        root,
        input,
        path,
        context,
        node,
        before,
        |feature, state, value, _, context| {
            reduce(
                feature,
                state,
                value.as_ref().expect("entered action"),
                context,
            );
            Ok(())
        },
    )
}

/// Lets the actor prune heterogeneous installed owners by structural lifetime.
pub(crate) trait InstalledNode {
    /// Returns the installation token without entering user code.
    fn scope(&self) -> ScopeToken;
}
/// Retains one installed Shared Feature until actor retirement or teardown.
pub(super) struct SharedNode<F: Feature> {
    /// The actor's strong ownership; escaping capabilities hold weak nodes only.
    owned: Arc<SharedInstalled<F>>,
}
/// Refers to installation metadata without retaining the child Feature.
pub(super) struct SharedWeakNode<F: Feature>(
    /// Upgraded only at serial actor boundaries or checked diagnostic inspection.
    Weak<SharedInstalled<F>>,
);
impl<F: Feature> Clone for SharedWeakNode<F> {
    fn clone(&self) -> Self {
        Self(Weak::clone(&self.0))
    }
}
impl<F: Feature> SharedWeakNode<F> {
    /// Removes the Feature before destroying it outside the metadata guard.
    pub(super) fn retire_feature(&self) {
        if let Some(node) = self.0.upgrade() {
            let feature = node.lock().feature.take();
            drop(feature);
        }
    }

    /// Updates capability generation while preserving the installed Feature.
    pub(super) fn refresh_configuration(&self, configuration: u64) {
        let node = self.0.upgrade().expect("current scope owns installed node");
        node.lock().configuration = configuration;
    }
}
impl<F: Feature> InstalledNode for SharedNode<F> {
    fn scope(&self) -> ScopeToken {
        self.owned.lock().token
    }
}
impl<F: Feature> SharedNode<F> {
    /// Creates metadata access that cannot extend the Feature's owned lifetime.
    pub(super) fn downgrade(&self) -> SharedWeakNode<F> {
        SharedWeakNode(Arc::downgrade(&self.owned))
    }

    /// Installs one already-constructed Feature under its committed identity.
    pub(super) fn new(feature: F, token: ScopeToken, configuration: u64) -> Self {
        Self {
            owned: Arc::new(SharedInstalled {
                node: Mutex::new(Installed {
                    feature: Some(feature),
                    token,
                    configuration,
                }),
                #[cfg(feature = "test-util")]
                pending: Mutex::new(None),
            }),
        }
    }
}

/// Carries one typed child request through the root transaction's admission gates.
pub(crate) trait MountedCall<R, A, C: Carriers<R, A>> {
    /// Attaches the held transaction's terminal observer before user entry.
    #[cfg(feature = "test-util")]
    fn attach_terminal(&mut self, terminal: super::task_context::TerminalReceipt);
    /// Distinguishes direct state updates from child action reduction in diagnostics.
    #[cfg(feature = "test-util")]
    fn is_update(&self) -> bool {
        false
    }

    /// Checks input policy after frozen authority, without invoking user callbacks.
    fn input_rejection(&self, scopes: &super::scope::ScopeRegistry) -> Option<TaskAccessError>;
    /// Identifies a held input that a matching typed capability may inspect.
    #[cfg(feature = "test-util")]
    fn input_identity(&self) -> Option<MountedInputIdentity> {
        None
    }

    #[cfg(feature = "test-util")]
    /// Publishes weak input access after the diagnostic gate has selected this call.
    fn publish_input(&self) {}
    /// Enters the child and returns deferred work without registering it yet.
    fn apply(
        self: Box<Self>,
        root: &mut R,
        context: ReduceContext<'_>,
        before: Option<&mut super::reaction::BeforeMutation<'_, R>>,
    ) -> MountedApply<C::Reply, C::MountedWork>;
    /// Rejects before entry, preserving original typed input when recovery is supported.
    fn reject(self: Box<Self>, error: TaskAccessError);
}
/// Separates entered mutation results from already-published rejection.
pub(crate) enum MountedApply<P, W: ?Sized> {
    /// The transaction still owns commit stamping, structural work and completion.
    Applied {
        /// Retains the pending terminal reply until the transaction commits.
        reply: P,
        /// Omitted when the child declared no effects or tasks.
        work: Option<Box<W>>,
        /// Attributes returned work to the selected child installation.
        scope: ScopeToken,
    },
    /// Input ownership and reply publication have already been resolved.
    Rejected(TaskAccessError),
}
/// Erases a child domain while retaining its typed deferred work and route.
pub(crate) trait MountedWork<R, A, C: Carriers<R, A>> {
    /// Tests whether any declared work remains after scope pruning.
    fn is_empty(&self) -> bool;
    /// Drops work whose enclosing installation or intrinsic effect scopes retired.
    fn prune_scopes(&mut self, current: &dyn Fn(ScopeToken) -> bool);
    /// Registers work in declaration order under the transaction's causal origin.
    fn register(
        self: Box<Self>,
        parts: &mut TransactionParts<'_, R, A, C>,
        origin: WorkOrigin<'_>,
        launches: &mut Launches<C::Future>,
    );
}
/// Borrows causal metadata shared by work registered during one transaction.
pub(crate) struct WorkOrigin<'a> {
    /// Attributes newly registered tasks and timers to their owning installation.
    pub(super) scope: ScopeToken,
    /// Creates the causal lineage lazily when the first asynchronous work needs it.
    pub(super) lineage: &'a mut Option<Arc<LineageState>>,
    /// Extends cancellation ancestry while traversing nested effect declarations.
    pub(super) generations: &'a mut Vec<GenerationToken>,
    /// Retains named-operation ancestry independently of cancellation keys.
    pub(super) operations: &'a crate::effect::operation::OperationAncestry,
    /// Marks the inherited prefix that cancellation isolation must preserve.
    pub(super) inherited_generations: usize,
    /// Links this work to the sequence continuation waiting for its completion.
    pub(super) group: Option<&'a Arc<WorkGroup>>,
}
impl WorkOrigin<'_> {
    /// Lends the same mutable traversal state to the next declared work item.
    pub(super) fn reborrow(&mut self) -> WorkOrigin<'_> {
        WorkOrigin {
            scope: self.scope,
            lineage: self.lineage,
            generations: self.generations,
            operations: self.operations,
            inherited_generations: self.inherited_generations,
            group: self.group,
        }
    }
}

/// Resumes a routed effect after the preceding sequence segment completes.
pub(crate) trait DeferredProgram<R, A, C: Carriers<R, A>> {
    /// Transfers the owned effect and route back into transaction compilation.
    fn resume(
        self: Box<Self>,
        parts: &mut TransactionParts<'_, R, A, C>,
        origin: WorkOrigin<'_>,
        launches: &mut Launches<C::Future>,
    );
    /// Removes retired intrinsic scopes without starting the deferred effect.
    fn prune_scopes(&mut self, current: &dyn Fn(ScopeToken) -> bool);
    /// Reports whether pruning left no program to resume.
    fn is_none(&self) -> bool;
}
/// Keeps root effects concrete while erasing heterogeneous child domains.
pub(super) enum DeferredEffect<R, A, C: Carriers<R, A>> {
    /// A root effect needs no child route or additional erased program.
    Root(EffectOf<R, A, C>),
    /// A child effect retains its route until resumed or discarded.
    Routed(Box<C::Deferred>),
}
/// Owns the remaining ordered segments of a root or child sequence.
pub(super) enum DeferredQueue<R, A, C: Carriers<R, A>> {
    /// Concrete root effects, preserving their original declaration order.
    Root(VecDeque<EffectOf<R, A, C>>),
    /// Individually routed child segments with the same root command carrier.
    Routed(VecDeque<Box<C::Deferred>>),
}
impl<R, A, C: Carriers<R, A>> DeferredQueue<R, A, C> {
    /// Reports whether the sequence has any deferred segments left.
    pub(super) fn is_empty(&self) -> bool {
        match self {
            Self::Root(q) => q.is_empty(),
            Self::Routed(q) => q.is_empty(),
        }
    }

    /// Transfers the next segment without starting or cloning it.
    pub(super) fn pop_front(&mut self) -> Option<DeferredEffect<R, A, C>> {
        match self {
            Self::Root(q) => q.pop_front().map(DeferredEffect::Root),
            Self::Routed(q) => q.pop_front().map(DeferredEffect::Routed),
        }
    }

    /// Prunes each program before removing segments that became empty.
    pub(super) fn prune_scopes(&mut self, current: &dyn Fn(ScopeToken) -> bool) {
        match self {
            Self::Root(q) => {
                for effect in q.iter_mut() {
                    effect.prune_scopes(&|scope| current(scope));
                }
                q.retain(|e| !e.is_none());
            }
            Self::Routed(q) => {
                for effect in q.iter_mut() {
                    effect.prune_scopes(current);
                }
                q.retain(|e| !e.is_none());
            }
        }
    }
}

/// Converts a due child timer into an ordinary root-queued mounted command.
pub(crate) trait RoutedTimer<R, A, C: Carriers<R, A>> {
    /// Exposes frozen child authority for the actor's pre-fire validation.
    fn authority(&self) -> &ScopeAuthority;
    /// Distinguishes a recurring interval from a consumed one-shot action.
    fn period(&self) -> Option<std::time::Duration>;
    /// Produces one action after validation; one-shot timers may fire only once.
    fn fire(&mut self, provenance: Provenance) -> Command<R, A, C>;
}
/// Retains the action-domain adapter until its timer is due or cancelled.
pub(super) enum DeferredTimer<R, A, C: Carriers<R, A>> {
    /// A root action or action-free wait needs no routed child object.
    Root(TimerKind<A, IntervalOf<R, A, C>>),
    /// A child timer converts its own action domain when the actor fires it.
    Routed(Box<C::Timer>),
}
impl<R, A, C: Carriers<R, A>> DeferredTimer<R, A, C> {
    /// Identifies an action-free wait, which has no child delivery callback.
    pub(super) fn is_wait(&self) -> bool {
        matches!(self, Self::Root(TimerKind::Wait))
    }
}
/// Adapts one action domain to the transaction's runtime and root command carrier.
pub(super) trait Delivery<X, E: EffectLeaves<X>, R, A, C: Carriers<R, A>>: Clone {
    /// Registers one prepared leaf without eagerly running its user factory.
    fn register_leaf(
        &self,
        registry: &mut TaskRegistry,
        prepared: PreparedTask,
        leaf: EffectLeaf<X, E>,
        ingress: &Weak<CommandIngress<Command<R, A, C>>>,
        provenance: Provenance,
    ) -> C::Future;
    /// Retains routing for ordered segments that must not run yet.
    fn sequence(&self, effects: VecDeque<E::Effect>) -> DeferredQueue<R, A, C>;
    /// Retains timer actions and factories without firing either.
    fn timer(&self, kind: TimerKind<X, E::Interval>) -> DeferredTimer<R, A, C>;
}
/// Uses the carrier's root operations without adding an erased child route.
#[derive(Clone, Copy)]
pub(super) struct RootDelivery;
impl<R: 'static, A: 'static, C: Execution<R, A>> Delivery<A, C::Effects, R, A, C> for RootDelivery {
    fn register_leaf(
        &self,
        registry: &mut TaskRegistry,
        prepared: PreparedTask,
        leaf: EffectLeaf<A, C::Effects>,
        ingress: &Weak<CommandIngress<Command<R, A, C>>>,
        provenance: Provenance,
    ) -> C::Future {
        C::register_effect(registry, prepared, leaf, Weak::clone(ingress), provenance)
    }

    fn sequence(&self, effects: VecDeque<EffectOf<R, A, C>>) -> DeferredQueue<R, A, C> {
        DeferredQueue::Root(effects)
    }

    fn timer(&self, kind: TimerKind<A, IntervalOf<R, A, C>>) -> DeferredTimer<R, A, C> {
        DeferredTimer::Root(kind)
    }
}

/// Defers child selection until the admitted read runs on the actor.
struct ProjectedRead<P, Q: ?Sized> {
    /// Owns the selected-value callback and its terminal reply.
    call: Box<Q>,
    /// Retains metadata only; selected state remains borrowed from the actor.
    path: P,
}
impl<R, P: BorrowPath<R>, Q: ReadInput<P::State> + ?Sized> ReadCall<R> for ProjectedRead<P, Q> {
    fn execute(mut self: Box<Self>, state: &R) {
        self.call.mark_entered();
        if let Some(state) = self.path.read(state) {
            self.call.execute(state);
        } else {
            self.call.reject(scope_ended(TaskProgress::MayHaveRun));
        }
    }

    fn reject(self: Box<Self>, error: TaskAccessError) {
        self.call.reject(error);
    }
}
/// Defers mutable child selection until the admitted update transaction enters.
struct ProjectedUpdate<P, Q: ?Sized> {
    /// Owns the update callback and later commit response.
    call: Box<Q>,
    /// Selects once without moving live child state out of the root.
    path: P,
}
impl<R, P: BorrowPath<R>, Q: UpdateCall<P::State> + ?Sized> UpdateCall<R>
    for ProjectedUpdate<P, Q>
{
    type Reply = Q::Reply;
    #[cfg(feature = "test-util")]
    fn attach_terminal(&mut self, terminal: super::task_context::TerminalReceipt) {
        self.call.attach_terminal(terminal);
    }

    fn mark_entered(&mut self) {
        self.call.mark_entered();
    }

    fn apply(mut self: Box<Self>, state: &mut R) -> Apply<Self::Reply> {
        self.call.mark_entered();
        if let Some(state) = self.path.write(state) {
            self.call.apply(state)
        } else {
            let error = scope_ended(TaskProgress::MayHaveRun);
            self.call.reject(error);
            Apply::Rejected(error)
        }
    }

    fn reject(self: Box<Self>, error: TaskAccessError) {
        self.call.reject(error);
    }
}
/// Reports lost child authority without changing the caller's known entry progress.
pub(super) fn scope_ended(progress: TaskProgress) -> TaskAccessError {
    TaskAccessError::new(TaskAccessReason::ScopeEnded, progress)
}

/// Keeps the original action with its appropriate pre-entry reply owner.
enum ChildInput<X> {
    /// Direct typed storage when diagnostic inspection is disabled.
    #[cfg(not(feature = "test-util"))]
    Typed(TypedSend<X>),
    /// Inspectable typed storage; the whole request moves into and out of its slot.
    #[cfg(feature = "test-util")]
    Typed(Arc<SendSlot<X>>),
    /// Inspectable effect storage, including its original commit acknowledgement.
    #[cfg(feature = "test-util")]
    Effect(Arc<EffectSlot<X>>),
    /// Direct effect storage without diagnostic allocation or publication.
    #[cfg(not(feature = "test-util"))]
    Effect {
        /// Consumed by reduction or destroyed when delivery ends.
        action: X,
        /// Acknowledges commit to the originating effect sender.
        reply: CommitSender,
    },
}
/// Retains terminal publication after child input can no longer be recovered.
enum ChildReply<X> {
    /// Preserves typed entry progress and the eventual causal receipt.
    Typed(EnteredSend<X>),
    /// Preserves an effect sender's commit acknowledgement.
    Effect(CommitSender),
}
impl<X> ChildInput<X> {
    /// Connects a selected diagnostic transaction to this request's terminal owner.
    #[cfg(feature = "test-util")]
    fn attach_terminal(&mut self, terminal: super::task_context::TerminalReceipt) {
        match self {
            Self::Typed(slot) => slot.attach_terminal(terminal),
            Self::Effect(slot) => slot.attach_terminal(terminal),
        }
    }

    /// Adds inspection storage only when the diagnostic feature needs it.
    fn typed(call: TypedSend<X>) -> Self {
        #[cfg(feature = "test-util")]
        {
            Self::Typed(Arc::new(SendSlot::new(call)))
        }
        #[cfg(not(feature = "test-util"))]
        {
            Self::Typed(call)
        }
    }

    /// Keeps effect input and acknowledgement together through optional inspection.
    fn effect(action: X, reply: CommitSender) -> Self {
        #[cfg(feature = "test-util")]
        {
            Self::Effect(Arc::new(EffectSlot::new(EffectInput { action, reply })))
        }
        #[cfg(not(feature = "test-util"))]
        {
            Self::Effect { action, reply }
        }
    }

    #[cfg(all(test, feature = "test-util", feature = "allocation-measurement"))]
    /// Exposes the typed inspection slot to the existing allocation assay.
    fn slot(&self) -> Option<&Arc<SendSlot<X>>> {
        match self {
            Self::Typed(slot) => Some(slot),
            Self::Effect(_) => None,
        }
    }

    #[cfg(feature = "test-util")]
    /// Identifies this request while its owning command keeps the slot alive.
    fn identity(&self) -> usize {
        match self {
            Self::Typed(slot) => Arc::as_ptr(slot) as usize,
            Self::Effect(slot) => Arc::as_ptr(slot) as usize,
        }
    }

    #[cfg(feature = "test-util")]
    /// Produces weak publication metadata without retaining the pending request.
    fn pending_slot(&self) -> (usize, PendingChildSlot<X>) {
        match self {
            Self::Typed(slot) => (
                self.identity(),
                PendingChildSlot::Typed(Arc::downgrade(slot)),
            ),
            Self::Effect(slot) => (
                self.identity(),
                PendingChildSlot::Effect(Arc::downgrade(slot)),
            ),
        }
    }

    /// Ends typed recovery before any selector, mapper or child reducer runs.
    fn enter(self) -> (X, ChildReply<X>) {
        match self {
            Self::Typed(call) => {
                #[cfg(feature = "test-util")]
                let call = call.take();
                let (action, reply) = call.enter();
                (action, ChildReply::Typed(reply))
            }
            #[cfg(feature = "test-util")]
            Self::Effect(slot) => {
                let EffectInput { action, mut reply } = slot.take();
                reply.mark_entered();
                (action, ChildReply::Effect(reply))
            }
            #[cfg(not(feature = "test-util"))]
            Self::Effect { action, reply } => (action, ChildReply::Effect(reply)),
        }
    }

    /// Rejects untouched input, returning typed actions or failing effect acknowledgement.
    fn reject(self, error: TaskAccessError) {
        match self {
            Self::Typed(call) => {
                #[cfg(feature = "test-util")]
                let call = call.take();
                call.reject(error);
            }
            #[cfg(feature = "test-util")]
            Self::Effect(slot) => {
                let EffectInput { reply, .. } = slot.take();
                reply.record_terminal(Err(error));
                let _ = reply.send(Err(effect_error(error)));
            }
            #[cfg(not(feature = "test-util"))]
            Self::Effect { reply, .. } => {
                let _ = reply.send(Err(effect_error(error)));
            }
        }
    }
}
impl<X> ChildReply<X> {
    /// Publishes entered failure without claiming the action remains recoverable.
    fn fail(self, error: TaskAccessError) {
        match self {
            Self::Typed(reply) => reply.fail(error),
            Self::Effect(reply) => {
                #[cfg(feature = "test-util")]
                reply.record_terminal(Err(error));
                let _ = reply.send(Err(effect_error(error)));
            }
        }
    }
}
/// Preserves scope/input refusal while mapping other task failures to cancellation.
fn effect_error(error: TaskAccessError) -> CommitError {
    if error.reason() == TaskAccessReason::ScopeEnded {
        CommitError::ScopeEnded
    } else if error.reason() == TaskAccessReason::InputClosed {
        CommitError::InputClosed
    } else {
        CommitError::Cancelled
    }
}
impl<X> CommitReply for ChildReply<X> {
    fn mark_committed(&mut self, revision: u64) {
        if let Self::Typed(reply) = self {
            reply.mark_committed(revision);
        }
        #[cfg(feature = "test-util")]
        if let Self::Effect(reply) = self {
            reply.mark_committed(revision);
        }
    }

    fn complete(self, task: ActionTask) {
        match self {
            Self::Typed(reply) => reply.complete(task),
            Self::Effect(reply) => {
                let _ = reply.send(Ok(task));
            }
        }
    }
}

/// Retains Shared routing metadata without retaining the Store or installed Feature.
struct SharedRoute<R, A, F: Feature, P = OptionalPath<R, <F as Feature>::State>> {
    /// Upgraded only while actor ownership or checked inspection permits access.
    node: Weak<SharedInstalled<F>>,
    /// Owns selectors and any shared ancestry metadata, not live child state.
    path: P,
    /// Identifies the captured installation lifetime.
    token: ScopeToken,
    /// Prevents inspection through a context captured before configuration refresh.
    configuration: u64,
    /// Allows admission without extending Store ownership between requests.
    ingress: Weak<CommandIngress<Command<R, A>>>,
    /// Freezes all relevant ancestor lifetimes and configuration generations.
    authority: ScopeAuthority,
}
impl<R, A, F: Feature, P: Clone> Clone for SharedRoute<R, A, F, P> {
    fn clone(&self) -> Self {
        Self {
            node: Weak::clone(&self.node),
            path: self.path.clone(),
            token: self.token,
            configuration: self.configuration,
            ingress: Weak::clone(&self.ingress),
            authority: self.authority.clone(),
        }
    }
}
/// Provides typed task access under one frozen Shared route and causal origin.
struct SharedEndpoint<R, A, F: Feature, P = OptionalPath<R, <F as Feature>::State>> {
    /// Resolves child state and input only after transaction admission.
    route: SharedRoute<R, A, F, P>,
    /// Distinguishes external requests from cancellable, causally attributed task work.
    provenance: Provenance,
}
impl<
    R: Send + 'static,
    A: Send + 'static,
    F: Feature + Send + 'static,
    P: BorrowPath<R, State = F::State> + Send + Sync + 'static,
> super::mount::commands::Dispatch<R, A, F::Action> for SharedRoute<R, A, F, P>
where
    F::State: Send + 'static,
    F::Action: Send + 'static,
{
    fn authority(&self) -> &ScopeAuthority {
        &self.authority
    }

    fn ingress(&self) -> Weak<CommandIngress<Command<R, A>>> {
        Weak::clone(&self.ingress)
    }

    fn scope(&self) -> ScopeToken {
        self.token
    }

    fn apply(
        &self,
        root: &mut R,
        context: ReduceContext<'_>,
        input: &mut super::command_port::input::MappedInput<'_, F::Action>,
        before: Option<&mut super::reaction::BeforeMutation<'_, R>>,
    ) -> MountedApply<(), <Shared as Carriers<R, A>>::MountedWork> {
        let node = self
            .node
            .upgrade()
            .expect("validated installed child has an actor owner");
        let ((), context) = match apply_child_input(
            root,
            BorrowedInput(input),
            &self.path,
            FeatureContext::new(context.in_scope(self.token)),
            node.as_ref(),
            |root, context| {
                before.is_none_or(|capture| capture(root, context.reducer_context().scopes()))
            },
            |feature, state, input, (), context| {
                self.authority.admission().map_err(|error| {
                    TaskAccessError::new(error.reason(), TaskProgress::MayHaveRun)
                })?;
                let action = input.take().expect("entered command mapping")();
                self.authority.admission().map_err(|error| {
                    TaskAccessError::new(error.reason(), TaskProgress::MayHaveRun)
                })?;
                F::update(feature, state, &action, context);
                Ok(())
            },
        ) {
            Ok(applied) => applied,
            Err(error) => return MountedApply::Rejected(error),
        };
        let work = context.into_work();
        let work: Option<Box<<Shared as Carriers<R, A>>::MountedWork>> = if work.is_empty() {
            None
        } else {
            Some(Box::new(SharedWork {
                route: self.clone(),
                work,
            }))
        };
        MountedApply::Applied {
            reply: (),
            work,
            scope: self.token,
        }
    }
}
/// Owns a queued Shared child action until rejection or synchronous reduction.
struct SharedCall<R, A, F: Feature, P = OptionalPath<R, <F as Feature>::State>> {
    /// Keeps frozen routing metadata through application and returned work construction.
    route: SharedRoute<R, A, F, P>,
    /// Owns the original action and terminal reply, including diagnostic inspection.
    input: ChildInput<F::Action>,
    #[cfg(feature = "test-util")]
    /// Removes only this request's weak publication when the call leaves the gate.
    publication: SharedPublication<F>,
}
#[cfg(feature = "test-util")]
/// Clears stale diagnostic metadata without owning its installed node or request.
struct SharedPublication<F: Feature> {
    /// Allows cleanup while actor ownership still retains the node.
    node: Weak<SharedInstalled<F>>,
    /// Prevents an older call's Drop from clearing a newer publication.
    request: usize,
}
#[cfg(feature = "test-util")]
impl<F: Feature> Drop for SharedPublication<F> {
    fn drop(&mut self) {
        if let Some(node) = self.node.upgrade() {
            let mut pending = node.pending.lock();
            if pending.as_ref().is_some_and(|(id, _)| *id == self.request) {
                pending.take();
            }
        }
    }
}
impl<R, A, F: Feature, P> SharedCall<R, A, F, P> {
    /// Couples routing and input before any diagnostic publication occurs.
    fn new(route: SharedRoute<R, A, F, P>, input: ChildInput<F::Action>) -> Self {
        Self {
            #[cfg(feature = "test-util")]
            publication: SharedPublication {
                node: Weak::clone(&route.node),
                request: input.identity(),
            },
            route,
            input,
        }
    }
}
/// Admits independent commands without retaining their actor-owned mapping callback.
struct SharedCommandEndpoint<R, A, F: Feature, C, P = OptionalPath<R, <F as Feature>::State>> {
    /// Captures the child destination and its authority.
    route: SharedRoute<R, A, F, P>,
    /// Retires with the declaration even if external ports remain alive.
    mapper: Weak<dyn CommandMapper<C, F::Action>>,
    /// Controls input-closure policy without bypassing authority or queue capacity.
    class: InputClass,
}
/// Keeps an independent command recoverable until actor-side mapping begins.
struct MappedCall<R, A, F: Feature, C, P = OptionalPath<R, <F as Feature>::State>> {
    /// Owns typed recovery and lends mapping only during synchronous dispatch.
    input: super::command_port::input::Request<C>,
    /// Selects the child before the mapper consumes the original command.
    route: SharedRoute<R, A, F, P>,
    /// Upgraded after admission while the declaration remains actor-owned.
    mapper: Weak<dyn CommandMapper<C, F::Action>>,
    /// Preserves the originating port's ordinary or control input policy.
    class: InputClass,
    #[cfg(feature = "test-util")]
    /// Identifies only this call's diagnostic publication for later cleanup.
    request: usize,
}
impl<R, A, F: Feature, C, P> MappedCall<R, A, F, C, P> {
    /// Stores an unmapped command without invoking its mapper or selecting state.
    fn new(
        route: SharedRoute<R, A, F, P>,
        input: TypedSend<C>,
        mapper: Weak<dyn CommandMapper<C, F::Action>>,
        class: InputClass,
    ) -> Self {
        let input = super::command_port::input::Request::new(input);
        Self {
            #[cfg(feature = "test-util")]
            request: Arc::as_ptr(input.slot()) as usize,
            input,
            route,
            mapper,
            class,
        }
    }
}
#[cfg(feature = "test-util")]
impl<R, A, F: Feature, C, P> Drop for MappedCall<R, A, F, C, P> {
    fn drop(&mut self) {
        if let Some(mapper) = self.mapper.upgrade() {
            mapper.clear(self.request);
        }
    }
}

#[derive(Clone, Copy)]
/// Selects input-closure policy independently of route liveness and causal work.
pub(super) enum InputClass {
    /// Rejects new input when the mounted child closes its ordinary input gate.
    Ordinary,
    /// Bypasses ordinary input closure only for this declared command.
    Control,
}
/// Retains heterogeneous Shared child work until transaction registration.
struct SharedWork<R, A, F: Feature, P = OptionalPath<R, <F as Feature>::State>> {
    /// Routes every returned action back to the captured installation.
    route: SharedRoute<R, A, F, P>,
    /// Preserves interleaved effect/task declaration order and concrete child types.
    #[expect(
        clippy::type_complexity,
        reason = "Work retains the concrete child state and action types without another erased layer"
    )]
    work: Work<crate::Effect<F::Action>, crate::Task<F::State, F::Action>>,
}
/// Freezes a selected output-handler destination before consuming its output.
struct SharedHandlerRoute<R, A, F: Feature, P> {
    /// Selects handler state and routes any work the handler declares.
    route: SharedRoute<R, A, F, P>,
    /// Keeps the same checked structural authority throughout handler application.
    registration: super::output::FrozenRegistration,
}
impl<
    R: Send + 'static,
    A: Send + 'static,
    F: Feature + Send + 'static,
    P: BorrowPath<R, State = F::State> + Send + Sync + 'static,
> super::output::HandlerRoute<R, A, F::State, F::Action> for SharedHandlerRoute<R, A, F, P>
where
    F::State: Send + 'static,
    F::Action: Send + 'static,
{
    fn registration(&self) -> &super::output::FrozenRegistration {
        &self.registration
    }

    #[cfg(all(test, feature = "test-util"))]
    fn read<'a>(&self, root: &'a R) -> Option<&'a F::State> {
        self.route.path.read(root)
    }

    fn select<'a>(&self, root: &'a mut R) -> Option<&'a mut F::State> {
        self.route.path.write(root)
    }

    fn into_work(
        self: Box<Self>,
        work: Work<crate::Effect<F::Action>, crate::Task<F::State, F::Action>>,
    ) -> Box<dyn MountedWork<R, A, Shared> + Send> {
        Box::new(SharedWork {
            route: self.route,
            work,
        })
    }
}

/// Preserves typed rejection before mapped queue admission erases the request kind.
enum EndpointCommand<R, A, C: Carriers<R, A>> {
    /// A root-shaped read or update projected from the child context.
    Task(TaskRequest<R, A, C>),
    /// A child-domain action whose Feature remains independently typed.
    Mounted(Box<C::Mounted>),
}
impl<R, A, C: Carriers<R, A>> EndpointCommand<R, A, C> {
    /// Resolves an unadmitted request after bounded transport closes.
    fn reject(self, error: TaskAccessError) {
        match self {
            Self::Task(request) => request.reject(error),
            Self::Mounted(call) => call.reject(error),
        }
    }
}

impl<F: Feature + Send + 'static> SharedWeakNode<F>
where
    F::State: Send + 'static,
    F::Action: Send + 'static,
{
    /// Captures a child route while serial acquisition owns the current node.
    pub(super) fn command_dispatch<R: Send + 'static, A: Send + 'static>(
        &self,
        path: impl BorrowPath<R, State = F::State> + Send + Sync + 'static,
        ingress: Weak<CommandIngress<Command<R, A>>>,
        scopes: &super::scope::ScopeRegistry,
    ) -> Arc<dyn super::mount::commands::Dispatch<R, A, F::Action>> {
        let node = self
            .0
            .upgrade()
            .expect("serial acquisition owns current installed node");
        let (token, configuration) = {
            let node = node.lock();
            (node.token, node.configuration)
        };
        Arc::new(SharedRoute {
            node: Weak::clone(&self.0),
            path,
            token,
            configuration,
            ingress,
            authority: scopes.authority(token),
        })
    }

    /// Freezes an output-handler route only while its installation remains current.
    #[expect(
        clippy::type_complexity,
        reason = "The erased route must preserve the exact root and child state/action pairing"
    )]
    pub(super) fn handler_route<R: Send + 'static, A: Send + 'static>(
        &self,
        path: impl BorrowPath<R, State = F::State> + Send + Sync + 'static,
        ingress: Weak<CommandIngress<Command<R, A>>>,
        scopes: &super::scope::ScopeRegistry,
    ) -> Option<Box<dyn super::output::HandlerRoute<R, A, F::State, F::Action> + Send>> {
        let node = self.0.upgrade()?;
        let (token, configuration) = {
            let node = node.lock();
            (node.token, node.configuration)
        };
        drop(node);
        if !scopes.is_current(token) {
            return None;
        }
        let authority = scopes.authority(token);
        Some(Box::new(SharedHandlerRoute {
            registration: super::output::FrozenRegistration {
                scope: token,
                authority: authority.clone(),
            },
            route: SharedRoute {
                node: Weak::clone(&self.0),
                path,
                token,
                configuration,
                ingress,
                authority,
            },
        }))
    }

    /// Captures a weak mapper and destination without retaining either owner.
    pub(super) fn command_port<R: Send + 'static, A: Send + 'static, C: Send + 'static>(
        &self,
        path: impl BorrowPath<R, State = F::State> + Send + Sync + 'static,
        ingress: Weak<CommandIngress<Command<R, A>>>,
        scopes: &super::scope::ScopeRegistry,
        mapper: Weak<dyn CommandMapper<C, F::Action>>,
        class: InputClass,
    ) -> CommandPort<C> {
        let node = self
            .0
            .upgrade()
            .expect("serial acquisition owns current installed node");
        let (token, configuration) = {
            let node = node.lock();
            (node.token, node.configuration)
        };
        let authority = scopes.authority(token);
        drop(node);
        CommandPort::from_endpoint(Arc::new(SharedCommandEndpoint {
            route: SharedRoute {
                node: Weak::clone(&self.0),
                path,
                token,
                configuration,
                ingress,
                authority,
            },
            mapper,
            class,
        }))
    }

    /// Routes one initial task under the structural transaction's frozen authority.
    pub(super) fn initial_work<R: Send + 'static, A: Send + 'static>(
        &self,
        path: impl BorrowPath<R, State = F::State> + Send + Sync + 'static,
        ingress: Weak<CommandIngress<Command<R, A>>>,
        authority: ScopeAuthority,
        task: crate::Task<F::State, F::Action>,
    ) -> Box<dyn MountedWork<R, A, Shared> + Send> {
        let node = self
            .0
            .upgrade()
            .expect("frozen initial roster owns installed node");
        let (token, configuration) = {
            let node = node.lock();
            (node.token, node.configuration)
        };
        drop(node);
        let mut work = Work::default();
        work.push(WorkItem::Task(task));
        Box::new(SharedWork {
            route: SharedRoute {
                node: Weak::clone(&self.0),
                path,
                token,
                configuration,
                ingress,
                authority,
            },
            work,
        })
    }

    /// Captures an external typed context at the actor's current configuration.
    pub(super) fn context<R: Send + 'static, A: Send + 'static>(
        &self,
        path: impl BorrowPath<R, State = F::State> + Send + Sync + 'static,
        ingress: Weak<CommandIngress<Command<R, A>>>,
        scopes: &super::scope::ScopeRegistry,
    ) -> crate::TaskContext<F::State, F::Action> {
        let node = self
            .0
            .upgrade()
            .expect("serial acquisition owns current installed node");
        let (token, configuration) = {
            let node = node.lock();
            (node.token, node.configuration)
        };
        let authority = scopes.authority(token);
        drop(node);
        crate::TaskContext::from_endpoint(Arc::new(SharedEndpoint {
            route: SharedRoute {
                node: Weak::clone(&self.0),
                path,
                token,
                configuration,
                ingress,
                authority,
            },
            provenance: Provenance::External { handle: None },
        }))
    }
}
impl<
    R: Send + 'static,
    A: Send + 'static,
    F: Feature + Send + 'static,
    P: BorrowPath<R, State = F::State> + Send + Sync + 'static,
> TaskEndpoint<F::State, F::Action> for SharedEndpoint<R, A, F, P>
where
    F::State: Send + 'static,
    F::Action: Send + 'static,
{
    #[cfg(feature = "test-util")]
    fn inspect_input(
        &self,
        identity: MountedInputIdentity,
        authority: &ScopeAuthority,
    ) -> Result<Option<SendInspection<F::Action>>, TaskAccessError> {
        self.route.authority.admission()?;
        if !self.route.authority.same_root(authority) {
            return Err(scope_ended(TaskProgress::NotStarted));
        }
        let node = self
            .route
            .node
            .upgrade()
            .ok_or_else(|| scope_ended(TaskProgress::NotStarted))?;
        {
            let installed = node.lock();
            if (installed.token, installed.configuration)
                != (self.route.token, self.route.configuration)
            {
                return Err(scope_ended(TaskProgress::NotStarted));
            }
        }
        if self.route.node.as_ptr() as usize != identity.node {
            return Ok(None);
        }
        if !self.route.authority.same_capture(authority) {
            return Err(scope_ended(TaskProgress::NotStarted));
        }
        let slot = node
            .pending
            .lock()
            .as_ref()
            .filter(|(request, _)| *request == identity.request)
            .map(|(_, slot)| slot.clone());
        Ok(slot.and_then(PendingChildSlot::inspect))
    }

    fn output_source(&self) -> super::output::OutputSource {
        super::output::OutputSource {
            scope: self.route.token,
            provenance: self.provenance.clone(),
            authority: self.route.authority.clone(),
        }
    }

    fn operation(&self, identity: crate::effect::CancellationIdentity) -> crate::Operation {
        crate::Operation::from_frozen_identity(
            Weak::clone(&self.route.ingress),
            self.provenance.clone(),
            self.route.token,
            self.route.authority.clone(),
            identity,
        )
    }

    fn submit(&self, request: TaskInput<F::State, F::Action>) -> crate::RuntimeFuture {
        let route = self.route.clone();
        let provenance = self.provenance.clone();
        Box::pin(async move {
            if let Err(error) = route.authority.admission() {
                request.reject(error);
                return;
            }
            if !provenance.is_live() {
                request.reject(TaskAccessError::new(
                    TaskAccessReason::Cancelled,
                    TaskProgress::NotStarted,
                ));
                return;
            }
            #[cfg(all(test, feature = "test-util"))]
            ordinary_test_items! {
                mounted_admission_test_hook::before_ingress();
            }
            let Some(ingress) = route.ingress.upgrade() else {
                request.reject(TaskAccessError::new(
                    TaskAccessReason::StoreClosed,
                    TaskProgress::NotStarted,
                ));
                return;
            };
            let operation_work = match provenance.operations().try_retain_work() {
                Ok(work) => work,
                Err((reason, partial)) => {
                    request.reject(TaskAccessError::new(reason, TaskProgress::NotStarted));
                    drop(partial);
                    return;
                }
            };
            #[cfg(all(test, feature = "test-util"))]
            ordinary_test_items! {
                mounted_admission_test_hook::after_retain();
            }
            if !provenance.is_live() {
                request.reject(TaskAccessError::new(
                    TaskAccessReason::Cancelled,
                    TaskProgress::NotStarted,
                ));
                drop(operation_work);
                return;
            }
            let authority = route.authority.clone();
            let command =
                match request {
                    TaskRequest::Read(call) => EndpointCommand::<R, A, Shared>::Task(
                        TaskRequest::<R, A>::Read(Box::new(ProjectedRead {
                            call,
                            path: route.path,
                        })),
                    ),
                    TaskRequest::Update(call) => EndpointCommand::<R, A, Shared>::Task(
                        TaskRequest::<R, A>::Update(Box::new(ProjectedUpdate {
                            call,
                            path: route.path,
                        })),
                    ),
                    TaskRequest::Send(input) => EndpointCommand::<R, A, Shared>::Mounted(Box::new(
                        SharedCall::new(route, ChildInput::typed(input)),
                    )
                        as Box<<Shared as Carriers<R, A>>::Mounted>),
                };
            if let Err(async_channel::SendError((command, _, _, operation_work))) = ingress
                .send_map(
                    (command, provenance, authority, operation_work),
                    |(command, provenance, authority, operation_work)| match command {
                        EndpointCommand::Task(request) => Command::Task {
                            request,
                            provenance,
                            authority,
                            operation_work,
                        },
                        EndpointCommand::Mounted(call) => Command::Mounted {
                            call,
                            provenance,
                            authority,
                            operation_work,
                        },
                    },
                )
                .await
            {
                command.reject(TaskAccessError::new(
                    TaskAccessReason::StoreClosed,
                    TaskProgress::NotStarted,
                ));
                drop(operation_work);
            }
        })
    }
}
impl<
    R: Send + 'static,
    A: Send + 'static,
    F: Feature + Send + 'static,
    C: Send + 'static,
    P: BorrowPath<R, State = F::State> + Send + Sync + 'static,
> CommandEndpoint<C> for SharedCommandEndpoint<R, A, F, C, P>
where
    F::State: Send + 'static,
    F::Action: Send + 'static,
{
    #[cfg(feature = "test-util")]
    fn inspect_input(
        &self,
        identity: MountedInputIdentity,
        authority: &ScopeAuthority,
    ) -> Result<Option<SendInspection<C>>, TaskAccessError> {
        self.route.authority.admission()?;
        if !self.route.authority.same_root(authority) {
            return Err(scope_ended(TaskProgress::NotStarted));
        }
        if self.mapper.as_ptr().cast::<()>() as usize != identity.node {
            return Ok(None);
        }
        if !self.route.authority.same_capture(authority) {
            return Err(scope_ended(TaskProgress::NotStarted));
        }
        #[cfg(all(test, feature = "test-util"))]
        ordinary_test_items! {
            mounted_admission_test_hook::before_mapper();
        }
        let mapper = self
            .mapper
            .upgrade()
            .ok_or_else(|| scope_ended(TaskProgress::NotStarted))?;
        Ok(mapper.inspect(identity.request))
    }

    fn submit(&self, request: TypedSend<C>) -> crate::RuntimeFuture {
        let route = self.route.clone();
        let mapper = Weak::clone(&self.mapper);
        let class = self.class;
        Box::pin(async move {
            if let Err(error) = route.authority.admission() {
                request.reject(error);
                return;
            }
            #[cfg(all(test, feature = "test-util"))]
            ordinary_test_items! {
                mounted_admission_test_hook::before_ingress();
            }
            let Some(ingress) = route.ingress.upgrade() else {
                request.reject(TaskAccessError::new(
                    TaskAccessReason::StoreClosed,
                    TaskProgress::NotStarted,
                ));
                return;
            };
            let authority = route.authority.clone();
            if let Err(async_channel::SendError((_, request, _, _, _))) = ingress
                .send_map(
                    (route, request, mapper, authority, class),
                    |(route, input, mapper, authority, class)| Command::Mounted {
                        call: Box::new(MappedCall::new(route, input, mapper, class)),
                        provenance: Provenance::External { handle: None },
                        authority,
                        operation_work: OperationWork::default(),
                    },
                )
                .await
            {
                request.reject(TaskAccessError::new(
                    TaskAccessReason::StoreClosed,
                    TaskProgress::NotStarted,
                ));
            }
        })
    }

    fn try_submit(&self, request: TypedSend<C>) -> Result<(), TryAdmission<C>> {
        let class = self.class;
        if let Err(error) = self.route.authority.admission() {
            return Err(TryAdmission::Rejected { request, error });
        }
        #[cfg(all(test, feature = "test-util"))]
        ordinary_test_items! {
            mounted_admission_test_hook::before_ingress();
        }
        let Some(ingress) = self.route.ingress.upgrade() else {
            return Err(TryAdmission::Rejected {
                request,
                error: TaskAccessError::new(
                    TaskAccessReason::StoreClosed,
                    TaskProgress::NotStarted,
                ),
            });
        };
        #[cfg(all(test, feature = "test-util"))]
        ordinary_test_items! {
            mounted_admission_test_hook::after_ingress();
        }
        match ingress.try_send_map(
            (
                self.route.clone(),
                request,
                Weak::clone(&self.mapper),
                self.route.authority.clone(),
                class,
            ),
            |(route, input, mapper, authority, class)| Command::Mounted {
                call: Box::new(MappedCall::new(route, input, mapper, class)),
                provenance: Provenance::External { handle: None },
                authority,
                operation_work: OperationWork::default(),
            },
        ) {
            Ok(()) => Ok(()),
            Err(async_channel::TrySendError::Full((_, request, _, _, _))) => {
                Err(TryAdmission::Full(request))
            }
            Err(async_channel::TrySendError::Closed((_, request, _, _, _))) => {
                Err(TryAdmission::Rejected {
                    request,
                    error: TaskAccessError::new(
                        TaskAccessReason::StoreClosed,
                        TaskProgress::NotStarted,
                    ),
                })
            }
        }
    }
}

impl<
    R: Send + 'static,
    A: Send + 'static,
    F: Feature + Send + 'static,
    C: Send + 'static,
    P: BorrowPath<R, State = F::State> + Send + Sync + 'static,
> MountedCall<R, A, Shared> for MappedCall<R, A, F, C, P>
where
    F::State: Send + 'static,
    F::Action: Send + 'static,
{
    #[cfg(feature = "test-util")]
    fn attach_terminal(&mut self, terminal: super::task_context::TerminalReceipt) {
        self.input.attach_terminal(terminal);
    }

    fn input_rejection(&self, scopes: &super::scope::ScopeRegistry) -> Option<TaskAccessError> {
        match self.class {
            InputClass::Ordinary => scopes.input_rejection(self.route.token),
            InputClass::Control => None,
        }
    }

    #[cfg(feature = "test-util")]
    fn input_identity(&self) -> Option<MountedInputIdentity> {
        Some(MountedInputIdentity {
            node: self.mapper.as_ptr().cast::<()>() as usize,
            request: self.request,
        })
    }

    #[cfg(feature = "test-util")]
    fn publish_input(&self) {
        // Gate selection validated authority before this callback-free prefix.
        self.mapper
            .upgrade()
            .expect("selected command owns its mapper")
            .publish(self.input.slot());
    }

    fn reject(mut self: Box<Self>, error: TaskAccessError) {
        self.input.reject(error);
    }

    fn apply(
        mut self: Box<Self>,
        root: &mut R,
        context: ReduceContext<'_>,
        before: Option<&mut super::reaction::BeforeMutation<'_, R>>,
    ) -> MountedApply<SharedReply, <Shared as Carriers<R, A>>::MountedWork> {
        // The validated declaration remains actor-owned throughout dispatch;
        // selectors may close authority but cannot retire this actor's roster.
        let mapper = self
            .mapper
            .upgrade()
            .expect("validated command owns its mapper");
        let Self { route, input, .. } = &mut *self;
        let result = input.apply(
            |command| mapper.map(command),
            |mapping| {
                super::mount::commands::Dispatch::apply(route, root, context, mapping, before)
            },
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

impl<
    R: Send + 'static,
    A: Send + 'static,
    F: Feature + Send + 'static,
    P: BorrowPath<R, State = F::State> + Send + Sync + 'static,
> MountedCall<R, A, Shared> for SharedCall<R, A, F, P>
where
    F::State: Send + 'static,
    F::Action: Send + 'static,
{
    #[cfg(feature = "test-util")]
    fn attach_terminal(&mut self, terminal: super::task_context::TerminalReceipt) {
        self.input.attach_terminal(terminal);
    }

    fn input_rejection(&self, scopes: &super::scope::ScopeRegistry) -> Option<TaskAccessError> {
        scopes.input_rejection(self.route.token)
    }

    #[cfg(feature = "test-util")]
    fn input_identity(&self) -> Option<MountedInputIdentity> {
        Some(MountedInputIdentity {
            node: self.route.node.as_ptr() as usize,
            request: self.input.identity(),
        })
    }

    #[cfg(feature = "test-util")]
    fn publish_input(&self) {
        // Gate selection validates authority while the actor owns this node.
        let node = self
            .route
            .node
            .upgrade()
            .expect("selected child remains actor-owned during publication");
        *node.pending.lock() = Some(self.input.pending_slot());
    }

    fn reject(self: Box<Self>, error: TaskAccessError) {
        self.input.reject(error);
    }

    fn apply(
        self: Box<Self>,
        root: &mut R,
        context: ReduceContext<'_>,
        before: Option<&mut super::reaction::BeforeMutation<'_, R>>,
    ) -> MountedApply<SharedReply, <Shared as Carriers<R, A>>::MountedWork> {
        let Self {
            route,
            input,
            #[cfg(feature = "test-util")]
                publication: _publication,
        } = *self;
        let node = route
            .node
            .upgrade()
            .expect("validated installed child has an actor owner");
        let (reply, context) = match apply_child(
            root,
            input,
            &route.path,
            FeatureContext::new(context.in_scope(route.token)),
            node.as_ref(),
            |root, context| {
                before.is_none_or(|capture| capture(root, context.reducer_context().scopes()))
            },
            F::update,
        ) {
            Ok(applied) => applied,
            Err(error) => return MountedApply::Rejected(error),
        };
        let scope = route.token;
        let work = context.into_work();
        let work: Option<Box<<Shared as Carriers<R, A>>::MountedWork>> = if work.is_empty() {
            None
        } else {
            Some(Box::new(SharedWork { route, work }))
        };
        MountedApply::Applied {
            reply: SharedReply::new(reply),
            work,
            scope,
        }
    }
}

impl<
    R: Send + 'static,
    A: Send + 'static,
    F: Feature + Send + 'static,
    P: BorrowPath<R, State = F::State> + Send + Sync + 'static,
> MountedWork<R, A, Shared> for SharedWork<R, A, F, P>
where
    F::State: Send + 'static,
    F::Action: Send + 'static,
{
    fn is_empty(&self) -> bool {
        self.work.is_empty()
    }

    fn prune_scopes(&mut self, current: &dyn Fn(ScopeToken) -> bool) {
        if current(self.route.token) {
            self.work.for_each_effect(|effect| {
                effect.prune_scopes(&|scope| current(scope));
            });
        } else {
            self.work = Work::default();
        }
    }

    fn register(
        self: Box<Self>,
        parts: &mut TransactionParts<'_, R, A>,
        mut origin: WorkOrigin<'_>,
        launches: &mut Launches<crate::RuntimeFuture>,
    ) {
        let Self { route, work } = *self;
        for item in work {
            match item {
                WorkItem::Effect(effect) => parts.compile_routed::<F::Action, SharedEffects, _>(
                    effect,
                    route.clone(),
                    origin.reborrow(),
                    launches,
                ),
                WorkItem::Task(task) => {
                    let (task, declaration) = task.into_parts();
                    let named = declaration.map(|declaration| {
                        parts.begin_operation(
                            declaration,
                            origin.scope,
                            origin.generations,
                            origin.operations,
                        )
                    });
                    let operations = named
                        .as_ref()
                        .map_or(origin.operations, |(_, ancestry, _)| ancestry);
                    let lineage =
                        Arc::clone(origin.lineage.get_or_insert_with(|| {
                            LineageState::new(Arc::downgrade(parts.control))
                        }));
                    let prepared = parts.registry.prepare_with_operations(
                        origin.scope,
                        Arc::clone(&lineage),
                        origin.generations,
                        operations,
                    );
                    let provenance = Provenance::Effect {
                        scope: origin.scope,
                        lineage,
                        task: prepared.sender_token(),
                        generations: prepared.generation_tokens(),
                        operations: prepared.operation_ancestry(),
                    };
                    let context = crate::TaskContext::from_endpoint(Arc::new(SharedEndpoint {
                        route: route.clone(),
                        provenance,
                    }));
                    launches.push(parts.registry.register(prepared, task.start(context)));
                    parts.end_operation(named, origin.generations);
                }
            }
        }
    }
}

/// Owns a deferred effect together with the delivery route needed to resume it.
struct BoundProgram<X, E: EffectLeaves<X>, D> {
    /// Remains unstarted until the preceding sequence segment finishes.
    effect: E::Effect,
    /// Retains metadata rather than a strong installed Feature or Store owner.
    delivery: D,
    /// Binds the action domain without adding ownership or auto-trait requirements.
    action: std::marker::PhantomData<fn(X)>,
}
impl<X, E: EffectLeaves<X>, D> BoundProgram<X, E, D> {
    /// Packages a program for later compilation without entering its user code.
    fn new(effect: E::Effect, delivery: D) -> Self {
        Self {
            effect,
            delivery,
            action: std::marker::PhantomData,
        }
    }
}
impl<X, E, D, R: 'static, A: 'static, C> DeferredProgram<R, A, C> for BoundProgram<X, E, D>
where
    E: EffectLeaves<X>,
    C: Execution<R, A>,
    D: Delivery<X, E, R, A, C>,
{
    fn resume(
        self: Box<Self>,
        parts: &mut TransactionParts<'_, R, A, C>,
        origin: WorkOrigin<'_>,
        launches: &mut Launches<C::Future>,
    ) {
        parts.compile_routed::<X, E, D>(self.effect, self.delivery, origin, launches);
    }

    fn prune_scopes(&mut self, current: &dyn Fn(ScopeToken) -> bool) {
        self.effect.prune_scopes(&|scope| current(scope));
    }

    fn is_none(&self) -> bool {
        self.effect.is_none()
    }
}
/// Turns a child action into one ordinary command with its existing causal leases.
trait ActionDelivery<X, R, A, C: Carriers<R, A>> {
    /// Transfers action, reply and named-work ownership into queued delivery.
    fn command(
        &self,
        action: X,
        reply: CommitSender,
        provenance: Provenance,
        operation_work: OperationWork,
    ) -> Command<R, A, C>;
    /// Exposes the captured child authority without allocating another owner.
    fn authority(&self) -> &ScopeAuthority;
}
/// Stores either a single owned action or a recurring action factory.
enum ChildTimerValue<X, I> {
    /// Taken once by the actor; a fired one-shot must not be scheduled again.
    Once(Option<X>),
    /// Retains the factory without calling it during timer registration.
    Interval {
        /// Determines the next interval deadline after an actual firing.
        period: std::time::Duration,
        /// Called only after the actor validates due-time authority.
        make_action: I,
    },
}
/// Couples child timer storage with its frozen action-delivery metadata.
struct ChildTimer<X, I, D> {
    /// Owns the action or factory until firing or cancellation destroys it.
    value: ChildTimerValue<X, I>,
    /// Produces the mounted command after due-time validation.
    delivery: D,
}
impl<X, I: FnMut() -> X, D, R, A, C: Carriers<R, A>> RoutedTimer<R, A, C> for ChildTimer<X, I, D>
where
    D: ActionDelivery<X, R, A, C>,
{
    fn authority(&self) -> &ScopeAuthority {
        self.delivery.authority()
    }

    fn period(&self) -> Option<std::time::Duration> {
        match &self.value {
            ChildTimerValue::Once(_) => None,
            ChildTimerValue::Interval { period, .. } => Some(*period),
        }
    }

    fn fire(&mut self, provenance: Provenance) -> Command<R, A, C> {
        let action = match &mut self.value {
            ChildTimerValue::Once(action) => action.take().expect("one-shot timer fires once"),
            ChildTimerValue::Interval { make_action, .. } => make_action(),
        };
        let operation_work = provenance.operations().retain_work();
        self.delivery.command(
            action,
            CommitSender::disconnected(),
            provenance,
            operation_work,
        )
    }
}
impl<
    R: Send + 'static,
    A: Send + 'static,
    F: Feature + Send + 'static,
    P: BorrowPath<R, State = F::State> + Send + Sync + 'static,
> ActionDelivery<F::Action, R, A, Shared> for SharedRoute<R, A, F, P>
where
    F::State: Send + 'static,
    F::Action: Send + 'static,
{
    fn command(
        &self,
        action: F::Action,
        reply: CommitSender,
        provenance: Provenance,
        operation_work: OperationWork,
    ) -> Command<R, A> {
        Command::Mounted {
            call: Box::new(SharedCall::new(
                self.clone(),
                ChildInput::effect(action, reply),
            )),
            provenance,
            authority: self.authority.clone(),
            operation_work,
        }
    }

    fn authority(&self) -> &ScopeAuthority {
        &self.authority
    }
}
impl<
    R: Send + 'static,
    A: Send + 'static,
    F: Feature + Send + 'static,
    P: BorrowPath<R, State = F::State> + Send + Sync + 'static,
> SharedRoute<R, A, F, P>
where
    F::State: Send + 'static,
    F::Action: Send + 'static,
{
    /// Attempts admission immediately and allocates a waiting future only when full.
    ///
    /// Named work stays owned until its action is destroyed or admitted. A pending
    /// send retains ingress, then releases it before awaiting commit acknowledgement.
    #[expect(
        clippy::map_err_ignore,
        reason = "Retain wildcard SendError destruction at the existing map_err boundary before ingress drops"
    )]
    fn send(&self, provenance: &Provenance, action: F::Action) -> crate::effect::EffectSendFuture {
        if !provenance.is_live() {
            return crate::effect::EffectSendFuture::Cancelled;
        }
        let Some(ingress) = self.ingress.upgrade() else {
            return crate::effect::EffectSendFuture::StoreStopped;
        };
        let operation_work = match provenance.operations().try_retain_work() {
            Ok(work) => work,
            Err((_, partial)) => {
                drop(action);
                drop(partial);
                return crate::effect::EffectSendFuture::Cancelled;
            }
        };
        #[cfg(all(test, feature = "test-util"))]
        ordinary_test_items! {
            mounted_admission_test_hook::after_retain();
        }
        if !provenance.is_live() {
            drop(action);
            drop(operation_work);
            return crate::effect::EffectSendFuture::Cancelled;
        }
        let (reply, receive) = super::command::commit_channel();
        let command = self.command(action, reply, provenance.clone(), operation_work);
        match ingress.try_send(command) {
            Ok(()) => crate::effect::EffectSendFuture::Receiving(receive),
            Err(async_channel::TrySendError::Closed(_)) => {
                crate::effect::EffectSendFuture::StoreStopped
            }
            Err(async_channel::TrySendError::Full(command)) => {
                crate::effect::EffectSendFuture::Admitting(Box::pin(async move {
                    ingress
                        .send(command)
                        .await
                        .map_err(|_| crate::EffectSendError::store_stopped())?;
                    drop(ingress);
                    Ok(receive)
                }))
            }
        }
    }
}
impl<
    R: Send + 'static,
    A: Send + 'static,
    F: Feature + Send + 'static,
    P: BorrowPath<R, State = F::State> + Send + Sync + 'static,
> Delivery<F::Action, SharedEffects, R, A, Shared> for SharedRoute<R, A, F, P>
where
    F::State: Send + 'static,
    F::Action: Send + 'static,
{
    fn register_leaf(
        &self,
        registry: &mut TaskRegistry,
        prepared: PreparedTask,
        leaf: EffectLeaf<F::Action, SharedEffects>,
        _: &Weak<CommandIngress<Command<R, A>>>,
        provenance: Provenance,
    ) -> crate::RuntimeFuture {
        let route = self.clone();
        let sender = move |authority| move |action| route.send(&authority, action);
        match leaf {
            EffectLeaf::Send(action) => {
                let send = sender(provenance);
                registry.register(prepared, async move {
                    let _ = send(action).await;
                })
            }
            EffectLeaf::Run(operation) => {
                let sender = crate::EffectSender::new(sender(provenance));
                registry.register(prepared, async move {
                    operation(sender).await;
                })
            }
            EffectLeaf::Fallible(operation) => {
                // Sending and failure routing independently retain the same authority.
                let sender = crate::EffectSender::new(sender(provenance.clone()));
                registry.register_with_cancellation(prepared, move |cancellation| async move {
                    operation(sender, provenance, cancellation).await;
                })
            }
            EffectLeaf::Future(future) => {
                let send = sender(provenance);
                registry.register(prepared, async move {
                    if let Some(action) = future.await {
                        let _ = send(action).await;
                    }
                })
            }
        }
    }

    fn sequence(&self, effects: VecDeque<crate::Effect<F::Action>>) -> DeferredQueue<R, A, Shared> {
        DeferredQueue::Routed(
            effects
                .into_iter()
                .map(|effect| {
                    Box::new(BoundProgram::<F::Action, SharedEffects, _>::new(
                        effect,
                        self.clone(),
                    )) as Box<<Shared as Carriers<R, A>>::Deferred>
                })
                .collect(),
        )
    }

    fn timer(
        &self,
        kind: TimerKind<F::Action, <SharedEffects as EffectLeaves<F::Action>>::Interval>,
    ) -> DeferredTimer<R, A, Shared> {
        let value = match kind {
            TimerKind::Wait => return DeferredTimer::Root(TimerKind::Wait),
            TimerKind::OneShot(action) => ChildTimerValue::Once(Some(action)),
            TimerKind::Interval {
                period,
                make_action,
            } => ChildTimerValue::Interval {
                period,
                make_action,
            },
        };
        DeferredTimer::Routed(Box::new(ChildTimer {
            value,
            delivery: self.clone(),
        }) as Box<<Shared as Carriers<R, A>>::Timer>)
    }
}

#[cfg(feature = "local")]
/// Keeps owner-thread child state behind short, non-reentrant metadata borrows.
struct LocalInstalled<F: local::Feature> {
    /// Guards extraction/restoration only; user code runs after the borrow ends.
    node: std::cell::RefCell<Installed<F>>,
    #[cfg(feature = "test-util")]
    /// Weakly publishes the current diagnostic request without retaining its input.
    pending: std::cell::RefCell<Option<(usize, PendingChildSlot<F::Action>)>>,
}
#[cfg(feature = "local")]
impl<F: local::Feature> LocalInstalled<F> {
    /// Borrows installation identity without calling selectors or user callbacks.
    fn borrow(&self) -> std::cell::Ref<'_, Installed<F>> {
        self.node.borrow()
    }

    /// Borrows the Feature slot only for actor-side extraction or restoration.
    fn borrow_mut(&self) -> std::cell::RefMut<'_, Installed<F>> {
        self.node.borrow_mut()
    }
}
#[cfg(feature = "local")]
impl<F: local::Feature> NodeCell<F> for LocalInstalled<F> {
    type Guard<'a>
        = std::cell::RefMut<'a, Installed<F>>
    where
        F: 'a;
    fn borrow_node(&self) -> Self::Guard<'_> {
        self.node.borrow_mut()
    }
}
#[cfg(feature = "local")]
/// Retains one Local Feature until owner-thread actor retirement or teardown.
pub(super) struct LocalNode<F: local::Feature> {
    /// Keeps installed metadata local without requiring atomic ownership or Send.
    owned: std::rc::Rc<LocalInstalled<F>>,
}
#[cfg(feature = "local")]
/// Refers to Local installation metadata without extending its Feature lifetime.
pub(super) struct LocalWeakNode<F: local::Feature>(
    /// Upgraded only at actor boundaries or checked owner-thread inspection.
    std::rc::Weak<LocalInstalled<F>>,
);
#[cfg(feature = "local")]
impl<F: local::Feature> Clone for LocalWeakNode<F> {
    fn clone(&self) -> Self {
        Self(std::rc::Weak::clone(&self.0))
    }
}
#[cfg(feature = "local")]
impl<F: local::Feature> LocalWeakNode<F> {
    /// Removes the Feature before running its destructor outside the cell borrow.
    pub(super) fn retire_feature(&self) {
        if let Some(node) = self.0.upgrade() {
            let feature = node.borrow_mut().feature.take();
            drop(feature);
        }
    }

    /// Retires old capability generations without replacing the Feature cache.
    pub(super) fn refresh_configuration(&self, configuration: u64) {
        let node = self.0.upgrade().expect("current scope owns installed node");
        node.borrow_mut().configuration = configuration;
    }
}
#[cfg(feature = "local")]
impl<F: local::Feature> InstalledNode for LocalNode<F> {
    fn scope(&self) -> ScopeToken {
        self.owned.borrow().token
    }
}
#[cfg(feature = "local")]
impl<F: local::Feature> LocalNode<F> {
    /// Creates a local metadata reference that cannot retain the Feature owner.
    pub(super) fn downgrade(&self) -> LocalWeakNode<F> {
        LocalWeakNode(std::rc::Rc::downgrade(&self.owned))
    }

    /// Stores an already-constructed Feature under its committed installation identity.
    pub(super) fn new(feature: F, token: ScopeToken, configuration: u64) -> Self {
        Self {
            owned: std::rc::Rc::new(LocalInstalled {
                node: std::cell::RefCell::new(Installed {
                    feature: Some(feature),
                    token,
                    configuration,
                }),
                #[cfg(feature = "test-util")]
                pending: std::cell::RefCell::new(None),
            }),
        }
    }
}
#[cfg(feature = "local")]
/// Retains owner-thread routing metadata without retaining Store or Feature ownership.
struct LocalRoute<R, A, F: local::Feature, P = OptionalPath<R, <F as local::Feature>::State>> {
    /// Lets admitted actor calls temporarily access the local installed metadata.
    node: std::rc::Weak<LocalInstalled<F>>,
    /// Owns selectors and ancestry metadata, not borrowed live state.
    path: P,
    /// Identifies the captured installation lifetime.
    token: ScopeToken,
    /// Distinguishes capabilities acquired before and after configuration refresh.
    configuration: u64,
    /// Supports local admission without keeping the Store alive between requests.
    ingress: Weak<CommandIngress<Command<R, A, local::Local>>>,
    /// Captures ancestor lifetimes and configuration generations at acquisition.
    authority: ScopeAuthority,
}
#[cfg(feature = "local")]
impl<R, A, F: local::Feature, P: Clone> Clone for LocalRoute<R, A, F, P> {
    fn clone(&self) -> Self {
        Self {
            node: std::rc::Weak::clone(&self.node),
            path: self.path.clone(),
            token: self.token,
            configuration: self.configuration,
            ingress: Weak::clone(&self.ingress),
            authority: self.authority.clone(),
        }
    }
}
#[cfg(feature = "local")]
impl<
    R: 'static,
    A: 'static,
    F: local::Feature + 'static,
    P: BorrowPath<R, State = F::State> + 'static,
> super::mount::local::commands::Dispatch<R, A, F::Action> for LocalRoute<R, A, F, P>
where
    F::State: 'static,
    F::Action: 'static,
{
    fn authority(&self) -> &ScopeAuthority {
        &self.authority
    }

    fn ingress(&self) -> Weak<CommandIngress<Command<R, A, local::Local>>> {
        Weak::clone(&self.ingress)
    }

    fn scope(&self) -> ScopeToken {
        self.token
    }

    fn apply(
        &self,
        root: &mut R,
        context: ReduceContext<'_>,
        input: &mut super::command_port::input::MappedInput<'_, F::Action>,
        before: Option<&mut super::reaction::BeforeMutation<'_, R>>,
    ) -> MountedApply<(), <local::Local as Carriers<R, A>>::MountedWork> {
        let node = self
            .node
            .upgrade()
            .expect("validated installed child has an actor owner");
        let ((), context) = match apply_child_input(
            root,
            BorrowedInput(input),
            &self.path,
            local::FeatureContext::new(context.in_scope(self.token)),
            node.as_ref(),
            |root, context| {
                before.is_none_or(|capture| capture(root, context.reducer_context().scopes()))
            },
            |feature, state, input, (), context| {
                self.authority.admission().map_err(|error| {
                    TaskAccessError::new(error.reason(), TaskProgress::MayHaveRun)
                })?;
                let action = input.take().expect("entered command mapping")();
                self.authority.admission().map_err(|error| {
                    TaskAccessError::new(error.reason(), TaskProgress::MayHaveRun)
                })?;
                F::update(feature, state, &action, context);
                Ok(())
            },
        ) {
            Ok(applied) => applied,
            Err(error) => return MountedApply::Rejected(error),
        };
        let work = context.into_work();
        let work: Option<Box<<local::Local as Carriers<R, A>>::MountedWork>> = if work.is_empty() {
            None
        } else {
            Some(Box::new(LocalWork {
                route: self.clone(),
                work,
            }))
        };
        MountedApply::Applied {
            reply: (),
            work,
            scope: self.token,
        }
    }
}

#[cfg(feature = "local")]
/// Provides owner-local task access under one captured route and causal origin.
struct LocalEndpoint<R, A, F: local::Feature, P = OptionalPath<R, <F as local::Feature>::State>> {
    /// Resolves state and input only after transaction admission.
    route: LocalRoute<R, A, F, P>,
    /// Carries external origin or the task's cancellation and named-work ancestry.
    provenance: Provenance,
}
#[cfg(feature = "local")]
/// Owns a local child action until rejection or synchronous actor reduction.
struct LocalCall<R, A, F: local::Feature, P = OptionalPath<R, <F as local::Feature>::State>> {
    /// Retains frozen metadata through application and deferred work construction.
    route: LocalRoute<R, A, F, P>,
    /// Owns action and reply without imposing Shared transport bounds.
    input: ChildInput<F::Action>,
    #[cfg(feature = "test-util")]
    /// Clears this request's diagnostic publication when its gate ownership ends.
    publication: LocalPublication<F>,
}
#[cfg(all(feature = "local", feature = "test-util"))]
/// Clears one owner-thread diagnostic publication without retaining its node.
struct LocalPublication<F: local::Feature> {
    /// Permits cleanup only while the actor still owns installed metadata.
    node: std::rc::Weak<LocalInstalled<F>>,
    /// Prevents an earlier request's destructor from clearing a later publication.
    request: usize,
}
#[cfg(all(feature = "local", feature = "test-util"))]
impl<F: local::Feature> Drop for LocalPublication<F> {
    fn drop(&mut self) {
        if let Some(node) = self.node.upgrade() {
            let mut pending = node.pending.borrow_mut();
            if pending.as_ref().is_some_and(|(id, _)| *id == self.request) {
                pending.take();
            }
        }
    }
}
#[cfg(feature = "local")]
impl<R, A, F: local::Feature, P> LocalCall<R, A, F, P> {
    /// Couples local input and routing before the actor publishes either.
    fn new(route: LocalRoute<R, A, F, P>, input: ChildInput<F::Action>) -> Self {
        Self {
            #[cfg(feature = "test-util")]
            publication: LocalPublication {
                node: std::rc::Weak::clone(&route.node),
                request: input.identity(),
            },
            route,
            input,
        }
    }
}
#[cfg(feature = "local")]
/// Retains typed Local work until the enclosing transaction registers it.
struct LocalWork<R, A, F: local::Feature, P = OptionalPath<R, <F as local::Feature>::State>> {
    /// Routes returned actions to the same captured child installation.
    route: LocalRoute<R, A, F, P>,
    /// Preserves interleaved effect/task declarations, including non-Send captures.
    #[expect(
        clippy::type_complexity,
        reason = "Work retains the concrete child state and action types without another erased layer"
    )]
    work: Work<local::Effect<F::Action>, local::Task<F::State, F::Action>>,
}
#[cfg(feature = "local")]
/// Freezes a local handler destination before output ownership can be consumed.
struct LocalHandlerRoute<R, A, F: local::Feature, P> {
    /// Selects handler state and routes its declared Local work.
    route: LocalRoute<R, A, F, P>,
    /// Preserves one checked installation throughout handler application.
    registration: super::output::FrozenRegistration,
}
#[cfg(feature = "local")]
impl<
    R: 'static,
    A: 'static,
    F: local::Feature + 'static,
    P: BorrowPath<R, State = F::State> + 'static,
> super::output::local::HandlerRoute<R, A, F::State, F::Action> for LocalHandlerRoute<R, A, F, P>
where
    F::State: 'static,
    F::Action: 'static,
{
    fn registration(&self) -> &super::output::FrozenRegistration {
        &self.registration
    }

    #[cfg(all(test, feature = "test-util"))]
    fn read<'a>(&self, root: &'a R) -> Option<&'a F::State> {
        self.route.path.read(root)
    }

    fn select<'a>(&self, root: &'a mut R) -> Option<&'a mut F::State> {
        self.route.path.write(root)
    }

    fn into_work(
        self: Box<Self>,
        work: Work<local::Effect<F::Action>, local::Task<F::State, F::Action>>,
    ) -> Box<dyn MountedWork<R, A, local::Local>> {
        Box::new(LocalWork {
            route: self.route,
            work,
        })
    }
}

#[cfg(feature = "local")]
impl<F: local::Feature + 'static> LocalNode<F>
where
    F::State: 'static,
    F::Action: 'static,
{
    #[cfg(test)]
    /// Builds a direct local context for production-kernel fixtures without acquisition.
    pub(super) fn context<R: 'static, A: 'static>(
        &self,
        path: impl BorrowPath<R, State = F::State> + 'static,
        ingress: Weak<CommandIngress<Command<R, A, local::Local>>>,
        provenance: Provenance,
        authority: ScopeAuthority,
    ) -> local::TaskContext<F::State, F::Action> {
        let (token, configuration) = {
            let node = self.owned.borrow();
            (node.token, node.configuration)
        };
        local::TaskContext::from_endpoint(std::rc::Rc::new(LocalEndpoint {
            route: LocalRoute {
                node: std::rc::Rc::downgrade(&self.owned),
                path,
                token,
                configuration,
                ingress,
                authority,
            },
            provenance,
        }))
    }
}
#[cfg(feature = "local")]
impl<F: local::Feature + 'static> LocalWeakNode<F>
where
    F::State: 'static,
    F::Action: 'static,
{
    /// Captures a local command route while serial acquisition owns its node.
    pub(super) fn command_dispatch<R: 'static, A: 'static>(
        &self,
        path: impl BorrowPath<R, State = F::State> + 'static,
        ingress: Weak<CommandIngress<Command<R, A, local::Local>>>,
        scopes: &super::scope::ScopeRegistry,
    ) -> std::rc::Rc<dyn super::mount::local::commands::Dispatch<R, A, F::Action>> {
        let node = self
            .0
            .upgrade()
            .expect("serial acquisition owns current installed node");
        let (token, configuration) = {
            let node = node.borrow();
            (node.token, node.configuration)
        };
        std::rc::Rc::new(LocalRoute {
            node: std::rc::Weak::clone(&self.0),
            path,
            token,
            configuration,
            ingress,
            authority: scopes.authority(token),
        })
    }

    /// Captures a local output-handler destination only while its token is current.
    #[expect(
        clippy::type_complexity,
        reason = "The erased route must preserve the exact root and child state/action pairing"
    )]
    pub(super) fn handler_route<R: 'static, A: 'static>(
        &self,
        path: impl BorrowPath<R, State = F::State> + 'static,
        ingress: Weak<CommandIngress<Command<R, A, local::Local>>>,
        scopes: &super::scope::ScopeRegistry,
    ) -> Option<Box<dyn super::output::local::HandlerRoute<R, A, F::State, F::Action>>> {
        let node = self.0.upgrade()?;
        let (token, configuration) = {
            let node = node.borrow();
            (node.token, node.configuration)
        };
        drop(node);
        if !scopes.is_current(token) {
            return None;
        }
        let authority = scopes.authority(token);
        Some(Box::new(LocalHandlerRoute {
            registration: super::output::FrozenRegistration {
                scope: token,
                authority: authority.clone(),
            },
            route: LocalRoute {
                node: std::rc::Weak::clone(&self.0),
                path,
                token,
                configuration,
                ingress,
                authority,
            },
        }))
    }

    /// Routes one Local initial task under already-frozen structural authority.
    pub(super) fn initial_work<R: 'static, A: 'static>(
        &self,
        path: impl BorrowPath<R, State = F::State> + 'static,
        ingress: Weak<CommandIngress<Command<R, A, local::Local>>>,
        authority: ScopeAuthority,
        task: local::Task<F::State, F::Action>,
    ) -> Box<dyn MountedWork<R, A, local::Local>> {
        let node = self
            .0
            .upgrade()
            .expect("frozen initial roster owns installed node");
        let (token, configuration) = {
            let node = node.borrow();
            (node.token, node.configuration)
        };
        drop(node);
        let mut work = Work::default();
        work.push(WorkItem::Task(task));
        Box::new(LocalWork {
            route: LocalRoute {
                node: std::rc::Weak::clone(&self.0),
                path,
                token,
                configuration,
                ingress,
                authority,
            },
            work,
        })
    }

    /// Captures an external local context at the actor's current configuration.
    pub(super) fn context<R: 'static, A: 'static>(
        &self,
        path: impl BorrowPath<R, State = F::State> + 'static,
        ingress: Weak<CommandIngress<Command<R, A, local::Local>>>,
        scopes: &super::scope::ScopeRegistry,
    ) -> local::TaskContext<F::State, F::Action> {
        let node = self
            .0
            .upgrade()
            .expect("serial acquisition owns current installed node");
        let (token, configuration) = {
            let node = node.borrow();
            (node.token, node.configuration)
        };
        let authority = scopes.authority(token);
        drop(node);
        local::TaskContext::from_endpoint(std::rc::Rc::new(LocalEndpoint {
            route: LocalRoute {
                node: std::rc::Weak::clone(&self.0),
                path,
                token,
                configuration,
                ingress,
                authority,
            },
            provenance: Provenance::External { handle: None },
        }))
    }
}
#[cfg(feature = "local")]
impl<
    R: 'static,
    A: 'static,
    F: local::Feature + 'static,
    P: BorrowPath<R, State = F::State> + 'static,
> super::task_context::local::Endpoint<F::State, F::Action> for LocalEndpoint<R, A, F, P>
where
    F::State: 'static,
    F::Action: 'static,
{
    #[cfg(feature = "test-util")]
    fn inspect_input(
        &self,
        identity: MountedInputIdentity,
        authority: &ScopeAuthority,
    ) -> Result<Option<SendInspection<F::Action>>, TaskAccessError> {
        self.route.authority.admission()?;
        if !self.route.authority.same_root(authority) {
            return Err(scope_ended(TaskProgress::NotStarted));
        }
        let node = self
            .route
            .node
            .upgrade()
            .ok_or_else(|| scope_ended(TaskProgress::NotStarted))?;
        {
            let installed = node.borrow();
            if (installed.token, installed.configuration)
                != (self.route.token, self.route.configuration)
            {
                return Err(scope_ended(TaskProgress::NotStarted));
            }
        }
        if self.route.node.as_ptr() as usize != identity.node {
            return Ok(None);
        }
        if !self.route.authority.same_capture(authority) {
            return Err(scope_ended(TaskProgress::NotStarted));
        }
        let slot = node
            .pending
            .borrow()
            .as_ref()
            .filter(|(request, _)| *request == identity.request)
            .map(|(_, slot)| slot.clone());
        Ok(slot.and_then(PendingChildSlot::inspect))
    }

    fn output_source(&self) -> super::output::OutputSource {
        super::output::OutputSource {
            scope: self.route.token,
            provenance: self.provenance.clone(),
            authority: self.route.authority.clone(),
        }
    }

    fn operation(&self, identity: crate::effect::CancellationIdentity) -> crate::local::Operation {
        crate::local::Operation::from_frozen_identity(
            Weak::clone(&self.route.ingress),
            self.provenance.clone(),
            self.route.token,
            self.route.authority.clone(),
            identity,
        )
    }

    fn submit(&self, request: TaskInput<F::State, F::Action, local::Local>) -> local::LocalFuture {
        let route = self.route.clone();
        let provenance = self.provenance.clone();
        Box::pin(async move {
            if let Err(error) = route.authority.admission() {
                request.reject(error);
                return;
            }
            if !provenance.is_live() {
                request.reject(TaskAccessError::new(
                    TaskAccessReason::Cancelled,
                    TaskProgress::NotStarted,
                ));
                return;
            }
            let ingress = route
                .ingress
                .upgrade()
                .expect("open local store owns ingress");
            let operation_work = match provenance.operations().try_retain_work() {
                Ok(work) => work,
                Err((reason, partial)) => {
                    request.reject(TaskAccessError::new(reason, TaskProgress::NotStarted));
                    drop(partial);
                    return;
                }
            };
            #[cfg(all(test, feature = "test-util"))]
            ordinary_test_items! {
                mounted_admission_test_hook::after_retain();
            }
            if !provenance.is_live() {
                request.reject(TaskAccessError::new(
                    TaskAccessReason::Cancelled,
                    TaskProgress::NotStarted,
                ));
                drop(operation_work);
                return;
            }
            let authority = route.authority.clone();
            let command = match request {
                TaskRequest::Read(call) => EndpointCommand::<R, A, local::Local>::Task(
                    TaskRequest::<R, A, local::Local>::Read(Box::new(ProjectedRead {
                        call,
                        path: route.path,
                    })),
                ),
                TaskRequest::Update(call) => EndpointCommand::<R, A, local::Local>::Task(
                    TaskRequest::<R, A, local::Local>::Update(Box::new(ProjectedUpdate {
                        call,
                        path: route.path,
                    })),
                ),
                TaskRequest::Send(input) => EndpointCommand::<R, A, local::Local>::Mounted(
                    Box::new(LocalCall::new(route, ChildInput::typed(input)))
                        as Box<<local::Local as Carriers<R, A>>::Mounted>,
                ),
            };
            if let Err(async_channel::SendError((command, _, _, operation_work))) = ingress
                .send_map(
                    (command, provenance, authority, operation_work),
                    |(command, provenance, authority, operation_work)| match command {
                        EndpointCommand::Task(request) => Command::Task {
                            request,
                            provenance,
                            authority,
                            operation_work,
                        },
                        EndpointCommand::Mounted(call) => Command::Mounted {
                            call,
                            provenance,
                            authority,
                            operation_work,
                        },
                    },
                )
                .await
            {
                command.reject(TaskAccessError::new(
                    TaskAccessReason::StoreClosed,
                    TaskProgress::NotStarted,
                ));
                drop(operation_work);
            }
        })
    }
}
#[cfg(feature = "local")]
impl<
    R: 'static,
    A: 'static,
    F: local::Feature + 'static,
    P: BorrowPath<R, State = F::State> + 'static,
> MountedCall<R, A, local::Local> for LocalCall<R, A, F, P>
where
    F::State: 'static,
    F::Action: 'static,
{
    #[cfg(feature = "test-util")]
    fn attach_terminal(&mut self, terminal: super::task_context::TerminalReceipt) {
        self.input.attach_terminal(terminal);
    }

    fn input_rejection(&self, scopes: &super::scope::ScopeRegistry) -> Option<TaskAccessError> {
        scopes.input_rejection(self.route.token)
    }

    #[cfg(feature = "test-util")]
    fn input_identity(&self) -> Option<MountedInputIdentity> {
        Some(MountedInputIdentity {
            node: self.route.node.as_ptr() as usize,
            request: self.input.identity(),
        })
    }

    #[cfg(feature = "test-util")]
    fn publish_input(&self) {
        // Gate selection validates authority while the actor owns this node.
        let node = self
            .route
            .node
            .upgrade()
            .expect("selected child remains actor-owned during publication");
        *node.pending.borrow_mut() = Some(self.input.pending_slot());
    }

    fn reject(self: Box<Self>, error: TaskAccessError) {
        self.input.reject(error);
    }

    fn apply(
        self: Box<Self>,
        root: &mut R,
        context: ReduceContext<'_>,
        before: Option<&mut super::reaction::BeforeMutation<'_, R>>,
    ) -> MountedApply<local::LocalReply, <local::Local as Carriers<R, A>>::MountedWork> {
        let Self {
            route,
            input,
            #[cfg(feature = "test-util")]
                publication: _publication,
        } = *self;
        let node = route
            .node
            .upgrade()
            .expect("validated installed child has an actor owner");
        let (reply, context) = match apply_child(
            root,
            input,
            &route.path,
            local::FeatureContext::new(context.in_scope(route.token)),
            node.as_ref(),
            |root, context| {
                before.is_none_or(|capture| capture(root, context.reducer_context().scopes()))
            },
            F::update,
        ) {
            Ok(applied) => applied,
            Err(error) => return MountedApply::Rejected(error),
        };
        let scope = route.token;
        let work = context.into_work();
        let work: Option<Box<<local::Local as Carriers<R, A>>::MountedWork>> = if work.is_empty() {
            None
        } else {
            Some(Box::new(LocalWork { route, work }))
        };
        MountedApply::Applied {
            reply: local::LocalReply::new(reply),
            work,
            scope,
        }
    }
}

#[cfg(feature = "local")]
impl<
    R: 'static,
    A: 'static,
    F: local::Feature + 'static,
    P: BorrowPath<R, State = F::State> + 'static,
> MountedWork<R, A, local::Local> for LocalWork<R, A, F, P>
where
    F::State: 'static,
    F::Action: 'static,
{
    fn is_empty(&self) -> bool {
        self.work.is_empty()
    }

    fn prune_scopes(&mut self, current: &dyn Fn(ScopeToken) -> bool) {
        // Local effects have no intrinsic reducer scopes; this route owns them.
        if !current(self.route.token) {
            self.work = Work::default();
        }
    }

    fn register(
        self: Box<Self>,
        parts: &mut TransactionParts<'_, R, A, local::Local>,
        mut origin: WorkOrigin<'_>,
        launches: &mut Launches<local::LocalFuture>,
    ) {
        let Self { route, work } = *self;
        for item in work {
            match item {
                WorkItem::Effect(effect) => parts
                    .compile_routed::<F::Action, local::LocalEffects, _>(
                        effect,
                        route.clone(),
                        origin.reborrow(),
                        launches,
                    ),
                WorkItem::Task(task) => {
                    let (task, declaration) = task.into_parts();
                    let named = declaration.map(|declaration| {
                        parts.begin_operation(
                            declaration,
                            origin.scope,
                            origin.generations,
                            origin.operations,
                        )
                    });
                    let operations = named
                        .as_ref()
                        .map_or(origin.operations, |(_, ancestry, _)| ancestry);
                    let lineage =
                        Arc::clone(origin.lineage.get_or_insert_with(|| {
                            LineageState::new(Arc::downgrade(parts.control))
                        }));
                    let prepared = parts.registry.prepare_with_operations(
                        origin.scope,
                        Arc::clone(&lineage),
                        origin.generations,
                        operations,
                    );
                    let provenance = Provenance::Effect {
                        scope: origin.scope,
                        lineage,
                        task: prepared.sender_token(),
                        generations: prepared.generation_tokens(),
                        operations: prepared.operation_ancestry(),
                    };
                    let context =
                        local::TaskContext::from_endpoint(std::rc::Rc::new(LocalEndpoint {
                            route: route.clone(),
                            provenance,
                        }));
                    launches.push(
                        parts
                            .registry
                            .register_local(prepared, move |_| task.start(context)),
                    );
                    parts.end_operation(named, origin.generations);
                }
            }
        }
    }
}

#[cfg(feature = "local")]
impl<
    R: 'static,
    A: 'static,
    F: local::Feature + 'static,
    P: BorrowPath<R, State = F::State> + 'static,
> ActionDelivery<F::Action, R, A, local::Local> for LocalRoute<R, A, F, P>
where
    F::State: 'static,
    F::Action: 'static,
{
    fn command(
        &self,
        action: F::Action,
        reply: CommitSender,
        provenance: Provenance,
        operation_work: OperationWork,
    ) -> Command<R, A, local::Local> {
        Command::Mounted {
            call: Box::new(LocalCall::new(
                self.clone(),
                ChildInput::effect(action, reply),
            )),
            provenance,
            authority: self.authority.clone(),
            operation_work,
        }
    }

    fn authority(&self) -> &ScopeAuthority {
        &self.authority
    }
}
#[cfg(feature = "local")]
impl<
    R: 'static,
    A: 'static,
    F: local::Feature + 'static,
    P: BorrowPath<R, State = F::State> + 'static,
> LocalRoute<R, A, F, P>
where
    F::State: 'static,
    F::Action: 'static,
{
    /// Attempts local admission immediately, retaining ingress only while capacity blocks.
    ///
    /// Named work is released after its action, and the completed admission drops
    /// ingress before the returned future waits for commit acknowledgement.
    #[expect(
        clippy::map_err_ignore,
        reason = "Retain wildcard SendError destruction at the existing map_err boundary before ingress drops"
    )]
    fn send(&self, provenance: &Provenance, action: F::Action) -> local::SendFuture {
        if !provenance.is_live() {
            return crate::effect::EffectSendFuture::Cancelled;
        }
        let Some(ingress) = self.ingress.upgrade() else {
            return crate::effect::EffectSendFuture::StoreStopped;
        };
        let operation_work = match provenance.operations().try_retain_work() {
            Ok(work) => work,
            Err((_, partial)) => {
                drop(action);
                drop(partial);
                return crate::effect::EffectSendFuture::Cancelled;
            }
        };
        #[cfg(all(test, feature = "local", feature = "test-util"))]
        ordinary_test_items! {
            mounted_admission_test_hook::after_retain();
        }
        if !provenance.is_live() {
            drop(action);
            drop(operation_work);
            return crate::effect::EffectSendFuture::Cancelled;
        }
        let (reply, receive) = super::command::commit_channel();
        let command = self.command(action, reply, provenance.clone(), operation_work);
        match ingress.try_send(command) {
            Ok(()) => crate::effect::EffectSendFuture::Receiving(receive),
            Err(async_channel::TrySendError::Closed(_)) => {
                crate::effect::EffectSendFuture::StoreStopped
            }
            Err(async_channel::TrySendError::Full(command)) => {
                crate::effect::EffectSendFuture::Admitting(Box::pin(async move {
                    ingress
                        .send(command)
                        .await
                        .map_err(|_| crate::EffectSendError::store_stopped())?;
                    drop(ingress);
                    Ok(receive)
                }))
            }
        }
    }
}
#[cfg(feature = "local")]
impl<
    R: 'static,
    A: 'static,
    F: local::Feature + 'static,
    P: BorrowPath<R, State = F::State> + 'static,
> Delivery<F::Action, local::LocalEffects, R, A, local::Local> for LocalRoute<R, A, F, P>
where
    F::State: 'static,
    F::Action: 'static,
{
    fn register_leaf(
        &self,
        registry: &mut TaskRegistry,
        prepared: PreparedTask,
        leaf: EffectLeaf<F::Action, local::LocalEffects>,
        _: &Weak<CommandIngress<Command<R, A, local::Local>>>,
        provenance: Provenance,
    ) -> local::LocalFuture {
        let route = self.clone();
        let sender = move |authority| move |action| route.send(&authority, action);
        match leaf {
            EffectLeaf::Send(action) => {
                let send = sender(provenance);
                registry.register_local(prepared, move |_| async move {
                    let _ = send(action).await;
                })
            }
            EffectLeaf::Run(operation) => {
                let sender = local::EffectSender::new(sender(provenance));
                registry.register_local(prepared, move |_| async move {
                    operation(sender).await;
                })
            }
            EffectLeaf::Fallible(operation) => {
                // Sending and failure routing independently retain the same authority.
                let sender = local::EffectSender::new(sender(provenance.clone()));
                registry.register_local(prepared, move |registration| {
                    let cancellation = registration.handle();
                    async move {
                        operation(sender, provenance, cancellation).await;
                    }
                })
            }
            EffectLeaf::Future(future) => {
                let send = sender(provenance);
                registry.register_local(prepared, move |_| async move {
                    if let Some(action) = future.await {
                        let _ = send(action).await;
                    }
                })
            }
        }
    }

    fn sequence(
        &self,
        effects: VecDeque<local::Effect<F::Action>>,
    ) -> DeferredQueue<R, A, local::Local> {
        DeferredQueue::Routed(
            effects
                .into_iter()
                .map(|effect| {
                    Box::new(BoundProgram::<F::Action, local::LocalEffects, _>::new(
                        effect,
                        self.clone(),
                    )) as Box<<local::Local as Carriers<R, A>>::Deferred>
                })
                .collect(),
        )
    }

    fn timer(
        &self,
        kind: TimerKind<F::Action, <local::LocalEffects as EffectLeaves<F::Action>>::Interval>,
    ) -> DeferredTimer<R, A, local::Local> {
        let value = match kind {
            TimerKind::Wait => return DeferredTimer::Root(TimerKind::Wait),
            TimerKind::OneShot(action) => ChildTimerValue::Once(Some(action)),
            TimerKind::Interval {
                period,
                make_action,
            } => ChildTimerValue::Interval {
                period,
                make_action,
            },
        };
        DeferredTimer::Routed(Box::new(ChildTimer {
            value,
            delivery: self.clone(),
        }) as Box<<local::Local as Carriers<R, A>>::Timer>)
    }
}

#[cfg(test)]
ordinary_test_items! {
    pub(in crate::store) mod tests;
    mod lease_tests;
    #[cfg(feature = "diagnostics")]
    mod lifecycle_tests;
    mod route_tests;
    mod path_lifetime_tests;
}

#[cfg(all(test, feature = "allocation-measurement"))]
pub(super) mod allocation_tests;

#[cfg(all(test, feature = "test-util"))]
ordinary_test_items! {
/// Injects one synchronous race at an existing admission boundary.
///
/// Hooks are thread-local, cannot nest and run outside their callback cell's
/// borrow. The guard clears both slots before destroying an unused capture.
pub(in crate::store) mod mounted_admission_test_hook {
    use std::cell::{Cell, RefCell};

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Point {
        BeforeLifecycle,
        BeforeIngress,
        AfterIngress,
        BeforeMapper,
        AfterRetain,
    }

    thread_local! {
        static ACTIVE: Cell<Option<Point>> = const { Cell::new(None) };
        static CALLBACK: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
    }

    pub(in crate::store) fn with_before_lifecycle<T>(
        callback: impl FnOnce() + 'static,
        first_poll: impl FnOnce() -> T,
    ) -> T {
        with_hook(Point::BeforeLifecycle, callback, first_poll)
    }

    pub(in crate::store) fn with_before_ingress<T>(
        callback: impl FnOnce() + 'static,
        first_poll: impl FnOnce() -> T,
    ) -> T {
        with_hook(Point::BeforeIngress, callback, first_poll)
    }

    pub(in crate::store) fn with_after_retain<T>(
        callback: impl FnOnce() + 'static,
        first_poll: impl FnOnce() -> T,
    ) -> T {
        with_hook(Point::AfterRetain, callback, first_poll)
    }

    pub(in crate::store) fn with_after_ingress<T>(
        callback: impl FnOnce() + 'static,
        first_poll: impl FnOnce() -> T,
    ) -> T {
        with_hook(Point::AfterIngress, callback, first_poll)
    }

    pub(in crate::store) fn with_before_mapper<T>(
        callback: impl FnOnce() + 'static,
        inspection: impl FnOnce() -> T,
    ) -> T {
        with_hook(Point::BeforeMapper, callback, inspection)
    }

    /// Restores idle hook state after normal return or unwind from the operation.
    fn with_hook<T>(
        point: Point,
        callback: impl FnOnce() + 'static,
        first_poll: impl FnOnce() -> T,
    ) -> T {
        /// Clears registration before a leftover callback's destructor can re-enter.
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                let unused = CALLBACK.with(|slot| slot.borrow_mut().take());
                ACTIVE.with(|active| active.set(None));
                drop(unused);
            }
        }

        ACTIVE.with(|active| {
            assert!(
                active.get().is_none(),
                "mounted admission hook already active"
            );
            active.set(Some(point));
        });
        let _reset = Reset;
        CALLBACK.with(|slot| *slot.borrow_mut() = Some(Box::new(callback)));
        first_poll()
    }

    pub(in crate::store) fn before_lifecycle() {
        run(Point::BeforeLifecycle);
    }

    pub(in crate::store) fn before_ingress() {
        run(Point::BeforeIngress);
    }

    pub(in crate::store) fn after_ingress() {
        run(Point::AfterIngress);
    }

    pub(in crate::store) fn before_mapper() {
        run(Point::BeforeMapper);
    }

    pub(in crate::store) fn after_retain() {
        run(Point::AfterRetain);
    }

    /// Consumes only the matching callback and releases its borrow before entry.
    fn run(point: Point) {
        if !ACTIVE.with(|active| active.get() == Some(point)) {
            return;
        }
        let callback = CALLBACK.with(|slot| slot.borrow_mut().take());
        if let Some(callback) = callback {
            callback();
        }
    }
}
}
