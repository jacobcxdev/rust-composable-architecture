//! Owns Store-local structural scope lifetimes.
//!
//! Structural tokens identify one installed incarnation and are never reused.
//! Sibling rings preserve retirement order, while collision chains locate equal
//! identities without treating a hash match as equality. The root has no record
//! and also serves as the collision-chain terminator.
//!
//! Captured authority retains metadata, not state or the lifecycle owner. Legacy
//! handles instead revalidate a user selector before each operation. Selectors,
//! identity comparisons and captured-value destructors can run user code. Their
//! panics propagate to the enclosing Store operation; this registry does not
//! offer recovery after partial retirement.

use std::any::{Any, TypeId};
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::num::NonZeroU64;
use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::effect::lineage::ControlWake;

/// Supplies one identity lookup without allocating ownership on a warm hit.
///
/// `Value` supplies the concrete type identity, so each value stores only its hash.
pub(crate) enum ScopeIdentity<Value = ()> {
    /// Identifies an unkeyed site, distinct from a keyed value of type `()`.
    Unit,
    /// Owns a fresh lookup value until the registry reuses or installs its scope.
    Value {
        /// Caches the user hash so lookup does not invoke it again.
        value_hash: u64,
        /// Becomes erased shared ownership only when a new scope is installed.
        value: Value,
    },
    /// Reuses an existing owner without cloning the concrete identity value.
    Shared {
        /// Uses the same value hash as the ordinary owned lookup representation.
        value_hash: u64,
        /// Transfers its existing allocation into an installed identity on a miss.
        value: Arc<Value>,
    },
}

/// Selects a collision chain, never establishes identity equality by itself.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct IdentityFingerprint(u64);

impl<Value: 'static> ScopeIdentity<Value> {
    /// Combines identity kind, concrete type and cached user hash for lookup.
    fn fingerprint(&self) -> IdentityFingerprint {
        let mut hasher = DefaultHasher::new();
        match self {
            Self::Unit => {
                // Kind bytes keep unit and value identities in separate hash domains.
                0_u8.hash(&mut hasher);
            }
            Self::Value { value_hash, .. } | Self::Shared { value_hash, .. } => {
                1_u8.hash(&mut hasher);
                TypeId::of::<Value>().hash(&mut hasher);
                value_hash.hash(&mut hasher);
            }
        }
        IdentityFingerprint(hasher.finish())
    }
}

impl ScopeIdentity<()> {
    /// Creates the allocation-free identity of an unkeyed site.
    pub(crate) const fn unit() -> Self {
        Self::Unit
    }
}

impl<Value> ScopeIdentity<Value>
where
    Value: Eq + Hash + Send + Sync + 'static,
{
    /// Hashes a moved lookup value once without allocating shared ownership.
    pub(crate) fn new(value: Value) -> Self {
        let mut hasher = DefaultHasher::new();
        value.hash(&mut hasher);
        Self::Value {
            value_hash: hasher.finish(),
            value,
        }
    }

    /// Erases the value while retaining its concrete equality operation.
    pub(crate) fn into_owned(self) -> OwnedScopeIdentity {
        OwnedScopeIdentity::from_identity(self)
    }

    /// Hashes an existing shared value without cloning its contents.
    pub(crate) fn shared(value: Arc<Value>) -> Self {
        let mut hasher = DefaultHasher::new();
        value.as_ref().hash(&mut hasher);
        Self::Shared {
            value_hash: hasher.finish(),
            value,
        }
    }
}

/// Owns an installed identity without exposing or duplicating its concrete value.
#[derive(Clone)]
pub(crate) enum OwnedScopeIdentity {
    /// Identifies an unkeyed site without an allocation.
    Unit,
    /// Retains a keyed value until the last identity owner is dropped.
    Value {
        /// Rejects unequal hashes before downcasting or invoking user equality.
        value_hash: u64,
        /// Preserves concrete type identity through checked downcasts.
        value: Arc<dyn Any + Send + Sync>,
    },
}

impl OwnedScopeIdentity {
    /// Compares a lookup value without allocating another erased owner.
    fn matches<Value>(&self, identity: &ScopeIdentity<Value>) -> bool
    where
        Value: Eq + 'static,
    {
        match identity {
            ScopeIdentity::Unit => matches!(self, Self::Unit),
            ScopeIdentity::Value { value_hash, value } => self.matches_borrowed(*value_hash, value),
            ScopeIdentity::Shared { value_hash, value } => {
                self.matches_borrowed(*value_hash, value.as_ref())
            }
        }
    }

    /// Compares a cached hash and borrowed value without cloning its contents.
    fn matches_borrowed<Value>(&self, other_hash: u64, other_value: &Value) -> bool
    where
        Value: Eq + 'static,
    {
        match self {
            Self::Unit => false,
            Self::Value { value_hash, value } => {
                *value_hash == other_hash
                    && value.as_ref().downcast_ref::<Value>() == Some(other_value)
            }
        }
    }

    /// Allocates only for an ordinary value, reusing an existing shared owner.
    fn from_identity<Value>(identity: ScopeIdentity<Value>) -> Self
    where
        Value: Eq + Hash + Send + Sync + 'static,
    {
        match identity {
            ScopeIdentity::Unit => Self::Unit,
            ScopeIdentity::Value { value_hash, value } => Self::Value {
                value_hash,
                value: Arc::new(value),
            },
            ScopeIdentity::Shared { value_hash, value } => Self::Value { value_hash, value },
        }
    }
}

/// Identifies one declaration site by allocation identity across Store instances.
#[derive(Clone)]
pub(crate) struct ScopeNodeId(Arc<()>);

impl ScopeNodeId {
    /// Allocates a new site identity that cannot alias a retained site.
    fn new() -> Self {
        Self(Arc::new(()))
    }
}

impl PartialEq for ScopeNodeId {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for ScopeNodeId {}
impl Hash for ScopeNodeId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.0).hash(state);
    }
}
impl core::fmt::Debug for ScopeNodeId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("ScopeNodeId(..)")
    }
}

/// Identifies one structural incarnation within its issuing registry.
///
/// Stores each diagnostic ID plus one so optional tokens need no extra word.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
#[repr(transparent)]
pub(crate) struct ScopeToken(NonZeroU64);

impl core::fmt::Debug for ScopeToken {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("ScopeToken")
            .field(&self.diagnostic_id())
            .finish()
    }
}

impl ScopeToken {
    /// Exposes a Store-local diagnostic label, not transferable authority.
    pub(crate) const fn diagnostic_id(self) -> u64 {
        self.0.get() - 1
    }
}

/// Lazily allocates one declaration identity on its first visited reduction.
#[derive(Debug, Default)]
pub(crate) struct ScopeSlot {
    /// Retains the site across visits without allocating for unvisited declarations.
    node: Option<ScopeNodeId>,
}

impl ScopeSlot {
    /// Creates an unvisited declaration without allocating an identity.
    pub(crate) const fn new() -> Self {
        Self { node: None }
    }
}

/// Separates equal identity values beneath different parents or declaration sites.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct MountSite {
    /// Binds lookup to one installed parent incarnation.
    parent: ScopeToken,
    /// Binds lookup to the declaration that owns the child.
    node: ScopeNodeId,
}

/// Links one installed identity into its mount site's collision chain.
struct MountRecord {
    /// Retains the declaration identity until this mount is unlinked.
    node: ScopeNodeId,
    /// Locates the chain without hashing the user value during retirement.
    fingerprint: IdentityFingerprint,
    /// Identifies the preceding collision, or the root sentinel for the head.
    previous_collision: ScopeToken,
    /// Identifies the following collision, or the root sentinel for the tail.
    next_collision: ScopeToken,
    /// Retains the user value for exact equality after fingerprint matches.
    identity: OwnedScopeIdentity,
}

/// Anchors a circular sibling ring in its current retirement order.
#[derive(Clone, Copy)]
struct ChildList {
    /// Starts the forward traversal of the nonempty ring.
    first: ScopeToken,
    /// Ends the traversal and links back to the first child.
    last: ScopeToken,
}

#[cfg(test)]
ordinary_test_items! { mod keyed_relink_tests; }

/// Owns the mutable metadata of one non-root structural incarnation.
struct ScopeRecord {
    /// Invalidates captured authority when the same incarnation changes configuration.
    configuration: u64,
    /// Gates ordinary input without retiring work or the structural lifetime.
    input_open: bool,
    /// Remains fixed for the entire incarnation.
    parent: ScopeToken,
    /// Links to the preceding sibling, including the wraparound at the first child.
    previous_sibling: ScopeToken,
    /// Links to the following sibling, including the wraparound at the last child.
    next_sibling: ScopeToken,
    /// Owns the independent identity-lookup chain links.
    mount: MountRecord,
}

/// Deduplicates departed tokens while preserving first-seen retirement order.
#[derive(Debug, Default)]
pub(crate) struct ScopeTransitionBatch {
    /// Retains ordered departures for diagnostics and transaction reconciliation.
    invalidated: Vec<ScopeToken>,
    /// Supports membership checks without repeatedly scanning ordered departures.
    membership: HashSet<ScopeToken>,
}

impl ScopeTransitionBatch {
    /// Appends each newly departed incarnation once, in the supplied order.
    pub(crate) fn extend(&mut self, tokens: impl IntoIterator<Item = ScopeToken>) {
        for token in tokens {
            if self.membership.insert(token) {
                self.invalidated.push(token);
            }
        }
    }

    /// Checks whether this transaction already recorded the departed incarnation.
    pub(crate) fn contains(&self, token: ScopeToken) -> bool {
        self.membership.contains(&token)
    }

    /// Reports whether reconciliation has any structural departures to process.
    pub(crate) fn is_empty(&self) -> bool {
        self.invalidated.is_empty()
    }

    /// Visits each departure once in the order first recorded.
    #[cfg(feature = "diagnostics")]
    pub(crate) fn iter(&self) -> impl Iterator<Item = ScopeToken> + '_ {
        self.invalidated.iter().copied()
    }
}

/// Identifies one legacy registration without reusing a departed registration's ID.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct HandleScopeId(u64);

/// Shares registration cancellation and permanent expiry without retaining the Store.
pub(crate) struct HandleSignal {
    /// Records cancellation of the pending acquisition before its guard is disarmed.
    cancelled: AtomicBool,
    /// Permanently rejects handles after their selected identity disappears or changes.
    ended: AtomicBool,
    /// Requests actor cleanup when the last strong signal owner disappears.
    control: Weak<ControlWake>,
}

impl HandleSignal {
    /// Creates an active acquisition signal with only weak Store ownership.
    pub(crate) fn new(control: Weak<ControlWake>) -> Arc<Self> {
        Arc::new(Self {
            cancelled: AtomicBool::new(false),
            ended: AtomicBool::new(false),
            control,
        })
    }

    /// Marks an abandoned acquisition before its last signal owner wakes cleanup.
    pub(crate) fn cancel_registration(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    /// Observes whether acquisition was abandoned before handle delivery.
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// Publishes permanent expiry before registered selector captures are released.
    fn end(&self) {
        self.ended.store(true, Ordering::Release);
    }

    /// Observes expiry without running the user selector or contacting the actor.
    pub(crate) fn is_ended(&self) -> bool {
        self.ended.load(Ordering::Acquire)
    }
}

impl Drop for HandleSignal {
    fn drop(&mut self) {
        if let Some(control) = self.control.upgrade() {
            control.notify();
        }
    }
}

/// Retains a legacy registration signal, not the selected state or Store.
#[derive(Clone)]
pub(crate) struct HandleScope {
    /// Selects the registration in the issuing Store's registry.
    id: HandleScopeId,
    /// Keeps the registration eligible for revalidation while handles remain owned.
    signal: Arc<HandleSignal>,
}

impl HandleScope {
    /// Reads permanent expiry without revalidating the selected identity.
    pub(crate) fn is_ended(&self) -> bool {
        self.signal.is_ended()
    }

    /// Captures non-owning authority for work that must not prolong registration.
    pub(crate) fn downgrade(&self) -> WeakHandleScope {
        WeakHandleScope {
            id: self.id,
            signal: Arc::downgrade(&self.signal),
        }
    }
}

/// Lets scoped work detect expiry without retaining an otherwise unused registration.
#[derive(Clone)]
pub(crate) struct WeakHandleScope {
    /// Preserves the issuing registration's identity across temporary upgrades.
    id: HandleScopeId,
    /// Releases registration ownership when all real handles are gone.
    signal: Weak<HandleSignal>,
}

impl WeakHandleScope {
    /// Temporarily owns a surviving registration unless permanent expiry was published.
    pub(crate) fn upgrade(&self) -> Option<HandleScope> {
        let signal = self.signal.upgrade()?;
        if signal.is_ended() {
            return None;
        }
        Some(HandleScope {
            id: self.id,
            signal,
        })
    }

