//! Checked operations over captured ordered pairs.

use core::cmp::Ordering;
use core::convert::Infallible;
use core::fmt;
use core::hash::{BuildHasher, Hash};
use core::ops::Range;
use std::backtrace::Backtrace;
use std::collections::HashSet;
use std::sync::Arc;

use indexmap::map::raw_entry_v1::{RawEntryApiV1, RawEntryMut};

use super::{Entry, KeyedState, Lineage};

/// Selects which state survives a captured-identifier merge conflict.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyedMergeDecision {
    /// Retains existing state and returns the entire unused incoming pair.
    KeepExisting,
    /// Retains the captured key and lifetime, returning incoming key and displaced state.
    ReplaceWithIncoming,
}

/// Reports an invalid current-order position, range, or repeated source offset.
#[derive(Debug)]
pub struct KeyedPositionError {
    /// Records rejection when backtrace capture is enabled.
    backtrace: Backtrace,
}

impl KeyedPositionError {
    /// Captures diagnostics only after position validation rejects the operation.
    fn new() -> Self {
        Self {
            backtrace: Backtrace::capture(),
        }
    }

    /// Returns the captured error backtrace.
    pub const fn backtrace(&self) -> &Backtrace {
        &self.backtrace
    }
}

impl fmt::Display for KeyedPositionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid KeyedState position or range\n{}",
            self.backtrace
        )
    }
}
impl std::error::Error for KeyedPositionError {}

/// Returns every proposed pair after rejected insertion, replacement, or merge.
pub struct KeyedMutationError<Id, State, E = Infallible> {
    /// Retains every proposed owner in its original order until recovery or drop.
    inputs: Vec<(Id, State)>,
    /// Associates a resolver cause only with a resolver failure.
    kind: MutationFailure<E>,
    /// Records the first rejection site when capture is enabled.
    backtrace: Backtrace,
}

/// Distinguishes validation failures from a caller-owned resolver error.
enum MutationFailure<E> {
    /// Rejects an existing identifier or repeated incoming identifier before mutation.
    Duplicate,
    /// Rejects an insertion offset beyond the current length.
    InvalidPosition,
    /// Retains the exact error returned while staging conflict decisions.
    Resolver(E),
}

impl<E> MutationFailure<E> {
    /// Keeps diagnostic summaries independent of caller-owned values and errors.
    const fn message(&self) -> &'static str {
        match self {
            Self::Duplicate => "duplicate identifier",
            Self::InvalidPosition => "invalid position",
            Self::Resolver(_) => "resolver failed",
        }
    }
}

impl<Id, State, E> KeyedMutationError<Id, State, E> {
    /// Takes rejected inputs without formatting or cloning their payloads.
    fn new(inputs: Vec<(Id, State)>, kind: MutationFailure<E>) -> Self {
        Self {
            inputs,
            kind,
            backtrace: Backtrace::capture(),
        }
    }

    /// Recovers every original proposed pair in input order.
    #[must_use]
    pub fn into_inputs(self) -> Vec<(Id, State)> {
        self.inputs
    }

    /// Recovers the proposed pairs and optional caller resolver error.
    #[must_use]
    pub fn into_parts(self) -> (Vec<(Id, State)>, Option<E>) {
        let resolver = match self.kind {
            MutationFailure::Resolver(error) => Some(error),
            MutationFailure::Duplicate | MutationFailure::InvalidPosition => None,
        };
        (self.inputs, resolver)
    }

    /// Reports whether a duplicate proposed or existing identifier rejected this operation.
    #[must_use]
    pub fn is_duplicate(&self) -> bool {
        matches!(self.kind, MutationFailure::Duplicate)
    }

    /// Reports whether the insertion offset was invalid.
    #[must_use]
    pub fn is_invalid_position(&self) -> bool {
        matches!(self.kind, MutationFailure::InvalidPosition)
    }

    /// Borrows the caller resolver error, if any.
    pub const fn resolver_error(&self) -> Option<&E> {
        match &self.kind {
            MutationFailure::Resolver(error) => Some(error),
            MutationFailure::Duplicate | MutationFailure::InvalidPosition => None,
        }
    }

    /// Returns the captured error backtrace.
    pub const fn backtrace(&self) -> &Backtrace {
        &self.backtrace
    }
}

impl<Id, State> KeyedMutationError<Id, State> {
    /// Transfers a validation failure without recapturing its original backtrace.
    pub(super) fn into_inputs_and_backtrace(self) -> (Vec<(Id, State)>, Backtrace) {
        (self.inputs, self.backtrace)
    }
}

impl<Id, State, E> fmt::Debug for KeyedMutationError<Id, State, E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeyedMutationError")
            .field("reason", &self.kind.message())
            .field("input_count", &self.inputs.len())
            .field("backtrace", &self.backtrace)
            .finish_non_exhaustive()
    }
}
impl<Id, State, E> fmt::Display for KeyedMutationError<Id, State, E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "KeyedState mutation rejected: {}\n{}",
            self.kind.message(),
            self.backtrace
        )
    }
}
impl<Id, State, E: std::error::Error + 'static> std::error::Error
    for KeyedMutationError<Id, State, E>
{
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.resolver_error().map(|e| e as &dyn std::error::Error)
    }
}

/// Reports allocation failure without changing observable pairs or child lifetimes.
#[derive(Debug)]
pub struct KeyedReserveError {
    /// Preserves the allocator or capacity-overflow cause behind the library's error type.
    source: indexmap::TryReserveError,
    /// Records failure when backtrace capture is enabled.
    backtrace: Backtrace,
}
impl KeyedReserveError {
    /// Returns the captured error backtrace.
    pub const fn backtrace(&self) -> &Backtrace {
        &self.backtrace
    }
}
impl fmt::Display for KeyedReserveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "KeyedState reservation failed: {}\n{}",
            self.source, self.backtrace
        )
    }
}
impl std::error::Error for KeyedReserveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Sorts offsets without touching captured pairs or calling a failed comparator again.
///
/// The caller supplies equally sized offset and scratch slices. Each recursive
/// merge moves offsets only; the collection stays unchanged if comparison fails.
fn try_sort_offsets<E>(
    offsets: &mut [usize],
    scratch: &mut [usize],
    compare: &mut impl FnMut(usize, usize) -> Result<Ordering, E>,
) -> Result<(), E> {
    if offsets.len() < 2 {
        return Ok(());
    }
    let middle = offsets.len() / 2;
    let (left, right) = offsets.split_at_mut(middle);
    let (left_scratch, right_scratch) = scratch.split_at_mut(middle);
    try_sort_offsets(left, left_scratch, compare)?;
    try_sort_offsets(right, right_scratch, compare)?;
    let (mut left_index, mut right_index) = (0, middle);
    for slot in scratch.iter_mut() {
        let take_left = right_index == offsets.len()
            || (left_index < middle
                && compare(offsets[left_index], offsets[right_index])? != Ordering::Greater);
        if take_left {
            *slot = offsets[left_index];
            left_index += 1;
        } else {
            *slot = offsets[right_index];
            right_index += 1;
        }
    }
    offsets.copy_from_slice(scratch);
    Ok(())
}

impl<Id, State> KeyedState<Id, State> {
    /// Borrows the captured pair at the current position.
    ///
    /// Positions are invalidated by structural/order mutation and never grant child authority.
    #[must_use]
    pub fn get_index(&self, index: usize) -> Option<(&Id, &State)> {
        self.entries
            .get_index(index)
            .map(|(id, entry)| (id, &entry.state))
    }

