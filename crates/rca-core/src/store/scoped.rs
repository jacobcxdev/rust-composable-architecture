//! Routes child operations through root authority and optional registered lifetimes.
//!
//! Required, unidentified projections follow their root Store. Registered routes
//! also capture one child lifetime, which cannot be renewed by reusing its ID.

mod weak;

#[doc(inline)]
pub use weak::WeakScopedStore;

use crate::store::mailbox;
use std::any::Any;
use std::fmt;
use std::hash::Hash;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;

use crate::observation::wait::{
    PredicateWait, PredicateWaitOutcome, WaitCancellation, WaitTerminal, wait_for_false,
};
use crate::observation::{
    ObservationId, ObservationSignal, TakeFuture, TakeOperation, TakeOutcome, TakeReply,
};
use crate::{
    ActionPath, ActionReceipt, ActionTask, CaseStatePath, KeyedAction, KeyedActionPath, KeyedState,
    KeyedStatePath, Observation, OptionalStatePath, ScopedStoreEnded, StackAction, StackId,
    StackState, StatePath, TryScopedSendError,
};

use super::command::{
    ActionCommand, ActionEmbed, Command, CommitError, DeferredAction, OwnedActionCommand,
    ScopedActionEmbed,
};
use super::observation::{
    CheckedSelector, ObservationSelection, PublishPolicy, new_registration_with_selection, new_take,
};
use super::read::new_checked_read;
use super::scope::{
    HandleRegistrationCommand, HandleScope, HandleSelector, HandleSignal, IdentifiedHandleSelector,
    ScopeIdentity, TypedHandleSelector,
};
use super::{RegistrationGuard, Store};

/// Owns a projected value without exposing its root driver's concrete type.
type ErasedValue = Box<dyn Any + Send>;
/// Consumes one query at its ordered actor position.
type ReadProjection<State> = Box<dyn FnOnce(&State) -> ErasedValue + Send>;
/// Produces an owned value for each observation selection.
type ObservationProjection<State> = Box<dyn Fn(&State) -> ErasedValue + Send>;
/// Hides root-specific asynchronous admission behind the child driver.
type ScopedFuture<T> = Pin<Box<dyn std::future::Future<Output = T> + Send + 'static>>;
/// Borrows the captured route throughout an ordinary scoped send.
type ScopedSendFuture<'a> =
    Pin<Box<dyn std::future::Future<Output = Result<ActionTask, ScopedStoreEnded>> + Send + 'a>>;
/// Reuses one weak-root admission callback across sequential observation takes.
///
/// Scoped takes always need asynchronous, type-erased admission. Boxing their
/// future avoids carrying the unused root fast-path state in this adapter.
type ErasedTakeOperation = Box<
    dyn Fn() -> ScopedFuture<Result<TakeReply<ErasedValue>, crate::StoreStopped>> + Send + Sync,
>;

/// Transfers a registered observation from the root driver to its typed owner.
struct ErasedObservation {
    /// Selects the actor's registered observation entry.
    id: ObservationId,
    /// Announces publications and closes when either relevant authority ends.
    signal: Arc<ObservationSignal>,
    /// Transfers the first selection without requiring the value to be cloned.
    initial: ErasedValue,
    /// Identifies the commit that produced the initial selection.
    version: u64,
    /// Requests later values without retaining root or registered child authority.
    take_operation: ErasedTakeOperation,
}

impl ErasedObservation {
    /// Restores the projection's concrete value type at the erased boundary.
    fn into_typed<Value: Send + 'static>(self) -> Observation<Value> {
        let initial = *self
            .initial
            .downcast::<Value>()
            .expect("ScopedStore observation returned an unexpected initial type");
        let take_operation = Box::new(TypedTakeOperation {
            erased: self.take_operation,
            value: std::marker::PhantomData,
        });
        Observation::new(self.id, self.signal, initial, self.version, take_operation)
    }
}

/// Restores typed replies while preserving the reusable erased take operation.
struct TypedTakeOperation<Value> {
    /// Owns admission independently of the observation's current take future.
    erased: ErasedTakeOperation,
    /// Records the reply type without imposing `Value: Sync`.
    value: std::marker::PhantomData<fn() -> Value>,
}

impl<Value: Send + 'static> TakeOperation<Value> for TypedTakeOperation<Value> {
    fn take(self: Box<Self>) -> TakeFuture<Value> {
        TakeFuture::<Value>::Deferred(Box::pin(async move {
            let reply = match (self.erased)().await? {
                TakeReply::Value { version, value } => TakeReply::Value {
                    version,
                    value: *value
                        .downcast::<Value>()
                        .expect("ScopedStore observation returned an unexpected projected type"),
                },
                TakeReply::Empty { version } => TakeReply::Empty { version },
                TakeReply::Closed => TakeReply::Closed,
            };
            Ok(TakeOutcome::<Value> {
                reply,
                operation: self,
            })
        }))
    }
}

/// Hides the root domain while preserving ordered admission and child authority.
trait ScopedDriver<State, Action>: Send + Sync {
    /// Submits a child action and resolves its root commit result.
    fn send(&self, action: Action) -> ScopedSendFuture<'_>;
    /// Links one send's causal work to explicit predicate-wait cancellation.
    fn send_owned(
        &self,
        action: Action,
        cancellation: Arc<WaitCancellation>,
    ) -> ScopedFuture<Result<ActionTask, ScopedStoreEnded>>;
    /// Retains child lifetime, but only weak root authority, for an owned wait.
    fn wait_terminal(&self) -> WaitTerminal;
    /// Admits immediately or returns the original, unembedded action.
    fn try_send(&self, action: Action) -> Result<ActionReceipt, TryScopedSendError<Action>>;
    /// Queues a consuming projection under this route's authority.
    fn read(
        &self,
        projection: ReadProjection<State>,
    ) -> ScopedFuture<Result<ErasedValue, ScopedStoreEnded>>;
    /// Registers a projection and its publication policy at one mailbox position.
    fn observe(
        &self,
        projection: ObservationProjection<State>,
        policy: PublishPolicy<ErasedValue>,
    ) -> ScopedFuture<Result<ErasedObservation, ScopedStoreEnded>>;
}

/// Sends, reads and observes one child domain through its root Store.
///
/// Strong handles retain root authority. Registered handles additionally capture
/// one child lifetime, as defined by their scope constructor. Once that lifetime
/// ends, the route cannot be renewed.
///
/// Child reads and observation selectors run on the root actor.
/// If one panics, the root actor unwinds and closes the Store for every scope.
/// The panic propagates on the actor task. A request awaiting its reply can instead receive [`ScopedStoreEnded`].
pub struct ScopedStore<State, Action> {
    /// Shares the captured route without requiring child values to be `Sync`.
    driver: Arc<dyn ScopedDriver<State, Action>>,
}

impl<State, Action> ScopedStore<State, Action> {
    /// Wraps an existing route without registering or renewing child authority.
    fn new(driver: Arc<dyn ScopedDriver<State, Action>>) -> Self {
        Self { driver }
    }

    /// Sends one child action and waits for its root commit.
    ///
    /// Unidentified required routes embed when this future is first polled.
    /// Registered routes embed on the root actor after lifetime validation.
    ///
    /// # Errors
    ///
    /// Returns [`ScopedStoreEnded`] if the registered child lifetime ends or
    /// the root cannot complete the commit.
    ///
    /// # Panics
    ///
    /// For an unidentified required route, an embedding panic propagates from
    /// this future. Registered routes propagate embedding panics on the actor.
    pub async fn send(&self, action: Action) -> Result<ActionTask, ScopedStoreEnded> {
        self.driver.send(action).await
    }

    /// Waits until a current or subsequent child predicate selection is false.
    ///
    /// This owned wait retains the selected child lifetime but not root Store
    /// authority after registration. Removing or replacing that child completes
    /// with [`PredicateWaitOutcome::ScopeEnded`]; root closure takes precedence
    /// when both have ended. Unidentified required projections report root
    /// closure only.
    /// Selections have the latest-state semantics of [`PredicateWait`].
    pub fn wait_while<F>(&self, predicate: F) -> PredicateWait
    where
        State: 'static,
        Action: 'static,
        F: Fn(&State) -> bool + Send + 'static,
    {
        let store = self.clone();
        let terminal = self.driver.wait_terminal();
        PredicateWait::new(
            async move {
                let observation = store.observe(predicate).await;
                drop(store);
                match observation {
                    Ok(observation) => wait_for_false(observation, terminal).await,
                    Err(_) => terminal().unwrap_or(PredicateWaitOutcome::StoreStopped),
                }
            },
            None,
        )
    }