    /// Samples ownership and expiry without invoking the selector.
    pub(crate) fn is_active(&self) -> bool {
        self.upgrade().is_some()
    }
}

/// Selects a child and its lifetime together, retaining ownership only on acquisition.
pub(crate) trait HandleSelector<State> {
    /// Captures the initial identity without retaining a reference into state.
    fn acquire(&self, state: &State) -> Option<OwnedScopeIdentity>;

    /// Borrows the exact selected child after comparing its candidate identity.
    ///
    /// Rejection must end the signal before candidate values are dropped. The
    /// returned reference borrows state, never the selector or expected identity.
    fn select<'a>(
        &self,
        state: &'a State,
        expected: &OwnedScopeIdentity,
        signal: &HandleSignal,
    ) -> Option<&'a dyn Any>;
}

/// Erases one typed projection without allocating candidate identity ownership.
pub(crate) struct TypedHandleSelector<Select, Child, Value> {
    /// Produces an identity and the exact borrowed child from one projection call.
    select: Select,
    /// Records the erased types without owning them or imposing `Sync` on children.
    types: std::marker::PhantomData<fn(Child, Value)>,
}

impl<Select, Child, Value> TypedHandleSelector<Select, Child, Value> {
    /// Keeps the concrete candidate type until acquisition or borrowed comparison.
    pub(crate) fn new<State>(select: Select) -> Self
    where
        Select: for<'a> Fn(&'a State) -> Option<(ScopeIdentity<Value>, &'a Child)>,
    {
        Self {
            select,
            types: std::marker::PhantomData,
        }
    }
}

impl<State, Select, Child, Value> HandleSelector<State>
    for TypedHandleSelector<Select, Child, Value>
where
    Select: for<'a> Fn(&'a State) -> Option<(ScopeIdentity<Value>, &'a Child)>,
    Child: 'static,
    Value: Eq + Hash + Send + Sync + 'static,
{
    fn acquire(&self, state: &State) -> Option<OwnedScopeIdentity> {
        (self.select)(state).map(|(identity, _child)| identity.into_owned())
    }

    fn select<'a>(
        &self,
        state: &'a State,
        expected: &OwnedScopeIdentity,
        signal: &HandleSignal,
    ) -> Option<&'a dyn Any> {
        match (self.select)(state) {
            Some((identity, child)) if expected.matches(&identity) => Some(child),
            rejected => {
                signal.end();
                drop(rejected);
                None
            }
        }
    }
}

/// Clones an identified child's identity only when acquiring its lifetime.
///
/// Later validation borrows both identity and child from the same selection.
/// It runs no identity clone or temporary identity destructor.
pub(crate) struct IdentifiedHandleSelector<Select, Child, Id> {
    /// Borrows the identity and exact child without retaining either reference.
    select: Select,
    /// Records the erased types without owning them or requiring child `Sync`.
    types: std::marker::PhantomData<fn(Child, Id)>,
}

impl<Select, Child, Id> IdentifiedHandleSelector<Select, Child, Id> {
    /// Keeps the combined getter and identity callback in actor-owned storage.
    pub(crate) fn new<State>(select: Select) -> Self
    where
        Select: for<'a> Fn(&'a State) -> Option<(&'a Id, &'a Child)>,
    {
        Self {
            select,
            types: std::marker::PhantomData,
        }
    }
}

impl<State, Select, Child, Id> HandleSelector<State> for IdentifiedHandleSelector<Select, Child, Id>
where
    Select: for<'a> Fn(&'a State) -> Option<(&'a Id, &'a Child)>,
    Child: 'static,
    Id: Clone + Eq + Hash + Send + Sync + 'static,
{
    fn acquire(&self, state: &State) -> Option<OwnedScopeIdentity> {
        (self.select)(state)
            .map(|(identity, _child)| ScopeIdentity::new(identity.clone()).into_owned())
    }

    fn select<'a>(
        &self,
        state: &'a State,
        expected: &OwnedScopeIdentity,
        signal: &HandleSignal,
    ) -> Option<&'a dyn Any> {
        if let Some((identity, child)) = (self.select)(state) {
            let mut hasher = DefaultHasher::new();
            identity.hash(&mut hasher);
            if expected.matches_borrowed(hasher.finish(), identity) {
                return Some(child);
            }
        }
        signal.end();
        None
    }
}

/// Carries a Shared acquisition request into the actor's ordered command stream.
pub(crate) type HandleRegistrationCommand<State> =
    HandleRegistrationCommandCore<dyn HandleSelector<State> + Send>;

/// Owns an acquisition selector and reply until the actor accepts or rejects it.
pub(crate) struct HandleRegistrationCommandCore<Selector: ?Sized> {
    /// Selects and stamps the child without borrowing state beyond the call.
    pub(crate) identity: Box<Selector>,
    /// Connects the caller's acquisition guard to actor-side cancellation checks.
    pub(crate) signal: Arc<HandleSignal>,
    /// Transfers a live handle, or reports that acquisition found no valid child.
    pub(crate) reply: async_channel::Sender<Result<HandleScope, ()>>,
}

/// Retains the original identity and selector while callers own a live signal.
struct HandleRegistration<Selector: ?Sized> {
    /// Remains the comparison target for the whole registration lifetime.
    current: OwnedScopeIdentity,
    /// Revalidates the selected child before access and after committed mutations.
    identity: Box<Selector>,
    /// Prevents the registry from keeping abandoned handles alive.
    active: Weak<HandleSignal>,
}

/// Borrows the actor-owned root or the exact child whose identity it validated.
pub(crate) struct SelectedChild<'state> {
    /// Borrows root state rather than the registry's identity or selector.
    child: &'state dyn Any,
}

impl<'state> SelectedChild<'state> {
    /// Borrows root state for an unscoped observation under actor authority.
    ///
    /// Root observations need no child-identity check. Checked observations must
    /// receive their child from `HandleRegistryCore::with_validation` instead.
    pub(super) fn root<State: 'static>(state: &'state State) -> Self {
        Self { child: state }
    }

    /// Restores the child type paired with this private registration.
    ///
    /// # Panics
    ///
    /// Panics if a private driver requests a different type from its registered
    /// selector. The checked cast keeps this type-erasure boundary safe.
    pub(crate) fn downcast<Child: 'static>(&self) -> &'state Child {
        self.child
            .downcast_ref::<Child>()
            .expect("scoped child type must match registration")
    }
}

#[cfg(test)]
ordinary_test_items! {
/// Selects the Shared registration representation for direct registry tests.
pub(crate) type HandleRegistry<State> = HandleRegistryCore<dyn HandleSelector<State> + Send>;
}

/// Serialises legacy identity validation with Store mutations and handle acquisition.
pub(crate) struct HandleRegistryCore<Selector: ?Sized> {
    /// Advances monotonically so expired registration IDs cannot be revived.
    next_id: u64,
    /// Owns selectors and identities, but only weak references to issued signals.
    registrations: HashMap<HandleScopeId, HandleRegistration<Selector>>,
}

impl<Selector: ?Sized> HandleRegistryCore<Selector> {
    /// Creates an empty registration table without allocating its backing storage.
    pub(crate) fn new() -> Self {
        Self {
            next_id: 0,
            registrations: HashMap::new(),
        }
    }

    /// Selects and validates once, then lends the exact child to the command.
    ///
    /// Missing or ended registrations reject before dropping the callback.
    /// Selection mismatch ends the signal before candidate values, registered
    /// selector captures, or the rejected command are dropped.
    pub(crate) fn with_validation<'state, State, Output>(
        &mut self,
        state: &'state State,
        handle: &HandleScope,
        operation: impl FnOnce(&SelectedChild<'state>) -> Output,
    ) -> Option<Output>
    where
        Selector: HandleSelector<State>,
    {
        let selected = self
            .registrations
            .get(&handle.id)
            .filter(|_| !handle.is_ended())
            .and_then(|registration| {
                registration
                    .identity
                    .select(state, &registration.current, &handle.signal)
            });
        if let Some(child) = selected {
            Some(operation(&SelectedChild { child }))
        } else {
            handle.signal.end();
            self.registrations.remove(&handle.id);
            drop(operation);
            None
        }
    }

    /// Selects the initial identity and delivers a handle only after registration.
    ///
    /// Cancelled requests skip the selector. Failed delivery ends the signal and
    /// removes the registration. Selector and reply-waker panics propagate.
    pub(crate) fn register<State>(
        &mut self,
        state: &State,
        command: HandleRegistrationCommandCore<Selector>,
    ) where
        Selector: HandleSelector<State>,
    {
        let HandleRegistrationCommandCore {
            identity,
            signal,
            reply,
        } = command;
        if signal.is_cancelled() {
            signal.end();
            let _ = reply.try_send(Err(()));
            return;
        }
        let Some(current) = identity.acquire(state) else {
            signal.end();
            let _ = reply.try_send(Err(()));
            return;
        };

        let id = HandleScopeId(self.next_id);
        self.next_id = self
            .next_id
            .checked_add(1)
            .expect("scoped handle ID space exhausted");
        let handle = HandleScope {
            id,
            signal: Arc::clone(&signal),
        };
        let previous = self.registrations.insert(
            id,
            HandleRegistration {
                current,
                identity,
                active: Arc::downgrade(&signal),
            },
        );
        debug_assert!(previous.is_none(), "scoped handle ID must be unique");
        if reply.try_send(Ok(handle)).is_err() {
            signal.end();
            self.registrations.remove(&id);
        }
    }

    /// Revalidates a handle, recording entry immediately before its identity selector.
    ///
    /// An absent registration invokes neither callback. Mismatch ends the handle
    /// before dropping selector captures; callback panics propagate to the caller.
    pub(crate) fn is_current<State>(
        &mut self,
        state: &State,
        handle: &HandleScope,
        before_select: impl FnOnce(),
    ) -> bool
    where
        Selector: HandleSelector<State>,
    {
        // Issued handles stay paired with their Store and IDs are never reused.
        // Their registration guard is disarmed before they can be used.
        let current = self
            .registrations
            .get(&handle.id)
            .filter(|_| !handle.is_ended())
            .is_some_and(|registration| {
                before_select();
                registration
                    .identity
                    .select(state, &registration.current, &handle.signal)
                    .is_some()
            });
        if !current {
            handle.signal.end();
            self.registrations.remove(&handle.id);
        }
        current
    }

    /// Retires abandoned handles or identities changed by the latest mutation.
    ///
    /// Surviving signals remain owned during selection. Expiry is published before
    /// selector captures are dropped; selector and destructor panics propagate.
    pub(crate) fn refresh<State>(&mut self, state: &State)
    where
        Selector: HandleSelector<State>,
    {
        self.registrations.retain(|_id, registration| {
            let Some(active) = registration.active.upgrade() else {
                return false;
            };
            let is_current = !active.is_cancelled()
                && registration
                    .identity
                    .select(state, &registration.current, &active)
                    .is_some();
            if !is_current {
                active.end();
            }
            is_current
        });
    }

    /// Releases abandoned acquisitions without running their selectors.
    ///
    /// Delivered handles have disarmed acquisition guards, so cancellation here
    /// cannot retire an otherwise usable public handle.
    pub(crate) fn remove_inactive(&mut self) {
        self.registrations.retain(|_id, registration| {
            registration
                .active
                .upgrade()
                .is_some_and(|active| !active.is_cancelled())
        });
    }

    /// Counts retained registrations for deterministic Store diagnostics.
    #[cfg(feature = "test-util")]
    pub(crate) fn len(&self) -> usize {
        self.registrations.len()
    }
}

/// Returns the selected incarnation and optional lifecycle-diagnostic metadata.
pub(crate) struct MountOutcome {
    /// Names either the existing equal mount or the newly installed incarnation.
    pub(crate) token: ScopeToken,
    /// Distinguishes installation from reuse without another registry lookup.
    #[cfg(feature = "diagnostics")]
    pub(crate) inserted: bool,
}

/// Owns structural incarnations, ordered child links and sparse reducer metadata.
///
/// Mutation stays on the Store actor. Neither tokens nor captured authority grant
/// direct access to these tables, so user callbacks cannot re-enter their mutation.
pub(crate) struct ScopeRegistry {
    /// Permanent implicit root and collision-chain sentinel, never a removable record.
    root: ScopeToken,
    /// Distinguishes tokens and captures from numerically equal IDs in other Stores.
    authority: Arc<()>,
    /// Invalidates every capture when root configuration changes.
    root_configuration: u64,
    /// Checks root closure without retaining the Store's lifecycle owner.
    lifecycle: Weak<super::lifecycle::Lifecycle>,
    /// Lazily identifies presentation values within this Store only.
    presentation_namespace: Option<Arc<()>>,
    /// Supplies fresh non-root incarnations without recycling retired tokens.
    next_token: u64,
    /// Owns live non-root scopes. The permanent root has no record.
    records: HashMap<ScopeToken, ScopeRecord>,
    /// Stores endpoints only for parents with at least one live child.
    child_lists: HashMap<ScopeToken, ChildList>,
    /// Locates collision-chain heads by parent, declaration site and fingerprint.
    active_mounts: HashMap<MountSite, HashMap<IdentityFingerprint, ScopeToken>>,
    /// Owns sparse reducer metadata until extraction or structural retirement.
    caches: HashMap<ScopeToken, Box<dyn Any + Send>>,
}

