//! Owns projected values inside Store actor authority.
//!
//! Shared and Local commands use the same registry and publication rules. Only
//! their erased callback bounds differ. Consumers retain weak command routes;
//! selectors and buffered values remain actor-owned. Entries publish in
//! unspecified order, with superseded values dropped before consumers wake.
//! User callback panics propagate. Registry teardown still closes remaining
//! signals when one consumer wake unwinds; another panic during unwinding aborts.

use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;

use crate::observation::{
    ObservationId, ObservationSignal, TakeFuture, TakeOperation, TakeOutcome, TakeReply,
};
use crate::store::scope::{
    HandleRegistryCore, HandleSelector, ScopeToken, ScopeTransitionBatch, SelectedChild,
    WeakHandleScope,
};

/// Owns a comparison callback that can update the previous-value slot.
type DistinctDecision<Value> = Box<dyn Fn(&mut Option<Value>, &Value) -> bool + Send + 'static>;
/// Projects an owned value without retaining a borrow of root state.
type Selector<State, Value> = Box<dyn Fn(&State) -> Value + Send + 'static>;

/// Projects root state or the exact child already validated by the actor.
pub(crate) trait ObservationSelection<State, Value> {
    /// Projects the root or the exact child already validated by the actor.
    fn select(&self, state: &State, validation: &SelectedChild<'_>) -> Value;
}

impl<State, Value, Select: Fn(&State) -> Value> ObservationSelection<State, Value> for Select {
    fn select(&self, state: &State, _validation: &SelectedChild<'_>) -> Value {
        self(state)
    }
}

/// Keeps checked child projection separate from the ordinary root selector path.
pub(crate) struct CheckedSelector<Select>(
    /// Receives the actor-validated child before producing an owned publication.
    Select,
);

impl<Select> CheckedSelector<Select> {
    /// Wraps a callback that projects the actor-validated child.
    pub(crate) fn new<State, Value>(select: Select) -> Self
    where
        Select: Fn(&State, &SelectedChild<'_>) -> Value,
    {
        Self(select)
    }
}

impl<State, Value, Select> ObservationSelection<State, Value> for CheckedSelector<Select>
where
    Select: Fn(&State, &SelectedChild<'_>) -> Value,
{
    fn select(&self, state: &State, validation: &SelectedChild<'_>) -> Value {
        (self.0)(state, validation)
    }
}

#[cfg(feature = "local")]
pub(super) mod local;

/// Creates a reusable take command without retaining Store authority.
pub(super) fn new_take_operation<State, Action, Value>(
    weak: std::sync::Weak<super::Ingress<State, Action>>,
    id: ObservationId,
) -> Box<dyn TakeOperation<Value> + Send + Sync>
where
    State: 'static,
    Action: Send + 'static,
    Value: Send + 'static,
{
    Box::new(RootTake {
        weak,
        id,
        reply: None,
    })
}

/// Reuses one command allocation across a root observation's successive takes.
struct RootTake<State, Action, Value, C = super::carriers::Shared>
where
    C: super::carriers::Carriers<State, Action> + crate::observation::ObservationFamily,
{
    /// Routes commands without keeping the Store alive between takes.
    weak: std::sync::Weak<super::ingress::CommandIngress<super::Command<State, Action, C>>>,
    /// Identifies the registration whose value type matches this command.
    id: ObservationId,
    /// Exists only while admitted; removed before returning this command in its reply.
    reply: Option<futures_channel::oneshot::Sender<TakeOutcome<Value, C>>>,
}

/// Shares admission, backpressure and allocation reuse across the two carrier families.
macro_rules! root_take {
    ($family:ty, [$($bound:path),*], $($command:ident)::+, $($registry:ident)::+) => {
        impl<State: 'static, Action: 'static $(+ $bound)*, Value: 'static $(+ $bound)*>
            TakeOperation<Value, $family> for RootTake<State, Action, Value, $family>
        {
            fn take(mut self: Box<Self>) -> TakeFuture<Value, $family> {
                let Some(ingress) = self.weak.upgrade() else {
                    return TakeFuture::Closed;
                };
                let (reply, receive) = futures_channel::oneshot::channel();
                self.reply = Some(reply);
                match ingress.try_send(super::Command::<State, Action, $family>::Observation(self)) {
                    Ok(()) => TakeFuture::Receiving { receive },
                    Err(async_channel::TrySendError::Full(command)) => {
                        TakeFuture::<Value, $family>::Deferred(Box::pin(async move {
                            ingress
                                .send(command)
                                .await
                                .map_err(|_closed| crate::StoreStopped::new())?;
                            receive.await.map_err(|_closed| crate::StoreStopped::new())
                        }))
                    }
                    Err(async_channel::TrySendError::Closed(_)) => TakeFuture::Closed,
                }
            }
        }

        impl<State: 'static, Action: 'static $(+ $bound)*, Value: 'static $(+ $bound)*>
            $($command)::+<State> for RootTake<State, Action, Value, $family>
        {
            fn execute(
                mut self: Box<Self>,
                _state: &State,
                registry: &mut $($registry)::+<State>,
                _commit_version: u64,
                _scope: ScopeToken,
                _handle: Option<WeakHandleScope>,
                _validation: &SelectedChild<'_>,
            ) {
                // Remove the sender before returning this allocation through its
                // channel: a sender in the buffered reply would create a cycle.
                let reply = self.reply.take().expect("admitted take owns its reply");
                let value = registry.take(self.id);
                let _ = reply.send(TakeOutcome::<Value, $family> {
                    reply: value,
                    operation: self,
                });
            }
        }
    };
}
root_take!(
    super::carriers::Shared,
    [Send],
    ObservationCommand,
    ObservationRegistry
);
#[cfg(feature = "local")]
root_take!(
    super::Local,
    [],
    local::ObservationCommand,
    local::ObservationRegistry
);

#[cfg(feature = "local")]
/// Creates an owner-local take command without retaining Store authority.
pub(super) fn new_local_take_operation<State: 'static, Action: 'static, Value: 'static>(
    weak: std::sync::Weak<
        super::ingress::CommandIngress<super::Command<State, Action, super::Local>>,
    >,
    id: ObservationId,
) -> Box<dyn TakeOperation<Value, super::Local>> {
    Box::new(RootTake::<State, Action, Value, super::Local> {
        weak,
        id,
        reply: None,
    })
}

/// Selects ordinary or distinct publication with a transferable comparison callback.
pub(crate) type PublishPolicy<Value> = PublishPolicyCore<Value, DistinctDecision<Value>>;

/// Keeps publication history separate from the latest value waiting for consumption.
pub(crate) enum PublishPolicyCore<Value, Decision> {
    /// Publishes every selected value without retaining comparison history.
    Always,
    /// Lets the callback retain and compare its previous published value.
    Distinct {
        /// Holds comparison history independently of the consumer's buffered value.
        previous: Option<Value>,
        /// Accepts the initial value and decides whether later values should publish.
        decide: Decision,
    },
}

impl<Value, Decision> PublishPolicyCore<Value, Decision> {
    /// Publishes each selection without allocating a comparison callback.
    pub(crate) const fn always() -> Self {
        Self::Always
    }