    /// Sends a child action, then waits until its predicate selection is false.
    ///
    /// Commit precedes registration, including synchronous post-send false.
    /// Explicit cancellation controls only this send's causal work; ordinary
    /// drop does not cancel an admitted action. Scope invalidation is checked
    /// before embedding or reduction. See [`Self::wait_while`] and
    /// [`PredicateWait::cancel`] for lifetime and cancellation semantics.
    pub fn send_and_wait_while<F>(&self, action: Action, predicate: F) -> PredicateWait
    where
        State: 'static,
        Action: Send + 'static,
        F: Fn(&State) -> bool + Send + 'static,
    {
        let store = self.clone();
        let terminal = self.driver.wait_terminal();
        let cancellation = Arc::new(WaitCancellation::default());
        let sending = Arc::clone(&cancellation);
        PredicateWait::new(
            async move {
                if store.driver.send_owned(action, sending).await.is_err() {
                    return terminal().unwrap_or(PredicateWaitOutcome::StoreStopped);
                }
                let observation = store.observe(predicate).await;
                drop(store);
                match observation {
                    Ok(observation) => wait_for_false(observation, terminal).await,
                    Err(_) => terminal().unwrap_or(PredicateWaitOutcome::StoreStopped),
                }
            },
            Some(cancellation),
        )
    }

    /// Attempts immediate child-action admission.
    ///
    /// Admission checks the current handle signal and mailbox capacity. The root
    /// actor revalidates the registered lifetime at this command's mailbox
    /// position. A preceding removal or replacement can therefore end the scope
    /// after this method returns a receipt. In that case,
    /// [`ActionReceipt::finish`] returns the coarse [`crate::StoreStopped`] error
    /// even though root Store authority may remain live.
    ///
    /// # Errors
    ///
    /// Retains `action` when ingress is full or the handle or root authority has
    /// already ended at admission.
    pub fn try_send(&self, action: Action) -> Result<ActionReceipt, TryScopedSendError<Action>> {
        self.driver.try_send(action)
    }

    /// Runs one child-state query at its ordered mailbox position.
    ///
    /// A panic in `query` closes the root Store, not just the child scope.
    ///
    /// # Errors
    ///
    /// Returns [`ScopedStoreEnded`] if the registered child lifetime ends or
    /// the root cannot return the query result.
    ///
    /// # Panics
    ///
    /// Panics if RCA's private erased projection invariant is violated.
    pub async fn read<F, Output>(&self, query: F) -> Result<Output, ScopedStoreEnded>
    where
        State: 'static,
        Action: 'static,
        F: FnOnce(&State) -> Output + Send + 'static,
        Output: Send + 'static,
    {
        let value = self
            .driver
            .read(Box::new(move |state| Box::new(query(state))))
            .await?;
        Ok(*value
            .downcast::<Output>()
            .expect("ScopedStore read projection returned an unexpected type"))
    }

    /// Registers a clone-free child projection at its mailbox position.
    ///
    /// If `selector` panics, the panic propagates on the root actor and closes the Store for every scope.
    ///
    /// # Errors
    ///
    /// Returns [`ScopedStoreEnded`] when child or root authority ends before
    /// registration.
    ///
    /// # Panics
    ///
    /// Panics if RCA's private erased projection invariant is violated.
    pub async fn observe<F, Value>(
        &self,
        selector: F,
    ) -> Result<Observation<Value>, ScopedStoreEnded>
    where
        State: 'static,
        Action: 'static,
        F: Fn(&State) -> Value + Send + 'static,
        Value: Send + 'static,
    {
        let projection: ObservationProjection<State> =
            Box::new(move |state| Box::new(selector(state)));
        self.driver
            .observe(projection, PublishPolicy::always())
            .await
            .map(ErasedObservation::into_typed)
    }

    /// Registers a distinct child projection at its mailbox position.
    ///
    /// The first value from [`Observation::next`] is the selection at registration.
    /// The actor retains a copy of the last published value for comparison.
    /// It does not clone an unchanged selection. A slow reader can miss intermediate changes.
    /// A selector panic closes the root Store as in [`Self::observe`].
    ///
    /// # Errors
    ///
    /// Returns [`ScopedStoreEnded`] when child or root authority ends before
    /// registration.
    ///
    /// # Panics
    ///
    /// Panics if RCA's private erased projection invariant is violated.
    pub async fn observe_distinct<F, Value>(
        &self,
        selector: F,
    ) -> Result<Observation<Value>, ScopedStoreEnded>
    where
        State: 'static,
        Action: 'static,
        F: Fn(&State) -> Value + Send + 'static,
        Value: Clone + PartialEq + Send + 'static,
    {
        self.register_distinct(selector, PartialEq::eq)
            .await
            .map(ErasedObservation::into_typed)
    }

    /// Observes the ordered IDs of a keyed collection in this child.
    ///
    /// If the projection selects the same collection, edits to its child values reuse the previous list.
    /// If a transaction replaces a row but preserves the full ID order, it publishes no new list.
    /// This follows the same rule as [`Store::observe_ordered_ids`].
    /// When the captured child scope ends, the observation closes.
    ///
    /// # Errors
    ///
    /// Returns [`ScopedStoreEnded`] if child or root authority ends before registration.
    ///
    /// # Panics
    ///
    /// Panics if RCA's private erased projection invariant is violated.
    pub async fn observe_ordered_ids<Id, Child>(
        &self,
        project: impl Fn(&State) -> &KeyedState<Id, Child> + Send + 'static,
    ) -> Result<Observation<Arc<[Id]>>, ScopedStoreEnded>
    where
        State: 'static,
        Action: 'static,
        Id: Clone + Eq + Send + Sync + 'static,
        Child: 'static,
    {
        self.register_distinct(KeyedState::ordered_ids_selector(project), Arc::ptr_eq)
            .await
            .map(ErasedObservation::into_typed)
    }

    /// Erases a typed projection and its equality check together before admission.
    fn register_distinct<Value>(
        &self,
        selector: impl Fn(&State) -> Value + Send + 'static,
        equal: impl Fn(&Value, &Value) -> bool + Send + 'static,
    ) -> ScopedFuture<Result<ErasedObservation, ScopedStoreEnded>>
    where
        State: 'static,
        Action: 'static,
        Value: Clone + Send + 'static,
    {
        let projection: ObservationProjection<State> =
            Box::new(move |state| Box::new(selector(state)));
        let policy = PublishPolicy::distinct_by(
            move |previous: &mut Option<ErasedValue>, value: &ErasedValue| {
                let value = value
                    .downcast_ref::<Value>()
                    .expect("ScopedStore distinct projection returned an unexpected type");
                match previous {
                    Some(previous) => {
                        let previous = previous
                            .downcast_mut::<Value>()
                            .expect("ScopedStore distinct projection retained an unexpected type");
                        if equal(previous, value) {
                            return false;
                        }
                        // Clone before dropping the old value, retaining its allocation.
                        *previous = value.clone();
                    }
                    None => *previous = Some(Box::new(value.clone())),
                }
                true
            },
        );
        self.driver.observe(projection, policy)
    }
}

impl<State, Action> Clone for ScopedStore<State, Action> {
    fn clone(&self) -> Self {
        Self {
            driver: Arc::clone(&self.driver),
        }
    }
}

impl<State, Action> fmt::Debug for ScopedStore<State, Action> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScopedStore").finish_non_exhaustive()
    }
}

/// Captures a required projection without registering a separate child lifetime.
pub(super) fn scope<
    ParentState,
    ParentAction,
    ChildState,
    ChildAction,
    Get,
    GetMut,
    Extract,
    Embed,
>(
    store: &Store<ParentState, ParentAction>,
    state_path: StatePath<ParentState, ChildState, Get, GetMut>,
    action_path: ActionPath<ParentAction, ChildAction, Extract, Embed>,
) -> ScopedStore<ChildState, ChildAction>
where
    ParentState: Send + 'static,
    ParentAction: Send + 'static,
    ChildState: Send + 'static,
    ChildAction: Send + 'static,
    Get: for<'a> Fn(&'a ParentState) -> &'a ChildState + Send + Sync + 'static,
    Embed: Fn(ChildAction) -> ParentAction + Send + Sync + 'static,
{
    let driver = RootScopedDriver {
        store: store.clone(),
        get: Arc::new(state_path.into_get()),
        embed: Arc::new(ScopedActionEmbed::new(action_path.into_embedder())),
        child: PhantomData,
    };
    ScopedStore::new(Arc::new(driver))
}

/// Registers the current identity of a required child projection.
pub(super) fn scope_identified<
    ParentState,
    ParentAction,
    ChildState,
    ChildAction,
    Get,
    GetMut,
    Extract,
    Embed,
    Identity,
    Id,
>(
    store: &Store<ParentState, ParentAction>,
    state_path: StatePath<ParentState, ChildState, Get, GetMut>,
    action_path: ActionPath<ParentAction, ChildAction, Extract, Embed>,
    identity: Identity,
) -> impl Future<Output = Result<ScopedStore<ChildState, ChildAction>, ScopedStoreEnded>> + Send + '_
where
    ParentState: Send + 'static,
    ParentAction: Send + 'static,
    ChildState: Send + 'static,
    ChildAction: Send + 'static,
    Get: for<'a> Fn(&'a ParentState) -> &'a ChildState + Send + 'static,
    Embed: Fn(ChildAction) -> ParentAction + Send + Sync + 'static,
    Identity: for<'a> Fn(&'a ChildState) -> &'a Id + Send + 'static,
    Id: Eq + Hash + Clone + Send + Sync + 'static,
{
    let get = state_path.into_get();
    let projection = Box::new(IdentifiedHandleSelector::new(move |state: &ParentState| {
        let child = get(state);
        Some((identity(child), child))
    }));
    registered_scope(
        store,
        projection,
        Arc::new(ScopedActionEmbed::<ChildAction, _>::new(
            action_path.into_embedder(),
        )),
    )
}