/// Captures immutable Store, incarnation, configuration and ancestry authority.
#[derive(Clone)]
pub(crate) struct ScopeAuthority {
    /// Retains Store identity without retaining its state or lifecycle owner.
    root: Arc<()>,
    /// Captures the root revision that must still match during admission.
    root_configuration: u64,
    /// Observes closure and readiness without prolonging the Store lifetime.
    lifecycle: Weak<super::lifecycle::Lifecycle>,
    /// Shares immutable child-to-root tokens, direct parents and configuration revisions.
    ancestry: Option<Arc<[(ScopeToken, ScopeToken, u64)]>>,
}

/// Identifies a retired child without granting live authority.
///
/// Equality includes its Store, incarnation and child configuration revision.
/// This inert metadata cannot route input, reacquire a child or retain user state.
#[derive(Clone)]
pub struct RemovalOrigin {
    /// Prevents retired-child metadata from aliasing another Store's token space.
    root: Arc<()>,
    /// Names the retired incarnation, never a future same-site replacement.
    child: ScopeToken,
    /// Records the child's configuration at the point of checked removal.
    configuration: u64,
}

impl RemovalOrigin {
    /// Returns the retired child's captured configuration revision.
    #[must_use]
    pub fn configuration(&self) -> u64 {
        self.configuration
    }
}

#[cfg(all(test, feature = "allocation-measurement"))]
mod output_origin_costs;
#[cfg(all(test, feature = "allocation-measurement"))]
pub(crate) use output_origin_costs::output_origin_costs;

impl PartialEq for RemovalOrigin {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.root, &other.root)
            && self.child == other.child
            && self.configuration == other.configuration
    }
}
impl Eq for RemovalOrigin {}
impl std::fmt::Debug for RemovalOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemovalOrigin")
            .field("configuration", &self.configuration)
            .finish_non_exhaustive()
    }
}

/// Controls ordinary input without changing the mounted child's lifetime.
///
/// Acquire this capability through `Store::input_gate`. Only the captured
/// direct parent's `FeatureContext` can close or reopen it. Cloning retains
/// metadata only, never the Store, State or Feature.
#[derive(Clone)]
pub struct InputGate {
    /// Names the child whose ordinary input can be closed or reopened.
    target: ScopeToken,
    /// Restricts mutation of the gate to its captured direct parent.
    parent: ScopeToken,
    /// Rejects foreign, retired or reconfigured captures before changing input state.
    authority: ScopeAuthority,
}

impl std::fmt::Debug for InputGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InputGate").finish_non_exhaustive()
    }
}

impl ScopeAuthority {
    /// Derives a gate from acquired child authority, retaining metadata only.
    ///
    /// Root authority has no direct parent and cannot produce an input gate.
    pub(super) fn input_gate(&self) -> InputGate {
        // Only acquired child contexts request gates; their ancestry is non-empty.
        let (target, parent, _) = self
            .ancestry
            .as_ref()
            .expect("input gates require acquired child authority")[0];
        InputGate {
            target,
            parent,
            authority: self.clone(),
        }
    }

    /// Compares Store identity before test diagnostics inspect capture details.
    #[cfg(feature = "test-util")]
    pub(super) fn same_root(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.root, &other.root)
    }

    /// Compares revision captures after the caller rejects foreign Store roots.
    #[cfg(feature = "test-util")]
    pub(super) fn same_capture(&self, other: &Self) -> bool {
        // Pending-input callers reject foreign roots before comparing captures.
        self.root_configuration == other.root_configuration && self.ancestry == other.ancestry
    }

    /// Rejects authority from a different Store without inspecting structural tokens.
    pub(super) fn belongs_to(&self, root: &Arc<()>) -> bool {
        Arc::ptr_eq(&self.root, root)
    }

    /// Checks lifecycle access without claiming that structural authority remains current.
    pub(super) fn admission(&self) -> Result<(), super::task_context::TaskAccessError> {
        use super::task_context::{TaskAccessError, TaskAccessReason, TaskProgress};
        self.lifecycle
            .upgrade()
            .ok_or_else(|| {
                TaskAccessError::new(TaskAccessReason::StoreClosed, TaskProgress::NotStarted)
            })?
            .access()
    }
}

impl ScopeRegistry {
    /// Changes input admission only for a current child's captured direct parent.
    ///
    /// Closed Stores, foreign authority, stale captures and other parents cannot
    /// change the gate. Closing input does not retire the child or its work.
    pub(crate) fn set_input_open(
        &mut self,
        parent: ScopeToken,
        gate: &InputGate,
        open: bool,
    ) -> Result<(), super::task_context::TaskAccessError> {
        use super::task_context::{TaskAccessError, TaskAccessReason, TaskProgress};
        gate.authority.admission()?;
        if parent != gate.parent || !self.accepts(&gate.authority) {
            return Err(TaskAccessError::new(
                TaskAccessReason::ScopeEnded,
                TaskProgress::NotStarted,
            ));
        }
        self.records
            .get_mut(&gate.target)
            .expect("validated gate owns installed record")
            .input_open = open;
        Ok(())
    }

    /// Reports an explicitly closed gate, leaving structural validity to the caller.
    pub(crate) fn input_rejection(
        &self,
        token: ScopeToken,
    ) -> Option<super::task_context::TaskAccessError> {
        use super::task_context::{TaskAccessError, TaskAccessReason, TaskProgress};
        self.records
            .get(&token)
            .filter(|record| !record.input_open)
            .map(|_| TaskAccessError::new(TaskAccessReason::InputClosed, TaskProgress::NotStarted))
    }

    /// Creates an empty registry using the lifecycle's existing root identity.
    pub(crate) fn new(lifecycle: &Arc<super::lifecycle::Lifecycle>) -> Self {
        let root = ScopeToken(NonZeroU64::MIN);
        Self {
            root,
            authority: lifecycle.root_identity(),
            root_configuration: 0,
            lifecycle: Arc::downgrade(lifecycle),
            presentation_namespace: None,
            next_token: 1,
            records: HashMap::new(),
            child_lists: HashMap::new(),
            active_mounts: HashMap::new(),
            caches: HashMap::new(),
        }
    }

    /// Returns the permanent Store-local root, including before readiness.
    pub(crate) const fn root(&self) -> ScopeToken {
        self.root
    }

    /// Reads the revision of an installed incarnation for lifecycle diagnostics.
    #[cfg(feature = "diagnostics")]
    pub(crate) fn configuration(&self, token: ScopeToken) -> u64 {
        if token == self.root {
            self.root_configuration
        } else {
            self.records[&token].configuration
        }
    }

    /// Retains only the Store's inert identity, not its lifecycle or state.
    pub(super) fn root_identity(&self) -> Arc<()> {
        Arc::clone(&self.authority)
    }

    /// Captures inert metadata while checked removal still owns the live child record.
    pub(super) fn removal_origin(&self, child: ScopeToken) -> RemovalOrigin {
        // Checked direct-child removal still owns this live record. The root
        // cannot itself be removed, so it has no historical-child case.
        RemovalOrigin {
            root: Arc::clone(&self.authority),
            child,
            configuration: self.records[&child].configuration,
        }
    }

    /// Counts edges to a strict live ancestor of a current source incarnation.
    ///
    /// The caller freezes a live source before this lookup. Equal endpoints and
    /// foreign branches have no ancestor distance.
    pub(super) fn ancestor_distance(
        &self,
        source: ScopeToken,
        ancestor: ScopeToken,
    ) -> Option<usize> {
        if !self.is_current(ancestor) || source == ancestor {
            return None;
        }
        let mut token = source;
        let mut distance = 0;
        while token != self.root {
            token = self.records[&token].parent;
            distance += 1;
            if token == ancestor {
                return Some(distance);
            }
        }
        None
    }

    /// Allows installation before readiness, but never after root closure.
    pub(crate) fn installation_is_live(&self) -> bool {
        self.lifecycle
            .upgrade()
            .is_some_and(|lifecycle| !lifecycle.is_closed())
    }

    /// Gives standalone fixtures an explicit owner of real lifecycle metadata.
    ///
    /// Production layout and admission have no special test mode.
    #[cfg(test)]
    pub(crate) fn for_test() -> (Arc<super::lifecycle::Lifecycle>, Self) {
        let lifecycle = Arc::new(super::lifecycle::Lifecycle::new());
        let scopes = Self::new(&lifecycle);
        (lifecycle, scopes)
    }

    /// Advances an installed incarnation's revision without replacing its structural token.
    ///
    /// Existing authority captures become stale. Revision exhaustion is a
    /// programming-limit panic, never permission to reuse an earlier revision.
    pub(super) fn refresh_configuration(&mut self, token: ScopeToken) -> u64 {
        let revision = if token == self.root {
            &mut self.root_configuration
        } else {
            &mut self
                .records
                .get_mut(&token)
                .expect("configuration refresh requires installed scope")
                .configuration
        };
        *revision = revision
            .checked_add(1)
            .expect("scope configuration revision space exhausted");
        *revision
    }

    /// Freezes an installed child's ancestry without retaining user identities or caches.
    ///
    /// Root captures allocate no ancestry. Direct children allocate their shared
    /// slice directly; deeper captures collect the chain first. Clones share that
    /// immutable ancestry allocation.
    pub(crate) fn authority(&self, mut token: ScopeToken) -> ScopeAuthority {
        let ancestry: Option<Arc<[(ScopeToken, ScopeToken, u64)]>> = if token == self.root {
            None
        } else {
            let record = self
                .records
                .get(&token)
                .expect("authority requires installed scope");
            let first = (token, record.parent, record.configuration);
            if record.parent == self.root {
                Some(Arc::new([first]))
            } else {
                let mut ancestry = Vec::new();
                ancestry.push(first);
                token = record.parent;
                while token != self.root {
                    let record = &self.records[&token];
                    ancestry.push((token, record.parent, record.configuration));
                    token = record.parent;
                }
                Some(ancestry.into())
            }
        };
        ScopeAuthority {
            root: Arc::clone(&self.authority),
            root_configuration: self.root_configuration,
            lifecycle: Weak::clone(&self.lifecycle),
            ancestry,
        }
    }

    /// Checks Store identity and every captured revision against installed incarnations.
    ///
    /// Lifecycle readiness and closure are separate admission checks. Tokens are
    /// never reused, so parent identity needs no repeated comparison.
    pub(crate) fn accepts(&self, authority: &ScopeAuthority) -> bool {
        Arc::ptr_eq(&self.authority, &authority.root)
            && self.root_configuration == authority.root_configuration
            && authority
                .ancestry
                .as_deref()
                .unwrap_or_default()
                .iter()
                .all(|(token, _, configuration)| {
                    // Tokens are never reused; parent and node identity cannot change.
                    self.records
                        .get(token)
                        .is_some_and(|record| record.configuration == *configuration)
                })
    }

    /// Lazily creates the Store-local namespace shared by its presentation values.
    pub(crate) fn presentation_namespace(&mut self) -> Arc<()> {
        Arc::clone(
            self.presentation_namespace
                .get_or_insert_with(|| Arc::new(())),
        )
    }

    /// Acquires a declaration's stable identity, allocating only on its first visit.
    pub(crate) fn node(slot: &mut ScopeSlot) -> ScopeNodeId {
        slot.node.get_or_insert_with(ScopeNodeId::new).clone()
    }