    /// Updates comparison history before a selected value enters the publication slot.
    fn should_publish(&mut self, value: &Value) -> bool
    where
        Decision: Fn(&mut Option<Value>, &Value) -> bool,
    {
        match self {
            Self::Always => true,
            Self::Distinct { previous, decide } => decide(previous, value),
        }
    }
}

impl<Value> PublishPolicy<Value> {
    /// Owns a custom comparison callback that must accept the initial value.
    pub(crate) fn distinct_by(
        decide: impl Fn(&mut Option<Value>, &Value) -> bool + Send + 'static,
    ) -> Self {
        Self::Distinct {
            previous: None,
            decide: Box::new(decide),
        }
    }
}

/// Clones changed values before replacing and dropping the previous comparison value.
fn distinct<Value: Clone + PartialEq>(previous: &mut Option<Value>, value: &Value) -> bool {
    if previous.as_ref().is_some_and(|previous| previous == value) {
        false
    } else {
        *previous = Some(value.clone());
        true
    }
}

impl<Value: Clone + PartialEq + 'static> PublishPolicy<Value> {
    /// Suppresses equal values while retaining one clone for comparison.
    pub(crate) fn distinct() -> Self {
        Self::distinct_by(distinct)
    }
}

/// Transfers the initial value and its registration metadata to the consumer.
pub(crate) struct RegistrationReply<Value> {
    /// Identifies subsequent take commands with the same concrete value type.
    pub(crate) id: ObservationId,
    /// Records the commit that produced the initial value.
    pub(crate) version: u64,
    /// Moves directly to the consumer instead of entering the registry's slot.
    pub(crate) initial: Value,
}

/// Runs a transferable observation operation inside actor authority.
pub(crate) trait ObservationCommand<State>: Send {
    /// Consumes the command with current state and an actor-borrowed root or validated child view.
    fn execute(
        self: Box<Self>,
        state: &State,
        registry: &mut ObservationRegistry<State>,
        commit_version: u64,
        scope: ScopeToken,
        handle: Option<WeakHandleScope>,
        validation: &SelectedChild<'_>,
    );
}

/// Exposes actor-owned entry operations independently of projection and value types.
pub(crate) trait ErasedObservation<State> {
    /// Borrows the typed value slot, not the selector-dependent entry representation.
    fn as_any_mut(&mut self) -> &mut dyn Any;
    /// Identifies the structural scope whose retirement closes this entry.
    fn scope(&self) -> ScopeToken;
    /// Checks optional facade liveness without invoking its selector.
    fn handle_is_active(&self) -> bool;
    /// Borrows weak facade authority for exact-child validation before projection.
    fn handle(&self) -> Option<&WeakHandleScope>;
    /// Projects a validated value, dropping superseded output before notifying consumers.
    ///
    /// A callback already in progress may finish after concurrent consumer cancellation.
    fn publish(&mut self, state: &State, commit_version: u64, validation: &SelectedChild<'_>);
    /// Checks whether the consumer still accepts publications.
    fn is_active(&self) -> bool;
    /// Deactivates the consumer signal before the entry's owned values are dropped.
    fn close(&self);
}

#[cfg(test)]
ordinary_test_items! {
/// Names the ordinary Shared entry representation used by registry fixtures.
type ObservationEntry<State, Value> =
    ObservationEntryCore<Value, Selector<State, Value>, DistinctDecision<Value>>;
}

/// Owns a projection, its comparison history and its latest undelivered value.
struct ObservationEntryCore<Value, Select, Decision> {
    /// Associates the entry with structural retirement.
    scope: ScopeToken,
    /// Links facade expiry without keeping its route or selector alive.
    handle: Option<WeakHandleScope>,
    /// Produces owned values from root state or an actor-validated child.
    selector: Select,
    /// Retains only the history required by the selected publication policy.
    policy: PublishPolicyCore<Value, Decision>,
    /// Coalesces publications independently of comparison history.
    slot: ObservationSlot<Value>,
    /// Shares liveness and version metadata with the consumer.
    signal: Arc<ObservationSignal>,
}

/// Erases selector types without erasing the buffered value's concrete type.
struct ObservationSlot<Value> {
    /// Coalesces the latest owned publication until a successful take.
    value: Option<Value>,
    /// Identifies the last publication even after its buffered value is taken.
    version: u64,
}

impl<State, Value, Select, Decision> ErasedObservation<State>
    for ObservationEntryCore<Value, Select, Decision>
where
    Value: 'static,
    Select: ObservationSelection<State, Value> + 'static,
    Decision: Fn(&mut Option<Value>, &Value) -> bool + 'static,
{
    fn as_any_mut(&mut self) -> &mut dyn Any {
        &mut self.slot
    }

    fn scope(&self) -> ScopeToken {
        self.scope
    }

    fn handle_is_active(&self) -> bool {
        self.handle.as_ref().is_none_or(WeakHandleScope::is_active)
    }

    fn handle(&self) -> Option<&WeakHandleScope> {
        self.handle.as_ref()
    }

    fn publish(&mut self, state: &State, commit_version: u64, validation: &SelectedChild<'_>) {
        let value = self.selector.select(state, validation);
        if !self.policy.should_publish(&value) {
            return;
        }
        drop(self.slot.value.replace(value));
        self.slot.version = commit_version;
        self.signal.publish(commit_version);
    }

    fn is_active(&self) -> bool {
        self.signal.is_active()
    }

    fn close(&self) {
        self.signal.deactivate();
    }
}

/// Stores transferable entries under the Shared actor's exclusive access.
pub(crate) type ObservationRegistry<State> =
    ObservationRegistryCore<State, dyn ErasedObservation<State> + Send>;

/// Owns observation entries and never reuses a registration identifier.
pub(crate) struct ObservationRegistryCore<State, Entry: ?Sized + ErasedObservation<State>> {
    /// Holds heterogeneous values and callbacks exclusively on the actor.
    entries: HashMap<ObservationId, Box<Entry>>,
    /// Prevents stale take commands from selecting a later registration.
    next_id: u64,
    /// Records the borrowed state input without implying ownership of it.
    state: std::marker::PhantomData<fn(&State)>,
}

impl<State, Entry: ?Sized + ErasedObservation<State>> ObservationRegistryCore<State, Entry> {
    /// Starts an empty registry without allocating entry storage.
    pub(crate) fn new() -> Self {
        Self {
            entries: HashMap::new(),
            next_id: 0,
            state: std::marker::PhantomData,
        }
    }