    /// Borrows the captured identifier and mutable state at the current position.
    ///
    /// Mutating state does not regenerate its captured identifier or child lifetime.
    pub fn get_index_mut(&mut self, index: usize) -> Option<(&Id, &mut State)> {
        self.entries
            .get_index_mut(index)
            .map(|(id, entry)| (id, &mut entry.state))
    }

    /// Returns the successor of a valid element position, including the end sentinel.
    ///
    /// End and out-of-range inputs return `None`. The returned `len()`
    /// sentinel is not a readable element position or runtime authority.
    #[must_use]
    pub fn checked_successor(&self, index: usize) -> Option<usize> {
        (index < self.len()).then(|| index + 1)
    }

    /// Collects selected borrowed pairs in original order without cloning.
    ///
    /// # Errors
    /// Returns the first predicate error, dropping the partial view. The collection
    /// stays unchanged; caller side effects are not rolled back.
    /// # Panics
    /// Panics if the caller predicate panics.
    pub fn try_filter<E>(
        &self,
        mut keep: impl FnMut(&Id, &State) -> Result<bool, E>,
    ) -> Result<Vec<(&Id, &State)>, E> {
        let mut selected = Vec::new();
        for (id, state) in self {
            if keep(id, state)? {
                selected.push((id, state));
            }
        }
        Ok(selected)
    }

    /// Attempts reservation without changing observable pairs or private lifetimes.
    ///
    /// No particular capacity or performance improvement is guaranteed.
    /// # Errors
    /// Returns a typed allocation error on capacity overflow or allocation failure.
    pub fn try_reserve(&mut self, additional: usize) -> Result<(), KeyedReserveError> {
        self.entries
            .try_reserve(additional)
            .map_err(|source| KeyedReserveError {
                source,
                backtrace: Backtrace::capture(),
            })
    }

    /// Removes a checked half-open range, returning owned pairs in original order.
    ///
    /// # Errors
    /// Rejects reversed ranges or endpoints beyond `len()` before mutation.
    /// # Panics
    /// Panics if private revision allocation overflows.
    pub fn try_remove_range(
        &mut self,
        range: Range<usize>,
    ) -> Result<Vec<(Id, State)>, KeyedPositionError> {
        if range.start > range.end || range.end > self.len() {
            return Err(KeyedPositionError::new());
        }
        if range.is_empty() {
            return Ok(Vec::new());
        }
        self.structural_revision = self.lineage().allocate_revision();
        Ok(self
            .entries
            .drain(range)
            .map(|(id, entry)| (id, entry.state))
            .collect())
    }

    /// Invalidates structure before transferring the last pair to stack operations.
    pub(in crate::composition) fn pop(&mut self) -> Option<(Id, State)> {
        if self.entries.is_empty() {
            return None;
        }
        self.structural_revision = self.lineage().allocate_revision();
        self.entries.pop().map(|(id, entry)| (id, entry.state))
    }

    /// Reverses all captured pairs exactly, preserving private child lifetimes.
    ///
    /// # Panics
    /// Panics if private revision allocation overflows.
    pub fn reverse(&mut self) {
        if self.len() > 1 {
            self.structural_revision = self.lineage().allocate_revision();
            self.entries.reverse();
        }
    }

    /// Exchanges two valid current positions, preserving every captured pair.
    ///
    /// Equal valid positions are a no-op.
    /// # Errors
    /// Rejects either out-of-range position before mutation.
    /// # Panics
    /// Panics if private revision allocation overflows.
    pub fn try_swap_indices(&mut self, a: usize, b: usize) -> Result<(), KeyedPositionError> {
        if a >= self.len() || b >= self.len() {
            return Err(KeyedPositionError::new());
        }
        if a != b {
            self.structural_revision = self.lineage().allocate_revision();
            self.entries.swap_indices(a, b);
        }
        Ok(())
    }

    /// Moves selected sources before an original-order destination, preserving source order.
    ///
    /// `destination` may equal `len()`. Removed sources earlier than the original
    /// destination are subtracted before insertion. Source argument order is ignored.
    /// # Errors
    /// Rejects repeated or invalid sources and destinations beyond `len()` before mutation.
    /// # Panics
    /// Panics if private revision allocation overflows.
    pub fn try_move_indices(
        &mut self,
        sources: &[usize],
        destination: usize,
    ) -> Result<(), KeyedPositionError> {
        if destination > self.len() {
            return Err(KeyedPositionError::new());
        }
        if sources.is_empty() {
            return Ok(());
        }
        let mut sources = sources.to_vec();
        sources.sort_unstable();
        if sources.last().is_some_and(|i| *i >= self.len())
            || sources.windows(2).any(|pair| pair[0] == pair[1])
        {
            return Err(KeyedPositionError::new());
        }
        let insertion = destination - sources.partition_point(|source| *source < destination);
        let mut selected = sources.iter().copied().peekable();
        let mut order = Vec::with_capacity(self.len());
        order.extend((0..self.len()).filter(|index| selected.next_if_eq(index).is_none()));
        order.splice(insertion..insertion, sources);
        let scratch = vec![0; self.len()];
        self.apply_order(order, scratch);
        Ok(())
    }

    /// Applies a complete source permutation, reusing equally sized scratch storage.
    ///
    /// Both callers construct a permutation of every current index. The scratch
    /// follows moved entries to their destination; one revision covers all swaps.
    /// No user hashing, equality, callback or payload destructor runs during swaps.
    fn apply_order(&mut self, order: Vec<usize>, mut scratch: Vec<usize>) {
        // Each slot follows its current entry and names that entry's destination.
        for (target, original) in order.into_iter().enumerate() {
            scratch[original] = target;
        }
        let mut changed = false;
        for source in 0..scratch.len() {
            while scratch[source] != source {
                let target = scratch[source];
                if !changed {
                    self.structural_revision = self.lineage().allocate_revision();
                    changed = true;
                }
                self.entries.swap_indices(source, target);
                scratch.swap(source, target);
            }
        }
    }

    /// Partitions captured pairs and returns the first unselected position.
    ///
    /// No stability or original-order rollback is promised. Errors and unwinds
    /// retain all pairs, lookup consistency and private child lifetimes.
    /// # Errors
    /// Returns the first caller predicate error.
    /// # Panics
    /// Panics if the caller panics or private revision allocation overflows.
    pub fn try_partition<E>(
        &mut self,
        mut selected: impl FnMut(&Id, &State) -> Result<bool, E>,
    ) -> Result<usize, E> {
        let mut boundary = 0;
        for index in 0..self.len() {
            let (id, state) = self.get_index(index).expect("iteration index exists");
            if selected(id, state)? {
                if boundary != index {
                    self.structural_revision = self.lineage().allocate_revision();
                    self.entries.swap_indices(boundary, index);
                }
                boundary += 1;
            }
        }
        Ok(boundary)
    }

