//! Keeps ordered child state and reconciles its Store-local insertion lifetimes.
//!
//! Public values exclude runtime identity. Clones preserve existing insertion
//! lifetimes but allocate distinct revisions for later structural changes.
//! Reducer declarations retain only scope slots; each Store owns its own cache.

use core::any::type_name;
use core::fmt;
use core::hash::{BuildHasher, Hash, Hasher};
use core::iter::FusedIterator;
use core::marker::PhantomData;
use core::ops::Index;
use std::backtrace::Backtrace;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use indexmap::IndexMap;
use indexmap::map::raw_entry_v1::{RawEntryApiV1, RawEntryMut};

use super::missing_child::{MissingChildPolicy, missing_child};

use crate::effect::Program;
use crate::store::scope::{ScopeIdentity, ScopeNodeId, ScopeSlot, ScopeToken};
use crate::{Effect, KeyedActionPath, KeyedStatePath, ReduceContext, Reducer};

mod codec;
mod operations;
#[doc(inline)]
pub use codec::KeyedDecodeError;
#[doc(inline)]
pub use operations::{
    KeyedMergeDecision, KeyedMutationError, KeyedPositionError, KeyedReserveError,
};

/// Reserves zero for a collection that has not changed structurally.
const FIRST_ENTRY_REVISION: u64 = 1;

/// Allocates distinct revisions across collections cloned from one lineage.
struct Lineage {
    /// Supplies uniqueness only; collection contents are not published through this atomic.
    next_revision: AtomicU64,
}

impl Lineage {
    /// Starts revision allocation without assigning an insertion lifetime yet.
    const fn new() -> Self {
        Self {
            next_revision: AtomicU64::new(FIRST_ENTRY_REVISION),
        }
    }

    /// Allocates once across clone branches, panicking before revision reuse.
    fn allocate_revision(&self) -> u64 {
        self.next_revision
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |revision| {
                revision.checked_add(1)
            })
            .unwrap_or_else(|_| panic!("KeyedState entry revision overflow"))
    }
}

/// Retains the collection identity used by a completed reconciliation.
#[derive(Clone)]
pub(crate) struct CollectionStamp {
    /// Keeps an allocated lineage alive; never-used empty collections need none.
    lineage: Option<Arc<Lineage>>,
    /// Changes after membership or order changes, but not state-only mutation.
    revision: u64,
}

/// Compares current structure without adding a lineage owner on warm reads.
#[derive(Clone, Copy)]
pub(crate) struct BorrowedCollectionStamp<'a> {
    /// Borrows the collection's lineage owner for this comparison.
    lineage: Option<&'a Arc<Lineage>>,
    /// Identifies the observed membership and order within that lineage.
    revision: u64,
}

impl BorrowedCollectionStamp<'_> {
    /// Retains the lineage when publishing a completed reconciliation cache.
    pub(crate) fn to_owned(self) -> CollectionStamp {
        CollectionStamp {
            lineage: self.lineage.cloned(),
            revision: self.revision,
        }
    }
}

impl CollectionStamp {
    /// Matches structure, including lineage identity across equal-valued collections.
    pub(crate) fn matches(&self, other: BorrowedCollectionStamp<'_>) -> bool {
        let same_lineage = match (&self.lineage, other.lineage) {
            (Some(left), Some(right)) => Arc::ptr_eq(left, right),
            (None, None) => true,
            _ => false,
        };
        same_lineage && self.revision == other.revision
    }
}

/// Publishes partial removal even when a predicate or removed payload panics.
struct StructuralRevisionGuard<'a> {
    /// Receives the revision after the retain operation starts removing entries.
    target: &'a mut u64,
    /// Reserves a unique revision before any mutation can begin.
    candidate: u64,
    /// Records actual removals, so no-op retention preserves the previous stamp.
    changed: bool,
}

impl Drop for StructuralRevisionGuard<'_> {
    fn drop(&mut self) {
        if self.changed {
            *self.target = self.candidate;
        }
    }
}

/// Associates owned state with one insertion lifetime, independent of its identifier.
#[derive(Clone, Debug)]
struct Entry<State> {
    /// Changes on removal and reinsertion, not ordinary state mutation.
    revision: u64,
    /// Remains caller-owned state, without extra cloning or reference counting.
    state: State,
}

/// Reports a duplicate [`KeyedState`] identifier.
pub struct DuplicateId<Id, State> {
    /// Preserves the exact rejected identifier rather than returning an existing key.
    id: Id,
    /// Returns the proposed state without replacing the stored entry.
    state: State,
    /// Records the rejection location when backtrace capture is enabled.
    backtrace: Backtrace,
}

impl<Id, State> DuplicateId<Id, State> {
    /// Takes the rejected inputs after duplicate lookup has completed.
    fn new(id: Id, state: State) -> Self {
        Self {
            id,
            state,
            backtrace: Backtrace::capture(),
        }
    }

    /// Returns the duplicate identifier and state.
    #[must_use]
    pub fn into_parts(self) -> (Id, State) {
        (self.id, self.state)
    }

    /// Returns the backtrace captured when the duplicate was detected.
    pub const fn backtrace(&self) -> &Backtrace {
        &self.backtrace
    }
}

impl<Id: fmt::Debug, State: fmt::Debug> fmt::Debug for DuplicateId<Id, State> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DuplicateId")
            .field("id", &self.id)
            .field("state", &self.state)
            .field("backtrace", &self.backtrace)
            .finish()
    }
}

impl<Id, State> fmt::Display for DuplicateId<Id, State> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "duplicate KeyedState ID\n{}", self.backtrace)
    }
}

impl<Id: fmt::Debug, State: fmt::Debug> std::error::Error for DuplicateId<Id, State> {}

/// Routes one owned action to one keyed child.
///
/// The identifier and action remain owned at ingress. Borrowing paths can lend
/// both fields during reduction, while [`Self::into_parts`] and [`Self::map`]
/// preserve ownership for explicit routing and effect output mapping.
///
/// # Examples
///
/// ```
/// use rca_core::KeyedAction;
///
/// let action = KeyedAction::new(7_u64, String::from("tap"));
/// assert_eq!(*action.id(), 7);
/// assert_eq!(action.action(), "tap");
/// assert_eq!(action.into_parts(), (7, String::from("tap")));
/// ```
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct KeyedAction<Id, Action> {
    /// Selects the child without deriving identity from mutable child state.
    id: Id,
    /// Remains owned until a reducer borrows it or a mapper consumes it.
    action: Action,
}

/// Restricts payload decomposition to RCA's owned keyed representation and aliases.
mod sealed {
    /// Prevents external payloads from weakening the keyed representation contract.
    pub trait KeyedActionPayload {}
}

/// Decomposes a payload accepted by keyed action paths.
///
/// `ActionPaths` uses this contract to support aliases of [`KeyedAction`]
/// without inspecting a payload's source spelling. Implementations define how
/// a payload lends and reconstructs its keyed action representation. This
/// trait is sealed, so RCA accepts only [`KeyedAction`] payloads and aliases
/// that normalize to them.
pub trait KeyedActionPayload: sealed::KeyedActionPayload + Sized {
    /// Identifies the keyed child route.
    type Id;

    /// Carries the child action.
    type Action;

    /// Borrows the keyed action representation.
    #[must_use]
    fn as_keyed_action(&self) -> &KeyedAction<Self::Id, Self::Action>;

    /// Reconstructs this payload from a keyed action.
    #[must_use]
    fn from_keyed_action(action: KeyedAction<Self::Id, Self::Action>) -> Self;
}

impl<Id, Action> sealed::KeyedActionPayload for KeyedAction<Id, Action> {}

impl<Id, Action> KeyedActionPayload for KeyedAction<Id, Action> {
    type Id = Id;
    type Action = Action;

    fn as_keyed_action(&self) -> &KeyedAction<Self::Id, Self::Action> {
        self
    }

    fn from_keyed_action(action: KeyedAction<Self::Id, Self::Action>) -> Self {
        action
    }
}

impl<Id, Action> KeyedAction<Id, Action> {
    /// Creates a keyed action from owned parts.
    #[must_use]
    pub const fn new(id: Id, action: Action) -> Self {
        Self { id, action }
    }

    /// Borrows the target identifier.
    #[must_use]
    pub const fn id(&self) -> &Id {
        &self.id
    }

    /// Borrows the child action.
    #[must_use]
    pub const fn action(&self) -> &Action {
        &self.action
    }

    /// Returns the owned identifier and child action.
    #[must_use]
    pub fn into_parts(self) -> (Id, Action) {
        (self.id, self.action)
    }

    /// Maps the child action while preserving the owned identifier.
    ///
    /// # Panics
    ///
    /// Propagates panics from `map` and payload destructors during unwinding.
    #[must_use]
    pub fn map<Mapped>(self, map: impl FnOnce(Action) -> Mapped) -> KeyedAction<Id, Mapped> {
        KeyedAction::new(self.id, map(self.action))
    }
}

/// Stores uniquely identified states in stable order.
///
/// Insertion order remains visible until entries are moved or swap-removed.
/// Removing and reinserting an identifier creates a new private entry lifetime.
/// Clones preserve existing lifetimes while sharing future revision allocation.
/// Lineage is allocated on first insertion and retained after clearing. Never-used
/// empty clones initialise independently because they have no entry lifetimes.
/// Duplicate collection through [`FromIterator`] or [`Extend`] panics.
pub struct KeyedState<Id, State> {
    /// Is allocated on first insertion and retained after clearing for clone uniqueness.
    lineage: Option<Arc<Lineage>>,
    /// Invalidates reconciliation after membership or order changes.
    structural_revision: u64,
    /// Preserves public order while keeping each insertion's private revision.
    entries: IndexMap<Id, Entry<State>>,
}

impl<Id, State> KeyedState<Id, State> {
    /// Creates an empty keyed state collection without allocating.
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(0)
    }

    /// Creates an empty keyed state collection with reserved capacity.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            lineage: None,
            structural_revision: 0,
            entries: IndexMap::with_capacity(capacity),
        }
    }

    /// Returns the number of states.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns whether the collection contains no states.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Releases excess collection capacity without changing order or child lifetimes.
    ///
    /// This explicit, potentially linear-time operation preserves stored values,
    /// entry identities and structural revisions. Clearing then shrinking releases
    /// backing storage but retains any previously allocated lineage. The underlying
    /// collection may retain more capacity than the current length requires.
    pub fn shrink_to_fit(&mut self) {
        self.entries.shrink_to_fit();
    }

    /// Moves an entry between ordered indexes.
    ///
    /// # Panics
    ///
    /// Panics when either index is outside the collection or revision allocation overflows.
    pub fn move_index(&mut self, from: usize, to: usize) {
        assert!(from < self.len(), "KeyedState source index out of bounds");
        assert!(
            to < self.len(),
            "KeyedState destination index out of bounds"
        );
        if from == to {
            return;
        }
        let revision = self.lineage().allocate_revision();
        self.entries.move_index(from, to);
        self.structural_revision = revision;
    }

    /// Retains states selected by `keep` in their existing order.
    ///
    /// Earlier removals remain visible if a later callback or destructor panics.
    /// The structural stamp still changes when an entry has been removed.
    ///
    /// # Panics
    ///
    /// Panics when revision allocation overflows, `keep` panics, or a removed value panics on drop.
    pub fn retain(&mut self, mut keep: impl FnMut(&Id, &mut State) -> bool) {
        if self.entries.is_empty() {
            return;
        }
        let candidate = self.lineage().allocate_revision();
        let mut guard = StructuralRevisionGuard {
            target: &mut self.structural_revision,
            candidate,
            changed: false,
        };
        self.entries.retain(|id, entry| {
            let retained = keep(id, &mut entry.state);
            if !retained {
                guard.changed = true;
            }
            retained
        });
    }

    /// Removes every state.
    ///
    /// Publishes the structural change before dropping entries. A destructor
    /// panic does not restore entries or their previous structural stamp.
    ///
    /// # Panics
    ///
    /// Panics when revision allocation overflows or a removed value panics on drop.
    pub fn clear(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        self.structural_revision = self.lineage().allocate_revision();
        self.entries.clear();
    }

    /// Iterates over identifiers in order.
    #[must_use = "iterators are lazy and do nothing unless consumed"]
    pub fn ids(&self) -> impl ExactSizeIterator<Item = &Id> {
        self.entries.keys()
    }

    /// Iterates over states in order.
    #[must_use = "iterators are lazy and do nothing unless consumed"]
    pub fn values(&self) -> impl ExactSizeIterator<Item = &State> {
        self.entries.values().map(|entry| &entry.state)
    }

    /// Mutably iterates over states in order.
    pub fn values_mut(&mut self) -> impl ExactSizeIterator<Item = &mut State> {
        self.entries.values_mut().map(|entry| &mut entry.state)
    }

    /// Iterates over identifier and state pairs in order.
    #[must_use = "iterators are lazy and do nothing unless consumed"]
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&Id, &State)> {
        self.entries.iter().map(|(id, entry)| (id, &entry.state))
    }

    /// Mutably iterates over identifier and state pairs in order.
    pub fn iter_mut(&mut self) -> impl ExactSizeIterator<Item = (&Id, &mut State)> {
        self.entries
            .iter_mut()
            .map(|(id, entry)| (id, &mut entry.state))
    }

    /// Borrows insertion identities in current collection order without cloning keys.
    pub(crate) fn entry_stamps(
        &self,
    ) -> impl ExactSizeIterator<Item = (&Id, BorrowedEntryStamp<'_>)> + DoubleEndedIterator {
        self.entries.iter().map(|(id, entry)| {
            (
                id,
                BorrowedEntryStamp {
                    lineage: self.lineage(),
                    revision: entry.revision,
                },
            )
        })
    }

    /// Borrows the structure revision for an allocation-free reconciliation lookup.
    pub(crate) fn structural_stamp(&self) -> BorrowedCollectionStamp<'_> {
        BorrowedCollectionStamp {
            lineage: self.lineage.as_ref(),
            revision: self.structural_revision,
        }
    }

    /// Requires an existing insertion or structural mutation to have allocated lineage.
    fn lineage(&self) -> &Arc<Lineage> {
        self.lineage.as_ref().expect("keyed entries have a lineage")
    }
}