    #[inline]
    /// Lets actor publication skip state access when no entries remain.
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Publishes live entries and retires rejected child selections before dropping captures.
    pub(crate) fn publish<Selector>(
        &mut self,
        state: &State,
        commit_version: u64,
        handles: &mut HandleRegistryCore<Selector>,
    ) where
        State: 'static,
        Selector: ?Sized + HandleSelector<State>,
    {
        self.entries.retain(|_, entry| {
            let active = if !entry.is_active() {
                false
            } else if let Some(handle) = entry.handle() {
                handle.upgrade().is_some_and(|handle| {
                    handles
                        .with_validation(state, &handle, |validation| {
                            entry.publish(state, commit_version, validation);
                        })
                        .is_some()
                })
            } else {
                entry.publish(state, commit_version, &SelectedChild::root(state));
                true
            };
            if !active {
                entry.close();
            }
            active
        });
    }

    /// Releases cancelled or expired entries without running their user projections.
    pub(crate) fn remove_inactive(&mut self) {
        self.entries.retain(|_, entry| {
            let active = entry.is_active() && entry.handle_is_active();
            if !active {
                entry.close();
            }
            active
        });
    }

    #[cfg(feature = "test-util")]
    /// Counts actor-owned entries for deterministic test inspection.
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Installs an entry under a fresh identifier, panicking on identifier exhaustion.
    fn insert_entry(&mut self, entry: Box<Entry>) -> ObservationId {
        let id = ObservationId::new(self.next_id);
        self.next_id = self
            .next_id
            .checked_add(1)
            .expect("observation ID space exhausted");
        let previous = self.entries.insert(id, entry);
        debug_assert!(previous.is_none(), "observation ID must be unique");
        id
    }