    /// Borrows live reducer metadata with a checked concrete-type boundary.
    ///
    /// Stale tokens and mismatched cache types are internal contract violations.
    pub(crate) fn scope_cache<T: Send + 'static>(&self, token: ScopeToken) -> Option<&T> {
        assert!(self.is_current(token), "scope cache requires a live token");
        self.caches.get(&token).map(|cache| {
            cache
                .downcast_ref()
                .expect("scope cache type must match its reducer site")
        })
    }

    /// Transfers live reducer metadata out of the registry for reconciliation.
    ///
    /// A type mismatch drops the removed value and panics. Callers cannot recover
    /// that metadata by retrying the lookup with another type.
    pub(crate) fn take_scope_cache<T: Send + 'static>(
        &mut self,
        token: ScopeToken,
    ) -> Option<Box<T>> {
        assert!(self.is_current(token), "scope cache requires a live token");
        self.caches.remove(&token).map(|cache| {
            cache
                .downcast()
                .unwrap_or_else(|_| panic!("scope cache type must match its reducer site"))
        })
    }

    /// Installs metadata into an empty cache slot owned by a live non-root scope.
    ///
    /// An occupied slot is a contract violation, not an implicit replacement.
    pub(crate) fn put_scope_cache<T: Send + 'static>(&mut self, token: ScopeToken, cache: Box<T>) {
        assert!(self.is_current(token), "scope cache requires a live token");
        assert_ne!(token, self.root, "root scope cannot own reducer metadata");
        assert!(
            !self.caches.contains_key(&token),
            "scope cache is already occupied"
        );
        self.caches.insert(token, cache);
    }

    /// Reuses an equal live mount or installs a fresh child incarnation.
    pub(crate) fn mount<Value>(
        &mut self,
        parent: ScopeToken,
        node: ScopeNodeId,
        identity: ScopeIdentity<Value>,
    ) -> ScopeToken
    where
        Value: Eq + Hash + Send + Sync + 'static,
    {
        self.mount_outcome(parent, node, identity).token
    }

    /// Resolves collisions by exact equality before installing a fresh incarnation.
    ///
    /// Lookup can invoke user equality. Installation retains the supplied value
    /// only on a miss and appends the child after all existing siblings.
    pub(crate) fn mount_outcome<Value>(
        &mut self,
        parent: ScopeToken,
        node: ScopeNodeId,
        identity: ScopeIdentity<Value>,
    ) -> MountOutcome
    where
        Value: Eq + Hash + Send + Sync + 'static,
    {
        assert!(
            self.is_current(parent),
            "cannot mount beneath an inactive scope"
        );
        let site = MountSite { parent, node };
        let fingerprint = identity.fingerprint();
        let collision_head = self
            .active_mounts
            .get(&site)
            .and_then(|mounts| mounts.get(&fingerprint))
            .copied()
            .unwrap_or(self.root);
        let mut candidate = collision_head;
        while candidate != self.root {
            let mount = &self
                .records
                .get(&candidate)
                .expect("active mount must have a scope record")
                .mount;
            if mount.identity.matches(&identity) {
                return MountOutcome {
                    token: candidate,
                    #[cfg(feature = "diagnostics")]
                    inserted: false,
                };
            }
            candidate = mount.next_collision;
        }

        let encoded = NonZeroU64::MIN
            .checked_add(self.next_token)
            .expect("scope token ID space exhausted");
        let token = ScopeToken(encoded);
        self.next_token = encoded.get();
        let previous = self.records.insert(
            token,
            ScopeRecord {
                configuration: 0,
                input_open: true,
                parent,
                previous_sibling: self.root,
                next_sibling: self.root,
                mount: MountRecord {
                    node: site.node.clone(),
                    fingerprint,
                    previous_collision: self.root,
                    next_collision: collision_head,
                    identity: OwnedScopeIdentity::from_identity(identity),
                },
            },
        );
        debug_assert!(previous.is_none(), "scope token must be unique");
        if collision_head != self.root {
            self.records
                .get_mut(&collision_head)
                .expect("collision-chain mount must have a scope record")
                .mount
                .previous_collision = token;
        }
        self.active_mounts
            .entry(site)
            .or_default()
            .insert(fingerprint, token);
        self.append_child(parent, token);
        MountOutcome {
            token,
            #[cfg(feature = "diagnostics")]
            inserted: true,
        }
    }

    /// Retires a subtree in sibling-ordered postorder without recursive stack growth.
    ///
    /// A previously retired token is a no-op. The permanent root cannot leave.
    /// Cache and identity destructors run during retirement and can propagate panics.
    pub(crate) fn leave(&mut self, token: ScopeToken) -> Vec<ScopeToken> {
        assert_ne!(token, self.root, "the root scope cannot leave");
        if !self.is_current(token) {
            return Vec::new();
        }

        let mut invalidated = Vec::new();
        self.invalidate_subtree(token, &mut invalidated);
        invalidated
    }

    /// Checks structural membership, independently of Store readiness and root closure.
    pub(crate) fn is_current(&self, token: ScopeToken) -> bool {
        token == self.root || self.records.contains_key(&token)
    }

    /// Reads a live child's immutable parent, returning none for root or retired tokens.
    pub(crate) fn parent(&self, token: ScopeToken) -> Option<ScopeToken> {
        self.records.get(&token).map(|record| record.parent)
    }

    /// Permutes native children without moving legacy slots or changing child authority.
    ///
    /// The roster supplies distinct live direct children of this parent. Their
    /// positions in the complete sibling ring receive the requested order. All
    /// allocation finishes before link mutation, which invokes no user code.
    pub(crate) fn relink_native_children(&mut self, parent: ScopeToken, desired: &[ScopeToken]) {
        if desired.len() < 2 {
            return;
        }
        let selected: HashSet<_> = desired.iter().copied().collect();
        let children = self
            .child_lists
            .get(&parent)
            .expect("selected children have a sibling ring");
        let mut order = Vec::new();
        let mut cursor = children.first;
        loop {
            order.push(cursor);
            if cursor == children.last {
                break;
            }
            cursor = self.records[&cursor].next_sibling;
        }
        #[cfg(all(test, feature = "allocation-measurement"))]
        super::mount::collection_costs::record_ring(order.len());
        let mut native = desired.iter();
        let mut changed = false;
        for token in &mut order {
            if selected.contains(token) {
                let replacement = *native.next().expect("selected native slot");
                changed |= *token != replacement;
                *token = replacement;
            }
        }
        if !changed {
            return;
        }
        // All allocation is complete. This loop runs no user code.
        for (index, token) in order.iter().enumerate() {
            let record = self.records.get_mut(token).expect("validated sibling");
            record.previous_sibling = order[(index + order.len() - 1) % order.len()];
            record.next_sibling = order[(index + 1) % order.len()];
        }
        let children = self
            .child_lists
            .get_mut(&parent)
            .expect("validated sibling ring");
        children.first = order[0];
        children.last = *order.last().expect("nonempty sibling ring");
    }

    /// Checks that structural removal already revoked output authority before cancellation.
    pub(crate) fn invalidate_outputs(&self, transitions: &ScopeTransitionBatch) {
        debug_assert!(
            transitions
                .invalidated
                .iter()
                .all(|token| !self.is_current(*token)),
            "departed scopes must reject outputs before resource cancellation"
        );
    }

    /// Removes descendants before parents, releasing cache metadata before identity ownership.
    ///
    /// The traversal follows stored links and needs no recursive call stack.
    /// Destructors can panic, so partial retirement has no recovery contract.
    fn invalidate_subtree(&mut self, token: ScopeToken, invalidated: &mut Vec<ScopeToken>) {
        let mut cursor = token;
        loop {
            if let Some(children) = self.child_lists.get(&cursor) {
                cursor = children.first;
                continue;
            }

            let record = self
                .records
                .remove(&cursor)
                .expect("current scope must have a record");
            self.caches.remove(&cursor);
            let parent = record.parent;
            let has_next_sibling = self
                .child_lists
                .get(&parent)
                .expect("current child must have a parent child list")
                .last
                != cursor;
            let next_sibling = record.next_sibling;
            self.unlink_child(cursor, &record);
            self.unlink_mount(parent, cursor, record.mount);
            invalidated.push(cursor);

            if cursor == token {
                break;
            }
            cursor = if has_next_sibling {
                next_sibling
            } else {
                parent
            };
        }
    }

    /// Appends a freshly allocated token to the parent's circular sibling ring.
    fn append_child(&mut self, parent: ScopeToken, token: ScopeToken) {
        assert!(
            self.is_current(parent),
            "cannot append beneath an inactive scope"
        );
        let Some(children) = self.child_lists.get(&parent).copied() else {
            let child = self
                .records
                .get_mut(&token)
                .expect("appended child must have a scope record");
            child.previous_sibling = token;
            child.next_sibling = token;
            let previous = self.child_lists.insert(
                parent,
                ChildList {
                    first: token,
                    last: token,
                },
            );
            debug_assert!(previous.is_none(), "new child list must be unique");
            return;
        };

        if children.first == children.last {
            // The fresh token cannot alias the single existing sibling.
            let [existing, child] = self.records.get_disjoint_mut([&children.first, &token]);
            let existing = existing.expect("first child must have a scope record");
            let child = child.expect("appended child must have a scope record");
            child.previous_sibling = children.first;
            child.next_sibling = children.first;
            existing.next_sibling = token;
            existing.previous_sibling = token;
        } else {
            // Mount allocated a fresh token, distinct from both existing siblings.
            let [first, last, child] =
                self.records
                    .get_disjoint_mut([&children.first, &children.last, &token]);
            let first = first.expect("first child must have a scope record");
            let last = last.expect("last child must have a scope record");
            let child = child.expect("appended child must have a scope record");
            child.previous_sibling = children.last;
            child.next_sibling = children.first;
            last.next_sibling = token;
            first.previous_sibling = token;
        }
        self.child_lists
            .get_mut(&parent)
            .expect("current parent must have a child list")
            .last = token;
    }

    /// Removes one already-detached record from its still-live parent's sibling ring.
    fn unlink_child(&mut self, token: ScopeToken, record: &ScopeRecord) {
        assert!(
            self.is_current(record.parent),
            "child parent must remain current while unlinking"
        );
        let children = self
            .child_lists
            .get(&record.parent)
            .copied()
            .expect("current child must have a parent child list");
        if children.first == token && children.last == token {
            let removed = self.child_lists.remove(&record.parent);
            debug_assert!(removed.is_some(), "only child list must exist");
            return;
        }

        self.records
            .get_mut(&record.previous_sibling)
            .expect("previous sibling must have a scope record")
            .next_sibling = record.next_sibling;
        self.records
            .get_mut(&record.next_sibling)
            .expect("next sibling must have a scope record")
            .previous_sibling = record.previous_sibling;
        let children = self
            .child_lists
            .get_mut(&record.parent)
            .expect("current parent must have a child list");
        if children.first == token {
            children.first = record.next_sibling;
        }
        if children.last == token {
            children.last = record.previous_sibling;
        }
    }

    /// Releases a removed identity and reconnects its collision-chain neighbours.
    ///
    /// Identity destruction precedes collision-link updates. A destructor panic
    /// propagates, and the enclosing Store must not resume registry operations.
    fn unlink_mount(&mut self, parent: ScopeToken, token: ScopeToken, mount: MountRecord) {
        let MountRecord {
            node,
            fingerprint,
            previous_collision,
            next_collision,
            identity,
        } = mount;
        drop(identity);
        let site = MountSite { parent, node };
        if previous_collision == self.root {
            let fingerprints = self
                .active_mounts
                .get_mut(&site)
                .expect("active mount site must exist");
            if next_collision == self.root {
                fingerprints.remove(&fingerprint);
            } else {
                fingerprints.insert(fingerprint, next_collision);
            }
            if fingerprints.is_empty() {
                self.active_mounts.remove(&site);
            }
        } else {
            self.records
                .get_mut(&previous_collision)
                .expect("previous collision must have a scope record")
                .mount
                .next_collision = next_collision;
        }
        if next_collision != self.root {
            self.records
                .get_mut(&next_collision)
                .expect("next collision must have a scope record")
                .mount
                .previous_collision = previous_collision;
        }

        debug_assert!(!self.records.contains_key(&token));
    }
}