impl<Id, State> KeyedState<Id, State>
where
    Id: Eq + Hash,
{
    /// Returns whether `id` identifies a state.
    #[must_use]
    pub fn contains_id(&self, id: &Id) -> bool {
        self.entries.contains_key(id)
    }

    /// Returns the state identified by `id`.
    #[must_use]
    pub fn get(&self, id: &Id) -> Option<&State> {
        self.entries.get(id).map(|entry| &entry.state)
    }

    /// Returns the mutable state identified by `id`.
    #[must_use]
    pub fn get_mut(&mut self, id: &Id) -> Option<&mut State> {
        self.entries.get_mut(id).map(|entry| &mut entry.state)
    }

    /// Inserts one uniquely identified state.
    ///
    /// # Panics
    ///
    /// Panics for a duplicate ID, revision overflow, or panicking identifier hashing or equality.
    pub fn insert(&mut self, id: Id, state: State) {
        if let Err(error) = self.try_insert(id, state) {
            panic!("{error}");
        }
    }

    /// Attempts to insert one uniquely identified state.
    ///
    /// # Errors
    ///
    /// Returns [`DuplicateId`] with the owned inputs when `id` already exists.
    ///
    /// # Panics
    ///
    /// Panics when revision allocation overflows or identifier hashing or equality panics.
    pub fn try_insert(&mut self, id: Id, state: State) -> Result<(), DuplicateId<Id, State>> {
        // Borrow the input for lookup so duplicate errors retain the exact owned ID.
        // Reuse this map's hash for insertion instead of hashing the ID twice.
        let hash = self.entries.hasher().hash_one(&id);
        match self
            .entries
            .raw_entry_mut_v1()
            .from_key_hashed_nocheck(hash, &id)
        {
            RawEntryMut::Occupied(_) => Err(DuplicateId::new(id, state)),
            RawEntryMut::Vacant(entry) => {
                let lineage = self.lineage.get_or_insert_with(|| Arc::new(Lineage::new()));
                let revision = lineage.allocate_revision();
                entry.insert_hashed_nocheck(hash, id, Entry { revision, state });
                self.structural_revision = revision;
                Ok(())
            }
        }
    }

    /// Removes a state while preserving remaining order.
    ///
    /// # Panics
    ///
    /// Panics on revision overflow or an inconsistent internal index. Propagates
    /// identifier hashing, equality and removed-identifier destructor panics.
    pub fn remove(&mut self, id: &Id) -> Option<State> {
        let index = self.entries.get_index_of(id)?;
        let revision = self.lineage().allocate_revision();
        let (removed_id, entry) = self
            .entries
            .shift_remove_index(index)
            .expect("resolved KeyedState index exists");
        self.structural_revision = revision;
        drop(removed_id);
        Some(entry.state)
    }

    /// Removes a state by swapping with the final entry.
    ///
    /// # Panics
    ///
    /// Panics on revision overflow or an inconsistent internal index. Propagates
    /// identifier hashing, equality and removed-identifier destructor panics.
    pub fn swap_remove(&mut self, id: &Id) -> Option<State> {
        let index = self.entries.get_index_of(id)?;
        let revision = self.lineage().allocate_revision();
        let (removed_id, entry) = self
            .entries
            .swap_remove_index(index)
            .expect("resolved KeyedState index exists");
        self.structural_revision = revision;
        drop(removed_id);
        Some(entry.state)
    }

    /// Borrows one insertion identity without retaining state or adding an owner.
    pub(crate) fn entry_stamp(&self, id: &Id) -> Option<BorrowedEntryStamp<'_>> {
        self.entries.get(id).map(|entry| BorrowedEntryStamp {
            lineage: self.lineage(),
            revision: entry.revision,
        })
    }

    /// Borrows an entry and its insertion stamp from the same lookup.
    pub(crate) fn get_with_stamp(&self, id: &Id) -> Option<(&State, BorrowedEntryStamp<'_>)> {
        self.entries.get(id).map(|entry| {
            (
                &entry.state,
                BorrowedEntryStamp {
                    lineage: self.lineage(),
                    revision: entry.revision,
                },
            )
        })
    }

    /// Lends mutable state and its identity from one lookup before reducer entry.
    pub(super) fn get_mut_with_stamp(
        &mut self,
        id: &Id,
    ) -> Option<(&mut State, BorrowedEntryStamp<'_>)> {
        let lineage = self.lineage.as_ref()?;
        let entry = self.entries.get_mut(id)?;
        Some((
            &mut entry.state,
            BorrowedEntryStamp {
                lineage,
                revision: entry.revision,
            },
        ))
    }
}

impl<Id, State> Default for KeyedState<Id, State> {
    fn default() -> Self {
        Self::new()
    }
}

impl<Id, State> Clone for KeyedState<Id, State>
where
    Id: Clone,
    State: Clone,
{
    fn clone(&self) -> Self {
        Self {
            lineage: self.lineage.clone(),
            structural_revision: self.structural_revision,
            entries: self.entries.clone(),
        }
    }
}

impl<Id, State> fmt::Debug for KeyedState<Id, State>
where
    Id: fmt::Debug,
    State: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeyedState")
            .field("entries", &PublicEntries(&self.entries))
            .finish_non_exhaustive()
    }
}

/// Hides private insertion revisions from the collection's public debug output.
struct PublicEntries<'a, Id, State>(&'a IndexMap<Id, Entry<State>>);

impl<Id, State> fmt::Debug for PublicEntries<'_, Id, State>
where
    Id: fmt::Debug,
    State: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map()
            .entries(self.0.iter().map(|(id, entry)| (id, &entry.state)))
            .finish()
    }
}

impl<Id, State> PartialEq for KeyedState<Id, State>
where
    Id: PartialEq,
    State: PartialEq,
{
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len()
            && self.iter().zip(other.iter()).all(
                |((left_id, left_state), (right_id, right_state))| {
                    left_id == right_id && left_state == right_state
                },
            )
    }
}

impl<Id, State> Eq for KeyedState<Id, State>
where
    Id: Eq,
    State: Eq,
{
}

impl<Id, State> FromIterator<(Id, State)> for KeyedState<Id, State>
where
    Id: Eq + Hash,
{
    fn from_iter<T: IntoIterator<Item = (Id, State)>>(iter: T) -> Self {
        let iterator = iter.into_iter();
        let mut states = Self::with_capacity(iterator.size_hint().0);
        states.extend(iterator);
        states
    }
}

impl<Id, State> Extend<(Id, State)> for KeyedState<Id, State>
where
    Id: Eq + Hash,
{
    fn extend<T: IntoIterator<Item = (Id, State)>>(&mut self, iter: T) {
        for (id, state) in iter {
            self.insert(id, state);
        }
    }
}

impl<Id, State> Index<&Id> for KeyedState<Id, State>
where
    Id: Eq + Hash,
{
    type Output = State;

    fn index(&self, index: &Id) -> &Self::Output {
        self.get(index).expect("missing KeyedState ID")
    }
}

/// Iterates over borrowed keyed states.
#[doc(hidden)]
pub struct KeyedIter<'a, Id, State> {
    /// Borrows entries while exposing only their identifiers and user state.
    inner: indexmap::map::Iter<'a, Id, Entry<State>>,
}

impl<Id, State> fmt::Debug for KeyedIter<'_, Id, State> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeyedIter")
            .field("remaining", &self.len())
            .finish()
    }
}

impl<'a, Id, State> Iterator for KeyedIter<'a, Id, State> {
    type Item = (&'a Id, &'a State);

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|(id, entry)| (id, &entry.state))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<Id, State> DoubleEndedIterator for KeyedIter<'_, Id, State> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.inner.next_back().map(|(id, entry)| (id, &entry.state))
    }
}

impl<Id, State> ExactSizeIterator for KeyedIter<'_, Id, State> {
    fn len(&self) -> usize {
        self.inner.len()
    }
}

impl<Id, State> FusedIterator for KeyedIter<'_, Id, State> {}

/// Iterates over mutably borrowed keyed states.
#[doc(hidden)]
pub struct KeyedIterMut<'a, Id, State> {
    /// Lends mutable state without allowing changes to identifiers or insertion revisions.
    inner: indexmap::map::IterMut<'a, Id, Entry<State>>,
}

impl<Id, State> fmt::Debug for KeyedIterMut<'_, Id, State> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeyedIterMut")
            .field("remaining", &self.len())
            .finish()
    }
}

impl<'a, Id, State> Iterator for KeyedIterMut<'a, Id, State> {
    type Item = (&'a Id, &'a mut State);

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|(id, entry)| (id, &mut entry.state))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<Id, State> DoubleEndedIterator for KeyedIterMut<'_, Id, State> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.inner
            .next_back()
            .map(|(id, entry)| (id, &mut entry.state))
    }
}

impl<Id, State> ExactSizeIterator for KeyedIterMut<'_, Id, State> {
    fn len(&self) -> usize {
        self.inner.len()
    }
}

impl<Id, State> FusedIterator for KeyedIterMut<'_, Id, State> {}

/// Iterates over owned keyed states.
#[doc(hidden)]
pub struct KeyedIntoIter<Id, State> {
    /// Owns unconsumed entries and drops them if iteration ends early.
    inner: indexmap::map::IntoIter<Id, Entry<State>>,
}

impl<Id, State> fmt::Debug for KeyedIntoIter<Id, State> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeyedIntoIter")
            .field("remaining", &self.len())
            .finish()
    }
}

impl<Id, State> Iterator for KeyedIntoIter<Id, State> {
    type Item = (Id, State);

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|(id, entry)| (id, entry.state))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<Id, State> DoubleEndedIterator for KeyedIntoIter<Id, State> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.inner.next_back().map(|(id, entry)| (id, entry.state))
    }
}

impl<Id, State> ExactSizeIterator for KeyedIntoIter<Id, State> {
    fn len(&self) -> usize {
        self.inner.len()
    }
}

impl<Id, State> FusedIterator for KeyedIntoIter<Id, State> {}

impl<'a, Id, State> IntoIterator for &'a KeyedState<Id, State> {
    type Item = (&'a Id, &'a State);
    type IntoIter = KeyedIter<'a, Id, State>;

    fn into_iter(self) -> Self::IntoIter {
        KeyedIter {
            inner: self.entries.iter(),
        }
    }
}

impl<'a, Id, State> IntoIterator for &'a mut KeyedState<Id, State> {
    type Item = (&'a Id, &'a mut State);
    type IntoIter = KeyedIterMut<'a, Id, State>;

    fn into_iter(self) -> Self::IntoIter {
        KeyedIterMut {
            inner: self.entries.iter_mut(),
        }
    }
}

impl<Id, State> IntoIterator for KeyedState<Id, State> {
    type Item = (Id, State);
    type IntoIter = KeyedIntoIter<Id, State>;

    fn into_iter(self) -> Self::IntoIter {
        KeyedIntoIter {
            inner: self.entries.into_iter(),
        }
    }
}

/// Retains one insertion lifetime without retaining its identifier or state.
#[derive(Clone)]
pub(crate) struct EntryStamp {
    /// Prevents allocator address reuse while the insertion identity remains observable.
    lineage: Arc<Lineage>,
    /// Distinguishes insertions and clone-branch mutations within the lineage.
    revision: u64,
}

/// Borrows an insertion identity for a synchronous authority check.
#[derive(Clone, Copy)]
pub(crate) struct BorrowedEntryStamp<'a> {
    /// Borrows the collection's owner instead of incrementing its reference count.
    lineage: &'a Arc<Lineage>,
    /// Identifies the selected insertion within that lineage.
    revision: u64,
}

impl BorrowedEntryStamp<'_> {
    /// Retains an insertion identity when a handle or cache must outlive the borrow.
    pub(crate) fn to_owned(self) -> EntryStamp {
        EntryStamp {
            lineage: Arc::clone(self.lineage),
            revision: self.revision,
        }
    }
}

impl EntryStamp {
    /// Compares insertion lifetimes rather than state equality or identifier equality.
    pub(crate) fn matches(&self, other: BorrowedEntryStamp<'_>) -> bool {
        Arc::ptr_eq(&self.lineage, other.lineage) && self.revision == other.revision
    }
}

impl PartialEq for EntryStamp {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.lineage, &other.lineage) && self.revision == other.revision
    }
}

impl Eq for EntryStamp {}

impl Hash for EntryStamp {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.lineage).hash(state);
        self.revision.hash(state);
    }
}

/// Links one cached identifier to its current Store-local lifetime.
struct ActiveEntry {
    /// Shares insertion identity ownership with the mounted scope record.
    stamp: Arc<EntryStamp>,
    /// Selects the lifetime used for cancellation and descendant scope parenting.
    token: ScopeToken,
    /// Marks membership during the current full scan without a temporary key set.
    seen_epoch: u64,
}

impl ActiveEntry {
    /// Mounts a lifetime and shares its insertion stamp with the registry.
    ///
    /// Cold insertion calls this before cloning its cached identifier.
    fn new(
        stamp: BorrowedEntryStamp<'_>,
        epoch: u64,
        node: &ScopeNodeId,
        context: &mut ReduceContext<'_>,
    ) -> Self {
        let stamp = Arc::new(stamp.to_owned());
        let token = context.mount(node.clone(), ScopeIdentity::shared(Arc::clone(&stamp)));
        Self {
            stamp,
            token,
            seen_epoch: epoch,
        }
    }

    /// Marks live membership and retires a changed insertion before replacement.
    fn refresh(
        &mut self,
        stamp: BorrowedEntryStamp<'_>,
        epoch: u64,
        node: &ScopeNodeId,
        context: &mut ReduceContext<'_>,
    ) {
        self.seen_epoch = epoch;
        if !self.stamp.matches(stamp) {
            context.leave(self.token);
            let replacement = Self::new(stamp, epoch, node, context);
            self.token = replacement.token;
            self.stamp = replacement.stamp;
        }
    }
}

/// Retains declaration sites while leaving per-Store reconciliation state in the registry.
pub(super) struct KeyedLifecycle<Id> {
    /// Separates this collection's cache from other declarations and parent scopes.
    structural_slot: ScopeSlot,
    /// Identifies child declarations under the collection's structural scope.
    entry_slot: ScopeSlot,
    /// Tracks the key type without owning keys or imposing their auto-trait bounds.
    marker: PhantomData<fn() -> Id>,
    #[cfg(test)]
    /// Counts full scans for deterministic warm-path tests, not runtime scheduling.
    full_reconciliations: u64,
}

/// Owns one Store-and-parent-specific reconciliation result.
pub(super) struct KeyedSnapshot<Id> {
    /// Enables the warm path only after the complete scan has succeeded.
    last_reconciled: Option<CollectionStamp>,
    /// Retains mapped identifiers for delayed child-effect output and current scopes.
    entries: IndexMap<Arc<Id>, ActiveEntry>,
    /// Marks entries seen by each full scan; checked exhaustion prevents false retention.
    epoch: u64,
}