/// Registers the current presence of an optional child.
pub(super) fn scope_some<
    ParentState,
    ParentAction,
    ChildState,
    ChildAction,
    Get,
    GetMut,
    Extract,
    Embed,
>(
    store: &Store<ParentState, ParentAction>,
    state_path: OptionalStatePath<ParentState, ChildState, Get, GetMut>,
    action_path: ActionPath<ParentAction, ChildAction, Extract, Embed>,
) -> impl Future<Output = Result<ScopedStore<ChildState, ChildAction>, ScopedStoreEnded>> + Send + '_
where
    ParentState: Send + 'static,
    ParentAction: Send + 'static,
    ChildState: Send + 'static,
    ChildAction: Send + 'static,
    Get: for<'a> Fn(&'a ParentState) -> Option<&'a ChildState> + Send + 'static,
    Embed: Fn(ChildAction) -> ParentAction + Send + Sync + 'static,
{
    let get = state_path.into_get();
    let projection = Box::new(TypedHandleSelector::new(move |state: &ParentState| {
        get(state).map(|child| (ScopeIdentity::unit(), child))
    }));
    registered_scope(
        store,
        projection,
        Arc::new(ScopedActionEmbed::<ChildAction, _>::new(
            action_path.into_embedder(),
        )),
    )
}

/// Registers the current identity of a present optional child.
pub(super) fn scope_some_identified<
    ParentState,
    ParentAction,
    ChildState,
    ChildAction,
    Get,
    GetMut,
    Extract,
    Embed,
    Identity,
    Id,
>(
    store: &Store<ParentState, ParentAction>,
    state_path: OptionalStatePath<ParentState, ChildState, Get, GetMut>,
    action_path: ActionPath<ParentAction, ChildAction, Extract, Embed>,
    identity: Identity,
) -> impl Future<Output = Result<ScopedStore<ChildState, ChildAction>, ScopedStoreEnded>> + Send + '_
where
    ParentState: Send + 'static,
    ParentAction: Send + 'static,
    ChildState: Send + 'static,
    ChildAction: Send + 'static,
    Get: for<'a> Fn(&'a ParentState) -> Option<&'a ChildState> + Send + 'static,
    Embed: Fn(ChildAction) -> ParentAction + Send + Sync + 'static,
    Identity: for<'a> Fn(&'a ChildState) -> &'a Id + Send + 'static,
    Id: Eq + Hash + Clone + Send + Sync + 'static,
{
    let get = state_path.into_get();
    let projection = Box::new(IdentifiedHandleSelector::new(move |state: &ParentState| {
        get(state).map(|child| (identity(child), child))
    }));
    registered_scope(
        store,
        projection,
        Arc::new(ScopedActionEmbed::<ChildAction, _>::new(
            action_path.into_embedder(),
        )),
    )
}

/// Registers the current presence of one enum case's child.
pub(super) fn scope_case<
    ParentState,
    ParentAction,
    ChildState,
    ChildAction,
    Get,
    GetMut,
    Extract,
    Embed,
>(
    store: &Store<ParentState, ParentAction>,
    state_path: CaseStatePath<ParentState, ChildState, Get, GetMut>,
    action_path: ActionPath<ParentAction, ChildAction, Extract, Embed>,
) -> impl Future<Output = Result<ScopedStore<ChildState, ChildAction>, ScopedStoreEnded>> + Send + '_
where
    ParentState: Send + 'static,
    ParentAction: Send + 'static,
    ChildState: Send + 'static,
    ChildAction: Send + 'static,
    Get: for<'a> Fn(&'a ParentState) -> Option<&'a ChildState> + Send + 'static,
    Embed: Fn(ChildAction) -> ParentAction + Send + Sync + 'static,
{
    let get = state_path.into_get();
    let projection = Box::new(TypedHandleSelector::new(move |state: &ParentState| {
        get(state).map(|child| (ScopeIdentity::unit(), child))
    }));
    registered_scope(
        store,
        projection,
        Arc::new(ScopedActionEmbed::<ChildAction, _>::new(
            action_path.into_embedder(),
        )),
    )
}

/// Registers the current identity of one enum case's child.
pub(super) fn scope_case_identified<
    ParentState,
    ParentAction,
    ChildState,
    ChildAction,
    Get,
    GetMut,
    Extract,
    Embed,
    Identity,
    Id,
>(
    store: &Store<ParentState, ParentAction>,
    state_path: CaseStatePath<ParentState, ChildState, Get, GetMut>,
    action_path: ActionPath<ParentAction, ChildAction, Extract, Embed>,
    identity: Identity,
) -> impl Future<Output = Result<ScopedStore<ChildState, ChildAction>, ScopedStoreEnded>> + Send + '_
where
    ParentState: Send + 'static,
    ParentAction: Send + 'static,
    ChildState: Send + 'static,
    ChildAction: Send + 'static,
    Get: for<'a> Fn(&'a ParentState) -> Option<&'a ChildState> + Send + 'static,
    Embed: Fn(ChildAction) -> ParentAction + Send + Sync + 'static,
    Identity: for<'a> Fn(&'a ChildState) -> &'a Id + Send + 'static,
    Id: Eq + Hash + Clone + Send + Sync + 'static,
{
    let get = state_path.into_get();
    let projection = Box::new(IdentifiedHandleSelector::new(move |state: &ParentState| {
        get(state).map(|child| (identity(child), child))
    }));
    registered_scope(
        store,
        projection,
        Arc::new(ScopedActionEmbed::<ChildAction, _>::new(
            action_path.into_embedder(),
        )),
    )
}

/// Registers a keyed entry's insertion lifetime, not merely its logical key.
pub(super) fn scope_keyed<
    ParentState,
    ParentAction,
    ChildState,
    ChildAction,
    Get,
    GetMut,
    Extract,
    Embed,
    Id,
>(
    store: &Store<ParentState, ParentAction>,
    state_path: KeyedStatePath<ParentState, Id, ChildState, Get, GetMut>,
    action_path: KeyedActionPath<ParentAction, Id, ChildAction, Extract, Embed>,
    id: Id,
) -> impl Future<Output = Result<ScopedStore<ChildState, ChildAction>, ScopedStoreEnded>> + Send + '_
where
    ParentState: Send + 'static,
    ParentAction: Send + 'static,
    ChildState: Send + 'static,
    ChildAction: Send + 'static,
    Get: for<'a> Fn(&'a ParentState) -> &'a KeyedState<Id, ChildState> + Send + 'static,
    Embed: Fn(KeyedAction<Id, ChildAction>) -> ParentAction + Send + Sync + 'static,
    Id: Eq + Hash + Clone + Send + Sync + 'static,
{
    let get = state_path.into_get();
    let projection_id = id.clone();
    let projection = Box::new(TypedHandleSelector::new(move |state: &ParentState| {
        get(state)
            .get_with_stamp(&projection_id)
            .map(|(child, stamp)| (ScopeIdentity::new(stamp.to_owned()), child))
    }));
    let embed = action_path.into_embedder();
    let action_id = id;
    let embed = Arc::new(ScopedActionEmbed::<ChildAction, _>::new(move |action| {
        embed(KeyedAction::new(action_id.clone(), action))
    }));
    registered_scope(store, projection, embed)
}

/// Registers a stack entry's current lifetime and routes its element actions.
pub(super) fn scope_stack<
    ParentState,
    ParentAction,
    ChildState,
    ChildAction,
    Push,
    Get,
    GetMut,
    Extract,
    Embed,
>(
    store: &Store<ParentState, ParentAction>,
    state_path: StatePath<ParentState, StackState<ChildState>, Get, GetMut>,
    action_path: ActionPath<ParentAction, StackAction<Push, ChildAction>, Extract, Embed>,
    id: StackId,
) -> impl Future<Output = Result<ScopedStore<ChildState, ChildAction>, ScopedStoreEnded>> + Send + '_
where
    ParentState: Send + 'static,
    ParentAction: Send + 'static,
    ChildState: Send + 'static,
    ChildAction: Send + 'static,
    Get: for<'a> Fn(&'a ParentState) -> &'a StackState<ChildState> + Send + 'static,
    Embed: Fn(StackAction<Push, ChildAction>) -> ParentAction + Send + Sync + 'static,
{
    let get = state_path.into_get();
    let projection_id = id.clone();
    let projection = Box::new(TypedHandleSelector::new(move |state: &ParentState| {
        get(state)
            .get_with_stamp(&projection_id)
            .map(|(child, stamp)| (ScopeIdentity::new(stamp.to_owned()), child))
    }));
    let embed = action_path.into_embedder();
    let embed = Arc::new(ScopedActionEmbed::<ChildAction, _>::new(move |action| {
        embed(StackAction::Element {
            id: id.clone(),
            action,
        })
    }));
    registered_scope(store, projection, embed)
}