#[cfg(test)]
ordinary_test_items! {
mod tests {
    use super::*;
    use crate::{DependencyValues, MonotonicInstant, ReduceContext};

    #[test]
    fn rejected_handle_identity_drops_after_permanent_expiry() {
        use std::sync::Mutex;

        use super::{HandleRegistrationCommand, HandleRegistry, HandleSignal};

        struct Identity {
            value: u8,
            signal: std::sync::Weak<HandleSignal>,
            drops: Arc<Mutex<Vec<(u8, bool)>>>,
        }

        impl PartialEq for Identity {
            fn eq(&self, other: &Self) -> bool {
                self.value == other.value
            }
        }
        impl Eq for Identity {}
        impl Hash for Identity {
            fn hash<H: Hasher>(&self, state: &mut H) {
                self.value.hash(state);
            }
        }
        impl Drop for Identity {
            fn drop(&mut self) {
                self.drops.lock().unwrap().push((
                    self.value,
                    self.signal.upgrade().is_some_and(|signal| signal.is_ended()),
                ));
            }
        }

        enum Path {
            Checked,
            Action,
            Refresh,
        }
        let mut outcomes = Vec::new();
        for path in [Path::Checked, Path::Action, Path::Refresh] {
            let signal = HandleSignal::new(std::sync::Weak::new());
            let weak = Arc::downgrade(&signal);
            let drops = Arc::new(Mutex::new(Vec::new()));
            let recorded = Arc::clone(&drops);
            let mut registry = HandleRegistry::new();
            let (reply, receive) = async_channel::bounded(1);
            registry.register(
                &7,
                HandleRegistrationCommand {
                    identity: Box::new(TypedHandleSelector::new(move |value: &u8| {
                        Some((
                            ScopeIdentity::new(Identity {
                                value: *value,
                                signal: std::sync::Weak::clone(&weak),
                                drops: Arc::clone(&recorded),
                            }),
                            value,
                        ))
                    })),
                    signal: Arc::clone(&signal),
                    reply,
                },
            );
            let handle = receive.try_recv().unwrap().unwrap();
            assert!(drops.lock().unwrap().is_empty());
            match path {
                Path::Checked => assert!(
                    registry
                        .with_validation(&8, &handle, |_| {
                            panic!("rejected selection must not run its command");
                        })
                        .is_none()
                ),
                Path::Action => assert!(!registry.is_current(&8, &handle, || {})),
                Path::Refresh => registry.refresh(&8),
            }
            assert!(handle.is_ended());
            assert!(registry.registrations.is_empty());
            outcomes.push(drops.lock().unwrap().clone());
        }
        assert_eq!(outcomes, vec![vec![(8, true), (7, true)]; 3]);
    }

    /// Depth used to prove native invalidation does not recurse.
    ///
    /// This must remain large enough to overflow a recursive traversal stack.
    #[cfg(not(miri))]
    const DEEP_TREE_DEPTH: u64 = 100_000;

    /// Depth used to exercise iterative invalidation under Miri.
    ///
    /// Miri interprets hash-table growth and is bounded to keep this safety gate sustainable.
    #[cfg(miri)]
    const DEEP_TREE_DEPTH: u64 = 1_000;

    /// Forces distinct values into the same hash chain for exact-equality tests.
    #[derive(Clone, Eq, PartialEq)]
    struct Collision(u8);

    impl Hash for Collision {
        fn hash<H: Hasher>(&self, state: &mut H) {
            0_u8.hash(state);
        }
    }

    #[test]
    fn different_identity_hashes_reject_without_invoking_equality() {
        #[derive(Clone)]
        struct Identity {
            value: u8,
            comparisons: Arc<std::sync::atomic::AtomicUsize>,
        }

        impl PartialEq for Identity {
            fn eq(&self, other: &Self) -> bool {
                self.comparisons.fetch_add(1, Ordering::Relaxed);
                self.value == other.value
            }
        }
        impl Eq for Identity {}
        impl Hash for Identity {
            fn hash<H: Hasher>(&self, state: &mut H) {
                self.value.hash(state);
            }
        }

        let comparisons = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        for value in [1, 2] {
            for shared_owner in [false, true] {
                let original = Identity {
                    value,
                    comparisons: Arc::clone(&comparisons),
                };
                let owned = if shared_owner {
                    ScopeIdentity::shared(Arc::new(original)).into_owned()
                } else {
                    ScopeIdentity::new(original).into_owned()
                };
                for shared_query in [false, true] {
                    for (query, expected, calls) in [(value, true, 1), (3 - value, false, 0)] {
                        let query = Identity {
                            value: query,
                            comparisons: Arc::clone(&comparisons),
                        };
                        let identity = if shared_query {
                            ScopeIdentity::shared(Arc::new(query))
                        } else {
                            ScopeIdentity::new(query)
                        };
                        let OwnedScopeIdentity::Value { value_hash, .. } = &owned else {
                            panic!("value identity required");
                        };
                        let other_hash = match &identity {
                            ScopeIdentity::Value { value_hash, .. }
                            | ScopeIdentity::Shared { value_hash, .. } => value_hash,
                            ScopeIdentity::Unit => panic!("value identity required"),
                        };
                        assert_eq!(value_hash == other_hash, expected);
                        comparisons.store(0, Ordering::Relaxed);
                        assert_eq!(owned.matches(&identity), expected);
                        assert_eq!(comparisons.load(Ordering::Relaxed), calls);
                    }
                }
            }
        }
    }

    #[test]
    fn fingerprints_and_erased_clones_never_repeat_user_hashing() {
        #[derive(Clone)]
        struct Identity(Arc<std::sync::atomic::AtomicUsize>);
        impl PartialEq for Identity {
            fn eq(&self, _: &Self) -> bool {
                true
            }
        }
        impl Eq for Identity {}
        impl Hash for Identity {
            fn hash<H: Hasher>(&self, state: &mut H) {
                assert_eq!(state.finish(), DefaultHasher::new().finish());
                self.0.fetch_add(1, Ordering::Relaxed);
                7_u8.hash(state);
            }
        }
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let query = ScopeIdentity::new(Identity(Arc::clone(&calls)));
        for identity in [
            ScopeIdentity::new(Identity(Arc::clone(&calls))),
            ScopeIdentity::shared(Arc::new(Identity(Arc::clone(&calls)))),
        ] {
            let fingerprint = identity.fingerprint();
            assert_eq!(identity.fingerprint(), fingerprint);
            let owned = identity.into_owned();
            assert!(owned.clone().matches(&query));
        }
        assert_eq!(calls.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn non_clone_identities_keep_erased_and_registry_ownership() {
        // These paths move identity values or share their owners, never clone them.
        #[derive(Eq, Hash, PartialEq)]
        struct Identity(u8);

        let value = Arc::new(Identity(7));
        let weak = Arc::downgrade(&value);
        let owned = ScopeIdentity::new(Identity(7)).into_owned();
        let shared = ScopeIdentity::shared(Arc::clone(&value));
        assert!(owned.matches(&shared));
        let shared = shared.into_owned();
        let clone = shared.clone();
        assert!(clone.matches(&ScopeIdentity::new(Identity(7))));
        drop(shared);
        drop(clone);

        let (_lifecycle, mut scopes) = ScopeRegistry::for_test();
        let node = ScopeNodeId::new();
        let first = scopes.mount(scopes.root(), node.clone(), ScopeIdentity::shared(value));
        let mut dependencies = DependencyValues::new();
        let mut transitions = ScopeTransitionBatch::default();
        let mut context = ReduceContext::new(
            &mut dependencies,
            MonotonicInstant::ZERO,
            &mut scopes,
            &mut transitions,
        );
        let same = context.mount(node, ScopeIdentity::new(Identity(7)));
        assert_eq!(same, first);
        drop(context);

        assert!(
            weak.upgrade().is_some(),
            "the registry retains the original owner"
        );
        drop(scopes);
        assert!(
            weak.upgrade().is_none(),
            "registry teardown releases the final owner"
        );
    }

    #[test]
    fn shared_identity_retains_the_original_value_until_last_owner_leaves() {
        let value = Arc::new(Collision(1));
        let weak = Arc::downgrade(&value);
        let owned = ScopeIdentity::shared(Arc::clone(&value)).into_owned();
        drop(value);
        assert!(
            weak.upgrade().is_some(),
            "the original identity must remain owned"
        );
        assert!(owned.matches(&ScopeIdentity::new(Collision(1))));
        drop(owned);
        assert!(
            weak.upgrade().is_none(),
            "the final owner must release the identity"
        );
    }

    #[test]
    fn shared_and_ordinary_identities_agree_after_hash_collisions() {
        let sources = [
            ScopeIdentity::new(Collision(1)),
            ScopeIdentity::shared(Arc::new(Collision(1))),
        ];
        for source in sources {
            let fingerprint = source.fingerprint();
            let owned = source.into_owned();
            for (identity, expected) in [
                (ScopeIdentity::new(Collision(1)), true),
                (ScopeIdentity::shared(Arc::new(Collision(1))), true),
                (ScopeIdentity::new(Collision(2)), false),
                (ScopeIdentity::shared(Arc::new(Collision(2))), false),
            ] {
                assert_eq!(fingerprint, identity.fingerprint());
                assert_eq!(owned.matches(&identity), expected);
                let other = identity.into_owned();
                assert_eq!(other.matches(&ScopeIdentity::new(Collision(1))), expected);
                assert_eq!(
                    other.matches(&ScopeIdentity::shared(Arc::new(Collision(1)))),
                    expected
                );
            }
        }
    }

    #[test]
    fn shared_identities_preserve_concrete_type_and_unit_distinctions() {
        for identity in [
            ScopeIdentity::new(Collision(0)),
            ScopeIdentity::shared(Arc::new(Collision(0))),
        ] {
            let owned = identity.into_owned();
            for other in [
                ScopeIdentity::new(0_u8),
                ScopeIdentity::shared(Arc::new(0_u8)),
            ] {
                assert!(!owned.matches(&other));
                let other = other.into_owned();
                assert!(!other.matches(&ScopeIdentity::new(Collision(0))));
                assert!(!other.matches(&ScopeIdentity::shared(Arc::new(Collision(0)))));
            }
        }
        let unit = ScopeIdentity::unit().into_owned();
        let value = ScopeIdentity::shared(Arc::new(()));
        assert!(!unit.matches(&value));
        let value = value.into_owned();
        assert!(!value.matches(&ScopeIdentity::unit()));
        assert!(value.matches(&ScopeIdentity::new(())));
    }

    #[test]
    fn erased_arc_values_keep_their_concrete_type() {
        for identity in [
            ScopeIdentity::new(Arc::new(7_u8)),
            ScopeIdentity::shared(Arc::new(Arc::new(7_u8))),
        ] {
            let owned = identity.into_owned();
            assert!(owned.matches(&ScopeIdentity::new(Arc::new(7_u8))));
            let shared = ScopeIdentity::shared(Arc::new(Arc::new(7_u8)));
            assert!(owned.matches(&shared));
            assert!(
                shared
                    .into_owned()
                    .matches(&ScopeIdentity::new(Arc::new(7_u8)))
            );
            assert!(!owned.matches(&ScopeIdentity::new(7_u8)));
            assert!(!owned.matches(&ScopeIdentity::shared(Arc::new(7_u8))));
        }
    }

    #[test]
    fn shared_mounts_reuse_equal_values_and_release_only_after_scope_and_caller() {
        let value = Arc::new(Collision(1));
        let weak = Arc::downgrade(&value);
        let (_registry_lifecycle, mut registry) = ScopeRegistry::for_test();
        let root = registry.root();
        let node = ScopeNodeId::new();
        let token = registry.mount(
            root,
            node.clone(),
            ScopeIdentity::shared(Arc::clone(&value)),
        );
        assert_eq!(
            registry.mount(root, node.clone(), ScopeIdentity::new(Collision(1))),
            token
        );
        assert_eq!(
            registry.mount(
                root,
                node.clone(),
                ScopeIdentity::shared(Arc::new(Collision(1)))
            ),
            token
        );
        let different = registry.mount(
            root,
            node.clone(),
            ScopeIdentity::shared(Arc::new(Collision(2))),
        );
        assert_ne!(different, token);
        assert_eq!(registry.leave(token), [token]);
        assert!(
            weak.upgrade().is_some(),
            "caller still owns the original stamp"
        );
        drop(value);
        assert!(weak.upgrade().is_none());
        assert!(registry.is_current(different));
    }

    #[test]
    fn owned_identity_checks_values_after_hash_collision() {
        let first = ScopeIdentity::new(Collision(1));
        let same = ScopeIdentity::new(Collision(1));
        let different = ScopeIdentity::new(Collision(2));
        assert_eq!(first.fingerprint(), different.fingerprint());
        let owned = first.into_owned();
        assert!(owned.matches(&same));
        assert!(!owned.matches(&different));
        let same = same.into_owned();
        let different = different.into_owned();
        assert!(same.matches(&ScopeIdentity::new(Collision(1))));
        assert!(!different.matches(&ScopeIdentity::new(Collision(1))));
    }

    #[test]
    fn owned_identity_rejects_different_types_with_identical_value_hashes() {
        let first = ScopeIdentity::new(Collision(0));
        let different_type = ScopeIdentity::new(0_u8);
        let ScopeIdentity::Value {
            value_hash: first_hash,
            ..
        } = &first
        else {
            unreachable!();
        };
        let ScopeIdentity::Value {
            value_hash: other_hash,
            ..
        } = &different_type
        else {
            unreachable!();
        };
        assert_eq!(first_hash, other_hash);
        let owned = ScopeIdentity::new(Collision(0)).into_owned();
        let other_owned = ScopeIdentity::new(0_u8).into_owned();
        assert!(!owned.matches(&different_type));
        assert!(!other_owned.matches(&first));
    }

    #[test]
    fn owned_identity_distinguishes_unit_from_unit_value() {
        let unit = ScopeIdentity::unit().into_owned();
        let value = ScopeIdentity::new(()).into_owned();
        assert!(unit.matches(&ScopeIdentity::unit()));
        assert!(value.matches(&ScopeIdentity::new(())));
        assert!(unit.clone().matches(&ScopeIdentity::unit()));
        assert!(value.clone().matches(&ScopeIdentity::new(())));
        assert!(!unit.matches(&ScopeIdentity::new(())));
        assert!(!value.matches(&ScopeIdentity::unit()));
    }

    #[test]
    fn empty_registry_keeps_only_root_current_without_records() {
        let (lifecycle, registry) = ScopeRegistry::for_test();
        assert!(Arc::ptr_eq(
            &lifecycle.root_identity(),
            &registry.root_identity()
        ));
        assert!(registry.is_current(registry.root()));
        assert!(!registry.is_current(ScopeToken(NonZeroU64::MAX)));
        assert!(registry.records.is_empty());
        let weak_lifecycle = Arc::downgrade(&lifecycle);
        drop(lifecycle);
        assert!(weak_lifecycle.upgrade().is_none());
        assert!(!registry.installation_is_live());
    }

    #[test]
    fn root_leave_panics_without_mutating_registry() {
        let node_1 = ScopeNodeId::new();
        let (_registry_lifecycle, mut registry) = ScopeRegistry::for_test();
        let root = registry.root();
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            registry.leave(root);
        }));
        assert!(panic.is_err());
        assert!(registry.is_current(root));
        assert!(registry.records.is_empty());
        assert!(registry.child_lists.is_empty());
        assert!(registry.active_mounts.is_empty());
        let child = registry.mount(root, node_1.clone(), ScopeIdentity::unit());
        assert!(registry.is_current(child));
    }

    #[test]
    fn last_subtree_removal_preserves_root_for_fresh_remount() {
        let node_1 = ScopeNodeId::new();
        let node_2 = ScopeNodeId::new();
        let (_registry_lifecycle, mut registry) = ScopeRegistry::for_test();
        let root = registry.root();
        let parent = registry.mount(root, node_1.clone(), ScopeIdentity::new(7_u8));
        let child = registry.mount(parent, node_2.clone(), ScopeIdentity::unit());
        assert_eq!(registry.leave(parent), [child, parent]);
        assert!(registry.is_current(root));
        assert!(!registry.is_current(parent));
        assert!(!registry.is_current(child));
        assert!(registry.records.is_empty());
        let remounted = registry.mount(root, node_1.clone(), ScopeIdentity::new(7_u8));
        assert_ne!(remounted, parent);
        assert!(registry.is_current(remounted));
        assert!(!registry.is_current(parent));
    }

    #[test]
    fn leaving_parent_invalidates_descendants_only() {
        let node_1 = ScopeNodeId::new();
        let node_2 = ScopeNodeId::new();
        let node_3 = ScopeNodeId::new();
        let (_registry_lifecycle, mut registry) = ScopeRegistry::for_test();
        let root = registry.root();
        let parent = registry.mount(root, node_1.clone(), ScopeIdentity::unit());
        let child = registry.mount(parent, node_2.clone(), ScopeIdentity::unit());
        let sibling = registry.mount(root, node_3.clone(), ScopeIdentity::unit());

        let invalidated = registry.leave(parent);

        assert_eq!(invalidated, [child, parent]);
        assert!(!registry.is_current(parent));
        assert!(!registry.is_current(child));
        assert!(registry.is_current(sibling));
    }

    #[test]
    fn leaving_mixed_tree_preserves_insertion_ordered_postorder() {
        let node_1 = ScopeNodeId::new();
        let node_2 = ScopeNodeId::new();
        let node_3 = ScopeNodeId::new();
        let (_registry_lifecycle, mut registry) = ScopeRegistry::for_test();
        let root = registry.root();
        let parent = registry.mount(root, node_1.clone(), ScopeIdentity::unit());
        let first = registry.mount(parent, node_2.clone(), ScopeIdentity::new(1_u8));
        let nested = registry.mount(first, node_3.clone(), ScopeIdentity::unit());
        let middle = registry.mount(parent, node_2.clone(), ScopeIdentity::new(2_u8));
        let last = registry.mount(parent, node_2.clone(), ScopeIdentity::new(3_u8));

        assert_eq!(
            registry.leave(parent),
            [nested, first, middle, last, parent]
        );
    }

    #[test]
    fn leaving_mixed_tree_after_middle_removal_preserves_ordered_postorder() {
        let node_1 = ScopeNodeId::new();
        let node_2 = ScopeNodeId::new();
        let node_3 = ScopeNodeId::new();
        let (_registry_lifecycle, mut registry) = ScopeRegistry::for_test();
        let root = registry.root();
        let parent = registry.mount(root, node_1.clone(), ScopeIdentity::unit());
        let first = registry.mount(parent, node_2.clone(), ScopeIdentity::new(1_u8));
        let nested = registry.mount(first, node_3.clone(), ScopeIdentity::unit());
        let middle = registry.mount(parent, node_2.clone(), ScopeIdentity::new(2_u8));
        let last = registry.mount(parent, node_2.clone(), ScopeIdentity::new(3_u8));

        assert_eq!(registry.leave(middle), [middle]);
        assert_eq!(registry.leave(parent), [nested, first, last, parent]);
    }

    #[test]
    fn leaving_deep_tree_is_iterative() {
        let node_0 = ScopeNodeId::new();
        let (_registry_lifecycle, mut registry) = ScopeRegistry::for_test();
        let root = registry.root();
        let first = registry.mount(root, node_0.clone(), ScopeIdentity::unit());
        let mut deepest = first;
        for _ in 1..DEEP_TREE_DEPTH {
            deepest = registry.mount(deepest, ScopeNodeId::new(), ScopeIdentity::unit());
        }

        let invalidated = registry.leave(first);

        assert_eq!(
            invalidated.len(),
            usize::try_from(DEEP_TREE_DEPTH).expect("deep-tree test depth must fit usize")
        );
        assert_eq!(invalidated.first(), Some(&deepest));
        assert_eq!(invalidated.last(), Some(&first));
        assert!(registry.is_current(root));
    }

    #[test]
    fn mounting_equal_identity_reuses_only_the_same_parent_and_node() {
        let node_1 = ScopeNodeId::new();
        let node_2 = ScopeNodeId::new();
        let node_3 = ScopeNodeId::new();
        let (_registry_lifecycle, mut registry) = ScopeRegistry::for_test();
        let root = registry.root();
        let parent = registry.mount(root, node_1.clone(), ScopeIdentity::unit());
        let sibling = registry.mount(root, node_2.clone(), ScopeIdentity::unit());

        let first = registry.mount(parent, node_3.clone(), ScopeIdentity::new(7_u8));
        let same = registry.mount(parent, node_3.clone(), ScopeIdentity::new(7_u8));
        let nested = registry.mount(first, node_3.clone(), ScopeIdentity::new(7_u8));
        let other_sibling = registry.mount(sibling, node_3.clone(), ScopeIdentity::new(7_u8));

        assert_eq!(same, first);
        assert_ne!(nested, first);
        assert_ne!(other_sibling, first);
        assert_ne!(other_sibling, nested);
    }

    #[test]
    fn identity_checks_concrete_values_after_hash_collision() {
        let node_1 = ScopeNodeId::new();
        let (_registry_lifecycle, mut registry) = ScopeRegistry::for_test();
        let root = registry.root();

        let first = registry.mount(root, node_1.clone(), ScopeIdentity::new(Collision(1)));
        let second = registry.mount(root, node_1.clone(), ScopeIdentity::new(Collision(2)));

        assert_ne!(first, second);
    }

    #[test]
    fn collision_chain_middle_removal_preserves_other_mounts() {
        let (_registry_lifecycle, mut registry) = ScopeRegistry::for_test();
        let root = registry.root();
        let node = ScopeNodeId::new();
        let first = registry.mount(root, node.clone(), ScopeIdentity::new(Collision(1)));
        let middle = registry.mount(root, node.clone(), ScopeIdentity::new(Collision(2)));
        let last = registry.mount(root, node.clone(), ScopeIdentity::new(Collision(3)));

        assert_eq!(registry.leave(middle), [middle]);
        assert_eq!(
            registry.mount(root, node.clone(), ScopeIdentity::new(Collision(1))),
            first
        );
        assert_eq!(
            registry.mount(root, node.clone(), ScopeIdentity::new(Collision(3))),
            last
        );
        assert_ne!(
            registry.mount(root, node.clone(), ScopeIdentity::new(Collision(2))),
            middle
        );
    }

    #[test]
    fn collision_chain_head_removal_preserves_other_mounts() {
        let (_registry_lifecycle, mut registry) = ScopeRegistry::for_test();
        let root = registry.root();
        let node = ScopeNodeId::new();
        let first = registry.mount(root, node.clone(), ScopeIdentity::new(Collision(1)));
        let middle = registry.mount(root, node.clone(), ScopeIdentity::new(Collision(2)));
        let head = registry.mount(root, node.clone(), ScopeIdentity::new(Collision(3)));

        assert_eq!(registry.leave(head), [head]);
        assert_eq!(
            registry.mount(root, node.clone(), ScopeIdentity::new(Collision(1))),
            first
        );
        assert_eq!(
            registry.mount(root, node.clone(), ScopeIdentity::new(Collision(2))),
            middle
        );
        assert_ne!(
            registry.mount(root, node.clone(), ScopeIdentity::new(Collision(3))),
            head
        );
    }

    #[test]
    fn collision_chain_tail_removal_preserves_other_mounts() {
        let (_registry_lifecycle, mut registry) = ScopeRegistry::for_test();
        let root = registry.root();
        let node = ScopeNodeId::new();
        let tail = registry.mount(root, node.clone(), ScopeIdentity::new(Collision(1)));
        let middle = registry.mount(root, node.clone(), ScopeIdentity::new(Collision(2)));
        let last = registry.mount(root, node.clone(), ScopeIdentity::new(Collision(3)));

        assert_eq!(registry.leave(tail), [tail]);
        assert_eq!(
            registry.mount(root, node.clone(), ScopeIdentity::new(Collision(2))),
            middle
        );
        assert_eq!(
            registry.mount(root, node.clone(), ScopeIdentity::new(Collision(3))),
            last
        );
        assert_ne!(
            registry.mount(root, node.clone(), ScopeIdentity::new(Collision(1))),
            tail
        );
    }

    #[test]
    fn identity_equality_panic_drops_query_without_installing_a_record() {
        #[derive(Clone)]
        struct Identity {
            panic: Arc<AtomicBool>,
            drops: Arc<std::sync::atomic::AtomicUsize>,
        }
        impl PartialEq for Identity {
            fn eq(&self, _: &Self) -> bool {
                assert!(
                    !self.panic.load(Ordering::Relaxed),
                    "identity equality panic"
                );
                true
            }
        }
        impl Eq for Identity {}
        impl Hash for Identity {
            fn hash<H: Hasher>(&self, state: &mut H) {
                0_u8.hash(state);
            }
        }
        impl Drop for Identity {
            fn drop(&mut self) {
                self.drops.fetch_add(1, Ordering::Relaxed);
            }
        }

        let panic = Arc::new(AtomicBool::new(false));
        let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let identity = || Identity {
            panic: Arc::clone(&panic),
            drops: Arc::clone(&drops),
        };
        let (_lifecycle, mut scopes) = ScopeRegistry::for_test();
        let root = scopes.root();
        let node = ScopeNodeId::new();
        let original = scopes.mount(root, node.clone(), ScopeIdentity::new(identity()));
        let next_token = scopes.next_token;
        panic.store(true, Ordering::Relaxed);

        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            scopes.mount(root, node, ScopeIdentity::new(identity()));
        }))
        .expect_err("identity comparison must propagate the user panic");

        assert_eq!(
            drops.load(Ordering::Relaxed),
            1,
            "only the query was dropped"
        );
        assert_eq!(scopes.next_token, next_token);
        assert_eq!(scopes.records.len(), 1);
        assert_eq!(scopes.child_lists[&root].first, original);
        assert_eq!(scopes.child_lists[&root].last, original);
        drop(scopes);
        assert_eq!(drops.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn cache_or_identity_panic_releases_removed_and_remaining_owners_once() {
        /// Panics once at the chosen ownership boundary, after recording destruction.
        #[derive(Clone)]
        struct DropProbe {
            counts: Arc<[std::sync::atomic::AtomicUsize; 4]>,
            index: usize,
            panic: bool,
        }
        impl Drop for DropProbe {
            fn drop(&mut self) {
                self.counts[self.index].fetch_add(1, Ordering::Relaxed);
                assert!(!self.panic, "retirement destructor panic");
            }
        }
        /// Keeps identity comparison infallible while its owned destructor can panic.
        #[derive(Clone)]
        struct Identity(DropProbe);
        impl PartialEq for Identity {
            fn eq(&self, other: &Self) -> bool {
                self.0.index == other.0.index
            }
        }
        impl Eq for Identity {}
        impl Hash for Identity {
            fn hash<H: Hasher>(&self, state: &mut H) {
                self.0.index.hash(state);
            }
        }

        // Parent identity/cache use slots 0/1; child identity/cache use slots 2/3.
        for panic_index in [2, 3] {
            let counts = Arc::new(std::array::from_fn(|_| {
                std::sync::atomic::AtomicUsize::new(0)
            }));
            let probe = |index| DropProbe {
                counts: Arc::clone(&counts),
                index,
                panic: index == panic_index,
            };
            let (_lifecycle, mut scopes) = ScopeRegistry::for_test();
            let parent = scopes.mount(
                scopes.root(),
                ScopeNodeId::new(),
                ScopeIdentity::new(Identity(probe(0))),
            );
            let child = scopes.mount(
                parent,
                ScopeNodeId::new(),
                ScopeIdentity::new(Identity(probe(2))),
            );
            scopes.put_scope_cache(parent, Box::new(probe(1)));
            scopes.put_scope_cache(child, Box::new(probe(3)));

            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| scopes.leave(parent)))
                .expect_err("retirement must propagate the chosen destructor panic");

            assert_eq!(
                counts.each_ref().map(|count| count.load(Ordering::Relaxed)),
                [0, 0, 1, 1]
            );
            // Only inspect ownership and tear down: partial retirement cannot resume.
            drop(scopes);
            assert_eq!(
                counts.each_ref().map(|count| count.load(Ordering::Relaxed)),
                [1; 4]
            );
            assert_eq!(Arc::strong_count(&counts), 1);
        }
    }

    #[test]
    fn leaving_is_idempotent_and_remounts_a_fresh_token() {
        let (_registry_lifecycle, mut registry) = ScopeRegistry::for_test();
        let root = registry.root();
        let node = ScopeNodeId::new();
        let first = registry.mount(root, node.clone(), ScopeIdentity::unit());

        assert_eq!(registry.leave(first), [first]);
        assert!(registry.leave(first).is_empty());
        let second = registry.mount(root, node.clone(), ScopeIdentity::unit());

        assert_ne!(first, second);
        assert!(registry.is_current(second));
    }

    #[test]
    fn scope_token_and_option_each_occupy_one_u64_word() {
        assert_eq!(
            std::mem::size_of::<ScopeToken>(),
            std::mem::size_of::<u64>()
        );
        assert_eq!(
            std::mem::size_of::<Option<ScopeToken>>(),
            std::mem::size_of::<u64>()
        );
    }

    #[test]
    fn scope_token_labels_follow_installation_order() {
        let (_lifecycle, mut registry) = ScopeRegistry::for_test();
        let root = registry.root();
        let first = registry.mount(root, ScopeNodeId::new(), ScopeIdentity::unit());
        let second = registry.mount(root, ScopeNodeId::new(), ScopeIdentity::unit());

        assert_eq!(root.diagnostic_id(), 0);
        assert_eq!(first.diagnostic_id(), 1);
        assert_eq!(second.diagnostic_id(), 2);
        #[expect(clippy::clone_on_copy, reason = "Exercise the derived Clone body in production coverage.")]
        let cloned = first.clone();
        assert_eq!(cloned, first);
        assert_eq!(format!("{root:?}"), "ScopeToken(0)");
        assert_eq!(format!("{first:?}"), "ScopeToken(1)");
        assert_eq!(format!("{second:?}"), "ScopeToken(2)");
    }

    #[test]
    fn transition_batch_deduplicates_departures_in_first_seen_order() {
        let (_registry_lifecycle, mut registry) = ScopeRegistry::for_test();
        let root = registry.root();
        let parent = registry.mount(root, ScopeNodeId::new(), ScopeIdentity::unit());
        let child = registry.mount(parent, ScopeNodeId::new(), ScopeIdentity::unit());
        let sibling = registry.mount(root, ScopeNodeId::new(), ScopeIdentity::unit());
        let departed = registry.leave(parent);
        assert_eq!(departed, [child, parent]);
        assert_eq!(registry.leave(sibling), [sibling]);

        let mut transitions = ScopeTransitionBatch::default();
        assert!(transitions.is_empty());
        transitions.extend(departed);
        transitions.extend([parent, sibling, child, sibling, parent]);

        assert_eq!(transitions.invalidated, [child, parent, sibling]);
        assert_eq!(transitions.membership.len(), 3);
        for token in [child, parent, sibling] {
            assert!(transitions.contains(token));
        }
        assert!(!transitions.contains(root));
        assert!(!transitions.is_empty());
        registry.invalidate_outputs(&transitions);
    }

    #[test]
    fn departed_token_stays_stale_after_fresh_remount() {
        let (_registry_lifecycle, mut registry) = ScopeRegistry::for_test();
        let root = registry.root();
        let node = ScopeNodeId::new();
        let first = registry.mount(root, node.clone(), ScopeIdentity::unit());
        let mut transitions = ScopeTransitionBatch::default();

        transitions.extend(registry.leave(first));
        let second = registry.mount(root, node.clone(), ScopeIdentity::unit());

        assert_ne!(first, second);
        assert!(transitions.contains(first));
        assert!(!transitions.contains(second));
        assert!(!registry.is_current(first));
        assert!(registry.is_current(second));
        registry.invalidate_outputs(&transitions);
    }

    #[test]
    fn separate_registries_do_not_share_root_authority() {
        let node_1 = ScopeNodeId::new();
        let (_first_lifecycle, mut first) = ScopeRegistry::for_test();
        let (_second_lifecycle, mut second) = ScopeRegistry::for_test();
        let first_child = first.mount(first.root(), node_1.clone(), ScopeIdentity::unit());
        let second_child = second.mount(second.root(), node_1.clone(), ScopeIdentity::unit());

        first.leave(first_child);

        assert!(!first.is_current(first_child));
        assert!(second.is_current(second_child));
    }

    #[test]
    fn imported_slot_does_not_alias_a_prepopulated_registry_node() {
        let (_second_lifecycle, mut second) = ScopeRegistry::for_test();
        let mut shared = ScopeSlot::new();
        let original = ScopeRegistry::node(&mut shared);
        let local = ScopeRegistry::node(&mut ScopeSlot::new());
        let imported = ScopeRegistry::node(&mut shared);
        assert_eq!(original, imported);
        assert_ne!(local, imported);
        let decoy = second.mount(second.root(), local, ScopeIdentity::unit());
        let target = second.mount(second.root(), imported, ScopeIdentity::unit());
        assert_ne!(decoy, target);
        assert_eq!(second.leave(target), [target]);
        assert!(second.is_current(decoy));
        assert_eq!(format!("{original:?}"), "ScopeNodeId(..)");
    }

    /// Counts destruction of registry-owned metadata across retirement and Store drop.
    struct CacheDrop(Arc<std::sync::atomic::AtomicUsize>);
    impl Drop for CacheDrop {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn cloned_authority_checks_ancestors_and_retains_only_metadata() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<super::ScopeAuthority>();
        let (lifecycle, mut registry) = ScopeRegistry::for_test();
        let (_foreign_lifecycle, mut foreign) = ScopeRegistry::for_test();
        let nodes = [ScopeNodeId::new(), ScopeNodeId::new(), ScopeNodeId::new()];
        let parent = registry.mount(registry.root(), nodes[0].clone(), ScopeIdentity::unit());
        let middle = registry.mount(parent, nodes[1].clone(), ScopeIdentity::unit());
        let key = Arc::new(7_u8);
        let weak_key = Arc::downgrade(&key);
        let leaf = registry.mount(middle, nodes[2].clone(), ScopeIdentity::shared(key));
        let mut foreign_leaf = foreign.root();
        for node in &nodes {
            foreign_leaf = foreign.mount(foreign_leaf, node.clone(), ScopeIdentity::unit());
        }
        assert_eq!(leaf, foreign_leaf);
        let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        registry.put_scope_cache(leaf, Box::new(CacheDrop(Arc::clone(&drops))));
        let original = registry.authority(leaf);
        let cloned = original.clone();
        assert!(registry.accepts(&original));
        assert!(registry.accepts(&cloned));
        assert!(
            !foreign.accepts(&cloned),
            "matching ancestry cannot cross Store roots"
        );

        registry.refresh_configuration(middle);
        assert!(!registry.accepts(&original));
        assert!(
            !registry.accepts(&cloned),
            "middle-ancestor refresh retires every capture"
        );
        let refreshed = registry.authority(leaf);
        assert!(registry.accepts(&refreshed));
        assert_eq!(registry.leave(middle), [leaf, middle]);
        assert!(!registry.accepts(&refreshed));
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(
            weak_key.upgrade().is_none(),
            "captures do not retain identity values"
        );

        let replacement_middle = registry.mount(parent, nodes[1].clone(), ScopeIdentity::unit());
        let replacement_leaf = registry.mount(
            replacement_middle,
            nodes[2].clone(),
            ScopeIdentity::shared(Arc::new(7_u8)),
        );
        assert_ne!(replacement_leaf, leaf);
        assert!(
            !registry.accepts(&refreshed),
            "same-site remount cannot revive old authority"
        );
        assert!(registry.accepts(&registry.authority(replacement_leaf)));

        let weak_lifecycle = Arc::downgrade(&lifecycle);
        drop(registry);
        drop(lifecycle);
        assert!(
            weak_lifecycle.upgrade().is_none(),
            "captures do not retain the lifecycle owner"
        );
        assert_eq!(
            cloned.admission().unwrap_err().reason(),
            crate::TaskAccessReason::StoreClosed,
        );
        drop((original, cloned, refreshed));
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn registry_caches_drop_on_subtree_retirement_and_store_drop_without_churn_history() {
        let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (_first_lifecycle, mut first) = ScopeRegistry::for_test();
        let (_second_lifecycle, mut second) = ScopeRegistry::for_test();
        let node = ScopeNodeId::new();
        let survivor = second.mount(second.root(), node.clone(), ScopeIdentity::unit());
        second.put_scope_cache(survivor, Box::new(CacheDrop(Arc::clone(&drops))));
        for iteration in 0..128 {
            let parent = first.mount(first.root(), node.clone(), ScopeIdentity::unit());
            let child = first.mount(parent, node.clone(), ScopeIdentity::unit());
            first.put_scope_cache(parent, Box::new(CacheDrop(Arc::clone(&drops))));
            first.put_scope_cache(child, Box::new(CacheDrop(Arc::clone(&drops))));
            assert_eq!(first.caches.len(), 2);
            assert_eq!(first.leave(parent), [child, parent]);
            assert!(first.caches.is_empty());
            assert_eq!(drops.load(Ordering::SeqCst), (iteration + 1) * 2);
            assert!(second.scope_cache::<CacheDrop>(survivor).is_some());
        }
        drop(first);
        assert_eq!(drops.load(Ordering::SeqCst), 256);
        drop(second);
        assert_eq!(drops.load(Ordering::SeqCst), 257);
    }

    #[test]
    fn registry_cache_types_and_live_ownership_are_checked() {
        use std::panic::{AssertUnwindSafe, catch_unwind};
        let (_registry_lifecycle, mut registry) = ScopeRegistry::for_test();
        let token = registry.mount(registry.root(), ScopeNodeId::new(), ScopeIdentity::unit());
        registry.put_scope_cache(token, Box::new(7_u8));
        assert_eq!(registry.scope_cache::<u8>(token), Some(&7));
        catch_unwind(AssertUnwindSafe(|| registry.scope_cache::<u16>(token))).unwrap_err();
        assert!(
            catch_unwind(AssertUnwindSafe(
                || registry.put_scope_cache(token, Box::new(8_u8))
            ))
            .is_err()
        );
        assert_eq!(registry.scope_cache::<u8>(token), Some(&7));
        let cached_value = registry.take_scope_cache::<u8>(token).unwrap();
        assert!(registry.scope_cache::<u8>(token).is_none());
        registry.put_scope_cache(token, cached_value);
        registry.leave(token);
        catch_unwind(AssertUnwindSafe(|| registry.scope_cache::<u8>(token))).unwrap_err();
        assert!(
            catch_unwind(AssertUnwindSafe(
                || registry.put_scope_cache(token, Box::new(9_u8))
            ))
            .is_err()
        );
        assert!(registry.caches.is_empty());
    }

    // Safe internal misuse must fail fast and release the removed owned metadata.
    #[test]
    fn mismatched_owned_cache_take_panics_and_drops_removed_value_once() {
        use std::panic::{AssertUnwindSafe, catch_unwind};

        let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (_registry_lifecycle, mut registry) = ScopeRegistry::for_test();
        let token = registry.mount(registry.root(), ScopeNodeId::new(), ScopeIdentity::unit());
        registry.put_scope_cache(token, Box::new(CacheDrop(Arc::clone(&drops))));
        assert_eq!(drops.load(Ordering::SeqCst), 0);

        let panic = catch_unwind(AssertUnwindSafe(|| registry.take_scope_cache::<u16>(token)))
            .expect_err("a mismatched owned cache must not be accepted");
        let message = panic
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| panic.downcast_ref::<String>().map(String::as_str));
        assert_eq!(
            message,
            Some("scope cache type must match its reducer site")
        );
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(!registry.caches.contains_key(&token));
        assert!(registry.is_current(token));

        // Inspect teardown only; catching an invariant panic promises no recovery API.
        drop(registry);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(Arc::strong_count(&drops), 1);
    }

    #[test]
    fn owned_node_and_slot_layout_is_pointer_sized() {
        use core::mem::size_of;
        eprintln!(
            "scope_layout: node={} slot={} token={} record={} registry={} stack_id={}",
            size_of::<ScopeNodeId>(),
            size_of::<ScopeSlot>(),
            size_of::<ScopeToken>(),
            size_of::<ScopeRecord>(),
            size_of::<ScopeRegistry>(),
            size_of::<crate::StackId>()
        );
        assert_eq!(size_of::<ScopeNodeId>(), size_of::<Arc<()>>());
        assert_eq!(size_of::<ScopeSlot>(), size_of::<Arc<()>>());
    }

    #[test]
    fn checked_token_increment_panics_on_overflow() {
        let node_1 = ScopeNodeId::new();
        let (_registry_lifecycle, mut registry) = ScopeRegistry::for_test();
        registry.next_token = u64::MAX;

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            registry.mount(registry.root(), node_1.clone(), ScopeIdentity::unit());
        }));

        assert!(panic.is_err());
        assert!(registry.records.is_empty());
        assert!(registry.active_mounts.is_empty());
        assert!(registry.child_lists.is_empty());
        assert_eq!(registry.next_token, u64::MAX);
    }

    #[test]
    fn last_scope_token_remains_reusable_after_exhaustion() {
        let node = ScopeNodeId::new();
        let (_lifecycle, mut registry) = ScopeRegistry::for_test();
        registry.next_token = u64::MAX - 1;
        let root = registry.root();
        let last = registry.mount(root, node.clone(), ScopeIdentity::unit());

        assert_eq!(last.diagnostic_id(), u64::MAX - 1);
        assert_eq!(format!("{last:?}"), "ScopeToken(18446744073709551614)");
        assert_eq!(registry.next_token, u64::MAX);
        assert_eq!(registry.mount(root, node, ScopeIdentity::unit()), last);

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            registry.mount(root, ScopeNodeId::new(), ScopeIdentity::unit());
        }));

        assert!(panic.is_err());
        assert_eq!(registry.next_token, u64::MAX);
        assert_eq!(registry.records.len(), 1);
        assert_eq!(registry.active_mounts.len(), 1);
        assert_eq!(registry.child_lists.len(), 1);
        assert_eq!(registry.child_lists[&root].first, last);
        assert_eq!(registry.child_lists[&root].last, last);
        assert!(registry.is_current(last));
    }

    #[test]
    fn context_restores_parent_scope_while_unwinding() {
        let node_1 = ScopeNodeId::new();
        let (_registry_lifecycle, mut registry) = ScopeRegistry::for_test();
        let root = registry.root();
        let child = registry.mount(root, node_1.clone(), ScopeIdentity::unit());
        let mut dependencies = DependencyValues::new();
        let mut context_transitions = crate::store::scope::ScopeTransitionBatch::default();
        let mut context = ReduceContext::new(
            &mut dependencies,
            MonotonicInstant::ZERO,
            &mut registry,
            &mut context_transitions,
        );

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            context.with_scope(child, |_context| panic!("intentional scope unwind"));
        }));
        assert!(panic.is_err());
        let sibling_node = ReduceContext::node(&mut ScopeSlot::new());
        let sibling = context.mount(sibling_node, ScopeIdentity::unit());
        drop(context);
        drop(context_transitions);

        assert_eq!(
            registry.records.get(&sibling).map(|record| record.parent),
            Some(root)
        );
    }

    #[test]
    fn context_records_departed_descendants_for_commit() {
        let node_1 = ScopeNodeId::new();
        let node_2 = ScopeNodeId::new();
        let node_3 = ScopeNodeId::new();
        let (_registry_lifecycle, mut registry) = ScopeRegistry::for_test();
        let root = registry.root();
        let parent = registry.mount(root, node_1.clone(), ScopeIdentity::unit());
        let child = registry.mount(parent, node_2.clone(), ScopeIdentity::unit());
        let sibling = registry.mount(root, node_3.clone(), ScopeIdentity::unit());
        let mut dependencies = DependencyValues::new();
        let mut context_transitions = crate::store::scope::ScopeTransitionBatch::default();
        let mut context = ReduceContext::new(
            &mut dependencies,
            MonotonicInstant::ZERO,
            &mut registry,
            &mut context_transitions,
        );

        context.leave(parent);
        drop(context);
        let transitions = context_transitions;

        assert!(transitions.contains(parent));
        assert!(transitions.contains(child));
        assert!(!transitions.contains(sibling));
    }
}
}