    /// Sorts captured pairs into ascending caller order without stable ties.
    ///
    /// Sorts temporary offsets in `O(n log n)` time and `O(n)` space, then applies
    /// lookup-safe pair swaps. Comparator errors/unwinds leave this implementation's
    /// order unchanged; callers should rely only on pair/lookup/lifetime preservation.
    /// The comparator is not called again after its first error.
    /// # Errors
    /// Returns the first caller comparator error.
    /// # Panics
    /// Panics if the comparator panics or private revision allocation overflows.
    pub fn try_sort_by<E>(
        &mut self,
        mut compare: impl FnMut(&Id, &State, &Id, &State) -> Result<Ordering, E>,
    ) -> Result<(), E> {
        if self.len() < 2 {
            return Ok(());
        }
        let mut order: Vec<_> = (0..self.len()).collect();
        let mut scratch = vec![0; order.len()];
        try_sort_offsets(&mut order, &mut scratch, &mut |left, right| {
            let (a, left) = self.get_index(left).expect("staged source exists");
            let (b, right) = self.get_index(right).expect("staged source exists");
            compare(a, left, b, right)
        })?;
        self.apply_order(order, scratch);
        Ok(())
    }

    /// Shuffles captured pairs using only caller-supplied random words.
    ///
    /// Fisher–Yates selects each swap using a word modulo the remaining length.
    /// This guarantees a permutation, not unbiased or cryptographic randomness.
    /// No ambient random source is accessed. Unwind preserves all captured pairs.
    /// # Panics
    /// Panics if the source panics or private revision allocation overflows.
    pub fn shuffle_with(&mut self, mut random_word: impl FnMut() -> u64) {
        for last in (1..self.len()).rev() {
            let selected = usize::try_from(u128::from(random_word()) % (last as u128 + 1))
                .expect("selected index fits usize");
            if selected != last {
                self.structural_revision = self.lineage().allocate_revision();
                self.entries.swap_indices(selected, last);
            }
        }
    }
}

impl<Id: Eq + Hash, State> KeyedState<Id, State> {
    /// Merges a unique batch after staging every read-only conflict decision.
    ///
    /// New identifiers append in incoming order. Existing identifiers retain their
    /// captured keys, positions and private lifetimes. Returned pairs contain each
    /// unused incoming key and either its kept-out incoming state or displaced old
    /// state, in incoming order. No owned input is silently discarded.
    /// # Errors
    /// Duplicate incoming IDs reject the whole batch. The first resolver error
    /// returns every original input and leaves the collection unchanged. Caller
    /// external side effects are not rolled back.
    /// # Panics
    /// Panics if caller hashing/equality/resolution panics or revision allocation overflows.
    pub fn try_merge<E>(
        &mut self,
        inputs: Vec<(Id, State)>,
        mut resolve: impl FnMut(&Id, &State, &State) -> Result<KeyedMergeDecision, E>,
    ) -> Result<Vec<(Id, State)>, KeyedMutationError<Id, State, E>> {
        let mut ids = HashSet::with_capacity(inputs.len());
        let duplicate = inputs.iter().any(|(id, _)| !ids.insert(id));
        drop(ids);
        if duplicate {
            return Err(KeyedMutationError::new(inputs, MutationFailure::Duplicate));
        }
        let mut decisions = Vec::with_capacity(inputs.len());
        let mut conflicts = 0;
        for (id, incoming) in &inputs {
            let decision = match self.entries.get_full(id) {
                Some((index, captured_id, existing)) => {
                    match resolve(captured_id, &existing.state, incoming) {
                        Ok(decision) => {
                            conflicts += 1;
                            Some((index, decision))
                        }
                        Err(error) => {
                            return Err(KeyedMutationError::new(
                                inputs,
                                MutationFailure::Resolver(error),
                            ));
                        }
                    }
                }
                None => None,
            };
            decisions.push(decision);
        }
        let mut unused = Vec::with_capacity(conflicts);
        for ((id, state), decision) in inputs.into_iter().zip(decisions) {
            match decision {
                None => self.insert(id, state),
                Some((_, KeyedMergeDecision::KeepExisting)) => unused.push((id, state)),
                Some((index, KeyedMergeDecision::ReplaceWithIncoming)) => {
                    let (_, existing) = self
                        .get_index_mut(index)
                        .expect("staged index remains present");
                    unused.push((id, core::mem::replace(existing, state)));
                }
            }
        }
        Ok(unused)
    }

    /// Returns the captured identifier's current position, not child authority.
    #[must_use]
    pub fn index_of(&self, id: &Id) -> Option<usize> {
        self.entries.get_index_of(id)
    }

    /// Inserts a unique captured pair at a checked current position.
    ///
    /// # Errors
    /// Returns all inputs when the position exceeds `len()` or the identifier exists.
    /// # Panics
    /// Panics if identifier hashing/equality panics or private revision allocation overflows.
    pub fn try_insert_at(
        &mut self,
        index: usize,
        id: Id,
        state: State,
    ) -> Result<(), KeyedMutationError<Id, State>> {
        let input = (id, state);
        if index > self.len() {
            return Err(KeyedMutationError::new(
                vec![input],
                MutationFailure::InvalidPosition,
            ));
        }
        // Hash while the pair is still intact, including empty and singleton maps.
        // Validation unwind must drop the key before its state, as batch staging does.
        let hash = self.entries.hasher().hash_one(&input.0);
        match self
            .entries
            .raw_entry_mut_v1()
            .from_key_hashed_nocheck(hash, &input.0)
        {
            RawEntryMut::Occupied(_) => {
                return Err(KeyedMutationError::new(
                    vec![input],
                    MutationFailure::Duplicate,
                ));
            }
            RawEntryMut::Vacant(entry) => {
                // Validation is over; preserve ordinary insertion's owner-drop order on overflow.
                let (id, state) = input;
                let lineage = self.lineage.get_or_insert_with(|| Arc::new(Lineage::new()));
                let revision = lineage.allocate_revision();
                entry.insert_hashed_nocheck(hash, id, Entry { revision, state });
                self.structural_revision = revision;
            }
        }
        self.move_index(self.len() - 1, index);
        Ok(())
    }

    /// Inserts an entire unique batch at a checked current position.
    ///
    /// # Errors
    /// Invalid positions, duplicate batch identifiers and existing identifiers reject
    /// the whole batch before mutation, returning every input in original order.
    /// # Panics
    /// Panics if identifier hashing/equality panics or private revision allocation overflows.
    pub fn try_insert_many_at(
        &mut self,
        index: usize,
        inputs: Vec<(Id, State)>,
    ) -> Result<(), KeyedMutationError<Id, State>> {
        if index > self.len() {
            return Err(KeyedMutationError::new(
                inputs,
                MutationFailure::InvalidPosition,
            ));
        }
        let mut ids = HashSet::with_capacity(inputs.len());
        let duplicate = inputs
            .iter()
            .any(|(id, _)| !ids.insert(id) || self.contains_id(id));
        drop(ids);
        if duplicate {
            return Err(KeyedMutationError::new(inputs, MutationFailure::Duplicate));
        }
        for (offset, (id, state)) in inputs.into_iter().enumerate() {
            self.insert(id, state);
            self.move_index(self.len() - 1, index + offset);
        }
        Ok(())
    }