/// Installs one lifetime selector and transfers acknowledged authority to a driver.
async fn registered_scope<ParentState, ParentAction, ChildState, ChildAction, Embed>(
    store: &Store<ParentState, ParentAction>,
    identity: Box<dyn HandleSelector<ParentState> + Send>,
    embed: Arc<ScopedActionEmbed<ChildAction, Embed>>,
) -> Result<ScopedStore<ChildState, ChildAction>, ScopedStoreEnded>
where
    ParentState: Send + 'static,
    ParentAction: Send + 'static,
    ChildState: Send + 'static,
    ChildAction: Send + 'static,
    Embed: Fn(ChildAction) -> ParentAction + Send + Sync + 'static,
{
    let signal = HandleSignal::new(Arc::downgrade(&store.ingress.control));
    let mut guard = HandleRegistrationGuard::new(Arc::clone(&signal));
    let (reply, receive) = futures_channel::oneshot::channel();
    store
        .ingress
        .send(Command::RegisterHandle(HandleRegistrationCommand {
            identity,
            signal,
            reply,
        }))
        .await
        .map_err(|_closed| ScopedStoreEnded::store_stopped(crate::StoreStopped::new()))?;
    let handle = match receive.await {
        Ok(Ok(handle)) => handle,
        Ok(Err(())) => return Err(ScopedStoreEnded::scope_ended()),
        Err(_closed) => {
            return Err(ScopedStoreEnded::store_stopped(crate::StoreStopped::new()));
        }
    };
    guard.disarm();
    Ok(ScopedStore::new(Arc::new(RegisteredScopedDriver {
        store: store.clone(),
        embed,
        handle,
        child: PhantomData,
    })))
}

/// Retires cancelled registrations, including acknowledgements not yet received.
struct HandleRegistrationGuard {
    /// Reaches the actor's registration even while its acknowledgement is queued.
    signal: Arc<HandleSignal>,
    /// Stays set until the receiving driver takes ownership of the lifetime.
    armed: bool,
}

impl HandleRegistrationGuard {
    /// Arms cancellation before registration can enter the mailbox.
    fn new(signal: Arc<HandleSignal>) -> Self {
        Self {
            signal,
            armed: true,
        }
    }

    /// Hands responsibility to the acknowledged scoped driver.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for HandleRegistrationGuard {
    fn drop(&mut self) {
        if self.armed {
            self.signal.cancel_registration();
        }
    }
}

/// Routes a required projection whose authority lasts as long as the root's.
struct RootScopedDriver<ParentState, ParentAction, ChildState, ChildAction, Get, Embed> {
    /// Keeps the root able to accept child work.
    store: Store<ParentState, ParentAction>,
    /// Shares the required projection with queued queries and observations.
    get: Arc<Get>,
    /// Shares embedding with queued actions, including after the handle is dropped.
    embed: Arc<ScopedActionEmbed<ChildAction, Embed>>,
    /// Records the domain without imposing ownership or `Sync` bounds on its values.
    child: PhantomData<fn(ChildState, ChildAction)>,
}

impl<ParentState, ParentAction, ChildState, ChildAction, Get, Embed>
    ScopedDriver<ChildState, ChildAction>
    for RootScopedDriver<ParentState, ParentAction, ChildState, ChildAction, Get, Embed>
where
    ParentState: Send + 'static,
    ParentAction: Send + 'static,
    ChildState: Send + 'static,
    ChildAction: Send + 'static,
    Get: for<'a> Fn(&'a ParentState) -> &'a ChildState + Send + Sync + 'static,
    Embed: Fn(ChildAction) -> ParentAction + Send + Sync + 'static,
{
    fn send(&self, action: ChildAction) -> ScopedSendFuture<'_> {
        let store = &self.store;
        let action = self.embed.apply(action);
        Box::pin(async move {
            store
                .send(action)
                .await
                .map_err(ScopedStoreEnded::store_stopped)
        })
    }

    fn send_owned(
        &self,
        action: ChildAction,
        cancellation: Arc<WaitCancellation>,
    ) -> ScopedFuture<Result<ActionTask, ScopedStoreEnded>> {
        let store = self.store.clone();
        let embed = Arc::clone(&self.embed);
        Box::pin(send_owned_scoped(store, action, embed, None, cancellation))
    }

    fn wait_terminal(&self) -> WaitTerminal {
        super::root_wait_terminal(&self.store.lifecycle)
    }

    fn try_send(
        &self,
        action: ChildAction,
    ) -> Result<ActionReceipt, TryScopedSendError<ChildAction>> {
        let ingress = &self.store.ingress;
        let (reply, receive) = crate::store::command::commit_channel();
        let embed = Arc::clone(&self.embed);
        match ingress.try_send_map(
            (Box::new(DeferredAction { action, embed }), reply),
            |(action, reply)| Command::Action(ActionCommand::scoped_boxed(action, reply, None)),
        ) {
            Ok(()) => Ok(ActionReceipt::new(receive)),
            Err(mailbox::TrySendError::Full((action, _reply))) => {
                Err(TryScopedSendError::full(action.into_inner().action))
            }
            Err(mailbox::TrySendError::Disconnected((action, _reply))) => Err(
                TryScopedSendError::store_stopped(action.into_inner().action),
            ),
        }
    }

    fn read(
        &self,
        projection: ReadProjection<ChildState>,
    ) -> ScopedFuture<Result<ErasedValue, ScopedStoreEnded>> {
        let store = self.store.clone();
        let get = Arc::clone(&self.get);
        Box::pin(async move {
            store
                .read(move |parent: &ParentState| projection(get(parent)))
                .await
                .map_err(ScopedStoreEnded::store_stopped)
        })
    }

    fn observe(
        &self,
        projection: ObservationProjection<ChildState>,
        policy: PublishPolicy<ErasedValue>,
    ) -> ScopedFuture<Result<ErasedObservation, ScopedStoreEnded>> {
        let store = self.store.clone();
        let get = Arc::clone(&self.get);
        register_erased_observation(
            store,
            move |parent: &ParentState| projection((get.as_ref())(parent)),
            policy,
            None,
        )
    }
}

/// Routes operations through one registered lifetime and a retained root Store.
struct RegisteredScopedDriver<ParentState, ParentAction, ChildState, ChildAction, Embed> {
    /// Keeps root authority alive independently of the originally borrowed Store.
    store: Store<ParentState, ParentAction>,
    /// Defers child embedding until the action's lifetime check succeeds.
    embed: Arc<ScopedActionEmbed<ChildAction, Embed>>,
    /// Retains this generation without renewing it after removal or replacement.
    handle: HandleScope,
    /// Records the erased child domain without imposing `Sync` on its values.
    child: PhantomData<fn(ChildState, ChildAction)>,
}

impl<ParentState, ParentAction, ChildState, ChildAction, Embed>
    ScopedDriver<ChildState, ChildAction>
    for RegisteredScopedDriver<ParentState, ParentAction, ChildState, ChildAction, Embed>