#[cfg(all(test, feature = "allocation-measurement"))]
pub(crate) use allocation_tests::{
    authority_capture_clone_and_drop_costs,
    empty_registry_reuses_lifecycle_root_without_allocation,
    identified_warm_hit_does_not_allocate_identity_ownership,
    node_identity_allocates_once_per_visited_slot_and_not_on_warm_reuse,
};

#[cfg(all(test, feature = "allocation-measurement"))]
mod allocation_tests {
    //! Measures actual registry ownership phases in isolated allocation children.

    use std::sync::Arc;

    use super::{ScopeIdentity, ScopeNodeId, ScopeRegistry, ScopeSlot};

    /// Selects the existing exported harness case, not this helper's module path.
    #[cfg(feature = "allocation-measurement")]
    const IDENTIFIED_WARM_HIT_ALLOCATION_TEST: &str =
        "store::scope::tests::identified_warm_hit_does_not_allocate_identity_ownership";

    /// Keeps this legacy child protocol distinct from other allocation probes.
    #[cfg(feature = "allocation-measurement")]
    const SCOPE_ALLOCATION_CHILD_ENV: &str = "RCA_SCOPE_ALLOCATION_CHILD";

    /// Measures ancestry capture, shared cloning and complete metadata release.
    pub(crate) fn authority_capture_clone_and_drop_costs() {
        const NAME: &str = "store::scope::allocation_tests::authority_capture_clone_and_drop_costs";
        /// Records each ownership boundary without including fixture construction.
        fn sample(
            registry: &ScopeRegistry,
            token: super::ScopeToken,
        ) -> ([dhat::HeapStats; 5], [usize; 5]) {
            let node_owners = || {
                registry
                    .records
                    .values()
                    .map(|record| Arc::strong_count(&record.mount.node.0))
                    .sum()
            };
            let before = dhat::HeapStats::get();
            let before_nodes = node_owners();
            let authority = std::hint::black_box(registry.authority(token));
            let captured = dhat::HeapStats::get();
            let captured_nodes = node_owners();
            assert!(registry.accepts(&authority));
            let clone = std::hint::black_box(authority.clone());
            let cloned = dhat::HeapStats::get();
            let cloned_nodes = node_owners();
            drop(std::hint::black_box(authority));
            let first_drop = dhat::HeapStats::get();
            let first_drop_nodes = node_owners();
            drop(std::hint::black_box(clone));
            let final_drop = dhat::HeapStats::get();
            let final_drop_nodes = node_owners();
            assert_eq!(final_drop.curr_blocks, before.curr_blocks);
            assert_eq!(final_drop.curr_bytes, before.curr_bytes);
            assert_eq!(final_drop_nodes, before_nodes);
            (
                [before, captured, cloned, first_drop, final_drop],
                [
                    before_nodes,
                    captured_nodes,
                    cloned_nodes,
                    first_drop_nodes,
                    final_drop_nodes,
                ],
            )
        }

        if crate::allocation_test::run_in_child(NAME) {
            return;
        }
        let profiler = dhat::Profiler::builder().testing().build();
        for depth in [0, 1, 5, 20] {
            let (_lifecycle, mut registry) = ScopeRegistry::for_test();
            let mut token = registry.root();
            for _ in 0..depth {
                token = registry.mount(token, ScopeNodeId::new(), ScopeIdentity::unit());
            }
            let (stats, nodes) = sample(&registry, token);
            for (phase, pair) in ["capture", "clone", "first_drop", "final_drop"]
                .into_iter()
                .zip(stats.windows(2))
            {
                eprintln!(
                    "authority_cost variant=production depth={depth} phase={phase} events={} requested_bytes={} live_blocks_before={} live_blocks_after={} live_bytes_before={} live_bytes_after={}",
                    pair[1].total_blocks - pair[0].total_blocks,
                    pair[1].total_bytes - pair[0].total_bytes,
                    pair[0].curr_blocks - stats[0].curr_blocks,
                    pair[1].curr_blocks - stats[0].curr_blocks,
                    pair[0].curr_bytes - stats[0].curr_bytes,
                    pair[1].curr_bytes - stats[0].curr_bytes,
                );
            }
            eprintln!(
                "authority_node_owners variant=production depth={depth} checkpoints={nodes:?}"
            );
            assert_eq!(
                stats[2].total_blocks, stats[1].total_blocks,
                "authority clone must not allocate at depth {depth}",
            );
            assert_eq!(stats[2].total_bytes, stats[1].total_bytes);
            if depth == 0 {
                assert_eq!(stats[4].total_blocks, stats[0].total_blocks);
                assert_eq!(stats[4].total_bytes, stats[0].total_bytes);
            } else if depth == 1 {
                assert_eq!(
                    stats[1].total_blocks - stats[0].total_blocks,
                    1,
                    "direct-child authority allocates only its shared ancestry"
                );
            }
            assert_eq!(
                nodes, [nodes[0]; 5],
                "authority never retains node ownership"
            );
        }
        eprintln!(
            "scope_layout authority={} lookup_unit={} lookup_u64={} lookup_string={} owned_identity={} record={}",
            std::mem::size_of::<super::ScopeAuthority>(),
            std::mem::size_of::<ScopeIdentity>(),
            std::mem::size_of::<ScopeIdentity<u64>>(),
            std::mem::size_of::<ScopeIdentity<String>>(),
            std::mem::size_of::<super::OwnedScopeIdentity>(),
            std::mem::size_of::<super::ScopeRecord>(),
        );
        drop(profiler);
    }