impl<Id> Default for KeyedSnapshot<Id> {
    fn default() -> Self {
        Self {
            last_reconciled: None,
            entries: IndexMap::new(),
            epoch: 0,
        }
    }
}

impl<Id: Eq + Hash> KeyedSnapshot<Id> {
    /// Retains the route and insertion identity until the mutable selection is checked.
    pub(super) fn active(&self, id: &Id) -> Option<(Arc<Id>, ScopeToken, Arc<EntryStamp>)> {
        self.entries.get_full(id).map(|(_, active_id, active)| {
            (
                Arc::clone(active_id),
                active.token,
                Arc::clone(&active.stamp),
            )
        })
    }
}

impl<Id> KeyedLifecycle<Id> {
    /// Creates lazy declaration slots without retaining any Store's keys or scopes.
    pub(super) fn new() -> Self {
        Self {
            structural_slot: ScopeSlot::new(),
            entry_slot: ScopeSlot::new(),
            marker: PhantomData,
            #[cfg(test)]
            full_reconciliations: 0,
        }
    }

    #[cfg(test)]
    /// Reports scans across contexts so tests can distinguish cache hits from rebuilds.
    pub(super) const fn full_reconciliations(&self) -> u64 {
        self.full_reconciliations
    }

    #[cfg(test)]
    /// Records a full scan after allocating its epoch, before visiting entries.
    fn record_full_reconciliation(&mut self) {
        self.full_reconciliations += 1;
    }

    #[cfg(not(test))]
    /// Keeps production reconciliation free from test-only counters.
    const fn record_full_reconciliation(&mut self) {
        let _ = self;
    }
}

impl<Id: Eq + Hash + Clone + Send + Sync + 'static> KeyedLifecycle<Id> {
    /// Reconciles child lifetimes without inspecting the completed cache.
    pub(super) fn reconcile<ChildState>(
        &mut self,
        states: &KeyedState<Id, ChildState>,
        context: &mut ReduceContext<'_>,
    ) -> ScopeToken {
        self.reconcile_with(states, context, |_| ()).0
    }

    /// Reconciles first, then lends the completed cache to synchronous routing.
    ///
    /// Unchanged structure needs no full scan. Changed structure retires replaced
    /// and absent lifetimes before publishing its stamp. The cache survives an
    /// inspection panic; a failed scan cannot publish a completed stamp.
    /// No rollback is promised when cloning, hashing or scope teardown panics.
    pub(super) fn reconcile_with<ChildState, Output>(
        &mut self,
        states: &KeyedState<Id, ChildState>,
        context: &mut ReduceContext<'_>,
        inspect: impl FnOnce(&KeyedSnapshot<Id>) -> Output,
    ) -> (ScopeToken, Output) {
        let structural_node = ReduceContext::node(&mut self.structural_slot);
        let structural_token = context.mount(structural_node, ScopeIdentity::unit());
        let structural_stamp = states.structural_stamp();
        if let Some(snapshot) = context
            .scope_cache::<KeyedSnapshot<Id>>(structural_token)
            .filter(|snapshot| {
                snapshot
                    .last_reconciled
                    .as_ref()
                    .is_some_and(|stamp| stamp.matches(structural_stamp))
            })
        {
            return (structural_token, inspect(snapshot));
        }
        let mut snapshot = context
            .take_scope_cache::<KeyedSnapshot<Id>>(structural_token)
            .unwrap_or_default();
        snapshot.last_reconciled = None;

        snapshot.epoch = snapshot
            .epoch
            .checked_add(1)
            .unwrap_or_else(|| panic!("ForEach reconciliation epoch overflow"));
        let epoch = snapshot.epoch;
        let entry_node = ReduceContext::node(&mut self.entry_slot);
        self.record_full_reconciliation();
        context.with_scope(structural_token, |context| {
            for (id, stamp) in states.entry_stamps() {
                if snapshot.entries.len() <= 1 {
                    // IndexMap skips hashing for empty and singleton lookups.
                    if let Some(active) = snapshot.entries.get_mut(id) {
                        active.refresh(stamp, epoch, &entry_node, context);
                    } else {
                        let active = ActiveEntry::new(stamp, epoch, &entry_node, context);
                        let previous = snapshot.entries.insert(Arc::new(id.clone()), active);
                        debug_assert!(
                            previous.is_none(),
                            "missing active ID checked before insertion"
                        );
                    }
                } else {
                    // Larger lookups already hash; retain that hash for a cold insertion.
                    let hash = snapshot.entries.hasher().hash_one(id);
                    match snapshot
                        .entries
                        .raw_entry_mut_v1()
                        .from_key_hashed_nocheck(hash, id)
                    {
                        RawEntryMut::Occupied(entry) => {
                            entry.into_mut().refresh(stamp, epoch, &entry_node, context);
                        }
                        RawEntryMut::Vacant(entry) => {
                            let active = ActiveEntry::new(stamp, epoch, &entry_node, context);
                            entry.insert_hashed_nocheck(hash, Arc::new(id.clone()), active);
                        }
                    }
                }
            }
        });
        snapshot.entries.retain(|_, active| {
            let retain = active.seen_epoch == epoch;
            if !retain {
                context.leave(active.token);
            }
            retain
        });
        snapshot.last_reconciled = Some(structural_stamp.to_owned());
        // A routing callback may panic after reconciliation has completed.
        // Restore the completed snapshot before propagating that same panic.
        let output = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| inspect(&snapshot)));
        context.put_scope_cache(structural_token, snapshot);
        let output = match output {
            Ok(output) => output,
            Err(payload) => std::panic::resume_unwind(payload),
        };
        (structural_token, output)
    }
}

/// Holds a parent reducer with one reducer shared across keyed children.
///
/// Matching child actions reduce the selected child before the parent. Entry
/// removal or replacement ends that entry's Store-local lifetime after parent
/// reduction. Reducer fields are shared sequentially across every entry.
/// Synchronous reduction permits borrowed or non-`Send` state and path captures,
/// and owned non-`Send` actions. Actions remain `'static` because delayed effect
/// output retains owned identifiers, actions and a thread-safe embedder.
/// Actor-backed Stores still require their own transport bounds.
///
/// # Panics
///
/// Missing matching IDs follow [`MissingChildPolicy`]: by default one required
/// issue must be accepted before the base runs. Fail-fast policy, rejected
/// diagnostic delivery, reporter panics, contradictory projections and private
/// reconciliation epoch overflow remain fatal. No rollback is provided.
pub struct ForEach<ParentState, ParentAction, Parent, Child, State, Action, Id> {
    /// Runs after a matching child, then supplies the final collection structure.
    parent: Parent,
    /// Shares reducer fields sequentially across entries; child state remains separate.
    child: Child,
    /// Borrows the collection for reconciliation and the selected child for reduction.
    state: State,
    /// Borrows the route and owns the embedder used by delayed child output.
    action: Action,
    /// Locates Store-local lifetime caches without retaining those caches itself.
    lifecycle: KeyedLifecycle<Id>,
    /// Applies only to an absent matching ID, not contradictory projections.
    missing_child_policy: MissingChildPolicy,
    /// Associates parent types without storing parent state or actions.
    marker: PhantomData<fn(ParentState, ParentAction)>,
}

impl<
    ParentState,
    ParentAction,
    Parent,
    Child,
    ChildState,
    ChildAction,
    Id,
    Get,
    GetMut,
    Extract,
    Embed,
>
    ForEach<
        ParentState,
        ParentAction,
        Parent,
        Child,
        KeyedStatePath<ParentState, Id, ChildState, Get, GetMut>,
        KeyedActionPath<ParentAction, Id, ChildAction, Extract, Embed>,
        Id,
    >
{
    /// Creates a keyed child-lifetime reducer.
    #[must_use]
    pub fn new(
        parent: Parent,
        state: KeyedStatePath<ParentState, Id, ChildState, Get, GetMut>,
        action: KeyedActionPath<ParentAction, Id, ChildAction, Extract, Embed>,
        child: Child,
    ) -> Self {
        Self {
            parent,
            child,
            state,
            action,
            lifecycle: KeyedLifecycle::new(),
            missing_child_policy: MissingChildPolicy::ReportAndBase,
            marker: PhantomData,
        }
    }

    #[cfg(test)]
    ordinary_test_items! {
    /// Exposes full scans without exposing the lifecycle's Store-local cache.
    const fn full_reconciliations(&self) -> u64 {
        self.lifecycle.full_reconciliations()
    }
    }
}

impl<ParentState, ParentAction, Parent, Child, State, Action, Id> fmt::Debug
    for ForEach<ParentState, ParentAction, Parent, Child, State, Action, Id>
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ForEach").finish_non_exhaustive()
    }
}

impl<
    ParentState,
    ParentAction,
    Parent,
    Child,
    ChildState,
    ChildAction,
    Id,
    Get,
    GetMut,
    Extract,
    Embed,
> Reducer
    for ForEach<
        ParentState,
        ParentAction,
        Parent,
        Child,
        KeyedStatePath<ParentState, Id, ChildState, Get, GetMut>,
        KeyedActionPath<ParentAction, Id, ChildAction, Extract, Embed>,
        Id,
    >
where
    ParentAction: 'static,
    Parent: Reducer<State = ParentState, Action = ParentAction>,
    Child: Reducer<State = ChildState, Action = ChildAction>,
    ChildAction: 'static,
    Id: Eq + Hash + Clone + Send + Sync + 'static,
    Get: for<'a> Fn(&'a ParentState) -> &'a KeyedState<Id, ChildState>,
    GetMut: for<'a> Fn(&'a mut ParentState) -> &'a mut KeyedState<Id, ChildState>,
    Extract: for<'a> Fn(&'a ParentAction) -> Option<&'a KeyedAction<Id, ChildAction>>,
    Embed: Fn(KeyedAction<Id, ChildAction>) -> ParentAction + Clone + Send + Sync + 'static,
{
    type State = ParentState;
    type Action = ParentAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        let (_, route) =
            self.lifecycle
                .reconcile_with(self.state.get(state), context, |snapshot| {
                    self.action
                        .extract(action)
                        .map(|(id, child_action)| (id, child_action, snapshot.active(id)))
                });
        let child_effect = match route {
            Some((id, child_action, active)) => {
                self.reduce_child(state, id, child_action, active, context)
            }
            None => Effect::none(),
        };
        let parent_effect = self.parent.reduce(state, action, context);
        self.lifecycle.reconcile(self.state.get(state), context);
        Effect::merge([child_effect, parent_effect])
    }
}

impl<
    ParentState,
    ParentAction,
    Parent,
    Child,
    ChildState,
    ChildAction,
    Id,
    Get,
    GetMut,
    Extract,
    Embed,
>
    ForEach<
        ParentState,
        ParentAction,
        Parent,
        Child,
        KeyedStatePath<ParentState, Id, ChildState, Get, GetMut>,
        KeyedActionPath<ParentAction, Id, ChildAction, Extract, Embed>,
        Id,
    >
where
    ParentAction: 'static,
    Child: Reducer<State = ChildState, Action = ChildAction>,
    ChildAction: 'static,
    Id: Eq + Hash + Clone + Send + Sync + 'static,
    Get: for<'a> Fn(&'a ParentState) -> &'a KeyedState<Id, ChildState>,
    GetMut: for<'a> Fn(&'a mut ParentState) -> &'a mut KeyedState<Id, ChildState>,
    Extract: for<'a> Fn(&'a ParentAction) -> Option<&'a KeyedAction<Id, ChildAction>>,
    Embed: Fn(KeyedAction<Id, ChildAction>) -> ParentAction + Clone + Send + Sync + 'static,
{
    /// Reduces under the matched lifetime and attaches it to delayed child output.
    fn reduce_child(
        &mut self,
        state: &mut ParentState,
        id: &Id,
        child_action: &ChildAction,
        active: Option<(Arc<Id>, ScopeToken, Arc<EntryStamp>)>,
        context: &mut ReduceContext<'_>,
    ) -> Effect<ParentAction> {
        let Some((active_id, token, stamp)) = active else {
            missing_child::<ParentState, ParentAction, ChildState, ChildAction>(
                self.missing_child_policy,
                context,
                "ForEach",
                Some(type_name::<Id>()),
            );
            return Effect::none();
        };
        let (child_state, selected_stamp) = self
            .state
            .get_mut(state)
            .get_mut_with_stamp(id)
            .expect("active keyed child must remain borrowable through the mutable projection");
        assert!(
            stamp.matches(selected_stamp),
            "active keyed child must retain its insertion lifetime through the mutable projection"
        );
        drop(stamp);
        let effect = context.with_scope(token, |context| {
            self.child.reduce(child_state, child_action, context)
        });
        if matches!(&effect.program, Program::None) {
            Effect::none()
        } else {
            let embed = self.action.clone_embedder();
            effect
                .in_scope(token)
                .map(move |action| embed(KeyedAction::new(active_id.as_ref().clone(), action)))
        }
    }
}

/// Composes a parent reducer with one keyed child reducer.
#[must_use]
#[expect(
    clippy::type_complexity,
    reason = "the constructor preserves ForEach's concrete static reducer type"
)]
pub fn for_each<
    ParentState,
    ParentAction,
    Parent,
    Child,
    ChildState,
    ChildAction,
    Id,
    Get,
    GetMut,
    Extract,
    Embed,
>(
    parent: Parent,
    state: KeyedStatePath<ParentState, Id, ChildState, Get, GetMut>,
    action: KeyedActionPath<ParentAction, Id, ChildAction, Extract, Embed>,
    child: Child,
) -> ForEach<
    ParentState,
    ParentAction,
    Parent,
    Child,
    KeyedStatePath<ParentState, Id, ChildState, Get, GetMut>,
    KeyedActionPath<ParentAction, Id, ChildAction, Extract, Embed>,
    Id,
> {
    ForEach::new(parent, state, action, child)
}

impl<ParentState, ParentAction, Parent, Child, State, Action, Id>
    ForEach<ParentState, ParentAction, Parent, Child, State, Action, Id>
{
    /// Selects the response to an authorised matching action with absent child state.
    ///
    /// The default requires accepted issue delivery before base reduction.
    /// Contradictory projections and stale scope authority are never recovered.
    #[must_use]
    pub const fn missing_child_policy(mut self, policy: MissingChildPolicy) -> Self {
        self.missing_child_policy = policy;
        self
    }
}