    /// Inserts a new pair or replaces existing state without changing its lifetime.
    ///
    /// Existing entries keep their captured identifier and position. Replacement
    /// returns the unused incoming identifier and displaced state. New entries
    /// use `index` and return `None`.
    /// # Errors
    /// An invalid insertion position rejects the owned pair even if its ID exists.
    /// # Panics
    /// Panics if identifier hashing/equality panics or private revision allocation overflows.
    pub fn try_upsert_at(
        &mut self,
        index: usize,
        id: Id,
        state: State,
    ) -> Result<Option<(Id, State)>, KeyedMutationError<Id, State>> {
        if index > self.len() {
            return Err(KeyedMutationError::new(
                vec![(id, state)],
                MutationFailure::InvalidPosition,
            ));
        }
        if let Some(existing) = self.get_mut(&id) {
            return Ok(Some((id, core::mem::replace(existing, state))));
        }
        self.try_insert_at(index, id, state)?;
        Ok(None)
    }
}

#[cfg(test)]
ordinary_test_items! {
mod tests {
    use super::{KeyedMergeDecision, KeyedState};

    #[test]
    fn positional_insert_reuses_validation_hash_and_collision_lookup() {
        use std::cell::Cell;
        use std::hash::{Hash, Hasher};
        use std::rc::Rc;

        struct Id {
            value: usize,
            hashes: Rc<Cell<usize>>,
            comparisons: Rc<Cell<usize>>,
        }
        impl PartialEq for Id {
            fn eq(&self, other: &Self) -> bool {
                self.comparisons.set(self.comparisons.get() + 1);
                self.value == other.value
            }
        }
        impl Eq for Id {}
        impl Hash for Id {
            fn hash<H: Hasher>(&self, state: &mut H) {
                self.hashes.set(self.hashes.get() + 1);
                0_u8.hash(state);
            }
        }

        let mut measurements = Vec::new();
        for count in [0, 1, 16] {
            let hashes = Rc::new(Cell::new(0));
            let comparisons = Rc::new(Cell::new(0));
            let id = |value| Id {
                value,
                hashes: Rc::clone(&hashes),
                comparisons: Rc::clone(&comparisons),
            };
            let mut states: KeyedState<_, _> = (0..count).map(|value| (id(value), value)).collect();
            hashes.set(0);
            comparisons.set(0);

            states.try_insert_at(0, id(count), count).unwrap();

            measurements.push((count, hashes.get(), comparisons.get()));
            assert_eq!(
                states.get_index(0).map(|(id, state)| (id.value, *state)),
                Some((count, count))
            );
            for value in 0..count {
                assert_eq!(
                    states
                        .get_index(value + 1)
                        .map(|(id, state)| (id.value, *state)),
                    Some((value, value))
                );
            }
        }
        eprintln!("positional insert (existing, Hash, Eq): {measurements:?}");
        for (count, hashes, comparisons) in measurements {
            assert_eq!(hashes, 1);
            assert_eq!(comparisons, count);
        }
    }

    #[test]
    fn shuffle_unwind_preserves_completed_swaps_and_exact_pair_owners() {
        use std::rc::Rc;

        let mut states: KeyedState<_, _> = (0_u8..4).map(|id| (id, Rc::new(id))).collect();
        let retained: Vec<_> = states
            .iter()
            .map(|(id, state)| {
                (
                    Rc::downgrade(state),
                    states.entry_stamp(id).unwrap().to_owned(),
                )
            })
            .collect();
        let structural = states.structural_stamp().to_owned();
        let mut calls = 0;
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            states.shuffle_with(|| {
                calls += 1;
                assert_ne!(calls, 2, "deliberate random-source unwind");
                0
            });
        }));
        outcome.expect_err("the second random word must unwind");
        assert_eq!(calls, 2);
        assert_eq!(states.ids().copied().collect::<Vec<_>>(), [3, 1, 2, 0]);
        assert!(!structural.matches(states.structural_stamp()));
        for (index, (id, state)) in states.iter().enumerate() {
            let (owner, stamp) = &retained[usize::from(*id)];
            assert_eq!(**state, *id);
            assert_eq!(Rc::as_ptr(state), owner.as_ptr());
            assert_eq!(Rc::strong_count(state), 1);
            assert_eq!(states.index_of(id), Some(index));
            assert!(Rc::ptr_eq(states.get(id).unwrap(), state));
            assert!(*stamp == states.entry_stamp(id).unwrap().to_owned());
        }
        drop(states);
        for (owner, _) in retained {
            assert!(owner.upgrade().is_none());
        }
    }

    #[test]
    fn all_small_permutations_preserve_pairs_lifetimes_and_exact_release() {
        use std::cell::Cell;
        use std::rc::Rc;
        use std::sync::atomic::Ordering::Relaxed;

        struct Owner {
            slot: usize,
            payload: Box<u8>,
            drops: Rc<Cell<[u8; 7]>>,
        }
        impl Drop for Owner {
            fn drop(&mut self) {
                let mut counts = self.drops.get();
                counts[self.slot] += 1;
                self.drops.set(counts);
            }
        }
        fn visit(order: &mut [usize], at: usize, check: &mut impl FnMut(&[usize])) {
            if at == order.len() {
                check(order);
                return;
            }
            for next in at..order.len() {
                order.swap(at, next);
                visit(order, at + 1, check);
                order.swap(at, next);
            }
        }

        // The wanted ID list defines the result, independently of cycle application.
        // Wrong cycle direction, stale lookup, extra revision, or lost ownership fails.
        let payloads = [11, 23, 37, 53, 71, 89, 107];
        for (count, expected_cases) in [1, 1, 2, 6, 24, 120, 720, 5_040].into_iter().enumerate() {
            let mut seen = 0;
            let mut order: Vec<_> = (0..count).collect();
            visit(&mut order, 0, &mut |wanted| {
                let drops = Rc::new(Cell::new([0; 7]));
                let mut states: KeyedState<_, _> = (0..count)
                    .map(|id| {
                        (
                            id,
                            Owner {
                                slot: id,
                                payload: Box::new(payloads[id]),
                                drops: Rc::clone(&drops),
                            },
                        )
                    })
                    .collect();
                let before = states.structural_stamp().to_owned();
                let next_before = states
                    .lineage
                    .as_ref()
                    .map(|lineage| lineage.next_revision.load(Relaxed));
                let retained: Vec<_> = states
                    .iter()
                    .map(|(id, owner)| {
                        (
                            std::ptr::from_ref(&*owner.payload),
                            states.entry_stamp(id).unwrap().to_owned(),
                        )
                    })
                    .collect();

                // Identity scratch also exercises the previous implementation for baseline proof.
                states.apply_order(wanted.to_vec(), (0..count).collect());

                assert_eq!(states.ids().copied().collect::<Vec<_>>(), wanted);
                let changed = wanted.iter().copied().ne(0..count);
                assert_eq!(before.matches(states.structural_stamp()), !changed);
                assert_eq!(
                    states
                        .lineage
                        .as_ref()
                        .map(|lineage| lineage.next_revision.load(Relaxed)),
                    next_before.map(|revision| revision + u64::from(changed))
                );
                for (index, (id, owner)) in states.iter().enumerate() {
                    assert_eq!(owner.slot, *id);
                    assert_eq!(*owner.payload, payloads[*id]);
                    assert_eq!(std::ptr::from_ref(&*owner.payload), retained[*id].0);
                    assert!(retained[*id].1 == states.entry_stamp(id).unwrap().to_owned());
                    assert_eq!(states.index_of(id), Some(index));
                    assert!(std::ptr::eq(states.get(id).unwrap(), owner));
                    assert!(std::ptr::eq(states.get_index(index).unwrap().1, owner));
                }
                assert_eq!(drops.get(), [0; 7]);
                drop(states);
                let mut expected_drops = [0; 7];
                expected_drops[..count].fill(1);
                assert_eq!(drops.get(), expected_drops);
                seen += 1;
            });
            assert_eq!(seen, expected_cases);
        }
    }

    #[test]
    fn sort_scratch_reuse_preserves_current_comparison_and_tie_order() {
        // Characterise this implementation, without adding a public stability guarantee.
        let mut states: KeyedState<_, _> =
            [(1, 30), (2, 10), (3, 30), (4, 10)].into_iter().collect();
        let mut calls = Vec::new();
        states
            .try_sort_by(|a, left, b, right| {
                calls.push((*a, *b));
                Ok::<_, ()>(left.cmp(right))
            })
            .unwrap();
        assert_eq!(calls, [(1, 2), (3, 4), (2, 4), (1, 4), (1, 3)]);
        assert_eq!(states.ids().copied().collect::<Vec<_>>(), [2, 4, 1, 3]);
    }

    #[test]
    fn sort_scratch_failure_preserves_exact_order_and_revision() {
        use std::sync::atomic::Ordering::Relaxed;
        for unwind in [false, true] {
            let mut states: KeyedState<_, _> =
                [(1, 30), (2, 10), (3, 30), (4, 10)].into_iter().collect();
            let original = states.clone();
            let before = states.structural_stamp().to_owned();
            states.lineage().next_revision.store(u64::MAX, Relaxed);
            let mut calls = 0;
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                states.try_sort_by(|_, left, _, right| {
                    calls += 1;
                    if calls == 4 {
                        assert!(!unwind, "deliberate comparator unwind");
                        return Err("comparator stopped");
                    }
                    Ok(left.cmp(right))
                })
            }));
            if unwind {
                let error = result.unwrap_err();
                assert_eq!(
                    error.downcast_ref::<&str>(),
                    Some(&"deliberate comparator unwind")
                );
            } else {
                assert_eq!(result.unwrap(), Err("comparator stopped"));
            }
            assert_eq!(calls, 4);
            assert_eq!(states, original);
            assert_eq!(states.ids().copied().collect::<Vec<_>>(), [1, 2, 3, 4]);
            assert!(before.matches(states.structural_stamp()));
            assert_eq!(states.lineage().next_revision.load(Relaxed), u64::MAX);
            for (index, (id, stamp)) in original.entry_stamps().enumerate() {
                assert_eq!(states.index_of(id), Some(index));
                assert!(stamp.to_owned() == states.entry_stamp(id).unwrap().to_owned());
            }
        }
    }

    #[test]
    fn sort_and_move_share_unchanged_revision_exhaustion_boundaries() {
        use std::sync::atomic::Ordering::Relaxed;
        for sort in [false, true] {
            for changed in [false, true] {
                for revision in [u64::MAX - 1, u64::MAX] {
                    let mut states: KeyedState<_, _> = (0..4).map(|id| (id, id)).collect();
                    let original = states.clone();
                    let before = states.structural_stamp().to_owned();
                    states.lineage().next_revision.store(revision, Relaxed);
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        if sort {
                            states
                                .try_sort_by(|_, left, _, right| {
                                    Ok::<_, ()>(if changed {
                                        right.cmp(left)
                                    } else {
                                        left.cmp(right)
                                    })
                                })
                                .unwrap();
                        } else if changed {
                            states.try_move_indices(&[0, 2], 4).unwrap();
                        } else {
                            states.try_move_indices(&[1], 2).unwrap();
                        }
                    }));
                    let applied = changed && revision != u64::MAX;
                    if changed && !applied {
                        let error = result.unwrap_err();
                        assert_eq!(
                            error.downcast_ref::<&str>(),
                            Some(&"KeyedState entry revision overflow")
                        );
                    } else {
                        result.unwrap();
                    }
                    let expected = if !applied {
                        [0, 1, 2, 3]
                    } else if sort {
                        [3, 2, 1, 0]
                    } else {
                        [1, 3, 0, 2]
                    };
                    assert_eq!(states.ids().copied().collect::<Vec<_>>(), expected);
                    assert_eq!(before.matches(states.structural_stamp()), !applied);
                    assert_eq!(
                        states.lineage().next_revision.load(Relaxed),
                        revision + u64::from(applied)
                    );
                    for (index, (id, state)) in states.iter().enumerate() {
                        assert_eq!(states.index_of(id), Some(index));
                        assert_eq!(state, id);
                        assert!(
                            original.entry_stamp(id).unwrap().to_owned()
                                == states.entry_stamp(id).unwrap().to_owned()
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn positional_insert_revision_exhaustion_preserves_append_then_move_order() {
        for next_revision in [u64::MAX - 1, u64::MAX] {
            for index in [0, 1, 2] {
                let mut states: KeyedState<_, _> = [(1, 10), (2, 20)].into_iter().collect();
                let before = states.structural_stamp().to_owned();
                let retained = states.entry_stamp(&1).unwrap().to_owned();
                states
                    .lineage()
                    .next_revision
                    .store(next_revision, std::sync::atomic::Ordering::Relaxed);
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    states.try_insert_at(index, 3, 30).unwrap();
                }));
                if next_revision == u64::MAX {
                    result.unwrap_err();
                    assert_eq!(states.len(), 2);
                    assert!(before.matches(states.structural_stamp()));
                } else {
                    if index == 2 {
                        result.unwrap();
                    } else {
                        result.unwrap_err();
                    }
                    assert_eq!(states.len(), 3);
                    assert_eq!(states.get_index(2), Some((&3, &30)));
                    assert!(!before.matches(states.structural_stamp()));
                }
                assert_eq!(states.get_index(0), Some((&1, &10)));
                assert_eq!(states.get_index(1), Some((&2, &20)));
                assert!(retained == states.entry_stamp(&1).unwrap().to_owned());
            }
        }
    }

    #[test]
    fn positional_insert_preserves_validation_and_revision_drop_order() {
        use std::cell::{Cell, RefCell};
        use std::hash::{Hash, Hasher};
        use std::rc::Rc;

        struct Owned {
            label: &'static str,
            panic_at: Rc<Cell<u8>>,
            drops: Rc<RefCell<Vec<&'static str>>>,
        }
        impl PartialEq for Owned {
            fn eq(&self, other: &Self) -> bool {
                assert_ne!(self.panic_at.get(), 2, "deliberate equality unwind");
                self.label == other.label
            }
        }
        impl Eq for Owned {}
        impl Hash for Owned {
            fn hash<H: Hasher>(&self, state: &mut H) {
                assert_ne!(self.panic_at.get(), 1, "deliberate hash unwind");
                0_u8.hash(state);
            }
        }
        impl Drop for Owned {
            fn drop(&mut self) {
                self.drops.borrow_mut().push(self.label);
            }
        }

        for (index, panic_at) in [(2, 1), (1, 1), (1, 2), (1, 3)] {
            let mode = Rc::new(Cell::new(0));
            let drops = Rc::new(RefCell::new(Vec::new()));
            let owned = |label| Owned {
                label,
                panic_at: Rc::clone(&mode),
                drops: Rc::clone(&drops),
            };
            let mut states = KeyedState::new();
            states.insert(owned("retained-key"), owned("retained-state"));
            let before = states.structural_stamp().to_owned();
            mode.set(panic_at);
            if panic_at == 3 {
                states
                    .lineage()
                    .next_revision
                    .store(u64::MAX, std::sync::atomic::Ordering::Relaxed);
            }
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                states.try_insert_at(index, owned("input-key"), owned("input-state"))
            }));
            if index == 2 {
                let error = result.unwrap().unwrap_err();
                assert!(error.is_invalid_position());
                assert!(drops.borrow().is_empty());
                let inputs = error.into_inputs();
                assert_eq!(inputs.len(), 1);
                assert_eq!(inputs[0].0.label, "input-key");
                assert_eq!(inputs[0].1.label, "input-state");
                drop(inputs);
            } else {
                result.unwrap_err();
            }
            let expected = if panic_at == 3 {
                ["input-state", "input-key"]
            } else {
                ["input-key", "input-state"]
            };
            assert_eq!(*drops.borrow(), expected);
            assert_eq!(states.len(), 1);
            assert!(before.matches(states.structural_stamp()));
            drop(states);
            assert_eq!(drops.borrow().len(), 4);
        }
    }

    // This deliberately violates Eq consistency to reach the defensive duplicate
    // recheck. It is a logic-error robustness witness, not a supported key contract.
    #[test]
    fn upsert_inconsistent_equality_returns_exact_inputs_without_mutation() {
        use std::cell::Cell;
        use std::hash::{Hash, Hasher};
        use std::ptr;
        use std::rc::Rc;
        use std::sync::atomic::Ordering::Relaxed;

        struct Owned {
            value: Box<u8>,
            slot: usize,
            drops: Rc<Cell<[u8; 4]>>,
        }
        impl Drop for Owned {
            fn drop(&mut self) {
                let mut drops = self.drops.get();
                drops[self.slot] += 1;
                self.drops.set(drops);
            }
        }
        struct Id {
            owner: Owned,
            miss_once: Rc<Cell<bool>>,
            comparisons: Rc<Cell<usize>>,
        }
        impl PartialEq for Id {
            fn eq(&self, other: &Self) -> bool {
                self.comparisons.set(self.comparisons.get() + 1);
                !self.miss_once.replace(false) && self.owner.value == other.owner.value
            }
        }
        impl Eq for Id {}
        impl Hash for Id {
            fn hash<H: Hasher>(&self, state: &mut H) {
                self.owner.value.hash(state);
            }
        }

        let drops = Rc::new(Cell::new([0; 4]));
        let miss_once = Rc::new(Cell::new(false));
        let comparisons = Rc::new(Cell::new(0));
        let owned = |slot, value| Owned {
            value: Box::new(value),
            slot,
            drops: Rc::clone(&drops),
        };
        let key = |slot| Id {
            owner: owned(slot, 7),
            miss_once: Rc::clone(&miss_once),
            comparisons: Rc::clone(&comparisons),
        };
        let retained_key = key(0);
        let retained_state = owned(1, 10);
        let incoming_key = key(2);
        let incoming_state = owned(3, 20);
        let retained_key_ptr = ptr::from_ref(&*retained_key.owner.value);
        let retained_state_ptr = ptr::from_ref(&*retained_state.value);
        let incoming_key_ptr = ptr::from_ref(&*incoming_key.owner.value);
        let incoming_state_ptr = ptr::from_ref(&*incoming_state.value);
        let mut states = KeyedState::new();
        states.insert(retained_key, retained_state);
        let structural = states.structural_stamp().to_owned();
        let entry = states.entry_stamps().next().unwrap().1.to_owned();
        let next_revision = states.lineage().next_revision.load(Relaxed);

        // The singleton lookup misses; the validation lookup sees the same key.
        comparisons.set(0);
        miss_once.set(true);
        let error = states
            .try_upsert_at(0, incoming_key, incoming_state)
            .err()
            .expect("inconsistent equality must reach the duplicate recheck");
        assert_eq!(comparisons.get(), 2);
        assert!(error.is_duplicate());
        assert_eq!(drops.get(), [0; 4]);
        assert_eq!(states.len(), 1);
        assert!(structural.matches(states.structural_stamp()));
        assert!(entry == states.entry_stamps().next().unwrap().1.to_owned());
        assert_eq!(states.lineage().next_revision.load(Relaxed), next_revision);
        let (retained_key, retained_state) = states.get_index(0).unwrap();
        assert_eq!(ptr::from_ref(&*retained_key.owner.value), retained_key_ptr);
        assert_eq!(ptr::from_ref(&*retained_state.value), retained_state_ptr);
        assert_eq!(*retained_state.value, 10);

        let inputs = error.into_inputs();
        assert_eq!(inputs.len(), 1);
        assert_eq!(ptr::from_ref(&*inputs[0].0.owner.value), incoming_key_ptr);
        assert_eq!(ptr::from_ref(&*inputs[0].1.value), incoming_state_ptr);
        assert_eq!(*inputs[0].1.value, 20);
        assert_eq!(states.get(&inputs[0].0).map(|state| *state.value), Some(10));
        assert_eq!(drops.get(), [0; 4]);
        drop(inputs);
        assert_eq!(drops.get(), [0, 0, 1, 1]);
        drop(states);
        assert_eq!(drops.get(), [1; 4]);
    }

    #[test]
    fn pop_changes_structure_only_when_an_entry_is_removed() {
        let mut states: KeyedState<_, _> = [(1, 10), (2, 20)].into_iter().collect();
        let before = states.structural_stamp().to_owned();
        let retained = states.entry_stamp(&1).unwrap().to_owned();
        assert_eq!(states.pop(), Some((2, 20)));
        assert!(!before.matches(states.structural_stamp()));
        assert!(retained == states.entry_stamp(&1).unwrap().to_owned());
        assert_eq!(states.pop(), Some((1, 10)));
        let empty = states.structural_stamp().to_owned();
        assert!(states.pop().is_none());
        assert!(empty.matches(states.structural_stamp()));
    }

    #[test]
    fn pop_revision_exhaustion_preserves_owned_pairs() {
        let mut states: KeyedState<_, _> = [(1, 10), (2, 20)].into_iter().collect();
        let before = states.structural_stamp().to_owned();
        states
            .lineage()
            .next_revision
            .store(u64::MAX, std::sync::atomic::Ordering::Relaxed);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| states.pop()));
        result.unwrap_err();
        assert!(before.matches(states.structural_stamp()));
        assert_eq!(
            states
                .iter()
                .map(|(id, value)| (*id, *value))
                .collect::<Vec<_>>(),
            [(1, 10), (2, 20)]
        );
    }

    #[test]
    fn empty_moves_validate_destination_without_changing_entry_authority() {
        let mut states: KeyedState<_, _> = [(1, 10), (2, 20), (3, 30)].into_iter().collect();
        let original = states.clone();
        let structural = states.structural_stamp().to_owned();
        for destination in [0, 1, 3] {
            states.try_move_indices(&[], destination).unwrap();
        }
        for destination in [4, usize::MAX] {
            assert!(states.try_move_indices(&[], destination).is_err());
        }
        assert_eq!(states, original);
        assert!(structural.matches(states.structural_stamp()));
        for (id, stamp) in original.entry_stamps() {
            assert!(stamp.to_owned() == states.entry_stamp(id).unwrap().to_owned());
        }

        let mut empty = KeyedState::<u8, u8>::new();
        empty.try_move_indices(&[], 0).unwrap();
        assert!(empty.try_move_indices(&[], 1).is_err());
        assert!(empty.is_empty());
    }

    #[test]
    fn merge_resolver_unwind_drops_inputs_without_changing_existing_pairs() {
        struct MoveOnly {
            value: u8,
            drops: std::rc::Rc<std::cell::Cell<u8>>,
        }
        impl Drop for MoveOnly {
            fn drop(&mut self) {
                self.drops.set(self.drops.get() + 1);
            }
        }
        let drops = std::rc::Rc::new(std::cell::Cell::new(0));
        let value = |value| MoveOnly {
            value,
            drops: std::rc::Rc::clone(&drops),
        };
        let mut states: KeyedState<_, _> = [(1, value(10)), (2, value(20))].into_iter().collect();
        let before = states.structural_stamp().to_owned();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = states.try_merge(
                vec![(1, value(11)), (3, value(30)), (2, value(22))],
                |id, _, _| {
                    assert_ne!(*id, 2, "deliberate resolver unwind");
                    Ok::<_, ()>(KeyedMergeDecision::ReplaceWithIncoming)
                },
            );
        }));
        result.unwrap_err();
        assert_eq!(drops.get(), 3);
        assert!(before.matches(states.structural_stamp()));
        assert_eq!(
            states.get_index(0).map(|(id, value)| (*id, value.value)),
            Some((1, 10))
        );
        assert_eq!(
            states.get_index(1).map(|(id, value)| (*id, value.value)),
            Some((2, 20))
        );
        assert!(states.get(&3).is_none());
        drop(states);
        assert_eq!(drops.get(), 5);
    }

    #[test]
    fn d36_failures_preserve_private_entry_authority() {
        let mut states: KeyedState<_, _> = [(1, 10), (2, 20), (3, 30)].into_iter().collect();
        let original = states.clone();
        let structural = states.structural_stamp().to_owned();
        assert!(states.try_reserve(usize::MAX).is_err());
        assert!(structural.matches(states.structural_stamp()));
        states
            .try_merge(vec![(1, 11), (2, 22)], |id, _, _| {
                if *id == 2 {
                    Err(())
                } else {
                    Ok(KeyedMergeDecision::ReplaceWithIncoming)
                }
            })
            .unwrap_err();
        assert!(structural.matches(states.structural_stamp()));
        assert_eq!(states, original);
        assert!(states.try_sort_by(|_, _, _, _| Err(())).is_err());
        states
            .try_partition(|id, _| if *id == 3 { Err(()) } else { Ok(*id == 2) })
            .unwrap_err();
        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = states.try_partition(|id, _| {
                assert!(*id != 3, "deliberate partition unwind");
                Ok::<_, ()>(*id == 1)
            });
        }));
        assert!(unwind.is_err());
        for (id, stamp) in original.entry_stamps() {
            assert!(stamp.to_owned() == states.entry_stamp(id).unwrap().to_owned());
        }
    }

    #[test]
    fn d36_unwinding_sort_retains_lookup_and_entry_stamps() {
        let mut states: KeyedState<_, _> = (0..40)
            .map(|offset| {
                let id = offset * 17 % 40;
                (id, id)
            })
            .collect();
        let stamps: Vec<_> = states
            .entry_stamps()
            .map(|(id, stamp)| (*id, stamp.to_owned()))
            .collect();
        let mut calls = 0;
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = states.try_sort_by(|_, a, _, b| {
                calls += 1;
                assert!(calls != 20, "deliberate comparator unwind");
                Ok::<_, ()>(a.cmp(b))
            });
        }));
        assert!(outcome.is_err());
        for (index, (id, state)) in states.iter().enumerate() {
            assert_eq!(states.index_of(id), Some(index));
            assert_eq!(states.get(id), Some(state));
        }
        for (id, before) in stamps {
            assert!(before == states.entry_stamp(&id).unwrap().to_owned());
        }
    }
}
}