    /// Checks root-identity reuse and retained ownership after registry destruction.
    #[cfg(feature = "allocation-measurement")]
    pub(crate) fn empty_registry_reuses_lifecycle_root_without_allocation() {
        if crate::allocation_test::run_in_child(
            "store::scope::tests::empty_registry_reuses_lifecycle_root_without_allocation",
        ) {
            return;
        }
        let profiler = dhat::Profiler::builder().testing().build();
        let before = dhat::HeapStats::get();
        let lifecycle = Arc::new(super::super::lifecycle::Lifecycle::new());
        let constructed = dhat::HeapStats::get();
        let registry = std::hint::black_box(ScopeRegistry::new(&lifecycle));
        let registered = dhat::HeapStats::get();
        // Lifecycle owns its Arc and the root Arc; registry only clones the root.
        assert_eq!(constructed.total_blocks - before.total_blocks, 2);
        assert_eq!(registered.total_blocks, constructed.total_blocks);
        assert_eq!(registered.total_bytes, constructed.total_bytes);
        let root = registry.root_identity();
        assert!(Arc::ptr_eq(&root, &lifecycle.root_identity()));
        drop(registry);
        drop(lifecycle);
        let retained = dhat::HeapStats::get();
        assert_eq!(retained.curr_blocks - before.curr_blocks, 1);
        assert_eq!(retained.curr_bytes - before.curr_bytes, 16);
        drop(root);
        let released = dhat::HeapStats::get();
        assert_eq!(released.curr_blocks, before.curr_blocks);
        assert_eq!(released.curr_bytes, before.curr_bytes);
        drop(profiler);
    }