where
    ParentState: Send + 'static,
    ParentAction: Send + 'static,
    ChildState: Send + 'static,
    ChildAction: Send + 'static,
    Embed: Fn(ChildAction) -> ParentAction + Send + Sync + 'static,
{
    fn send(&self, action: ChildAction) -> ScopedSendFuture<'_> {
        if self.handle.is_ended() {
            return Box::pin(std::future::ready(Err(ScopedStoreEnded::scope_ended())));
        }
        let store = self.store.clone();
        let embed = Arc::clone(&self.embed);
        let handle = self.handle.clone();
        Box::pin(async move {
            let (reply, receive) = crate::store::command::commit_channel();
            store
                .ingress
                .send(Command::Action(ActionCommand::scoped(
                    action,
                    reply,
                    embed,
                    Some(handle),
                )))
                .await
                .map_err(|_closed| ScopedStoreEnded::store_stopped(crate::StoreStopped::new()))?;
            match receive.await {
                Ok(Ok(task)) => Ok(task),
                Ok(Err(CommitError::ScopeEnded)) => Err(ScopedStoreEnded::scope_ended()),
                Ok(Err(CommitError::Cancelled | CommitError::InputClosed)) | Err(_) => {
                    Err(ScopedStoreEnded::store_stopped(crate::StoreStopped::new()))
                }
            }
        })
    }

    fn send_owned(
        &self,
        action: ChildAction,
        cancellation: Arc<WaitCancellation>,
    ) -> ScopedFuture<Result<ActionTask, ScopedStoreEnded>> {
        let store = self.store.clone();
        let embed = Arc::clone(&self.embed);
        let handle = self.handle.clone();
        Box::pin(send_owned_scoped(
            store,
            action,
            embed,
            Some(handle),
            cancellation,
        ))
    }

    fn wait_terminal(&self) -> WaitTerminal {
        let lifecycle = Arc::downgrade(&self.store.lifecycle);
        let handle = self.handle.clone();
        Box::new(move || {
            if lifecycle.upgrade().is_none_or(|root| root.is_closed()) {
                Some(PredicateWaitOutcome::StoreStopped)
            } else if handle.is_ended() {
                Some(PredicateWaitOutcome::ScopeEnded)
            } else {
                None
            }
        })
    }

    fn try_send(
        &self,
        action: ChildAction,
    ) -> Result<ActionReceipt, TryScopedSendError<ChildAction>> {
        if self.handle.is_ended() {
            return Err(TryScopedSendError::scope_ended(action));
        }
        let (reply, receive) = crate::store::command::commit_channel();
        let embed = Arc::clone(&self.embed);
        match self.store.ingress.try_send_map(
            (
                Box::new(DeferredAction { action, embed }),
                reply,
                Some(self.handle.clone()),
            ),
            |(action, reply, handle)| {
                Command::Action(ActionCommand::scoped_boxed(action, reply, handle))
            },
        ) {
            Ok(()) => Ok(ActionReceipt::new(receive)),
            Err(mailbox::TrySendError::Full((action, _reply, _handle))) => {
                Err(TryScopedSendError::full(action.into_inner().action))
            }
            Err(mailbox::TrySendError::Disconnected((action, _reply, _handle))) => Err(
                TryScopedSendError::store_stopped(action.into_inner().action),
            ),
        }
    }

    fn read(
        &self,
        projection: ReadProjection<ChildState>,
    ) -> ScopedFuture<Result<ErasedValue, ScopedStoreEnded>> {
        if self.handle.is_ended() {
            return Box::pin(std::future::ready(Err(ScopedStoreEnded::scope_ended())));
        }
        let store = self.store.clone();
        let handle = self.handle.clone();
        Box::pin(async move {
            let (command, receive) = new_checked_read(move |_: &ParentState, selected| {
                projection(selected.downcast::<ChildState>())
            });
            store
                .ingress
                .send(Command::ScopedRead {
                    scope: handle.clone(),
                    command,
                })
                .await
                .map_err(|_closed| ScopedStoreEnded::store_stopped(crate::StoreStopped::new()))?;
            receive.await.map_err(|_closed| {
                if handle.is_ended() {
                    ScopedStoreEnded::scope_ended()
                } else {
                    ScopedStoreEnded::store_stopped(crate::StoreStopped::new())
                }
            })
        })
    }

    fn observe(
        &self,
        projection: ObservationProjection<ChildState>,
        policy: PublishPolicy<ErasedValue>,
    ) -> ScopedFuture<Result<ErasedObservation, ScopedStoreEnded>> {
        if self.handle.is_ended() {
            return Box::pin(std::future::ready(Err(ScopedStoreEnded::scope_ended())));
        }
        let store = self.store.clone();
        let handle = self.handle.clone();
        register_erased_observation(
            store,
            CheckedSelector::new(move |_: &ParentState, selected| {
                projection(selected.downcast::<ChildState>())
            }),
            policy,
            Some(handle),
        )
    }
}

/// Registers under strong authority, then returns an adapter holding weak authority.
fn register_erased_observation<State, Action>(
    store: Store<State, Action>,
    selector: impl ObservationSelection<State, ErasedValue> + Send + 'static,
    policy: PublishPolicy<ErasedValue>,
    handle: Option<HandleScope>,
) -> ScopedFuture<Result<ErasedObservation, ScopedStoreEnded>>
where
    State: Send + 'static,
    Action: Send + 'static,
{
    Box::pin(async move {
        let signal = ObservationSignal::new(Arc::downgrade(&store.ingress.control));
        let mut guard = RegistrationGuard::new(Arc::clone(&signal));
        let (command, receive) =
            new_registration_with_selection(selector, Arc::clone(&signal), policy);
        let actor_command = match handle.as_ref() {
            Some(scope) => Command::ScopedObservation {
                scope: scope.clone(),
                command,
            },
            None => Command::Observation(command),
        };
        store
            .ingress
            .send(actor_command)
            .await
            .map_err(|_closed| ScopedStoreEnded::store_stopped(crate::StoreStopped::new()))?;
        let registration = receive.await.map_err(|_closed| {
            if handle.as_ref().is_some_and(HandleScope::is_ended) {
                ScopedStoreEnded::scope_ended()
            } else {
                ScopedStoreEnded::store_stopped(crate::StoreStopped::new())
            }
        })?;

        let weak_ingress = Arc::downgrade(&store.ingress);
        let weak_handle = handle.as_ref().map(HandleScope::downgrade);
        let id = registration.id;
        let take_operation: ErasedTakeOperation = Box::new(move || {
            let weak_ingress = std::sync::Weak::clone(&weak_ingress);
            let weak_handle = weak_handle.clone();
            Box::pin(async move {
                let ingress = weak_ingress
                    .upgrade()
                    .ok_or_else(crate::StoreStopped::new)?;
                let (command, receive) = new_take(id);
                let command = match weak_handle.as_ref() {
                    Some(handle) => {
                        let Some(scope) = handle.upgrade() else {
                            return Ok(TakeReply::Closed);
                        };
                        Command::ScopedObservation { scope, command }
                    }
                    None => Command::Observation(command),
                };
                ingress
                    .send(command)
                    .await
                    .map_err(|_closed| crate::StoreStopped::new())?;
                match receive.await {
                    Ok(reply) => Ok(reply),
                    Err(_closed)
                        if weak_handle
                            .as_ref()
                            .is_some_and(|handle| !handle.is_active()) =>
                    {
                        Ok(TakeReply::Closed)
                    }
                    Err(_closed) => Err(crate::StoreStopped::new()),
                }
            })
        });
        guard.disarm();
        Ok(ErasedObservation {
            id: registration.id,
            signal,
            initial: registration.initial,
            version: registration.version,
            take_operation,
        })
    })
}

/// Submits an owned scoped action and waits for its commit.
async fn send_owned_scoped<State, Action, ChildAction, Embed>(
    store: Store<State, Action>,
    action: ChildAction,
    embed: Arc<Embed>,
    handle: Option<HandleScope>,
    cancellation: Arc<WaitCancellation>,
) -> Result<ActionTask, ScopedStoreEnded>
where
    ChildAction: Send + 'static,
    Embed: ActionEmbed<ChildAction, Action> + ?Sized + 'static,
{
    if handle.as_ref().is_some_and(HandleScope::is_ended) {
        return Err(ScopedStoreEnded::scope_ended());
    }
    let (reply, receive) = crate::store::command::commit_channel();
    let command = ActionCommand::scoped(action, reply, embed, handle);
    let command = Command::OwnedAction(Box::new(OwnedActionCommand {
        command,
        cancellation,
    }));
    store
        .ingress
        .send(command)
        .await
        .map_err(|_closed| ScopedStoreEnded::store_stopped(crate::StoreStopped::new()))?;
    match receive.await {
        Ok(Ok(task)) => Ok(task),
        Ok(Err(CommitError::ScopeEnded)) => Err(ScopedStoreEnded::scope_ended()),
        Ok(Err(CommitError::Cancelled | CommitError::InputClosed)) | Err(_) => {
            Err(ScopedStoreEnded::store_stopped(crate::StoreStopped::new()))
        }
    }
}

#[cfg(all(test, not(loom), feature = "allocation-measurement"))]
pub(crate) mod allocation_tests;

