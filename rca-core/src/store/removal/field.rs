//! Concrete owning paths; captured identity never replaces live scope checks.

use crate::composition::EntryStamp;
use crate::store::task_context::TaskAccessReason;
use std::hash::Hash;
use std::sync::Arc;

/// Extracts one owned child after selecting its actual mutable container.
pub(super) trait OwnedField<S, T> {
    /// Runs `before` only while selection, applicable identity and admission permit removal.
    ///
    /// The callback may mutate the child but cannot remove its container entry.
    /// Closure checks follow user selection, keyed lookup and callback boundaries.
    /// An entered callback is not rolled back if admission then closes.
    fn take(
        &self,
        state: &mut S,
        before: &mut dyn FnMut(&mut T),
        live: &dyn Fn() -> bool,
    ) -> Result<T, TaskAccessReason>;
}

/// Selects an owning optional field under separately checked child authority.
///
/// The public declaration's path law couples this field to the read selector
/// and declared identity. Selection borrows the field without replacing its
/// contents; the exclusive borrow then protects extraction through `before`.
pub(super) struct OptionalField<S, T>(
    /// Returns the actual field that remains exclusively borrowed through extraction.
    pub(super) for<'a> fn(&'a mut S) -> &'a mut Option<T>,
);

impl<S, T> OwnedField<S, T> for OptionalField<S, T> {
    fn take(
        &self,
        state: &mut S,
        before: &mut dyn FnMut(&mut T),
        live: &dyn Fn() -> bool,
    ) -> Result<T, TaskAccessReason> {
        let field = (self.0)(state);
        if !live() {
            return Err(TaskAccessReason::StoreClosed);
        }
        let Some(value) = field.as_mut() else {
            return Err(TaskAccessReason::ScopeEnded);
        };
        before(value);
        if !live() {
            return Err(TaskAccessReason::StoreClosed);
        }
        Ok(field
            .take()
            .expect("borrowed owning field remains occupied"))
    }
}

/// Removes only the captured insertion lifetime of one keyed entry.
pub(super) struct KeyedField<S, K, T, H = Arc<K>> {
    /// Selects the actual mutable collection once for this extraction.
    pub(super) write: for<'a> fn(&'a mut S) -> &'a mut crate::KeyedState<K, T>,
    /// Retains the lookup key without requiring the child value to be cloneable.
    pub(super) key: H,
    /// Distinguishes removal and reinsertion even when the key is unchanged.
    pub(super) stamp: EntryStamp,
}

/// Removes the captured stack entry only while it remains the final element.
pub(super) struct StackField<S, T, H = Arc<crate::StackId>> {
    /// Selects the actual mutable stack once for this extraction.
    pub(super) write: for<'a> fn(&'a mut S) -> &'a mut crate::StackState<T>,
    /// Retains the native stack ID independently of the child value.
    pub(super) key: H,
    /// Distinguishes replacement of the same stack ID from its original insertion.
    pub(super) stamp: EntryStamp,
}

impl<S, T, H: std::ops::Deref<Target = crate::StackId>> OwnedField<S, T> for StackField<S, T, H> {
    fn take(
        &self,
        state: &mut S,
        before: &mut dyn FnMut(&mut T),
        live: &dyn Fn() -> bool,
    ) -> Result<T, TaskAccessReason> {
        let stack = (self.write)(state);
        if !live() {
            return Err(TaskAccessReason::StoreClosed);
        }
        if !stack
            .entries()
            .entry_stamp(&self.key)
            .is_some_and(|stamp| self.stamp.matches(stamp))
        {
            return Err(TaskAccessReason::ScopeEnded);
        }
        if stack.ids().last() != Some(&*self.key) {
            return Err(TaskAccessReason::InvalidTarget);
        }
        before(
            stack
                .get_mut(&self.key)
                .expect("validated final entry remains occupied"),
        );
        if !live() {
            return Err(TaskAccessReason::StoreClosed);
        }
        Ok(stack
            .pop()
            .expect("borrowed final entry remains occupied")
            .1)
    }
}

impl<S, K: Eq + Hash, T, H: std::ops::Deref<Target = K>> OwnedField<S, T>
    for KeyedField<S, K, T, H>
{
    fn take(
        &self,
        state: &mut S,
        before: &mut dyn FnMut(&mut T),
        live: &dyn Fn() -> bool,
    ) -> Result<T, TaskAccessReason> {
        let entries = (self.write)(state);
        if !live() {
            return Err(TaskAccessReason::StoreClosed);
        }
        let matches = entries
            .entry_stamp(&self.key)
            .is_some_and(|stamp| self.stamp.matches(stamp));
        if !live() {
            return Err(TaskAccessReason::StoreClosed);
        }
        if !matches {
            return Err(TaskAccessReason::ScopeEnded);
        }
        let value = entries
            .get_mut(&self.key)
            .expect("validated keyed entry remains occupied");
        if !live() {
            return Err(TaskAccessReason::StoreClosed);
        }
        before(value);
        if !live() {
            return Err(TaskAccessReason::StoreClosed);
        }
        Ok(entries
            .remove(&self.key)
            .expect("borrowed keyed entry remains occupied"))
    }
}