#[cfg(all(test, feature = "allocation-measurement"))]
pub(crate) use allocation_tests::{
    merge_result_capacity_tracks_conflicts_instead_of_all_inputs,
    moves_bound_permutation_allocations, sort_reuses_completed_scratch_storage,
};

#[cfg(all(test, feature = "allocation-measurement"))]
mod allocation_tests {
    use super::{KeyedMergeDecision, KeyedState};

    pub(crate) fn sort_reuses_completed_scratch_storage() {
        if crate::allocation_test::run_in_child(
            "composition::keyed::operations::tests::sort_reuses_completed_scratch_storage",
        ) {
            return;
        }
        let mut measurements = Vec::new();
        for count in [0_usize, 1, 2, 16, 1_024] {
            for shape in ["sorted", "reversed", "ties"] {
                let mut states: KeyedState<_, _> = (0..count)
                    .map(|id| {
                        let value = match shape {
                            "sorted" => id,
                            "reversed" => count - id,
                            _ => id % 3,
                        };
                        (id, value)
                    })
                    .collect();
                let mut comparisons = 0;
                let profiler = dhat::Profiler::builder().testing().build();
                std::hint::black_box(&mut states)
                    .try_sort_by(|_, left, _, right| {
                        comparisons += 1;
                        Ok::<_, ()>(left.cmp(right))
                    })
                    .unwrap();
                let heap = dhat::HeapStats::get();
                drop(profiler);
                assert_eq!(states.len(), count);
                assert!(states.iter().map(|(_, value)| value).is_sorted());
                for (index, (id, value)) in states.iter().enumerate() {
                    assert_eq!(states.index_of(id), Some(index));
                    assert_eq!(states.get(id), Some(value));
                }
                measurements.push((shape, count, comparisons, heap));
            }
        }
        for (shape, count, comparisons, heap) in &measurements {
            eprintln!(
                "keyed_sort/{shape}/{count}: comparisons={comparisons} blocks={} bytes={} peak_bytes={} retained_bytes={}",
                heap.total_blocks, heap.total_bytes, heap.max_bytes, heap.curr_bytes,
            );
        }
        for (_, count, comparisons, heap) in measurements {
            let buffers = if count < 2 { 0 } else { 2 };
            let bytes = buffers * count * size_of::<usize>();
            assert_eq!(heap.total_blocks, u64::try_from(buffers).unwrap());
            assert_eq!(heap.total_bytes, u64::try_from(bytes).unwrap());
            assert_eq!(heap.max_bytes, bytes);
            assert_eq!(heap.curr_blocks, 0);
            assert_eq!(heap.curr_bytes, 0);
            assert_eq!(comparisons == 0, count < 2);
        }
    }