    /// Moves the latest value from a registration with the caller's matching type.
    ///
    /// Registration factories pair each identifier with its concrete value type.
    /// The checked cast restores that private invariant without unsafe code.
    fn take_entry<Value>(&mut self, id: ObservationId) -> TakeReply<Value>
    where
        Value: 'static,
    {
        let entry = self
            .entries
            .get_mut(&id)
            .filter(|entry| entry.is_active())
            .map(|entry| {
                entry
                    .as_any_mut()
                    .downcast_mut::<ObservationSlot<Value>>()
                    .expect("observation value type must match registration")
            });
        match entry {
            Some(entry) => match entry.value.take() {
                Some(value) => TakeReply::Value {
                    version: entry.version,
                    value,
                },
                None => TakeReply::Empty {
                    version: entry.version,
                },
            },
            None => TakeReply::Closed,
        }
    }

    /// Closes each retired structural entry before releasing its values and captures.
    pub(crate) fn close_scopes(&mut self, transitions: &ScopeTransitionBatch) {
        if transitions.is_empty() {
            return;
        }

        self.entries.retain(|_, entry| {
            let should_close = transitions.contains(entry.scope());
            if should_close {
                entry.close();
            }
            !should_close
        });
    }

    /// Closes all signals before releasing any entry's values or captures.
    ///
    /// If one consumer wake panics, a stack guard continues closing the remaining
    /// signals while the original panic propagates. A second panic aborts, as with
    /// other destructors that panic during unwinding.
    pub(crate) fn close_all(&mut self) {
        /// Resumes the same closure iterator if a consumer wake unwinds.
        struct CloseRemaining<F: FnMut()>(
            /// Borrows the unvisited entries without allocating another collection.
            F,
        );

        impl<F: FnMut()> Drop for CloseRemaining<F> {
            fn drop(&mut self) {
                (self.0)();
            }
        }

        let mut entries = self.entries.values();
        let mut remaining = CloseRemaining(|| {
            for entry in entries.by_ref() {
                entry.close();
            }
        });
        (remaining.0)();
        drop(remaining);
        self.entries.clear();
    }
}

impl<State: 'static> ObservationRegistry<State> {
    #[cfg(test)]
    ordinary_test_items! {
    #[cfg(test)]
    /// Installs an ordinary Shared entry for direct registry fixtures.
    fn insert<Value>(
        &mut self,
        selector: Box<dyn Fn(&State) -> Value + Send + 'static>,
        policy: PublishPolicy<Value>,
        signal: Arc<ObservationSignal>,
        commit_version: u64,
        scope: ScopeToken,
        handle: Option<WeakHandleScope>,
    ) -> ObservationId
    where
        Value: Send + 'static,
    {
        self.insert_entry(Box::new(ObservationEntry {
            scope,
            handle,
            selector,
            policy,
            slot: ObservationSlot {
                value: None,
                version: commit_version,
            },
            signal,
        }))
    }

    }