    /// Checks one allocation per visited site and no allocation on later visits.
    #[cfg(feature = "allocation-measurement")]
    pub(crate) fn node_identity_allocates_once_per_visited_slot_and_not_on_warm_reuse() {
        if crate::allocation_test::run_in_child(
            "store::scope::tests::node_identity_allocates_once_per_visited_slot_and_not_on_warm_reuse",
        ) {
            return;
        }
        let profiler = dhat::Profiler::builder().testing().build();
        let mut slot = ScopeSlot::new();
        let before = dhat::HeapStats::get();
        let first = ScopeRegistry::node(&mut slot);
        let cold = dhat::HeapStats::get();
        assert_eq!(cold.total_blocks - before.total_blocks, 1);
        let second = ScopeRegistry::node(&mut slot);
        let warm = dhat::HeapStats::get();
        assert_eq!(first, second);
        assert_eq!(warm.total_blocks, cold.total_blocks);
        assert_eq!(warm.total_bytes, cold.total_bytes);
        eprintln!(
            "scope_node_cold: blocks={} bytes={}",
            cold.total_blocks - before.total_blocks,
            cold.total_bytes - before.total_bytes
        );
        drop(profiler);
    }

    /// Measures a real warm lookup without allocating a new erased identity owner.
    #[cfg(feature = "allocation-measurement")]
    pub(crate) fn identified_warm_hit_does_not_allocate_identity_ownership() {
        if crate::allocation_test::run_in_marked_child(
            IDENTIFIED_WARM_HIT_ALLOCATION_TEST,
            SCOPE_ALLOCATION_CHILD_ENV,
        ) {
            return;
        }

        let (_registry_lifecycle, mut registry) = ScopeRegistry::for_test();
        let root = registry.root();
        let node = ScopeNodeId::new();
        let first = registry.mount(root, node.clone(), ScopeIdentity::new(7_u8));
        let profiler = dhat::Profiler::builder().testing().build();
        let before = dhat::HeapStats::get();

        let same = registry.mount(root, node.clone(), ScopeIdentity::new(7_u8));

        let after = dhat::HeapStats::get();
        assert_eq!(same, first);
        assert_eq!(after.total_blocks, before.total_blocks);
        assert_eq!(after.total_bytes, before.total_bytes);
        drop(profiler);
    }
}