#[cfg(all(test, not(loom)))]
ordinary_test_items! {
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::task::{Context, Poll};

    use futures_util::FutureExt as _;

    use super::{register_erased_observation, ErasedValue, TypedTakeOperation};
    use crate::observation::{TakeOperation, TakeReply};
    use crate::store::observation::PublishPolicy;
    use crate::test_support as support;
    use crate::{Effect, Reduce, Store};

    #[test]
    fn required_send_borrows_existing_store_ownership() {
        use std::cell::Cell;

        fn assert_send<T: Send>(_: &T) {}

        let runtime = support::ManualRuntime::new();
        let store = Store::new(
            Cell::new(0),
            Reduce::new(|state: &mut Cell<usize>, action: &Cell<usize>, _| {
                state.set(state.get() + action.get());
                Effect::none()
            }),
            runtime.runtime(),
        );
        let child = store.scope(crate::identity_state_path(), crate::identity_action_path());
        let mut actor = runtime.take_only_queued_task();
        let mut context = Context::from_waker(std::task::Waker::noop());
        let lifecycle_references = Arc::strong_count(&store.lifecycle);
        let ingress_references = Arc::strong_count(&store.ingress);
        {
            let send = child.send(Cell::new(7));
            assert_send(&send);
            let mut send = std::pin::pin!(send);
            assert!(send.as_mut().poll(&mut context).is_pending());
            assert_eq!(
                Arc::strong_count(&store.lifecycle),
                lifecycle_references,
                "a pending required send must borrow its existing lifecycle ownership",
            );
            assert_eq!(
                Arc::strong_count(&store.ingress),
                ingress_references,
                "a pending required send must borrow its existing ingress ownership",
            );
            assert!(actor.as_mut().poll(&mut context).is_pending());
            let Poll::Ready(Ok(task)) = send.as_mut().poll(&mut context) else {
                panic!("the actor must acknowledge the required child action");
            };
            assert!(task.is_finished());
        };
        let mut read = Box::pin(store.read(Cell::get));
        assert!(read.as_mut().poll(&mut context).is_pending());
        assert!(actor.as_mut().poll(&mut context).is_pending());
        assert!(matches!(read.as_mut().poll(&mut context), Poll::Ready(Ok(7))));
    }

    /// Records disposal without requiring a clone of the owned input.
    struct AdmissionPayload {
        value: Box<usize>,
        drops: Arc<AtomicUsize>,
    }

    impl Drop for AdmissionPayload {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Retains the real actor separately from its registered child authority.
    struct AdmissionFixture {
        store: Store<usize, Option<AdmissionPayload>>,
        child: super::ScopedStore<usize, AdmissionPayload>,
        actor: crate::RuntimeFuture,
        embeddings: Arc<AtomicUsize>,
        reductions: Arc<AtomicUsize>,
    }

    /// Registers one identity whose replacement is an ordered root action.
    fn admission_fixture(capacity: usize) -> AdmissionFixture {
        let runtime = support::ManualRuntime::new();
        let reductions = Arc::new(AtomicUsize::new(0));
        let reduced = Arc::clone(&reductions);
        let store = Store::builder(
            7,
            Reduce::new(move |state: &mut usize, action: &Option<AdmissionPayload>, _| {
                if let Some(action) = action {
                    reduced.fetch_add(*action.value, Ordering::SeqCst);
                } else {
                    *state += 1;
                }
                Effect::none()
            }),
            runtime.runtime(),
        )
        .mailbox_capacity(capacity)
        .build()
        .unwrap();
        let embeddings = Arc::new(AtomicUsize::new(0));
        let embedded = Arc::clone(&embeddings);
        let mut registration = Box::pin(super::registered_scope::<_, _, usize, _, _>(
            &store,
            Box::new(super::TypedHandleSelector::new(|state: &usize| {
                Some((super::ScopeIdentity::new(*state), state))
            })),
            Arc::new(super::ScopedActionEmbed::new(move |action| {
                embedded.fetch_add(1, Ordering::SeqCst);
                Some(action)
            })),
        ));
        let mut actor = runtime.take_only_queued_task();
        let mut context = Context::from_waker(std::task::Waker::noop());
        assert!(registration.as_mut().poll(&mut context).is_pending());
        assert!(actor.as_mut().poll(&mut context).is_pending());
        let Poll::Ready(Ok(child)) = registration.as_mut().poll(&mut context) else {
            panic!("the actor must acknowledge the registered child");
        };
        drop(registration);
        assert_eq!(runtime.spawn_count(), 1);
        AdmissionFixture {
            store,
            child,
            actor,
            embeddings,
            reductions,
        }
    }

    #[test]
    fn closed_ingress_rejects_scoped_registration_without_entering_selectors() {
        let runtime = support::ManualRuntime::new();
        let store = Store::new(
            7,
            Reduce::new(|_: &mut usize, (): &(), _| Effect::none()),
            runtime.runtime(),
        );
        let actor = runtime.take_only_queued_task();
        assert!(store.ingress.close());
        assert!(!store.is_closed(), "actor teardown has not run");
        let selections = Arc::new(AtomicUsize::new(0));
        let selected = Arc::clone(&selections);
        let capture = Arc::new(());
        let weak_capture = Arc::downgrade(&capture);
        let registration = super::registered_scope::<_, _, usize, (), _>(
            &store,
            Box::new(super::TypedHandleSelector::new(move |state: &usize| {
                std::hint::black_box(&capture);
                selected.fetch_add(1, Ordering::SeqCst);
                Some((super::ScopeIdentity::unit(), state))
            })),
            Arc::new(super::ScopedActionEmbed::new(|()| ())),
        );
        let error = registration.now_or_never().unwrap().unwrap_err();
        assert!(error.is_store_stopped());
        assert_eq!(selections.load(Ordering::SeqCst), 0);
        assert!(weak_capture.upgrade().is_none());

        let selected = Arc::clone(&selections);
        let capture = Arc::new(());
        let weak_capture = Arc::downgrade(&capture);
        let observation = register_erased_observation(
            store.clone(),
            move |state: &usize| {
                std::hint::black_box(&capture);
                selected.fetch_add(1, Ordering::SeqCst);
                Box::new(*state) as ErasedValue
            },
            PublishPolicy::always(),
            None,
        );
        let Some(Err(error)) = observation.now_or_never() else {
            panic!("closed transport must reject observation registration immediately");
        };
        assert!(error.is_store_stopped());
        assert_eq!(selections.load(Ordering::SeqCst), 0);
        assert!(weak_capture.upgrade().is_none());
        drop(actor);
        assert!(store.is_closed());
        assert_eq!(runtime.spawn_count(), 1);
    }

    #[test]
    fn registered_try_send_preserves_input_on_full_or_closed_transport() {
        for close_transport in [false, true] {
            let AdmissionFixture {
                store,
                child,
                mut actor,
                embeddings,
                reductions,
            } = admission_fixture(1);
            let queued = if close_transport {
                assert!(store.ingress.close());
                assert!(
                    !store.is_closed(),
                    "registered authority remains live before teardown"
                );
                None
            } else {
                Some(store.try_send(None).unwrap())
            };
            let drops = Arc::new(AtomicUsize::new(0));
            let payload = AdmissionPayload {
                value: Box::new(31),
                drops: Arc::clone(&drops),
            };
            let identity = (&raw const *payload.value).addr();
            let error = child.try_send(payload).unwrap_err();
            assert_eq!(error.is_store_stopped(), close_transport);
            assert_eq!(error.is_full(), !close_transport);
            assert!(!error.is_scope_ended());
            assert_eq!((&raw const *error.action().value).addr(), identity);
            let returned = error.into_action();
            assert_eq!(*returned.value, 31);
            assert_eq!(embeddings.load(Ordering::SeqCst), 0);
            assert_eq!(reductions.load(Ordering::SeqCst), 0);
            assert_eq!(drops.load(Ordering::SeqCst), 0);
            drop(returned);
            assert_eq!(drops.load(Ordering::SeqCst), 1);

            if let Some(queued) = queued {
                let mut context = Context::from_waker(std::task::Waker::noop());
                assert!(actor.as_mut().poll(&mut context).is_pending());
                assert!(queued.finish().now_or_never().unwrap().unwrap().is_finished());
                let mut read = Box::pin(store.read(|state| *state));
                assert!(read.as_mut().poll(&mut context).is_pending());
                assert!(actor.as_mut().poll(&mut context).is_pending());
                assert!(matches!(read.as_mut().poll(&mut context), Poll::Ready(Ok(8))));
            }
            drop(actor);
            assert!(store.is_closed());
            assert_eq!(embeddings.load(Ordering::SeqCst), 0);
            assert_eq!(reductions.load(Ordering::SeqCst), 0);
            assert_eq!(drops.load(Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn owned_scoped_send_rejects_closed_transport_or_preceding_replacement() {
        for close_transport in [false, true] {
            let AdmissionFixture {
                store,
                child,
                mut actor,
                embeddings,
                reductions,
            } = admission_fixture(2);
            let queued = if close_transport {
                assert!(store.ingress.close());
                None
            } else {
                Some(store.try_send(None).unwrap())
            };
            let drops = Arc::new(AtomicUsize::new(0));
            let mut send = child.driver.send_owned(
                AdmissionPayload {
                    value: Box::new(37),
                    drops: Arc::clone(&drops),
                },
                Arc::new(super::WaitCancellation::default()),
            );
            let mut context = Context::from_waker(std::task::Waker::noop());
            if !close_transport {
                assert!(send.as_mut().poll(&mut context).is_pending());
                assert_eq!(drops.load(Ordering::SeqCst), 0);
                assert!(actor.as_mut().poll(&mut context).is_pending());
            }
            let Poll::Ready(Err(error)) = send.as_mut().poll(&mut context) else {
                panic!("owned input must fail at its ordered admission or lifetime boundary");
            };
            assert_eq!(error.is_store_stopped(), close_transport);
            assert_eq!(error.is_scope_ended(), !close_transport);
            assert_eq!(embeddings.load(Ordering::SeqCst), 0);
            assert_eq!(reductions.load(Ordering::SeqCst), 0);
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            drop(send);
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            if let Some(queued) = queued {
                assert!(queued.finish().now_or_never().unwrap().unwrap().is_finished());
                assert!(!store.is_closed());
            }
            drop(actor);
            assert!(store.is_closed());
        }
    }

    #[test]
    fn cancelled_handle_ack_is_retired_without_reselecting() {
        use super::{HandleRegistrationCommand, HandleRegistrationGuard, HandleSignal, ScopeIdentity};
        use crate::store::scope::HandleRegistry;

        let control = crate::effect::lineage::ControlWake::new();
        let signal = HandleSignal::new(Arc::downgrade(&control));
        let signal_weak = Arc::downgrade(&signal);
        let guard = HandleRegistrationGuard::new(Arc::clone(&signal));
        let selections = Arc::new(AtomicUsize::new(0));
        let selected = Arc::clone(&selections);
        let capture = Arc::new(());
        let capture_weak = Arc::downgrade(&capture);
        let (reply, mut receive) = futures_channel::oneshot::channel();
        let mut registry = HandleRegistry::new();
        registry.register(
            &(),
            HandleRegistrationCommand {
                identity: Box::new(super::TypedHandleSelector::new(move |state: &()| {
                    std::hint::black_box(&capture);
                    selected.fetch_add(1, Ordering::Relaxed);
                    Some((ScopeIdentity::unit(), state))
                })),
                signal,
                reply,
            },
        );
        assert_eq!(selections.load(Ordering::Relaxed), 1);
        assert!(capture_weak.upgrade().is_some());
        assert!(!signal_weak.upgrade().unwrap().is_ended());

        // The guard cancels, but the queued acknowledgement still owns the signal.
        drop(guard);
        registry.refresh(&());
        assert_eq!(selections.load(Ordering::Relaxed), 1);
        assert!(signal_weak.upgrade().unwrap().is_ended());
        assert!(capture_weak.upgrade().is_none(), "retirement drops the selector capture");

        let handle = receive
            .try_recv()
            .expect("the reply was sent")
            .expect("the acknowledged handle remains available")
            .expect("the registration was initially accepted");
        assert!(handle.is_ended());
        drop(handle);
        assert!(signal_weak.upgrade().is_none());
    }

    #[test]
    fn retained_erased_take_reports_stopped_after_actor_loss() {
        for (release_store, invoke_private) in
            [(false, false), (false, true), (true, false), (true, true)]
        {
            let runtime = support::ManualRuntime::new();
            let store = Store::new(
                7,
                Reduce::new(|_: &mut usize, (): &(), _| Effect::none()),
                runtime.runtime(),
            );
            let weak_ingress = Arc::downgrade(&store.ingress);
            let mut actor = runtime.take_only_queued_task();
            let mut context = Context::from_waker(std::task::Waker::noop());
            let selections = Arc::new(AtomicUsize::new(0));
            let selected = Arc::clone(&selections);
            let capture = Arc::new(());
            let weak_capture = Arc::downgrade(&capture);
            let mut registration = register_erased_observation(
                store.clone(),
                move |state: &usize| {
                    std::hint::black_box(&capture);
                    selected.fetch_add(1, Ordering::Relaxed);
                    Box::new(*state) as ErasedValue
                },
                PublishPolicy::always(),
                None,
            );
            assert!(registration.as_mut().poll(&mut context).is_pending());
            assert!(actor.as_mut().poll(&mut context).is_pending());
            let Poll::Ready(Ok(registered)) = registration.as_mut().poll(&mut context) else {
                panic!("the actor must acknowledge real registration");
            };
            drop(registration);
            let (operation, mut values) = if invoke_private {
                assert_eq!(*registered.initial.downcast::<usize>().unwrap(), 7);
                (Some(registered.take_operation), None)
            } else {
                let mut values = registered.into_typed::<usize>();
                assert_eq!(values.next().now_or_never(), Some(Some(7)));
                (None, Some(values))
            };
            assert_eq!(selections.load(Ordering::Relaxed), 1);
            assert!(weak_capture.upgrade().is_some());

            // Retain the real protocol operation across actor loss. Public
            // next() normally observes the closed signal before invoking it.
            drop(actor);
            let mut retained_store = Some(store);
            if release_store {
                drop(retained_store.take());
            }
            assert_eq!(weak_ingress.upgrade().is_some(), !release_store);
            assert!(weak_capture.upgrade().is_none());
            if let Some(operation) = operation {
                assert!(matches!(operation().now_or_never(), Some(Err(_))));
            }
            assert_eq!(selections.load(Ordering::Relaxed), 1);
            if let Some(values) = values.as_mut() {
                assert_eq!(values.next().now_or_never(), Some(None));
            }
            drop(retained_store);
            assert!(weak_ingress.upgrade().is_none());
        }
    }

    #[test]
    fn empty_typed_take_preserves_later_owned_publication() {
        let runtime = support::ManualRuntime::new();
        let store = Store::new(
            0,
            Reduce::new(|state: &mut usize, (): &(), _| {
                *state += 1;
                Effect::none()
            }),
            runtime.runtime(),
        );
        let mut actor = runtime.take_only_queued_task();
        let mut context = Context::from_waker(std::task::Waker::noop());
        let owner = Arc::new(());
        let captured = Arc::clone(&owner);
        let selections = Arc::new(AtomicUsize::new(0));
        let selected = Arc::clone(&selections);
        let mut registration = register_erased_observation(
            store.clone(),
            move |_: &usize| {
                selected.fetch_add(1, Ordering::Relaxed);
                Box::new(Arc::clone(&captured)) as ErasedValue
            },
            PublishPolicy::always(),
            None,
        );
        assert!(registration.as_mut().poll(&mut context).is_pending());
        assert!(actor.as_mut().poll(&mut context).is_pending());
        let Poll::Ready(Ok(registered)) = registration.as_mut().poll(&mut context) else {
            panic!("the actor must acknowledge real registration");
        };
        drop(registration);
        assert_eq!(registered.version, 0);
        drop(registered.initial);
        assert_eq!(Arc::strong_count(&owner), 2);
        // Private protocol: public Observation waits for a publication first.
        let operation: Box<dyn TakeOperation<Arc<()>> + Send + Sync> = Box::new(TypedTakeOperation {
            erased: registered.take_operation,
            value: std::marker::PhantomData,
        });
        let mut take = Box::pin(operation.take());
        assert!(take.as_mut().poll(&mut context).is_pending());
        assert!(actor.as_mut().poll(&mut context).is_pending());
        let Poll::Ready(Ok(empty)) = take.as_mut().poll(&mut context) else {
            panic!("the empty take must complete");
        };
        assert!(matches!(empty.reply, TakeReply::Empty { version: 0 }));
        drop(take);
        assert_eq!(selections.load(Ordering::Relaxed), 1);
        let receipt = store.try_send(()).unwrap();
        assert!(actor.as_mut().poll(&mut context).is_pending());
        let task = receipt.finish().now_or_never().unwrap().unwrap();
        assert!(task.is_finished());
        let mut take = Box::pin(empty.operation.take());
        assert!(take.as_mut().poll(&mut context).is_pending());
        assert!(actor.as_mut().poll(&mut context).is_pending());
        let Poll::Ready(Ok(published)) = take.as_mut().poll(&mut context) else {
            panic!("the published take must complete");
        };
        let TakeReply::Value { version: 1, value } = published.reply else {
            panic!("the first publication must retain its version and value");
        };
        assert!(Arc::ptr_eq(&owner, &value));
        assert_eq!(Arc::strong_count(&owner), 3);
        drop(value);
        drop(take);
        let mut take = Box::pin(published.operation.take());
        assert!(take.as_mut().poll(&mut context).is_pending());
        assert!(actor.as_mut().poll(&mut context).is_pending());
        let Poll::Ready(Ok(empty)) = take.as_mut().poll(&mut context) else {
            panic!("the consumed publication must leave an empty slot");
        };
        assert!(matches!(empty.reply, TakeReply::Empty { version: 1 }));
        assert_eq!(selections.load(Ordering::Relaxed), 2);
        assert_eq!(Arc::strong_count(&owner), 2);
        drop(empty.operation);
        drop(take);
        drop(actor);
        drop(store);
        assert_eq!(Arc::strong_count(&owner), 1);
        assert_eq!(selections.load(Ordering::Relaxed), 2);
    }

    #[derive(Clone, Copy, PartialEq)]
    enum DistinctFailure {
        None,
        Clone,
        Compare,
        Drop,
    }

    type DistinctEvents = Arc<std::sync::Mutex<Vec<(&'static str, usize, bool)>>>;

    struct DistinctValue {
        number: usize,
        retained: bool,
        failure: DistinctFailure,
        events: DistinctEvents,
    }

    impl Clone for DistinctValue {
        fn clone(&self) -> Self {
            self.events
                .lock()
                .unwrap()
                .push(("clone", self.number, self.retained));
            assert!(
                self.failure != DistinctFailure::Clone || self.number != 1,
                "clone panic",
            );
            Self {
                retained: true,
                events: Arc::clone(&self.events),
                ..*self
            }
        }
    }

    impl PartialEq for DistinctValue {
        fn eq(&self, other: &Self) -> bool {
            self.events
                .lock()
                .unwrap()
                .push(("compare", self.number, self.retained));
            assert!(self.failure != DistinctFailure::Compare, "comparison panic");
            self.number == other.number
        }
    }

    impl Drop for DistinctValue {
        fn drop(&mut self) {
            self.events
                .lock()
                .unwrap()
                .push(("drop", self.number, self.retained));
            assert!(
                self.failure != DistinctFailure::Drop || !self.retained || self.number != 0,
                "previous value drop panic",
            );
        }
    }

    #[test]
    fn distinct_projection_preserves_clone_compare_and_drop_order() {
        use std::panic::{AssertUnwindSafe, catch_unwind};
        use std::sync::Mutex;

        for failure in [
            DistinctFailure::None,
            DistinctFailure::Clone,
            DistinctFailure::Compare,
            DistinctFailure::Drop,
        ] {
            let runtime = support::ManualRuntime::new();
            let store = Store::new(
                0,
                Reduce::new(|state: &mut usize, action: &usize, _| {
                    *state += action;
                    Effect::none()
                }),
                runtime.runtime(),
            );
            let child = store.scope(crate::identity_state_path(), crate::identity_action_path());
            let mut actor = runtime.take_only_queued_task();
            let mut context = Context::from_waker(std::task::Waker::noop());
            let events = Arc::new(Mutex::new(Vec::new()));
            let captured = Arc::clone(&events);
            let registration = child.observe_distinct(move |number| DistinctValue {
                number: *number,
                retained: false,
                failure,
                events: Arc::clone(&captured),
            });
            let mut registration = std::pin::pin!(registration);
            assert!(registration.as_mut().poll(&mut context).is_pending());
            assert!(actor.as_mut().poll(&mut context).is_pending());
            let Poll::Ready(Ok(mut values)) = registration.as_mut().poll(&mut context) else {
                panic!("the actor must acknowledge the distinct observation");
            };
            let initial = values.next().now_or_never().unwrap().unwrap();
            assert_eq!(initial.number, 0);
            drop(initial);
            events.lock().unwrap().clear();

            let receipt = store.try_send(1).unwrap();
            let outcome = catch_unwind(AssertUnwindSafe(|| actor.as_mut().poll(&mut context)));
            if failure == DistinctFailure::None {
                assert!(outcome.unwrap().is_pending());
                drop(receipt.finish().now_or_never().unwrap().unwrap());
                assert_eq!(
                    *events.lock().unwrap(),
                    [("compare", 0, true), ("clone", 1, false), ("drop", 0, true)],
                );
            } else {
                outcome.unwrap_err();
                receipt.finish().now_or_never().unwrap().unwrap_err();
            }
            drop(actor);
            assert!(store.is_closed());
            assert!(values.next().now_or_never().unwrap().is_none());
            let expected: &[(&str, usize, bool)] = match failure {
                DistinctFailure::None => &[
                    ("compare", 0, true),
                    ("clone", 1, false),
                    ("drop", 0, true),
                    ("drop", 1, true),
                    ("drop", 1, false),
                ],
                DistinctFailure::Clone => &[
                    ("compare", 0, true),
                    ("clone", 1, false),
                    ("drop", 1, false),
                    ("drop", 0, true),
                ],
                DistinctFailure::Compare => &[
                    ("compare", 0, true),
                    ("drop", 1, false),
                    ("drop", 0, true),
                ],
                DistinctFailure::Drop => &[
                    ("compare", 0, true),
                    ("clone", 1, false),
                    ("drop", 0, true),
                    ("drop", 1, false),
                    ("drop", 1, true),
                ],
            };
            assert_eq!(events.lock().unwrap().as_slice(), expected);
        }
    }

    #[test]
    fn identified_projection_cannot_change_children_after_validation() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Mutex;

        struct Child {
            id: usize,
        }
        struct Parent {
            selected: Arc<AtomicUsize>,
            children: [Child; 2],
        }

        let mut observed = Vec::new();
        for observe in [false, true] {
            let runtime = support::ManualRuntime::new();
            let selected = Arc::new(AtomicUsize::new(0));
            let flip = Arc::new(AtomicBool::new(false));
            let armed = Arc::clone(&flip);
            let selections = Arc::new(Mutex::new(Vec::new()));
            let recorded = Arc::clone(&selections);
            let store = Store::new(
                Parent {
                    selected,
                    children: [Child { id: 7 }, Child { id: 8 }],
                },
                Reduce::new(|_: &mut Parent, (): &(), _| Effect::none()),
                runtime.runtime(),
            );
            let path = crate::StatePath::new(
                move |state: &Parent| {
                    let index = state.selected.load(Ordering::SeqCst);
                    recorded.lock().unwrap().push(state.children[index].id);
                    if armed.swap(false, Ordering::SeqCst) {
                        state.selected.store(1, Ordering::SeqCst);
                    }
                    &state.children[index]
                },
                |state: &mut Parent| {
                    let index = state.selected.load(Ordering::SeqCst);
                    &mut state.children[index]
                },
            );
            let mut actor = runtime.take_only_queued_task();
            let mut context = Context::from_waker(std::task::Waker::noop());
            let registration = store.scope_identified(
                path,
                crate::identity_action_path(),
                |child| &child.id,
            );
            let mut registration = std::pin::pin!(registration);
            assert!(registration.as_mut().poll(&mut context).is_pending());
            assert!(actor.as_mut().poll(&mut context).is_pending());
            let Poll::Ready(Ok(child)) = registration.as_mut().poll(&mut context) else {
                panic!("the actor must acknowledge the identified route");
            };
            selections.lock().unwrap().clear();
            flip.store(true, Ordering::SeqCst);
            let query = async {
                if observe {
                    let mut values = child.observe(|child| child.id).await?;
                    Ok(values.next().await.expect("registered initial value"))
                } else {
                    child.read(|child| child.id).await
                }
            };
            let mut query = std::pin::pin!(query);
            assert!(query.as_mut().poll(&mut context).is_pending());
            assert!(actor.as_mut().poll(&mut context).is_pending());
            let Poll::Ready(result) = query.as_mut().poll(&mut context) else {
                panic!("the actor must resolve the child query");
            };
            println!(
                "identified observe={observe}: result={result:?}, selections={:?}",
                selections.lock().unwrap(),
            );
            observed.push(result.map_err(|error| error.is_scope_ended()));

            let later = child.read(|child| child.id);
            let mut later = std::pin::pin!(later);
            assert!(later.as_mut().poll(&mut context).is_pending());
            assert!(actor.as_mut().poll(&mut context).is_pending());
            let Poll::Ready(Err(error)) = later.as_mut().poll(&mut context) else {
                panic!("the next query must reject the changed identity");
            };
            assert!(error.is_scope_ended());
        }
        assert!(
            observed.iter().all(|value| matches!(value, Ok(7) | Err(true))),
            "old child-7 routes observed {observed:?}",
        );
    }

    #[test]
    fn retired_driver_releases_selectors_after_expiry_on_actor_thread() {
        use std::sync::Mutex;

        struct Child {
            id: usize,
        }
        type Route = Arc<Mutex<Option<super::WeakScopedStore<Child, ()>>>>;
        type Events = Arc<Mutex<Vec<(&'static str, bool, std::thread::ThreadId)>>>;
        struct Capture {
            name: &'static str,
            route: Route,
            events: Events,
        }
        impl Drop for Capture {
            fn drop(&mut self) {
                let route = self.route.lock().unwrap();
                let handle = route.as_ref().and_then(super::WeakScopedStore::upgrade);
                let ended = handle.is_some_and(|handle| {
                    matches!(handle.read(|_| ()).now_or_never(), Some(Err(error))
                        if error.is_scope_ended())
                });
                self.events.lock().unwrap().push((
                    self.name,
                    ended,
                    std::thread::current().id(),
                ));
            }
        }

        let runtime = support::ManualRuntime::new();
        let store = Store::new(
            Child { id: 7 },
            Reduce::new(|state: &mut Child, (): &(), _| {
                state.id = 8;
                Effect::none()
            }),
            runtime.runtime(),
        );
        let route = Arc::new(Mutex::new(None));
        let events = Arc::new(Mutex::new(Vec::new()));
        let get_capture = Capture {
            name: "get",
            route: Arc::clone(&route),
            events: Arc::clone(&events),
        };
        let identity_capture = Capture {
            name: "identity",
            route: Arc::clone(&route),
            events: Arc::clone(&events),
        };
        let path = crate::StatePath::new(
            move |state: &Child| {
                std::hint::black_box(&get_capture);
                state
            },
            |state: &mut Child| state,
        );
        let mut actor = runtime.take_only_queued_task();
        let mut context = Context::from_waker(std::task::Waker::noop());
        let mut registration = Box::pin(store.scope_identified(
            path,
            crate::identity_action_path(),
            move |child| {
                std::hint::black_box(&identity_capture);
                &child.id
            },
        ));
        assert!(registration.as_mut().poll(&mut context).is_pending());
        assert!(actor.as_mut().poll(&mut context).is_pending());
        let Poll::Ready(Ok(child)) = registration.as_mut().poll(&mut context) else {
            panic!("the actor must acknowledge the identified route");
        };
        drop(registration);
        *route.lock().unwrap() = Some(child.downgrade());
        assert!(events.lock().unwrap().is_empty());
        let receipt = store.try_send(()).unwrap();
        assert!(actor.as_mut().poll(&mut context).is_pending());
        assert!(receipt.finish().now_or_never().unwrap().unwrap().is_finished());
        assert!(child.read(|_| ()).now_or_never().unwrap().unwrap_err().is_scope_ended());
        let mut observed = events.lock().unwrap().clone();
        observed.sort_by_key(|event| event.0);
        assert_eq!(observed, [
            ("get", true, std::thread::current().id()),
            ("identity", true, std::thread::current().id()),
        ]);
        assert!(route.lock().unwrap().as_ref().unwrap().upgrade().is_some());
        assert!(!store.is_closed());
    }
}
}