    /// Takes a Shared value through the common typed-slot implementation.
    fn take<Value: Send + 'static>(&mut self, id: ObservationId) -> TakeReply<Value> {
        self.take_entry::<Value>(id)
    }
}

impl<State, Entry: ?Sized + ErasedObservation<State>> Drop
    for ObservationRegistryCore<State, Entry>
{
    fn drop(&mut self) {
        self.close_all();
    }
}

/// Moves a projection into the actor and returns its initial owned value.
struct RegisterObservation<Value, Select, Decision> {
    /// Runs only after admission and any checked-child validation.
    selector: Select,
    /// Initialises comparison history before installing the entry.
    policy: PublishPolicyCore<Value, Decision>,
    /// Detects cancellation before and immediately after initial selection.
    signal: Arc<ObservationSignal>,
    /// Transfers registration metadata and the initial value to the waiting caller.
    reply: futures_channel::oneshot::Sender<RegistrationReply<Value>>,
}

impl<State: 'static, Value: Send + 'static, Select> ObservationCommand<State>
    for RegisterObservation<Value, Select, DistinctDecision<Value>>
where
    Select: ObservationSelection<State, Value> + Send + 'static,
{
    fn execute(
        self: Box<Self>,
        state: &State,
        registry: &mut ObservationRegistry<State>,
        commit_version: u64,
        scope: ScopeToken,
        handle: Option<WeakHandleScope>,
        validation: &SelectedChild<'_>,
    ) {
        (*self).register(
            state,
            registry,
            commit_version,
            scope,
            handle,
            validation,
            |entry| Box::new(entry),
        );
    }
}

impl<Value, Select, Decision> RegisterObservation<Value, Select, Decision> {
    /// Installs a live registration using the carrier family's entry erasure.
    ///
    /// Initial selection precedes policy evaluation. Cancellation at either
    /// selection boundary discards the command without installing an entry.
    #[expect(
        clippy::too_many_arguments,
        reason = "the shared registry body receives actor metadata and the family-specific erasure closure separately"
    )]
    fn register<State, Entry>(
        self,
        state: &State,
        registry: &mut ObservationRegistryCore<State, Entry>,
        commit_version: u64,
        scope: ScopeToken,
        handle: Option<WeakHandleScope>,
        validation: &SelectedChild<'_>,
        erase: impl FnOnce(ObservationEntryCore<Value, Select, Decision>) -> Box<Entry>,
    ) where
        Entry: ?Sized + ErasedObservation<State>,
        Select: ObservationSelection<State, Value>,
        Decision: Fn(&mut Option<Value>, &Value) -> bool,
    {
        let Self {
            selector,
            mut policy,
            signal,
            reply,
        } = self;
        if !signal.is_active() {
            return;
        }
        let initial = selector.select(state, validation);
        if !signal.is_active() {
            return;
        }
        let initial_published = policy.should_publish(&initial);
        debug_assert!(initial_published, "initial observation value must publish");
        let id = registry.insert_entry(erase(ObservationEntryCore {
            selector,
            policy,
            signal: Arc::clone(&signal),
            slot: ObservationSlot {
                value: None,
                version: commit_version,
            },
            scope,
            handle,
        }));
        signal.publish(commit_version);
        let _ = reply.send(RegistrationReply {
            id,
            version: commit_version,
            initial,
        });
    }
}

/// Takes a scoped facade's value after the actor validates its current authority.
struct TakeObservation<Value> {
    /// Identifies a registration paired with this command's value type.
    id: ObservationId,
    /// Transfers the buffered value or the registration's terminal state.
    reply: futures_channel::oneshot::Sender<TakeReply<Value>>,
}

impl<State: 'static, Value: Send + 'static> ObservationCommand<State> for TakeObservation<Value> {
    fn execute(
        self: Box<Self>,
        _state: &State,
        registry: &mut ObservationRegistry<State>,
        _commit_version: u64,
        _scope: ScopeToken,
        _handle: Option<WeakHandleScope>,
        _validation: &SelectedChild<'_>,
    ) {
        let Self { id, reply } = *self;
        let value = registry.take(id);
        let _ = reply.send(value);
    }
}