#[cfg(all(test, feature = "allocation-measurement"))]
/// Measures collection and reconciliation paths in isolated allocator processes.
mod allocation_tests {
    use std::hint::black_box;
    use std::sync::Arc;

    use super::{
        Effect, KeyedAction, KeyedActionPath, KeyedLifecycle, KeyedState, KeyedStatePath, Program,
        ReduceContext, Reducer, ScopeIdentity, ScopeSlot, for_each,
    };
    use crate::store::scope::ScopeRegistry;
    use crate::{DependencyValues, MonotonicInstant};

    /// Marks a child process by presence, preserving the existing assay protocol.
    const KEYED_ALLOCATION_CHILD_ENV: &str = "RCA_KEYED_ALLOCATION_CHILD";
    /// Selects the matching and unrelated warm-reconciliation budget.
    const WARMED_RECONCILIATION_ALLOCATION_TEST: &str =
        "composition::keyed::allocation_tests::warmed_keyed_reconciliation_does_not_allocate";
    /// Selects mutation budgets with capacity reserved outside measurement.
    const RESERVED_COLLECTION_ALLOCATION_TEST: &str = "composition::keyed::allocation_tests::reserved_keyed_collection_operations_do_not_allocate";

    /// Separates released backing storage from the intentionally retained lineage.
    pub(crate) fn cleared_shrink_releases_backing_storage_but_retains_lineage() {
        if run_allocation_test_in_child(
            "composition::keyed::allocation_tests::cleared_shrink_releases_backing_storage_but_retains_lineage",
        ) {
            return;
        }
        let profiler = dhat::Profiler::builder().testing().build();
        let before = dhat::HeapStats::get();
        let mut rows = keyed_rows(100_000, 0);
        rows.clear();
        let cleared = dhat::HeapStats::get();
        rows.shrink_to_fit();
        let shrunk = dhat::HeapStats::get();
        assert!(cleared.curr_bytes > shrunk.curr_bytes);
        assert_eq!(shrunk.curr_blocks - before.curr_blocks, 1);
        assert!(shrunk.curr_bytes > before.curr_bytes);
        assert_eq!(rows.entries.capacity(), 0);
        drop(rows);
        let dropped = dhat::HeapStats::get();
        assert_eq!(dropped.curr_blocks, before.curr_blocks);
        assert_eq!(dropped.curr_bytes, before.curr_bytes);
        drop(profiler);
        eprintln!(
            "cleared bytes={}, shrunk bytes={}",
            cleared.curr_bytes - before.curr_bytes,
            shrunk.curr_bytes - before.curr_bytes,
        );
    }

    /// Checks that empty construction and no-op operations never create lineage ownership.
    pub(crate) fn unused_empty_collections_do_not_allocate() {
        if run_allocation_test_in_child(
            "composition::keyed::allocation_tests::unused_empty_collections_do_not_allocate",
        ) {
            return;
        }
        let profiler = dhat::Profiler::builder().testing().build();
        assert_zero_allocation("unused_empty", 0, || {
            let mut rows = black_box(KeyedState::<u64, usize>::new());
            let copy = black_box(rows.clone());
            rows.retain(|_, _| panic!("empty collection has no entries"));
            rows.clear();
            assert!(rows.is_empty());
            assert!(copy.is_empty());
        });
        drop(profiler);
    }

    /// Counts real parent and child reduction alongside allocation measurements.
    struct State {
        /// Keeps every child separately addressable during warm reduction.
        rows: KeyedState<u64, usize>,
        /// Proves each measured action still reaches the parent reducer.
        parent_calls: usize,
    }

    /// Distinguishes routed reduction from a reconciliation-only parent action.
    enum Action {
        /// Exercises lookup and child reduction without producing follow-up work.
        Row(KeyedAction<u64, ()>),
        /// Exercises unchanged reconciliation without selecting a child.
        Parent,
    }

    /// Records parent entry without creating effect work.
    struct Parent;

    impl Reducer for Parent {
        type State = State;
        type Action = Action;

        fn reduce(
            &mut self,
            state: &mut Self::State,
            _action: &Self::Action,
            _context: &mut ReduceContext<'_>,
        ) -> Effect<Self::Action> {
            state.parent_calls += 1;
            Effect::none()
        }
    }

    /// Mutates the selected child so the measured route cannot be skipped.
    struct Child;

    impl Reducer for Child {
        type State = usize;
        type Action = ();

        fn reduce(
            &mut self,
            state: &mut Self::State,
            _action: &Self::Action,
            _context: &mut ReduceContext<'_>,
        ) -> Effect<Self::Action> {
            *state += 1;
            Effect::none()
        }
    }

    /// Measures matching and unrelated actions after the Store-local cache is populated.
    pub(crate) fn warmed_keyed_reconciliation_does_not_allocate() {
        if run_allocation_test_in_child(WARMED_RECONCILIATION_ALLOCATION_TEST) {
            return;
        }

        let profiler = dhat::Profiler::builder().testing().build();
        for size in [10_u64, 1_000, 100_000] {
            let mut reducer = for_each(
                Parent,
                KeyedStatePath::new(
                    |state: &State| &state.rows,
                    |state: &mut State| &mut state.rows,
                ),
                KeyedActionPath::new(
                    |action: &Action| match action {
                        Action::Row(action) => Some(action),
                        Action::Parent => None,
                    },
                    Action::Row,
                ),
                Child,
            );
            let mut state = State {
                rows: (0..size).map(|id| (id, 0)).collect(),
                parent_calls: 0,
            };
            let mut dependencies = DependencyValues::new();
            let (_scopes_lifecycle, mut scopes) = ScopeRegistry::for_test();
            let mut context_transitions = crate::store::scope::ScopeTransitionBatch::default();
            let mut context = ReduceContext::new(
                &mut dependencies,
                MonotonicInstant::ZERO,
                &mut scopes,
                &mut context_transitions,
            );
            let cold_before = dhat::HeapStats::get();
            let _ = reducer.reduce(&mut state, &Action::Parent, &mut context);
            let cold_after = dhat::HeapStats::get();
            eprintln!(
                "keyed_reconcile/cold/{size}: blocks={} bytes={}",
                cold_after.total_blocks - cold_before.total_blocks,
                cold_after.total_bytes - cold_before.total_bytes,
            );

            for (path, action) in [
                ("unrelated", Action::Parent),
                ("matching", Action::Row(KeyedAction::new(size / 2, ()))),
            ] {
                let before = dhat::HeapStats::get();
                let effect = black_box(reducer.reduce(&mut state, &action, &mut context));
                let after = dhat::HeapStats::get();
                let blocks = after.total_blocks - before.total_blocks;
                let bytes = after.total_bytes - before.total_bytes;

                eprintln!("keyed_reconcile/{path}/{size}: blocks={blocks} bytes={bytes}");
                assert!(matches!(effect.program, Program::None));
                assert_eq!(blocks, 0);
                assert_eq!(bytes, 0);
            }
        }
        // An owned Entry API allocates an ID owner before knowing whether it is needed.
        let mut entries = [(Arc::new(7_u64), ())]
            .into_iter()
            .collect::<indexmap::IndexMap<_, _>>();
        let before = dhat::HeapStats::get();
        for _ in 0..10 {
            black_box(entries.get_mut(&7));
        }
        let borrowed = dhat::HeapStats::get();
        for _ in 0..10 {
            black_box(entries.entry(Arc::new(7)).or_insert(()));
        }
        let owned = dhat::HeapStats::get();
        eprintln!(
            "keyed_reconcile/entry_probe: borrowed_blocks={} owned_blocks={} owned_bytes={}",
            borrowed.total_blocks - before.total_blocks,
            owned.total_blocks - borrowed.total_blocks,
            owned.total_bytes - borrowed.total_bytes,
        );
        assert_eq!(borrowed.total_blocks - before.total_blocks, 0);
        assert_eq!(owned.total_blocks - borrowed.total_blocks, 10);
        drop(profiler);
    }

    /// Measures existing-capacity collection operations without construction costs.
    pub(crate) fn reserved_keyed_collection_operations_do_not_allocate() {
        if run_allocation_test_in_child(RESERVED_COLLECTION_ALLOCATION_TEST) {
            return;
        }

        let profiler = dhat::Profiler::builder().testing().build();
        for size in [10_u64, 1_000, 100_000] {
            let lookup = keyed_rows(size, 0);
            assert_zero_allocation("lookup", size, || {
                black_box(lookup.get(black_box(&(size / 2))));
            });

            let mut insert = keyed_rows(size, 1);
            assert_zero_allocation("insert_reserved", size, || insert.insert(size, size));

            let mut ordered_remove = keyed_rows(size, 0);
            assert_zero_allocation("ordered_remove", size, || {
                black_box(ordered_remove.remove(black_box(&(size / 2))));
            });

            let mut swap_remove = keyed_rows(size, 0);
            assert_zero_allocation("swap_remove", size, || {
                black_box(swap_remove.swap_remove(black_box(&(size / 2))));
            });

            let mut moved = keyed_rows(size, 0);
            assert_zero_allocation("move_last_to_first", size, || {
                moved.move_index(usize::try_from(size - 1).expect("test size fits usize"), 0);
            });

            let mut cleared = keyed_rows(size, 0);
            assert_zero_allocation("clear", size, || cleared.clear());
        }
        for index in [0, 8, 16] {
            let mut inserted = keyed_rows(16, 1);
            assert_zero_allocation("try_insert_at_reserved", 16, || {
                inserted.try_insert_at(black_box(index), 16, 99).unwrap();
            });
            assert_eq!(inserted.len(), 17);
            assert_eq!(inserted.get_index(index), Some((&16, &99)));
        }
        drop(profiler);
    }

    /// Checks that one declaration reuses independent caches across Stores and parents.
    pub(crate) fn warmed_alternating_store_and_parent_contexts_do_not_allocate() {
        if run_allocation_test_in_child(
            "composition::keyed::allocation_tests::warmed_alternating_store_and_parent_contexts_do_not_allocate",
        ) {
            return;
        }
        let profiler = dhat::Profiler::builder().testing().build();
        for size in [10_u64, 1_000, 100_000] {
            let mut lifecycle = KeyedLifecycle::<u64>::new();
            let first: KeyedState<_, _> = (0..size).map(|id| (id, 0_u8)).collect();
            let second: KeyedState<_, _> = (0..size).map(|id| (id, 1_u8)).collect();
            let (_first_registry_lifecycle, mut first_registry) = ScopeRegistry::for_test();
            let (_second_registry_lifecycle, mut second_registry) = ScopeRegistry::for_test();
            let mut first_dependencies = DependencyValues::new();
            let mut second_dependencies = DependencyValues::new();
            let mut first_context_transitions =
                crate::store::scope::ScopeTransitionBatch::default();
            let mut first_context = ReduceContext::new(
                &mut first_dependencies,
                MonotonicInstant::ZERO,
                &mut first_registry,
                &mut first_context_transitions,
            );
            let mut second_context_transitions =
                crate::store::scope::ScopeTransitionBatch::default();
            let mut second_context = ReduceContext::new(
                &mut second_dependencies,
                MonotonicInstant::ZERO,
                &mut second_registry,
                &mut second_context_transitions,
            );
            let parent_node = ReduceContext::node(&mut ScopeSlot::new());
            let left = first_context.mount(parent_node.clone(), ScopeIdentity::new(1_u8));
            let right = first_context.mount(parent_node, ScopeIdentity::new(2_u8));
            let left_scope =
                first_context.with_scope(left, |context| lifecycle.reconcile(&first, context));
            let right_scope =
                first_context.with_scope(right, |context| lifecycle.reconcile(&second, context));
            let other_store_scope = lifecycle.reconcile(&second, &mut second_context);
            assert_ne!(left_scope, right_scope);
            assert_eq!(lifecycle.full_reconciliations(), 3);
            let before = dhat::HeapStats::get();
            for _ in 0..8 {
                let left_route = first_context.with_scope(left, |context| {
                    lifecycle
                        .reconcile_with(&first, context, |snapshot| snapshot.active(&(size / 2)))
                });
                let right_route = first_context.with_scope(right, |context| {
                    lifecycle
                        .reconcile_with(&second, context, |snapshot| snapshot.active(&(size / 2)))
                });
                let other_store_route =
                    lifecycle.reconcile_with(&second, &mut second_context, |snapshot| {
                        snapshot.active(&(size / 2))
                    });
                assert_eq!(left_route.0, left_scope);
                assert_eq!(right_route.0, right_scope);
                assert_eq!(other_store_route.0, other_store_scope);
                assert!(left_route.1.is_some());
                assert!(right_route.1.is_some());
                assert!(other_store_route.1.is_some());
            }
            let after = dhat::HeapStats::get();
            eprintln!(
                "keyed_alternating/{size}: blocks={} bytes={}",
                after.total_blocks - before.total_blocks,
                after.total_bytes - before.total_bytes
            );
            assert_eq!(after.total_blocks, before.total_blocks);
            assert_eq!(after.total_bytes, before.total_bytes);
            assert_eq!(lifecycle.full_reconciliations(), 3);
        }
        drop(profiler);
    }

    /// Isolates the global allocator while preserving each exact assay name.
    fn run_allocation_test_in_child(test_name: &str) -> bool {
        crate::allocation_test::run_in_marked_child(test_name, KEYED_ALLOCATION_CHILD_ENV)
    }

    /// Prepares owned entries and spare capacity before allocator measurement begins.
    fn keyed_rows(size: u64, spare_capacity: usize) -> KeyedState<u64, u64> {
        let capacity = usize::try_from(size).expect("test size fits usize") + spare_capacity;
        let mut rows = KeyedState::with_capacity(capacity);
        rows.extend((0..size).map(|id| (id, id)));
        rows
    }

    /// Runs one operation and compares cumulative allocation counts, not elapsed time.
    fn assert_zero_allocation(label: &str, size: u64, operation: impl FnOnce()) {
        let before = dhat::HeapStats::get();
        operation();
        let after = dhat::HeapStats::get();
        let blocks = after.total_blocks - before.total_blocks;
        let bytes = after.total_bytes - before.total_bytes;

        eprintln!("keyed_collection/{label}/{size}: blocks={blocks} bytes={bytes}");
        assert_eq!(blocks, 0);
        assert_eq!(bytes, 0);
    }
}