    #[cfg(feature = "allocation-measurement")]
    pub(crate) fn moves_bound_permutation_allocations() {
        if crate::allocation_test::run_in_child(
            "composition::keyed::operations::tests::moves_bound_permutation_allocations",
        ) {
            return;
        }
        let mut states: KeyedState<_, _> = (0..1_024).map(|id| (id, id)).collect();
        let profiler = dhat::Profiler::builder().testing().build();
        let before = dhat::HeapStats::get();
        for destination in [0, 512, 1_024] {
            std::hint::black_box(&mut states)
                .try_move_indices(&[], std::hint::black_box(destination))
                .unwrap();
        }
        let after = dhat::HeapStats::get();
        let blocks = after.total_blocks - before.total_blocks;
        let bytes = after.total_bytes - before.total_bytes;
        drop(profiler);
        eprintln!("three empty moves over 1024 entries: {blocks} allocations, {bytes} bytes");
        assert_eq!(blocks, 0);
        assert_eq!(bytes, 0);
        assert_eq!(states.len(), 1_024);

        let mut measurements = Vec::new();
        for count in [1, 16, 1_024] {
            for shape in ["single", "half", "all"] {
                let sources: Vec<_> = match shape {
                    "single" => vec![count / 2],
                    "half" => (0..count).step_by(2).collect(),
                    _ => (0..count).collect(),
                };
                for destination in [0, count / 2, count] {
                    let mut states: KeyedState<_, _> = (0..count).map(|id| (id, id)).collect();
                    let mut expected: Vec<_> =
                        (0..count).filter(|id| !sources.contains(id)).collect();
                    let insertion =
                        destination - sources.iter().filter(|id| **id < destination).count();
                    expected.splice(insertion..insertion, sources.iter().copied());

                    let profiler = dhat::Profiler::builder().testing().build();
                    std::hint::black_box(&mut states)
                        .try_move_indices(std::hint::black_box(&sources), destination)
                        .unwrap();
                    let heap = dhat::HeapStats::get();
                    drop(profiler);

                    assert_eq!(states.ids().copied().collect::<Vec<_>>(), expected);
                    for (index, (id, state)) in states.iter().enumerate() {
                        assert_eq!(*id, *state);
                        assert_eq!(states.index_of(id), Some(index));
                    }
                    assert_eq!(heap.curr_blocks, 0);
                    measurements.push((count, shape, sources.len(), destination, heap));
                }
            }
        }
        for (count, shape, sources, destination, heap) in measurements {
            eprintln!(
                "keyed_move/{count}/{shape}/{destination}: sources={sources} blocks={} bytes={} peak_bytes={}",
                heap.total_blocks, heap.total_bytes, heap.max_bytes
            );
            assert_eq!(heap.total_blocks, 3);
            assert_eq!(
                heap.total_bytes,
                u64::try_from((sources + 2 * count) * size_of::<usize>()).unwrap()
            );
            assert_eq!(heap.max_bytes, 2 * count * size_of::<usize>());
        }
    }