/// Prepares a root projection that publishes every selected value.
pub(crate) fn new_registration<State, Value>(
    selector: impl Fn(&State) -> Value + Send + 'static,
    signal: Arc<ObservationSignal>,
) -> (
    Box<dyn ObservationCommand<State>>,
    futures_channel::oneshot::Receiver<RegistrationReply<Value>>,
)
where
    State: 'static,
    Value: Send + 'static,
{
    new_registration_with_policy(selector, signal, PublishPolicy::always())
}

/// Prepares a root projection that suppresses consecutive equal values.
pub(crate) fn new_distinct_registration<State, Value>(
    selector: impl Fn(&State) -> Value + Send + 'static,
    signal: Arc<ObservationSignal>,
) -> (
    Box<dyn ObservationCommand<State>>,
    futures_channel::oneshot::Receiver<RegistrationReply<Value>>,
)
where
    State: 'static,
    Value: Clone + PartialEq + Send + 'static,
{
    new_registration_with_policy(selector, signal, PublishPolicy::distinct())
}

/// Boxes a root projection while preserving its chosen publication policy.
pub(crate) fn new_registration_with_policy<State, Value>(
    selector: impl Fn(&State) -> Value + Send + 'static,
    signal: Arc<ObservationSignal>,
    policy: PublishPolicy<Value>,
) -> (
    Box<dyn ObservationCommand<State>>,
    futures_channel::oneshot::Receiver<RegistrationReply<Value>>,
)
where
    State: 'static,
    Value: Send + 'static,
{
    let selector: Selector<State, Value> = Box::new(selector);
    new_registration_with_selection(selector, signal, policy)
}

/// Prepares a registration while retaining its root or checked selection strategy.
pub(crate) fn new_registration_with_selection<State, Value>(
    selector: impl ObservationSelection<State, Value> + Send + 'static,
    signal: Arc<ObservationSignal>,
    policy: PublishPolicy<Value>,
) -> (
    Box<dyn ObservationCommand<State>>,
    futures_channel::oneshot::Receiver<RegistrationReply<Value>>,
)
where
    State: 'static,
    Value: Send + 'static,
{
    let (reply, receive) = futures_channel::oneshot::channel();
    (
        Box::new(RegisterObservation {
            selector,
            policy,
            signal,
            reply,
        }),
        receive,
    )
}

/// Prepares a one-shot value transfer for the scoped command path.
pub(crate) fn new_take<State, Value>(
    id: ObservationId,
) -> (
    Box<dyn ObservationCommand<State>>,
    futures_channel::oneshot::Receiver<TakeReply<Value>>,
)
where
    State: 'static,
    Value: Send + 'static,
{
    let (reply, receive) = futures_channel::oneshot::channel();
    (Box::new(TakeObservation { id, reply }), receive)
}