#[cfg(test)]
ordinary_test_items! {
mod tests {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::thread;

    use proptest::prelude::*;

    use super::{EntryStamp, KeyedLifecycle, KeyedSnapshot, KeyedState, for_each};
    use crate::store::scope::{ScopeIdentity, ScopeRegistry};

    /// Records user callbacks without changing equality or collision behaviour.
    struct HashProbe {
        value: usize,
        calls: Arc<std::sync::Mutex<Vec<&'static str>>>,
        panic_on_hash: Arc<AtomicBool>,
    }

    impl Clone for HashProbe {
        fn clone(&self) -> Self {
            self.calls.lock().unwrap().push("clone");
            Self {
                value: self.value,
                calls: Arc::clone(&self.calls),
                panic_on_hash: Arc::clone(&self.panic_on_hash),
            }
        }
    }

    impl PartialEq for HashProbe {
        fn eq(&self, other: &Self) -> bool {
            self.calls.lock().unwrap().push("eq");
            self.value == other.value
        }
    }

    impl Eq for HashProbe {}

    impl Hash for HashProbe {
        fn hash<H: Hasher>(&self, state: &mut H) {
            self.calls.lock().unwrap().push("hash");
            assert!(
                !self.panic_on_hash.swap(false, Ordering::Relaxed),
                "probe hash panic"
            );
            // Equal hashes make collision comparisons reproducible with RandomState.
            0_usize.hash(state);
        }
    }

    /// Probes the cached-hash alternative without changing production reconciliation.
    fn cached_hash_entry(entries: &mut indexmap::IndexMap<Arc<HashProbe>, ()>, id: &HashProbe) {
        use indexmap::map::raw_entry_v1::{RawEntryApiV1, RawEntryMut};
        use std::hash::BuildHasher;
        let hash = entries.hasher().hash_one(id);
        if let RawEntryMut::Vacant(entry) =
            entries.raw_entry_mut_v1().from_key_hashed_nocheck(hash, id)
        {
            entry.insert_hashed_nocheck(hash, Arc::new(id.clone()), ());
        }
    }

    /// Reports callback counts separately from elapsed time and allocator work.
    fn probe_calls(calls: &std::sync::Mutex<Vec<&'static str>>) -> (usize, usize, usize) {
        let calls = calls.lock().unwrap();
        let count = |name| calls.iter().filter(|call| **call == name).count();
        (count("hash"), count("eq"), count("clone"))
    }

    #[test]
    fn reconciliation_hash_candidates_preserve_different_small_map_costs() {
        for size in [1, 2, 8] {
            let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
            let panic_on_hash = Arc::new(AtomicBool::new(false));
            let key = |value| HashProbe {
                value,
                calls: Arc::clone(&calls),
                panic_on_hash: Arc::clone(&panic_on_hash),
            };
            let mut states = (0..size)
                .map(|value| (key(value), ()))
                .collect::<KeyedState<_, _>>();
            let mut lifecycle = KeyedLifecycle::new();
            let mut dependencies = DependencyValues::new();
            let (_owner, mut scopes) = ScopeRegistry::for_test();
            let mut transitions = crate::store::scope::ScopeTransitionBatch::default();
            let mut context = ReduceContext::new(
                &mut dependencies,
                MonotonicInstant::ZERO,
                &mut scopes,
                &mut transitions,
            );

            calls.lock().unwrap().clear();
            lifecycle.reconcile(&states, &mut context);
            let hybrid = probe_calls(&calls);
            assert_eq!(&calls.lock().unwrap()[..2], &["clone", "hash"]);
            assert_eq!(
                hybrid,
                (size, size * (size - 1) / 2 + usize::from(size > 1), size)
            );

            calls.lock().unwrap().clear();
            lifecycle.reconcile(&states, &mut context);
            assert_eq!(
                probe_calls(&calls),
                (0, 0, 0),
                "unchanged structure does not scan"
            );

            calls.lock().unwrap().clear();
            let mut cached = indexmap::IndexMap::new();
            for (id, ()) in &states {
                cached_hash_entry(&mut cached, id);
            }
            let raw = probe_calls(&calls);
            assert_eq!(&calls.lock().unwrap()[..2], &["hash", "clone"]);
            assert_eq!(raw, (size, size * (size - 1) / 2, size));
            eprintln!("keyed_hash/cold/{size}: hybrid={hybrid:?} cached={raw:?}");

            calls.lock().unwrap().clear();
            for (id, ()) in &states {
                cached.entry(Arc::new(id.clone())).or_insert(());
            }
            assert_eq!(
                probe_calls(&calls).2,
                size,
                "owned Entry clones even existing keys"
            );

            if size == 1 {
                states.insert(key(9), ());
                assert_eq!(states.remove(&key(9)), Some(()));
                calls.lock().unwrap().clear();
                panic_on_hash.store(true, Ordering::Relaxed);
                lifecycle.reconcile(&states, &mut context);
                assert_eq!(probe_calls(&calls), (0, 1, 0));
                assert!(panic_on_hash.load(Ordering::Relaxed));
                catch_unwind(AssertUnwindSafe(|| cached_hash_entry(&mut cached, &key(0))))
                    .expect_err("cached hashing enters user Hash on the same existing key");
                assert_eq!(cached.len(), 1);
                eprintln!(
                    "keyed_hash/singleton_rescan: ordinary=(0, 1, 0); cached enters Hash before Eq"
                );
            }
        }
    }

    #[test]
    fn non_send_actions_keep_owned_mapping_after_reducer_drop() {
        use std::rc::Rc;

        let effect = {
            let mut reducer = for_each(
                crate::Reduce::new(
                    |_: &mut KeyedState<u8, u8>,
                     _: &KeyedAction<u8, Rc<u8>>,
                     _: &mut ReduceContext<'_>| Effect::none(),
                ),
                KeyedStatePath::new(|state| state, |state| state),
                KeyedActionPath::new(
                    |action: &KeyedAction<u8, Rc<u8>>| Some(action),
                    core::convert::identity,
                ),
                crate::Reduce::new(
                    |state: &mut u8, action: &Rc<u8>, _: &mut ReduceContext<'_>| {
                        *state += **action;
                        Effect::future(async { Some(9_u8) }).map(Rc::new)
                    },
                ),
            );
            let mut state = [(7, 4)].into_iter().collect();
            let mut dependencies = DependencyValues::new();
            let (_owner, mut scopes) = ScopeRegistry::for_test();
            let mut transitions = crate::store::scope::ScopeTransitionBatch::default();
            let mut context = ReduceContext::new(
                &mut dependencies,
                MonotonicInstant::ZERO,
                &mut scopes,
                &mut transitions,
            );
            let action = KeyedAction::new(7, Rc::new(3));
            let effect = reducer.reduce(&mut state, &action, &mut context);
            assert_eq!(state.get(&7), Some(&7));
            effect
        };

        let crate::effect::Program::Merge(mut children) = effect.program else {
            panic!("the merged child retains its deferred output");
        };
        assert_eq!(children.len(), 1);
        let child = children.pop().unwrap();
        assert!(child.scope.is_some());
        let crate::effect::Program::Future(future) = child.program else {
            panic!("the mapped child retains its future");
        };
        let output = futures_executor::block_on(future).unwrap();
        assert_eq!(output.id, 7);
        assert_eq!(*output.action, 9);
        assert_eq!(Rc::strong_count(&output.action), 1);
        let weak = Rc::downgrade(&output.action);
        drop(output);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn reconciliation_hash_panic_leaves_teardown_ownership_with_the_registry() {
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let panic_on_hash = Arc::new(AtomicBool::new(false));
        let key = |value| HashProbe {
            value,
            calls: Arc::clone(&calls),
            panic_on_hash: Arc::clone(&panic_on_hash),
        };
        let mut states = (0..3)
            .map(|value| (key(value), ()))
            .collect::<KeyedState<_, _>>();
        let mut lifecycle = KeyedLifecycle::new();
        let mut dependencies = DependencyValues::new();
        let (_owner, mut scopes) = ScopeRegistry::for_test();
        let mut transitions = crate::store::scope::ScopeTransitionBatch::default();
        let mut context = ReduceContext::new(
            &mut dependencies,
            MonotonicInstant::ZERO,
            &mut scopes,
            &mut transitions,
        );
        let structural = lifecycle.reconcile(&states, &mut context);
        let stamps = context
            .scope_cache::<KeyedSnapshot<HashProbe>>(structural)
            .unwrap()
            .entries
            .values()
            .map(|entry| Arc::downgrade(&entry.stamp))
            .collect::<Vec<_>>();
        states.insert(key(3), ());
        calls.lock().unwrap().clear();
        panic_on_hash.store(true, Ordering::Relaxed);

        catch_unwind(AssertUnwindSafe(|| {
            lifecycle.reconcile(&states, &mut context)
        }))
        .expect_err("the changed structure enters user hashing");
        assert_eq!(*calls.lock().unwrap(), ["hash"]);
        assert!(
            context
                .scope_cache::<KeyedSnapshot<HashProbe>>(structural)
                .is_none()
        );
        assert!(stamps.iter().all(|stamp| stamp.upgrade().is_some()));
        drop(context);
        assert_eq!(scopes.leave(structural).len(), 4);
        assert!(stamps.iter().all(|stamp| stamp.upgrade().is_none()));
    }

    #[test]
    fn singleton_and_larger_reconciliation_retire_replaced_lifetimes() {
        for size in [1, 3] {
            let mut states = (0..size).map(|id| (id, ())).collect::<KeyedState<_, _>>();
            let mut lifecycle = KeyedLifecycle::new();
            let mut dependencies = DependencyValues::new();
            let (_owner, mut scopes) = ScopeRegistry::for_test();
            let mut transitions = crate::store::scope::ScopeTransitionBatch::default();
            let mut context = ReduceContext::new(
                &mut dependencies,
                MonotonicInstant::ZERO,
                &mut scopes,
                &mut transitions,
            );
            let (_, old) = lifecycle.reconcile_with(&states, &mut context, |snapshot| {
                snapshot.active(&0).unwrap().1
            });
            assert_eq!(states.remove(&0), Some(()));
            states.insert(0, ());
            let (_, current) = lifecycle.reconcile_with(&states, &mut context, |snapshot| {
                snapshot.active(&0).unwrap().1
            });
            assert_ne!(old, current);
            drop(context);
            assert!(!scopes.is_current(old));
            assert!(scopes.is_current(current));
        }
    }

    #[test]
    fn synchronous_for_each_accepts_borrowed_non_send_state_and_path_captures() {
        use std::{cell::Cell, rc::Rc};

        type State<'a> = (KeyedState<u8, &'a Rc<Cell<u8>>>, &'a Cell<u8>);
        let child = Rc::new(Cell::new(4));
        let parent_calls = Cell::new(0);
        let get_calls = Rc::new(Cell::new(0));
        let get_mut_calls = Rc::new(Cell::new(0));
        let extract_calls = Rc::new(Cell::new(0));
        let mut reducer = for_each(
            crate::Reduce::new(
                |state: &mut State<'_>, _: &KeyedAction<u8, u8>, _: &mut ReduceContext<'_>| {
                    state.1.set(state.1.get() + 1);
                    Effect::none()
                },
            ),
            KeyedStatePath::new(
                |state: &State<'_>| {
                    get_calls.set(get_calls.get() + 1);
                    &state.0
                },
                |state: &mut State<'_>| {
                    get_mut_calls.set(get_mut_calls.get() + 1);
                    &mut state.0
                },
            ),
            KeyedActionPath::new(
                |action: &KeyedAction<u8, u8>| {
                    extract_calls.set(extract_calls.get() + 1);
                    Some(action)
                },
                core::convert::identity,
            ),
            crate::Reduce::new(
                |state: &mut &Rc<Cell<u8>>, action: &u8, _: &mut ReduceContext<'_>| {
                    state.set(state.get() + *action);
                    Effect::none()
                },
            ),
        );
        let mut state = ([(7, &child)].into_iter().collect(), &parent_calls);
        let mut dependencies = DependencyValues::new();
        let (_lifecycle, mut scopes) = ScopeRegistry::for_test();
        let mut transitions = crate::store::scope::ScopeTransitionBatch::default();
        let mut context = ReduceContext::new(
            &mut dependencies,
            MonotonicInstant::ZERO,
            &mut scopes,
            &mut transitions,
        );

        let _ = reducer.reduce(&mut state, &KeyedAction::new(7, 3), &mut context);
        assert_eq!(child.get(), 7);
        assert_eq!(parent_calls.get(), 1);
        assert_eq!(get_calls.get(), 2);
        assert_eq!(get_mut_calls.get(), 1);
        assert_eq!(extract_calls.get(), 1);
    }

    #[test]
    fn retired_contexts_release_owned_ids_while_shared_adapter_survives() {
        let mut lifecycle = KeyedLifecycle::<Arc<()>>::new();
        for _ in 0..128 {
            let id = Arc::new(());
            let weak = Arc::downgrade(&id);
            let mut states = KeyedState::new();
            states.insert(id, 1_u8);
            let (_registry_lifecycle, mut registry) = ScopeRegistry::for_test();
            let mut dependencies = DependencyValues::new();
            let mut context_transitions = crate::store::scope::ScopeTransitionBatch::default();
            let mut context = ReduceContext::new(
                &mut dependencies,
                MonotonicInstant::ZERO,
                &mut registry,
                &mut context_transitions,
            );
            let structural = lifecycle.reconcile(&states, &mut context);
            drop(states);
            assert!(weak.upgrade().is_some(), "live snapshot owns its ID");
            context.leave(structural);
            assert!(
                weak.upgrade().is_none(),
                "retirement releases snapshot ID immediately"
            );
        }
        let id = Arc::new(());
        let weak = Arc::downgrade(&id);
        let mut states = KeyedState::new();
        states.insert(id, 2_u8);
        let (_registry_lifecycle, mut registry) = ScopeRegistry::for_test();
        let mut dependencies = DependencyValues::new();
        let mut context_transitions = crate::store::scope::ScopeTransitionBatch::default();
        let mut context = ReduceContext::new(
            &mut dependencies,
            MonotonicInstant::ZERO,
            &mut registry,
            &mut context_transitions,
        );
        lifecycle.reconcile(&states, &mut context);
        drop(context);
        drop(context_transitions);
        drop(states);
        assert!(weak.upgrade().is_some());
        drop(registry);
        assert!(
            weak.upgrade().is_none(),
            "Store drop releases metadata without revisiting adapter"
        );
        assert_eq!(lifecycle.full_reconciliations(), 129);
    }
    use crate::{
        DependencyValues, Effect, KeyedAction, KeyedActionPath, KeyedStatePath, MonotonicInstant,
        ReduceContext, Reducer,
    };

    /// Panics once when an owned identifier is retired, without poisoning final cleanup.
    #[derive(Debug)]
    struct PanicDropId {
        /// Supplies stable equality and hashing independent of the panic trigger.
        value: u8,
        /// Disarms before panicking so remaining owners can be dropped safely.
        panic_on_drop: Arc<AtomicBool>,
    }

    impl PanicDropId {
        /// Keeps lookup keys independent from the removed identifier's drop trigger.
        fn new(value: u8, panic_on_drop: bool) -> Self {
            Self {
                value,
                panic_on_drop: Arc::new(AtomicBool::new(panic_on_drop)),
            }
        }
    }

    impl PartialEq for PanicDropId {
        fn eq(&self, other: &Self) -> bool {
            self.value == other.value
        }
    }

    impl Eq for PanicDropId {}

    impl Hash for PanicDropId {
        fn hash<H: Hasher>(&self, state: &mut H) {
            self.value.hash(state);
        }
    }

    impl Drop for PanicDropId {
        fn drop(&mut self) {
            assert!(
                !self.panic_on_drop.swap(false, Ordering::Relaxed),
                "identifier drop panic"
            );
        }
    }

    /// Interrupts snapshot construction at the user identifier-cloning boundary.
    #[derive(Debug)]
    struct PanicCloneId {
        /// Supplies stable equality and hashing despite the armed clone fault.
        value: u8,
        /// Shares one consumed fault across related input and cached identifiers.
        panic_on_clone: Arc<AtomicBool>,
    }

    impl PanicCloneId {
        /// Shares the caller-owned fault trigger without invoking identifier cloning.
        fn new(value: u8, panic_on_clone: Arc<AtomicBool>) -> Self {
            Self {
                value,
                panic_on_clone,
            }
        }

        /// Creates an unarmed key for ordinary mutations around the failing scan.
        fn stable(value: u8) -> Self {
            Self::new(value, Arc::new(AtomicBool::new(false)))
        }
    }

    impl Clone for PanicCloneId {
        fn clone(&self) -> Self {
            assert!(
                !self.panic_on_clone.swap(false, Ordering::Relaxed),
                "identifier clone panic"
            );
            Self::new(self.value, Arc::clone(&self.panic_on_clone))
        }
    }

    impl PartialEq for PanicCloneId {
        fn eq(&self, other: &Self) -> bool {
            self.value == other.value
        }
    }

    impl Eq for PanicCloneId {}

    impl Hash for PanicCloneId {
        fn hash<H: Hasher>(&self, state: &mut H) {
            self.value.hash(state);
        }
    }

    /// Generates structural mutations separately from ordinary state changes.
    #[derive(Clone, Copy, Debug)]
    enum StructuralOperation {
        Insert { id: u8, value: i16 },
        Remove { id: u8 },
        SwapRemove { id: u8 },
        Move { from: u8, to: u8 },
        Retain { selector: u8, delta: i8 },
        Clear,
        Mutate { id: u8, delta: i8 },
    }

    /// Generates repeated identifiers and no-op cases as well as real structure changes.
    fn structural_operation_strategy() -> impl Strategy<Value = StructuralOperation> {
        prop_oneof![
            (0_u8..8, any::<i16>())
                .prop_map(|(id, value)| StructuralOperation::Insert { id, value }),
            (0_u8..8).prop_map(|id| StructuralOperation::Remove { id }),
            (0_u8..8).prop_map(|id| StructuralOperation::SwapRemove { id }),
            (any::<u8>(), any::<u8>())
                .prop_map(|(from, to)| StructuralOperation::Move { from, to }),
            (any::<u8>(), any::<i8>())
                .prop_map(|(selector, delta)| StructuralOperation::Retain { selector, delta }),
            Just(StructuralOperation::Clear),
            (0_u8..8, any::<i8>())
                .prop_map(|(id, delta)| StructuralOperation::Mutate { id, delta }),
        ]
    }

    /// Bounds interpreter work without writing failure artefacts into the checkout.
    fn structural_property_config() -> ProptestConfig {
        let defaults = ProptestConfig::default();
        ProptestConfig {
            cases: if cfg!(miri) { 4 } else { defaults.cases },
            failure_persistence: None,
            ..defaults
        }
    }

    /// Applies generated operations through collection APIs while keeping indexes valid.
    fn apply_structural_operation(
        states: &mut KeyedState<u8, i16>,
        operation: StructuralOperation,
    ) {
        match operation {
            StructuralOperation::Insert { id, value } => {
                drop(states.try_insert(id, value));
            }
            StructuralOperation::Remove { id } => {
                let _ = states.remove(&id);
            }
            StructuralOperation::SwapRemove { id } => {
                let _ = states.swap_remove(&id);
            }
            StructuralOperation::Move { from, to } => {
                if !states.is_empty() {
                    let len = states.len();
                    states.move_index(usize::from(from) % len, usize::from(to) % len);
                }
            }
            StructuralOperation::Retain { selector, delta } => {
                states.retain(|id, value| {
                    *value = value.wrapping_add(i16::from(delta));
                    (*id ^ selector) & 1 == 0
                });
            }
            StructuralOperation::Clear => states.clear(),
            StructuralOperation::Mutate { id, delta } => {
                if let Some(value) = states.get_mut(&id) {
                    *value = value.wrapping_add(i16::from(delta));
                }
            }
        }
    }

    proptest! {
        #![proptest_config(structural_property_config())]

        #[test]
        fn structural_stamp_matches_generated_membership_and_order_changes(
            operations in prop::collection::vec(structural_operation_strategy(), 1..65)
        ) {
            let mut states = KeyedState::new();

            for operation in operations {
                let before_ids = states.ids().copied().collect::<Vec<_>>();
                let before_stamp = states.structural_stamp().to_owned();

                apply_structural_operation(&mut states, operation);

                let after_ids = states.ids().copied().collect::<Vec<_>>();
                let stamp_changed = !before_stamp.matches(states.structural_stamp());
                prop_assert_eq!(stamp_changed, before_ids != after_ids);

                let mut left = states.clone();
                let mut right = states.clone();
                prop_assert!(
                    left.structural_stamp()
                        .to_owned()
                        .matches(right.structural_stamp())
                );

                left.insert(254, 0);
                right.insert(255, 0);

                prop_assert!(
                    !left
                        .structural_stamp()
                        .to_owned()
                        .matches(right.structural_stamp())
                );
            }
        }
    }

    /// Compares identity rather than public value equality after structural mutation.
    fn assert_stamp_changed<Id, State>(
        before: &super::CollectionStamp,
        states: &KeyedState<Id, State>,
    ) {
        assert!(!before.matches(states.structural_stamp()));
    }

    /// Rejects unnecessary invalidation after no-op or state-only mutation.
    fn assert_stamp_unchanged<Id, State>(
        before: &super::CollectionStamp,
        states: &KeyedState<Id, State>,
    ) {
        assert!(before.matches(states.structural_stamp()));
    }

    #[test]
    fn structural_stamp_ignores_state_only_mutation() {
        let mut states = KeyedState::from_iter([(1_u8, 10_i16), (2, 20)]);
        let before = states.structural_stamp().to_owned();

        *states.get_mut(&1).expect("first entry exists") += 1;
        for value in states.values_mut() {
            *value += 1;
        }
        for (_, value) in &mut states {
            *value += 1;
        }
        states.retain(|_, value| {
            *value += 1;
            true
        });

        assert_stamp_unchanged(&before, &states);
        assert_eq!(
            states
                .iter()
                .map(|(id, value)| (*id, *value))
                .collect::<Vec<_>>(),
            [(1, 14), (2, 23)]
        );
    }

    #[test]
    fn structural_stamp_tracks_successful_mutations() {
        let mut states = KeyedState::from_iter([(1_u8, 10_i16), (2, 20), (3, 30)]);

        let before = states.structural_stamp().to_owned();
        states.insert(4, 40);
        assert_stamp_changed(&before, &states);

        let before = states.structural_stamp().to_owned();
        assert_eq!(states.remove(&2), Some(20));
        assert_stamp_changed(&before, &states);

        let before = states.structural_stamp().to_owned();
        assert_eq!(states.swap_remove(&3), Some(30));
        assert_stamp_changed(&before, &states);

        let before = states.structural_stamp().to_owned();
        states.move_index(1, 0);
        assert_stamp_changed(&before, &states);

        let before = states.structural_stamp().to_owned();
        states.retain(|id, _| *id != 4);
        assert_stamp_changed(&before, &states);

        let before = states.structural_stamp().to_owned();
        states.clear();
        assert_stamp_changed(&before, &states);
    }

    #[test]
    fn structural_stamp_ignores_noop_mutations() {
        let mut states = KeyedState::from_iter([(1_u8, 10_i16), (2, 20)]);
        let before = states.structural_stamp().to_owned();

        assert_eq!(states.remove(&3), None);
        assert_eq!(states.swap_remove(&3), None);
        states.move_index(0, 0);
        states.retain(|_, _| true);

        assert_stamp_unchanged(&before, &states);

        states.clear();
        let empty = states.structural_stamp().to_owned();
        states.clear();
        assert_stamp_unchanged(&empty, &states);
    }

    #[test]
    fn clone_branches_allocate_distinct_structural_stamps() {
        let states = KeyedState::from_iter([(1_u8, 10_i16)]);
        let mut left = states.clone();
        let mut right = states.clone();

        assert!(
            left.structural_stamp()
                .to_owned()
                .matches(right.structural_stamp())
        );

        left.insert(2, 20);
        right.insert(2, 20);

        assert!(
            !left
                .structural_stamp()
                .to_owned()
                .matches(right.structural_stamp())
        );
    }

    #[test]
    fn retain_panic_after_removal_publishes_structural_stamp() {
        let mut states = KeyedState::from_iter([(1_u8, 10_i16), (2, 20), (3, 30)]);
        let before = states.structural_stamp().to_owned();

        let panic = catch_unwind(AssertUnwindSafe(|| {
            states.retain(|id, _| match id {
                1 => false,
                2 => panic!("predicate panic"),
                _ => true,
            });
        }));

        assert!(panic.is_err());
        assert_eq!(states.ids().copied().collect::<Vec<_>>(), [2, 3]);
        assert_stamp_changed(&before, &states);
    }

    #[test]
    fn ordered_remove_drop_panic_publishes_structural_stamp() {
        let mut states = KeyedState::new();
        states.insert(PanicDropId::new(1, false), 10_i16);
        states.insert(PanicDropId::new(2, true), 20);
        states.insert(PanicDropId::new(3, false), 30);
        let before = states.structural_stamp().to_owned();

        let panic = catch_unwind(AssertUnwindSafe(|| {
            let _ = states.remove(&PanicDropId::new(2, false));
        }));

        assert!(panic.is_err());
        assert_eq!(states.ids().map(|id| id.value).collect::<Vec<_>>(), [1, 3]);
        assert_stamp_changed(&before, &states);
    }

    #[test]
    fn swap_remove_drop_panic_publishes_structural_stamp() {
        let mut states = KeyedState::new();
        states.insert(PanicDropId::new(1, false), 10_i16);
        states.insert(PanicDropId::new(2, true), 20);
        states.insert(PanicDropId::new(3, false), 30);
        let before = states.structural_stamp().to_owned();

        let panic = catch_unwind(AssertUnwindSafe(|| {
            let _ = states.swap_remove(&PanicDropId::new(2, false));
        }));

        assert!(panic.is_err());
        assert_eq!(states.ids().map(|id| id.value).collect::<Vec<_>>(), [1, 3]);
        assert_stamp_changed(&before, &states);
    }

    #[test]
    fn clear_drop_panic_publishes_structural_stamp() {
        let mut states = KeyedState::new();
        states.insert(PanicDropId::new(1, true), 10_i16);
        states.insert(PanicDropId::new(2, false), 20);
        let before = states.structural_stamp().to_owned();

        let panic = catch_unwind(AssertUnwindSafe(|| states.clear()));

        assert!(panic.is_err());
        assert_stamp_changed(&before, &states);
    }

    /// Keeps the collection visible across parent-driven structural transitions.
    struct ReconciliationState {
        /// Supplies insertion lifetimes observed by the reducer's cached snapshot.
        rows: KeyedState<u8, i16>,
    }

    /// Separates routing, state mutation and clone-lineage transitions in the fixture.
    enum ReconciliationAction {
        Child(KeyedAction<u8, ()>),
        Unrelated,
        Insert,
        Remove,
        Move,
        AssignExactClone,
        AssignDivergentClone,
    }

    /// Mutates collection structure only after any selected child has reduced.
    struct ReconciliationParent;

    impl Reducer for ReconciliationParent {
        type State = ReconciliationState;
        type Action = ReconciliationAction;

        fn reduce(
            &mut self,
            state: &mut Self::State,
            action: &Self::Action,
            _context: &mut ReduceContext<'_>,
        ) -> Effect<Self::Action> {
            match action {
                ReconciliationAction::Child(_) | ReconciliationAction::Unrelated => {}
                ReconciliationAction::Insert => state.rows.insert(3, 30),
                ReconciliationAction::Remove => {
                    assert_eq!(state.rows.remove(&2), Some(20));
                }
                ReconciliationAction::Move => state.rows.move_index(1, 0),
                ReconciliationAction::AssignExactClone => {
                    state.rows = state.rows.clone();
                }
                ReconciliationAction::AssignDivergentClone => {
                    let mut replacement = state.rows.clone();
                    replacement.insert(4, 40);
                    state.rows = replacement;
                }
            }
            Effect::none()
        }
    }

    /// Makes child entry observable without adding asynchronous effects.
    struct ReconciliationChild;

    impl Reducer for ReconciliationChild {
        type State = i16;
        type Action = ();

        fn reduce(
            &mut self,
            state: &mut Self::State,
            _action: &Self::Action,
            _context: &mut ReduceContext<'_>,
        ) -> Effect<Self::Action> {
            *state += 1;
            Effect::none()
        }
    }

    /// Retains caller state while a partial cache scan unwinds.
    struct UnwindReconciliationState {
        /// Owns keys whose clone can fail independently of collection mutation.
        rows: KeyedState<PanicCloneId, i16>,
    }

    /// Selects routing or a scan without requiring a routed child action.
    enum UnwindReconciliationAction {
        Child(KeyedAction<PanicCloneId, ()>),
        Unrelated,
    }

    /// Leaves the collection unchanged so only reconciliation can invalidate its cache.
    struct UnwindReconciliationParent;

    impl Reducer for UnwindReconciliationParent {
        type State = UnwindReconciliationState;
        type Action = UnwindReconciliationAction;

        fn reduce(
            &mut self,
            _state: &mut Self::State,
            _action: &Self::Action,
            _context: &mut ReduceContext<'_>,
        ) -> Effect<Self::Action> {
            Effect::none()
        }
    }

    /// Exercises public paths against distinct collections while counting reducer entry.
    fn assert_projection_lifetime(
        observed: KeyedState<u8, i16>,
        selected: KeyedState<u8, i16>,
        same_lifetime: bool,
    ) {
        type State = (KeyedState<u8, i16>, KeyedState<u8, i16>, usize);
        let mut reducer = for_each(
            crate::Reduce::new(|state: &mut State, _: &KeyedAction<u8, ()>, _| {
                state.2 += 1;
                Effect::none()
            }),
            KeyedStatePath::new(|state: &State| &state.0, |state: &mut State| &mut state.1),
            KeyedActionPath::new(
                |action: &KeyedAction<u8, ()>| Some(action),
                core::convert::identity,
            ),
            ReconciliationChild,
        );
        let mut state = (observed, selected, 0);
        let before = (*state.0.get(&7).unwrap(), state.1.get(&7).copied());
        let mut dependencies = DependencyValues::new();
        let (_scopes_lifecycle, mut scopes) = ScopeRegistry::for_test();
        let mut transitions = crate::store::scope::ScopeTransitionBatch::default();
        let mut context = ReduceContext::new(
            &mut dependencies,
            MonotonicInstant::ZERO,
            &mut scopes,
            &mut transitions,
        );

        let result = catch_unwind(AssertUnwindSafe(|| {
            reducer.reduce(&mut state, &KeyedAction::new(7, ()), &mut context)
        }));
        assert_eq!(state.0.get(&7), Some(&before.0));
        if same_lifetime {
            assert!(result.is_ok(), "an exact cloned insertion remains valid");
            assert_eq!(state.1.get(&7), Some(&(before.1.unwrap() + 1)));
            assert_eq!(state.2, 1);
        } else {
            assert!(
                result.is_err(),
                "a different insertion must fail before reduction"
            );
            assert_eq!(state.1.get(&7).copied(), before.1);
            assert_eq!(state.2, 0);
        }
    }

    #[test]
    fn contradictory_projection_rejects_equal_id_from_independent_collection() {
        assert_projection_lifetime(
            [(7, 10)].into_iter().collect(),
            [(7, 20)].into_iter().collect(),
            false,
        );
    }

    #[test]
    fn contradictory_projection_rejects_missing_id_from_used_collection() {
        assert_projection_lifetime(
            [(7, 10)].into_iter().collect(),
            [(8, 20)].into_iter().collect(),
            false,
        );
    }

    #[test]
    fn contradictory_projection_rejects_reinserted_id_from_clone_branch() {
        let original: KeyedState<_, _> = [(7, 10)].into_iter().collect();
        let mut replacement = original.clone();
        assert_eq!(replacement.remove(&7), Some(10));
        replacement.insert(7, 20);
        assert_projection_lifetime(original, replacement, false);
    }

    #[test]
    fn projection_accepts_same_insertion_with_state_only_change() {
        let original: KeyedState<_, _> = [(7, 10)].into_iter().collect();
        let mut replacement = original.clone();
        *replacement.get_mut(&7).unwrap() = 20;
        assert_projection_lifetime(original, replacement, true);
    }

    #[test]
    fn for_each_scans_once_per_observed_structure() {
        let mut reducer = for_each(
            ReconciliationParent,
            KeyedStatePath::new(
                |state: &ReconciliationState| &state.rows,
                |state: &mut ReconciliationState| &mut state.rows,
            ),
            KeyedActionPath::new(
                |action: &ReconciliationAction| match action {
                    ReconciliationAction::Child(action) => Some(action),
                    _ => None,
                },
                ReconciliationAction::Child,
            ),
            ReconciliationChild,
        );
        let mut state = ReconciliationState {
            rows: [(1, 10), (2, 20)].into_iter().collect(),
        };
        let mut dependencies = DependencyValues::new();
        let (_scopes_lifecycle, mut scopes) = ScopeRegistry::for_test();
        let mut context_transitions = crate::store::scope::ScopeTransitionBatch::default();
        let mut context = ReduceContext::new(
            &mut dependencies,
            MonotonicInstant::ZERO,
            &mut scopes,
            &mut context_transitions,
        );

        let _ = reducer.reduce(&mut state, &ReconciliationAction::Unrelated, &mut context);
        assert_eq!(reducer.full_reconciliations(), 1);

        let _ = reducer.reduce(&mut state, &ReconciliationAction::Unrelated, &mut context);
        assert_eq!(reducer.full_reconciliations(), 1);

        let _ = reducer.reduce(
            &mut state,
            &ReconciliationAction::Child(KeyedAction::new(1, ())),
            &mut context,
        );
        assert_eq!(reducer.full_reconciliations(), 1);
        assert_eq!(state.rows.get(&1), Some(&11));

        let _ = reducer.reduce(&mut state, &ReconciliationAction::Insert, &mut context);
        assert_eq!(reducer.full_reconciliations(), 2);

        let _ = reducer.reduce(&mut state, &ReconciliationAction::Remove, &mut context);
        assert_eq!(reducer.full_reconciliations(), 3);

        let _ = reducer.reduce(&mut state, &ReconciliationAction::Move, &mut context);
        assert_eq!(reducer.full_reconciliations(), 4);

        let _ = reducer.reduce(
            &mut state,
            &ReconciliationAction::AssignExactClone,
            &mut context,
        );
        assert_eq!(reducer.full_reconciliations(), 4);

        let _ = reducer.reduce(
            &mut state,
            &ReconciliationAction::AssignDivergentClone,
            &mut context,
        );
        assert_eq!(reducer.full_reconciliations(), 5);

        let structural_node = ReduceContext::node(&mut reducer.lifecycle.structural_slot);
        let structural_token = context.mount(structural_node, ScopeIdentity::unit());
        context.leave(structural_token);
        let _ = reducer.reduce(&mut state, &ReconciliationAction::Unrelated, &mut context);
        assert_eq!(reducer.full_reconciliations(), 6);
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "epoch exhaustion checks preserve one shared pre-panic and post-panic ownership scenario"
    )]
    fn for_each_epoch_exhaustion_preserves_parent_state_and_scope_ownership() {
        let mut reducer = for_each(
            ReconciliationParent,
            KeyedStatePath::new(
                |state: &ReconciliationState| &state.rows,
                |state: &mut ReconciliationState| &mut state.rows,
            ),
            KeyedActionPath::new(
                |action: &ReconciliationAction| match action {
                    ReconciliationAction::Child(action) => Some(action),
                    _ => None,
                },
                ReconciliationAction::Child,
            ),
            ReconciliationChild,
        );
        let mut state = ReconciliationState {
            rows: [(1, 10), (2, 20)].into_iter().collect(),
        };
        let mut dependencies = DependencyValues::new();
        let (_scopes_lifecycle, mut scopes) = ScopeRegistry::for_test();
        let mut context_transitions = crate::store::scope::ScopeTransitionBatch::default();
        let mut context = ReduceContext::new(
            &mut dependencies,
            MonotonicInstant::ZERO,
            &mut scopes,
            &mut context_transitions,
        );

        let _ = reducer.reduce(&mut state, &ReconciliationAction::Unrelated, &mut context);
        let structural_node = ReduceContext::node(&mut reducer.lifecycle.structural_slot);
        let structural_token = context.mount(structural_node, ScopeIdentity::unit());
        let mut snapshot = context
            .take_scope_cache::<KeyedSnapshot<u8>>(structural_token)
            .expect("ordinary reduction creates the snapshot");
        let retained = [1, 2].map(|id| {
            let (_, owned_id, active) = snapshot.entries.get_full(&id).unwrap();
            (
                active.token,
                Arc::downgrade(owned_id),
                Arc::downgrade(&active.stamp),
            )
        });

        // Seed a valid boundary snapshot: a completed scan gives every live entry
        // the current epoch. This counter is independent of entry revisions.
        let seeded_epoch = u64::MAX - 1;
        snapshot.epoch = seeded_epoch;
        for active in snapshot.entries.values_mut() {
            active.seen_epoch = seeded_epoch;
        }
        context.put_scope_cache(structural_token, snapshot);

        let _ = reducer.reduce(&mut state, &ReconciliationAction::Move, &mut context);
        assert_eq!(state.rows.get_index(0), Some((&2, &20)));
        assert_eq!(reducer.full_reconciliations(), 2);
        let snapshot = context
            .scope_cache::<KeyedSnapshot<u8>>(structural_token)
            .unwrap();
        assert_eq!(snapshot.epoch, u64::MAX);
        for (id, (token, _, _)) in [1, 2].into_iter().zip(&retained) {
            let active = snapshot.entries.get(&id).unwrap();
            assert_eq!(active.seen_epoch, u64::MAX);
            assert_eq!(active.token, *token, "reordering retains child lifetimes");
        }

        // At the limit, an unchanged structure must still take the fast path.
        let _ = reducer.reduce(
            &mut state,
            &ReconciliationAction::Child(KeyedAction::new(1, ())),
            &mut context,
        );
        assert_eq!(state.rows.get(&1), Some(&11));
        assert_eq!(reducer.full_reconciliations(), 2);

        let panic = catch_unwind(AssertUnwindSafe(|| {
            let _ = reducer.reduce(&mut state, &ReconciliationAction::Insert, &mut context);
        }))
        .expect_err("a changed structure cannot wrap its reconciliation epoch");
        let message = panic
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| panic.downcast_ref::<String>().map(String::as_str));
        assert_eq!(message, Some("ForEach reconciliation epoch overflow"));
        assert_eq!(
            state.rows.get(&3),
            Some(&30),
            "parent mutation survives overflow"
        );
        assert!(
            context
                .scope_cache::<KeyedSnapshot<u8>>(structural_token)
                .is_none(),
            "the failed scan must not leave a reusable snapshot",
        );
        for (_, owned_id, stamp) in &retained {
            assert!(
                owned_id.upgrade().is_none(),
                "snapshot IDs must be released"
            );
            assert!(stamp.upgrade().is_some(), "live scopes own the entry stamp");
        }
        drop(context);
        let transitions_empty = context_transitions.is_empty();
        drop(context_transitions);
        assert!(transitions_empty, "overflow does not retire live scopes");
        assert!(scopes.is_current(structural_token));
        for (token, _, _) in &retained {
            assert!(scopes.is_current(*token));
        }

        // Unwind inspection is not recovery: the owner still retires its subtree.
        let departed = scopes.leave(structural_token);
        assert_eq!(departed.len(), 3, "the new row never acquired a scope");
        assert!(departed.contains(&structural_token));
        for (token, _, stamp) in retained {
            assert!(departed.contains(&token));
            assert!(!scopes.is_current(token));
            assert!(
                stamp.upgrade().is_none(),
                "retirement releases the identity"
            );
        }
        assert!(!scopes.is_current(structural_token));
    }

    /// Arms a routing hash panic while preserving the original panic payload.
    struct RoutingFault {
        /// Is consumed before panicking so teardown and later lookups can complete.
        hash_armed: AtomicBool,
        /// Allows pointer-identity checks that the panic was propagated unchanged.
        payload: Arc<()>,
    }

    /// Keeps identity stable while injecting a failure at the action lookup boundary.
    #[derive(Clone)]
    struct RoutingId {
        /// Supplies the complete equality and hash value.
        value: u8,
        /// Shares the externally armed hash failure without changing key equality.
        fault: Arc<RoutingFault>,
    }

    impl PartialEq for RoutingId {
        fn eq(&self, other: &Self) -> bool {
            self.value == other.value
        }
    }

    impl Eq for RoutingId {}

    impl Hash for RoutingId {
        fn hash<H: Hasher>(&self, state: &mut H) {
            if self.fault.hash_armed.swap(false, Ordering::Relaxed) {
                std::panic::panic_any(Arc::clone(&self.fault.payload));
            }
            self.value.hash(state);
        }
    }

    /// Records parent entry independently of state borrowing during unwind inspection.
    struct RoutingParent(Arc<AtomicUsize>);

    impl Reducer for RoutingParent {
        type State = KeyedState<RoutingId, i16>;
        type Action = KeyedAction<RoutingId, ()>;

        fn reduce(
            &mut self,
            _state: &mut Self::State,
            _action: &Self::Action,
            _context: &mut ReduceContext<'_>,
        ) -> Effect<Self::Action> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Effect::none()
        }
    }

    /// Checks cache ownership and original panic propagation at both routing boundaries.
    fn routing_panic_preserves_completed_snapshot(warmed: bool, panic_in_hash: bool) {
        let payload = Arc::new(());
        let fault = Arc::new(RoutingFault {
            hash_armed: AtomicBool::new(false),
            payload: Arc::clone(&payload),
        });
        let id = RoutingId {
            value: 1,
            fault: Arc::clone(&fault),
        };
        let sibling = RoutingId {
            value: 2,
            fault: Arc::clone(&fault),
        };
        // A single-entry IndexMap uses equality without hashing.
        let mut state = [(id.clone(), 10), (sibling.clone(), 20)]
            .into_iter()
            .collect();
        let action = KeyedAction::new(id.clone(), ());
        let extractor_armed = Arc::new(AtomicBool::new(false));
        let extractor_fault = Arc::clone(&extractor_armed);
        let parent_calls = Arc::new(AtomicUsize::new(0));
        let mut reducer = for_each(
            RoutingParent(Arc::clone(&parent_calls)),
            KeyedStatePath::new(|state| state, |state| state),
            KeyedActionPath::new(
                move |action: &KeyedAction<RoutingId, ()>| {
                    if extractor_fault.swap(false, Ordering::Relaxed) {
                        if panic_in_hash {
                            fault.hash_armed.store(true, Ordering::Relaxed);
                        } else {
                            std::panic::panic_any(Arc::clone(&fault.payload));
                        }
                    }
                    Some(action)
                },
                |action| action,
            ),
            ReconciliationChild,
        );
        let mut dependencies = DependencyValues::new();
        let (_scopes_lifecycle, mut scopes) = ScopeRegistry::for_test();
        let mut transitions = crate::store::scope::ScopeTransitionBatch::default();
        let mut context = ReduceContext::new(
            &mut dependencies,
            MonotonicInstant::ZERO,
            &mut scopes,
            &mut transitions,
        );
        if warmed {
            let _ = reducer.reduce(&mut state, &action, &mut context);
        }
        extractor_armed.store(true, Ordering::Relaxed);
        let panic = catch_unwind(AssertUnwindSafe(|| {
            let _ = reducer.reduce(&mut state, &action, &mut context);
        }))
        .expect_err("routing callback must propagate its panic");
        assert!(Arc::ptr_eq(
            panic.downcast_ref::<Arc<()>>().unwrap(),
            &payload
        ));
        assert_eq!(parent_calls.load(Ordering::Relaxed), usize::from(warmed));
        assert_eq!(state.get(&id), Some(&(10 + i16::from(warmed))));
        assert_eq!(reducer.full_reconciliations(), 1);
        let structural_node = ReduceContext::node(&mut reducer.lifecycle.structural_slot);
        let structural = context.mount(structural_node, ScopeIdentity::unit());
        let retained = context
            .scope_cache::<KeyedSnapshot<RoutingId>>(structural)
            .expect("routing panic must retain the completed scan")
            .entries
            .get(&id)
            .unwrap()
            .token;

        let _ = reducer.reduce(&mut state, &action, &mut context);
        assert_eq!(state.get(&id), Some(&(11 + i16::from(warmed))));
        assert_eq!(
            parent_calls.load(Ordering::Relaxed),
            usize::from(warmed) + 1
        );
        assert_eq!(state.get(&sibling), Some(&20));
        assert_eq!(
            reducer.full_reconciliations(),
            1,
            "completed scan must be reused"
        );
        assert_eq!(
            context
                .scope_cache::<KeyedSnapshot<RoutingId>>(structural)
                .unwrap()
                .entries
                .get(&id)
                .unwrap()
                .token,
            retained
        );
    }

    #[test]
    fn cold_extractor_panic_preserves_completed_snapshot() {
        routing_panic_preserves_completed_snapshot(false, false);
    }

    #[test]
    fn warm_extractor_panic_preserves_completed_snapshot() {
        routing_panic_preserves_completed_snapshot(true, false);
    }

    #[test]
    fn cold_routing_hash_panic_preserves_completed_snapshot() {
        routing_panic_preserves_completed_snapshot(false, true);
    }

    #[test]
    fn warm_routing_hash_panic_preserves_completed_snapshot() {
        routing_panic_preserves_completed_snapshot(true, true);
    }

    #[test]
    fn failed_scan_clears_stamp_before_exact_clone_restore() {
        let mut reducer = for_each(
            UnwindReconciliationParent,
            KeyedStatePath::new(
                |state: &UnwindReconciliationState| &state.rows,
                |state: &mut UnwindReconciliationState| &mut state.rows,
            ),
            KeyedActionPath::new(
                |action: &UnwindReconciliationAction| match action {
                    UnwindReconciliationAction::Child(action) => Some(action),
                    UnwindReconciliationAction::Unrelated => None,
                },
                UnwindReconciliationAction::Child,
            ),
            ReconciliationChild,
        );
        let mut state = UnwindReconciliationState {
            rows: [(PanicCloneId::stable(1), 10), (PanicCloneId::stable(2), 20)]
                .into_iter()
                .collect(),
        };
        let mut dependencies = DependencyValues::new();
        let (_scopes_lifecycle, mut scopes) = ScopeRegistry::for_test();
        let mut context_transitions = crate::store::scope::ScopeTransitionBatch::default();
        let mut context = ReduceContext::new(
            &mut dependencies,
            MonotonicInstant::ZERO,
            &mut scopes,
            &mut context_transitions,
        );

        let _ = reducer.reduce(
            &mut state,
            &UnwindReconciliationAction::Unrelated,
            &mut context,
        );
        assert_eq!(reducer.full_reconciliations(), 1);
        let structural_node = ReduceContext::node(&mut reducer.lifecycle.structural_slot);
        let structural_token = context.mount(structural_node, ScopeIdentity::unit());
        let original_token = context
            .scope_cache::<KeyedSnapshot<PanicCloneId>>(structural_token)
            .expect("structural cache mounted after reduction")
            .entries
            .get(&PanicCloneId::stable(1))
            .expect("first entry is active")
            .token;
        let exact_clone = state.rows.clone();

        assert_eq!(state.rows.remove(&PanicCloneId::stable(1)), Some(10));
        state.rows.insert(PanicCloneId::stable(1), 11);
        let panic_on_clone = Arc::new(AtomicBool::new(false));
        state
            .rows
            .insert(PanicCloneId::new(3, Arc::clone(&panic_on_clone)), 30);
        panic_on_clone.store(true, Ordering::Relaxed);

        let panic = catch_unwind(AssertUnwindSafe(|| {
            let _ = reducer.reduce(
                &mut state,
                &UnwindReconciliationAction::Unrelated,
                &mut context,
            );
        }));
        assert!(panic.is_err());
        assert_eq!(reducer.full_reconciliations(), 2);

        state.rows = exact_clone;
        let _ = reducer.reduce(
            &mut state,
            &UnwindReconciliationAction::Unrelated,
            &mut context,
        );

        assert_eq!(reducer.full_reconciliations(), 3);
        let snapshot = context
            .scope_cache::<KeyedSnapshot<PanicCloneId>>(structural_token)
            .expect("structural cache restored after reduction");
        assert_eq!(snapshot.entries.len(), state.rows.len());
        for (id, stamp) in state.rows.entry_stamps() {
            assert!(
                snapshot
                    .entries
                    .get(id)
                    .expect("restored entry is active")
                    .stamp
                    .matches(stamp)
            );
        }
        let restored_token = snapshot
            .entries
            .get(&PanicCloneId::stable(1))
            .expect("restored first entry is active")
            .token;
        drop(context);
        drop(context_transitions);
        assert!(!scopes.is_current(original_token));
        assert!(scopes.is_current(restored_token));
    }

    #[test]
    fn first_insertion_uses_revision_one() {
        let mut states = KeyedState::new();
        states.insert(3_u8, ());

        assert_eq!(states.entry_stamp(&3).expect("entry exists").revision, 1);
    }

    // Rebuilding to release storage would replace retained entry identities.
    #[test]
    fn shrink_preserves_order_values_and_identity_stamps() {
        struct MoveOnly(u8);
        let mut states = KeyedState::with_capacity(100);
        states.insert(3_u8, MoveOnly(30));
        states.insert(1, MoveOnly(10));
        let stamp = states.structural_stamp().to_owned();
        let entries: Vec<_> = states
            .entry_stamps()
            .map(|(_, stamp)| stamp.to_owned())
            .collect();
        states.shrink_to_fit();
        assert_eq!(
            states
                .iter()
                .map(|(id, value)| (*id, value.0))
                .collect::<Vec<_>>(),
            [(3, 30), (1, 10)]
        );
        assert!(stamp.matches(states.structural_stamp()));
        assert!(
            entries
                .iter()
                .zip(states.entry_stamps())
                .all(|(before, (_, after))| before.matches(after))
        );
    }

    // Clearing and shrinking must not split an existing clone family's revision allocator.
    #[test]
    fn cleared_shrink_preserves_lineage_and_fresh_clone_revisions() {
        let mut first = KeyedState::from_iter([(7_u8, ())]);
        let retired = first.entry_stamp(&7).unwrap().to_owned();
        first.clear();
        let mut second = first.clone();
        let stamp = first.structural_stamp().to_owned();
        first.shrink_to_fit();
        assert!(stamp.matches(first.structural_stamp()));
        assert!(Arc::ptr_eq(first.lineage(), second.lineage()));
        first.insert(7, ());
        second.insert(7, ());
        let left = first.entry_stamp(&7).unwrap().to_owned();
        let right = second.entry_stamp(&7).unwrap().to_owned();
        assert!(left != retired);
        assert!(right != retired);
        assert!(left != right);
    }

    // No Hash, Eq, Clone or state bounds are needed to release empty backing storage.
    #[test]
    fn shrink_accepts_unconstrained_ids_and_values() {
        struct Opaque;
        let mut states = KeyedState::<Opaque, Opaque>::with_capacity(100);
        states.shrink_to_fit();
        assert!(states.is_empty());
        assert!(states.lineage.is_none());
        assert_eq!(states.entries.capacity(), 0);
    }

    // Shrinking moves owned entries without invoking user hashing, equality or cloning.
    #[test]
    fn shrink_does_not_call_user_hash_or_equality() {
        struct Id(u8, Arc<AtomicBool>);
        impl PartialEq for Id {
            fn eq(&self, other: &Self) -> bool {
                assert!(!self.1.load(Ordering::Relaxed), "unexpected equality");
                self.0 == other.0
            }
        }
        impl Eq for Id {}
        impl Hash for Id {
            fn hash<H: Hasher>(&self, state: &mut H) {
                assert!(!self.1.load(Ordering::Relaxed), "unexpected hashing");
                self.0.hash(state);
            }
        }
        let forbidden = Arc::new(AtomicBool::new(false));
        let mut states = KeyedState::with_capacity(100);
        states.insert(Id(3, Arc::clone(&forbidden)), Box::new(30));
        states.insert(Id(1, Arc::clone(&forbidden)), Box::new(10));
        forbidden.store(true, Ordering::Relaxed);
        states.shrink_to_fit();
        assert_eq!(
            states
                .iter()
                .map(|(id, value)| (id.0, **value))
                .collect::<Vec<_>>(),
            [(3, 30), (1, 10)]
        );
    }

    #[test]
    fn clone_preserves_lineage_and_existing_revision() {
        let mut states = KeyedState::new();
        states.insert(3_u8, ());
        let clone = states.clone();

        assert!(Arc::ptr_eq(states.lineage(), clone.lineage()));
        assert!(
            states.entry_stamp(&3).expect("entry exists").to_owned()
                == clone.entry_stamp(&3).expect("entry exists").to_owned()
        );
    }

    #[test]
    fn independent_collections_have_distinct_lineages() {
        let mut first = KeyedState::new();
        first.insert(3_u8, ());
        let mut second = KeyedState::new();
        second.insert(3_u8, ());

        assert!(
            first.entry_stamp(&3).expect("entry exists").to_owned()
                != second.entry_stamp(&3).expect("entry exists").to_owned()
        );
    }

    #[test]
    fn cleared_clone_branches_preserve_lineage_without_reusing_retired_entries() {
        let mut first = KeyedState::from_iter([(7_u8, ())]);
        let retired = first.entry_stamp(&7).expect("initial entry").to_owned();
        first.clear();
        let mut second = first.clone();
        first.insert(7, ());
        second.insert(7, ());
        assert!(Arc::ptr_eq(first.lineage(), second.lineage()));
        let left = first.entry_stamp(&7).expect("left entry").to_owned();
        let right = second.entry_stamp(&7).expect("right entry").to_owned();
        assert!(left != retired);
        assert!(right != retired);
        assert!(left != right);
    }

    #[test]
    fn remove_and_reinsert_changes_entry_identity() {
        let mut states = KeyedState::new();
        states.insert(3_u8, ());
        let before = states.entry_stamp(&3).expect("entry exists").to_owned();
        states.remove(&3);
        states.insert(3, ());

        assert!(before != states.entry_stamp(&3).expect("entry exists").to_owned());
    }

    #[test]
    fn clone_branch_insertions_allocate_unique_revisions_concurrently() {
        assert_concurrent_branch_identities(&KeyedState::new());
        let mut states = KeyedState::from_iter([(0_u8, ())]);
        states.clear();
        assert_concurrent_branch_identities(&states);
    }

    /// Checks revision uniqueness when related collections mutate on separate threads.
    fn assert_concurrent_branch_identities(states: &KeyedState<u8, ()>) {
        let left = states.clone();
        let right = states.clone();

        let left = thread::spawn(move || insert_and_identify(left, 7));
        let right = thread::spawn(move || insert_and_identify(right, 7));
        let left = left.join().expect("left branch completes");
        let right = right.join().expect("right branch completes");

        assert!(left != right);
    }

    /// Retains an inserted identity after the worker drops its collection.
    fn insert_and_identify(mut states: KeyedState<u8, ()>, id: u8) -> EntryStamp {
        states.insert(id, ());
        states.entry_stamp(&id).expect("entry exists").to_owned()
    }

    #[test]
    fn entry_stamp_hash_includes_lineage_and_revision() {
        let mut states = KeyedState::new();
        states.insert(1_u8, ());
        states.insert(2, ());
        let identities = states
            .entry_stamps()
            .map(|(_, stamp)| stamp.to_owned())
            .collect::<Vec<_>>();
        let first = states.entry_stamp(&1).expect("entry exists").to_owned();
        let first_clone = first.clone();

        assert_eq!(identities.len(), 2);
        assert!(identities.contains(&first));
        assert_eq!(identity_hash(&first), identity_hash(&first_clone));
    }

    /// Compares identity hash behaviour without assuming a persistent numeric hash.
    fn identity_hash(identity: &EntryStamp) -> u64 {
        let mut hasher = DefaultHasher::new();
        identity.hash(&mut hasher);
        hasher.finish()
    }

    #[test]
    fn revision_overflow_panics_without_inserting() {
        let mut states = KeyedState::from_iter([(0_u8, ())]);
        states.clear();
        states
            .lineage()
            .next_revision
            .store(u64::MAX, std::sync::atomic::Ordering::Relaxed);

        let panic = catch_unwind(AssertUnwindSafe(|| states.insert(1_u8, ())))
            .expect_err("overflow panics");
        let message = panic
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| panic.downcast_ref::<String>().map(String::as_str));

        assert_eq!(message, Some("KeyedState entry revision overflow"));
        assert!(states.is_empty());
    }
}
}

#[cfg(all(test, feature = "allocation-measurement"))]
pub(crate) use allocation_tests::{
    cleared_shrink_releases_backing_storage_but_retains_lineage,
    reserved_keyed_collection_operations_do_not_allocate, unused_empty_collections_do_not_allocate,
    warmed_alternating_store_and_parent_contexts_do_not_allocate,
    warmed_keyed_reconciliation_does_not_allocate,
};

#[cfg(all(test, feature = "allocation-measurement"))]
pub(crate) use operations::{
    merge_result_capacity_tracks_conflicts_instead_of_all_inputs,
    moves_bound_permutation_allocations, sort_reuses_completed_scratch_storage,
};