    #[cfg(feature = "allocation-measurement")]
    pub(crate) fn merge_result_capacity_tracks_conflicts_instead_of_all_inputs() {
        if crate::allocation_test::run_in_child(
            "composition::keyed::operations::tests::merge_result_capacity_tracks_conflicts_instead_of_all_inputs",
        ) {
            return;
        }
        let mut measurements = Vec::new();
        for count in [0_u64, 1, 1_024] {
            for (label, conflicts) in [("new", 0), ("mixed", count / 2), ("conflict", count)] {
                let mut states = KeyedState::with_capacity(
                    usize::try_from(count + 1).expect("fixture capacity fits usize"),
                );
                // Reserve and initialise lineage before measuring the merge itself.
                states.insert(u64::MAX, 99_u64);
                for id in 0..conflicts {
                    states.insert(id, 10);
                }
                let inputs = (0..count).map(|id| (id, 20)).collect();
                let mut resolver_calls = 0;
                let profiler = dhat::Profiler::builder().testing().build();
                let unused = std::hint::black_box(&mut states)
                    .try_merge(std::hint::black_box(inputs), |_, old, new| {
                        resolver_calls += 1;
                        assert_eq!(*old, 10);
                        assert_eq!(*new, 20);
                        Ok::<_, ()>(KeyedMergeDecision::ReplaceWithIncoming)
                    })
                    .unwrap();
                let heap = dhat::HeapStats::get();
                drop(profiler);
                let conflicts = usize::try_from(conflicts).expect("fixture length fits usize");
                assert_eq!(resolver_calls, conflicts);
                assert_eq!(unused.len(), conflicts);
                for (index, (id, value)) in unused.iter().enumerate() {
                    assert_eq!(*id, u64::try_from(index).expect("fixture index fits u64"));
                    assert_eq!(*value, 10);
                }
                for id in 0..count {
                    assert_eq!(states.get(&id), Some(&20));
                }
                measurements.push((label, count, conflicts, unused.capacity(), heap));
            }
        }
        for (label, count, conflicts, capacity, stats) in &measurements {
            eprintln!(
                "merge/{label}/{count}: conflicts={conflicts} capacity={capacity} blocks={} bytes={} retained_blocks={} retained_bytes={} peak_blocks={} peak_bytes={}",
                stats.total_blocks,
                stats.total_bytes,
                stats.curr_blocks,
                stats.curr_bytes,
                stats.max_blocks,
                stats.max_bytes,
            );
        }
        for (_, _, conflicts, capacity, stats) in measurements {
            assert_eq!(capacity, conflicts);
            assert_eq!(stats.curr_blocks, usize::from(conflicts != 0));
            assert_eq!(stats.curr_bytes, conflicts * size_of::<(u64, u64)>());
        }
    }
}