#[cfg(test)]
ordinary_test_items! {
mod tests {
    use std::any::Any;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Weak};

    use super::{
        ErasedObservation, SelectedChild, ObservationEntry, ObservationId, ObservationRegistry,
        ObservationSignal, ObservationSlot, PublishPolicy, ScopeToken, ScopeTransitionBatch,
        TakeReply, WeakHandleScope,
    };
    use crate::store::scope::{HandleRegistry, ScopeIdentity, ScopeRegistry, ScopeSlot};

    #[test]
    fn registry_drop_closes_every_signal_when_one_consumer_waker_panics() {
        use std::task::{Context, Poll, Wake, Waker};

        use futures_util::FutureExt as _;

        struct FirstWakePanics(AtomicUsize);

        impl Wake for FirstWakePanics {
            fn wake(self: Arc<Self>) {
                self.wake_by_ref();
            }

            fn wake_by_ref(self: &Arc<Self>) {
                assert_ne!(self.0.fetch_add(1, Ordering::SeqCst), 0, "first consumer wake");
            }
        }

        struct RecordCaptureDrop {
            signals: Arc<[Arc<ObservationSignal>]>,
            closed_before_drop: Arc<AtomicUsize>,
        }

        impl Drop for RecordCaptureDrop {
            fn drop(&mut self) {
                if self.signals.iter().all(|signal| !signal.is_active()) {
                    self.closed_before_drop.fetch_add(1, Ordering::SeqCst);
                }
            }
        }

        let mut registry = ObservationRegistry::new();
        let scope = ScopeRegistry::for_test().1.root();
        let signals: Arc<[_]> = (0..2)
            .map(|_| ObservationSignal::new(Weak::new()))
            .collect();
        let closed_before_drop = Arc::new(AtomicUsize::new(0));
        let mut observations = Vec::new();
        for signal in signals.iter() {
            let capture = RecordCaptureDrop {
                signals: Arc::clone(&signals),
                closed_before_drop: Arc::clone(&closed_before_drop),
            };
            let id = registry.insert(
                Box::new(move |state: &usize| {
                    let _ = &capture;
                    *state
                }),
                PublishPolicy::always(),
                Arc::clone(signal),
                0,
                scope,
                None,
            );
            let mut observation = crate::observation::Observation::new(
                id,
                Arc::clone(signal),
                0,
                0,
                super::new_take_operation::<usize, (), usize>(Weak::new(), id),
            );
            assert_eq!(observation.next().now_or_never(), Some(Some(0)));
            observations.push(observation);
        }
        let wake_count = Arc::new(FirstWakePanics(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&wake_count));
        let mut context = Context::from_waker(&waker);
        let mut pending: Vec<_> = observations
            .iter_mut()
            .map(|observation| Box::pin(observation.next()))
            .collect();
        for next in &mut pending {
            assert!(next.as_mut().poll(&mut context).is_pending());
        }

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(registry)));

        result.expect_err("the consumer waker panic must propagate");
        let active: Vec<_> = signals.iter().map(|signal| signal.is_active()).collect();
        let outcomes: Vec<_> = pending
            .iter_mut()
            .map(|next| next.as_mut().poll(&mut context))
            .collect();
        println!("registry teardown: active={active:?}, waiters={outcomes:?}");
        assert!(
            active.iter().all(|active| !active),
            "teardown must close the other observation before releasing its entry",
        );
        assert_eq!(wake_count.0.load(Ordering::SeqCst), 2);
        assert_eq!(outcomes, [Poll::Ready(None), Poll::Ready(None)]);
        assert_eq!(closed_before_drop.load(Ordering::SeqCst), 2);
    }

    #[cfg(feature = "local")]
    #[test]
    fn local_registration_coalesces_rc_values_on_owner() {
        use std::cell::{Cell, RefCell};
        use std::rc::Rc;

        use futures_util::FutureExt as _;

        use super::local::{ObservationRegistry as LocalObservationRegistry, new_registration};
        use crate::observation::local::Observation;
        use crate::observation::{TakeFuture, TakeOperation, TakeOutcome};
        use crate::store::Local;

        struct RegistryTake {
            registry: Rc<RefCell<LocalObservationRegistry<Rc<Cell<usize>>>>>,
            id: ObservationId,
        }

        impl TakeOperation<Rc<usize>, Local> for RegistryTake {
            fn take(self: Box<Self>) -> TakeFuture<Rc<usize>, Local> {
                let reply = self.registry.borrow_mut().take(self.id);
                TakeFuture::<Rc<usize>, Local>::Deferred(Box::pin(std::future::ready(Ok(
                    TakeOutcome::<Rc<usize>, Local> {
                        reply,
                        operation: self,
                    },
                ))))
            }
        }

        let owner = std::thread::current().id();
        let selections = Rc::new(Cell::new(0));
        let selected = Rc::clone(&selections);
        let signal = ObservationSignal::new(Weak::new());
        let mut registry = LocalObservationRegistry::new();
        let state = Rc::new(Cell::new(3));
        let (command, reply) = new_registration(
            move |state: &Rc<Cell<usize>>| {
                assert_eq!(std::thread::current().id(), owner);
                selected.set(selected.get() + 1);
                Rc::new(state.get())
            },
            Arc::clone(&signal),
        );
        command.execute(
            &state,
            &mut registry,
            0,
            ScopeRegistry::for_test().1.root(),
            None,
            &SelectedChild::root(&state),
        );
        let reply = reply.now_or_never().unwrap().unwrap();
        let registry = Rc::new(RefCell::new(registry));
        let mut values = Observation::new(
            reply.id,
            Arc::clone(&signal),
            reply.initial,
            reply.version,
            Box::new(RegistryTake {
                registry: Rc::clone(&registry),
                id: reply.id,
            }),
        );
        assert_eq!(values.next().now_or_never().unwrap().as_deref(), Some(&3));
        state.set(7);
        registry
            .borrow_mut()
            .publish(&state, 1, &mut HandleRegistry::new());
        state.set(11);
        registry
            .borrow_mut()
            .publish(&state, 2, &mut HandleRegistry::new());
        assert_eq!(values.next().now_or_never().unwrap().as_deref(), Some(&11));
        assert_eq!(selections.get(), 3);
        assert!(values.next().now_or_never().is_none());
        registry.borrow_mut().close_all();
        assert!(values.next().now_or_never().unwrap().is_none());
        drop(values);
        assert_eq!(Rc::strong_count(&selections), 1);
    }

    struct CountedObservation {
        entry: ObservationEntry<usize, usize>,
        scope_reads: Arc<AtomicUsize>,
    }

    impl ErasedObservation<usize> for CountedObservation {
        fn as_any_mut(&mut self) -> &mut dyn Any {
            self.entry.as_any_mut()
        }

        fn scope(&self) -> ScopeToken {
            self.scope_reads.fetch_add(1, Ordering::Relaxed);
            self.entry.scope()
        }

        fn handle_is_active(&self) -> bool {
            self.entry.handle_is_active()
        }

        fn handle(&self) -> Option<&WeakHandleScope> {
            self.entry.handle()
        }

        fn publish(
            &mut self,
            state: &usize,
            commit_version: u64,
            validation: &SelectedChild<'_>,
        ) {
            self.entry.publish(state, commit_version, validation);
        }

        fn is_active(&self) -> bool {
            self.entry.is_active()
        }

        fn close(&self) {
            self.entry.close();
        }
    }

    #[test]
    fn empty_scope_closure_does_not_visit_observations() {
        let mut visits = Vec::new();
        for count in [0_u64, 1, 1_024] {
            let scope = ScopeRegistry::for_test().1.root();
            let mut registry = ObservationRegistry::new();
            let scope_reads = Arc::new(AtomicUsize::new(0));
            let selections = Arc::new(AtomicUsize::new(0));
            for id in 0..count {
                let selected = Arc::clone(&selections);
                registry.entries.insert(
                    ObservationId::new(id),
                    Box::new(CountedObservation {
                        entry: ObservationEntry {
                            scope,
                            handle: None,
                            selector: Box::new(move |state| {
                                selected.fetch_add(1, Ordering::Relaxed);
                                *state
                            }),
                            policy: PublishPolicy::always(),
                            slot: ObservationSlot {
                                value: None,
                                version: 0,
                            },
                            signal: ObservationSignal::new(Weak::new()),
                        },
                        scope_reads: Arc::clone(&scope_reads),
                    }),
                );
            }

            registry.close_scopes(&ScopeTransitionBatch::default());

            visits.push(scope_reads.load(Ordering::Relaxed));
            assert_eq!(selections.load(Ordering::Relaxed), 0);
            assert_eq!(registry.entries.len() as u64, count);
            registry.publish(&7, 1, &mut HandleRegistry::new());
            assert_eq!(selections.load(Ordering::Relaxed) as u64, count);
            for id in 0..count {
                assert!(matches!(
                    registry.take::<usize>(ObservationId::new(id)),
                    TakeReply::Value {
                        version: 1,
                        value: 7
                    }
                ));
            }
        }
        assert_eq!(visits, [0, 0, 0]);
    }

    #[test]
    fn scope_closure_preserves_unaffected_observation_delivery() {
        let (_scopes_lifecycle, mut scopes) = ScopeRegistry::for_test();
        let root = scopes.root();
        let child = scopes.mount(
            root,
            ScopeRegistry::node(&mut ScopeSlot::new()),
            ScopeIdentity::unit(),
        );
        let mut registry = ObservationRegistry::new();
        let ended = ObservationSignal::new(Weak::new());
        let live = ObservationSignal::new(Weak::new());
        let ended_id = registry.insert(
            Box::new(|state: &usize| *state),
            PublishPolicy::always(),
            Arc::clone(&ended),
            0,
            child,
            None,
        );
        let live_id = registry.insert(
            Box::new(|state: &usize| *state),
            PublishPolicy::always(),
            Arc::clone(&live),
            0,
            root,
            None,
        );
        let mut transitions = ScopeTransitionBatch::default();
        transitions.extend(scopes.leave(child));

        registry.close_scopes(&transitions);
        registry.publish(&9, 2, &mut HandleRegistry::new());

        assert!(!ended.is_active());
        assert!(live.is_active());
        assert!(matches!(
            registry.take::<usize>(ended_id),
            TakeReply::Closed
        ));
        assert!(matches!(
            registry.take::<usize>(live_id),
            TakeReply::Value {
                version: 2,
                value: 9
            }
        ));
    }
}
}