#[cfg(test)]
ordinary_test_items! {
mod partition_tests {
    use std::rc::Rc;
    use std::sync::atomic::Ordering::Relaxed;

    use super::KeyedState;

    #[test]
    fn partition_without_swaps_preserves_pairs_and_revision() {
        for (selected_prefix, expected_boundary) in [(2, 2), (3, 3), (0, 0)] {
            let mut states: KeyedState<_, _> = [(1_u8, 10_u8), (2, 20), (3, 30)]
                .into_iter()
                .map(|(id, value)| (Rc::new(id), Rc::new(value)))
                .collect();
            let structural = states.structural_stamp().to_owned();
            let next_revision = states.lineage().next_revision.load(Relaxed);
            let retained: Vec<_> = states
                .iter()
                .map(|(id, value)| {
                    (
                        Rc::downgrade(id),
                        Rc::downgrade(value),
                        states.entry_stamp(id).unwrap().to_owned(),
                    )
                })
                .collect();
            let mut visited = Vec::new();

            let boundary = states
                .try_partition(|id, value| {
                    visited.push((**id, **value));
                    Ok::<_, ()>(**id <= selected_prefix)
                })
                .unwrap();

            assert_eq!(boundary, expected_boundary);
            assert_eq!(visited, [(1, 10), (2, 20), (3, 30)]);
            assert_eq!(
                states
                    .iter()
                    .map(|(id, value)| (**id, **value))
                    .collect::<Vec<_>>(),
                [(1, 10), (2, 20), (3, 30)]
            );
            assert!(structural.matches(states.structural_stamp()));
            assert_eq!(states.lineage().next_revision.load(Relaxed), next_revision);
            for (index, (id, value)) in states.iter().enumerate() {
                let (key_owner, value_owner, entry) = &retained[index];
                assert_eq!(Rc::as_ptr(id), key_owner.as_ptr());
                assert_eq!(Rc::as_ptr(value), value_owner.as_ptr());
                assert_eq!(Rc::strong_count(id), 1);
                assert_eq!(Rc::strong_count(value), 1);
                assert_eq!(states.index_of(id), Some(index));
                assert!(Rc::ptr_eq(states.get(id).unwrap(), value));
                let (indexed_id, indexed_value) = states.get_index(index).unwrap();
                assert!(Rc::ptr_eq(indexed_id, id));
                assert!(Rc::ptr_eq(indexed_value, value));
                assert!(*entry == states.entry_stamp(id).unwrap().to_owned());
            }
            drop(states);
            for (key_owner, value_owner, _) in retained {
                assert!(key_owner.upgrade().is_none());
                assert!(value_owner.upgrade().is_none());
            }
        }
    }
}
}
