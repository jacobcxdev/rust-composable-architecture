//! Selection metadata for mounted state, separate from runtime authority.

use super::OptionalPath;
use crate::KeyedState;
use std::hash::Hash;
use std::ops::Deref;

/// Selects state for the duration of a supplied root borrow.
///
/// A path does not establish an installed child's lifetime. Callers check route
/// authority separately. Selectors can change external state or panic, so
/// separate calls do not promise the same selection or a stable address.
pub(in crate::store) trait BorrowPath<R>: Clone {
    /// Selected state, without ownership or thread-transfer requirements here.
    type State;

    /// Selects shared state, or returns `None` when this route is absent.
    fn read<'a>(&self, root: &'a R) -> Option<&'a Self::State>;

    /// Selects state with liveness checks between separate structural operations.
    ///
    /// Callers check liveness before entry and after return. A direct selector
    /// runs without interruption. Composed and keyed paths check `live` before
    /// the next operation, but do not interrupt a selector or key lookup.
    fn read_structural<'a>(&self, root: &'a R, _: &dyn Fn() -> bool) -> Option<&'a Self::State> {
        self.read(root)
    }

    /// Selects exclusive state without structural liveness checks.
    fn write<'a>(&self, root: &'a mut R) -> Option<&'a mut Self::State>;
}

/// Selects the value inside an optional field without owning that value.
pub(in crate::store) struct OwnedOptionalPath<R, S> {
    /// Borrows the optional field for shared selection.
    pub(in crate::store) read: for<'a> fn(&'a R) -> &'a Option<S>,
    /// Borrows the optional field for exclusive selection.
    pub(in crate::store) write: for<'a> fn(&'a mut R) -> &'a mut Option<S>,
}

/// Exposes a stack's keyed entries through the common borrowing interface.
pub(in crate::store) struct StackContainerPath<R, S> {
    /// Borrows the stack before selecting its entries.
    pub(in crate::store) read: for<'a> fn(&'a R) -> &'a crate::StackState<S>,
    /// Borrows the stack exclusively before selecting its entries.
    pub(in crate::store) write: for<'a> fn(&'a mut R) -> &'a mut crate::StackState<S>,
}

impl<R, S> Clone for StackContainerPath<R, S> {
    fn clone(&self) -> Self {
        Self {
            read: self.read,
            write: self.write,
        }
    }
}

impl<R, S> BorrowPath<R> for StackContainerPath<R, S> {
    type State = KeyedState<crate::StackId, S>;

    fn read<'a>(&self, root: &'a R) -> Option<&'a Self::State> {
        Some((self.read)(root).entries())
    }

    fn write<'a>(&self, root: &'a mut R) -> Option<&'a mut Self::State> {
        Some((self.write)(root).entries_mut())
    }
}

impl<R, S> Clone for OwnedOptionalPath<R, S> {
    fn clone(&self) -> Self {
        Self {
            read: self.read,
            write: self.write,
        }
    }
}

impl<R, S> BorrowPath<R> for OwnedOptionalPath<R, S> {
    type State = S;

    fn read<'a>(&self, root: &'a R) -> Option<&'a S> {
        (self.read)(root).as_ref()
    }

    fn write<'a>(&self, root: &'a mut R) -> Option<&'a mut S> {
        (self.write)(root).as_mut()
    }
}

impl<R, S> BorrowPath<R> for OptionalPath<R, S> {
    type State = S;

    fn read<'a>(&self, root: &'a R) -> Option<&'a S> {
        (self.read)(root)
    }

    fn write<'a>(&self, root: &'a mut R) -> Option<&'a mut S> {
        (self.write)(root)
    }
}

/// Selects a child through a parent path without retaining selected references.
pub(in crate::store) struct ComposedPath<P, Q> {
    /// First selection, whose absence skips the child selection.
    parent: P,
    /// Selection within the borrowed parent state.
    child: Q,
}

impl<P: Clone, Q: Clone> Clone for ComposedPath<P, Q> {
    fn clone(&self) -> Self {
        Self {
            parent: self.parent.clone(),
            child: self.child.clone(),
        }
    }
}

impl<P, Q> ComposedPath<P, Q> {
    /// Retains both routes without invoking either selector.
    pub(in crate::store) fn new(parent: P, child: Q) -> Self {
        Self { parent, child }
    }
}

impl<R, P, Q> BorrowPath<R> for ComposedPath<P, Q>
where
    P: BorrowPath<R>,
    P::State: 'static,
    Q: BorrowPath<P::State>,
{
    type State = Q::State;

    fn read<'a>(&self, root: &'a R) -> Option<&'a Self::State> {
        self.child.read(self.parent.read(root)?)
    }

    fn read_structural<'a>(&self, root: &'a R, live: &dyn Fn() -> bool) -> Option<&'a Self::State> {
        let parent = self.parent.read_structural(root, live)?;
        if !live() {
            return None;
        }
        self.child.read_structural(parent, live)
    }

    fn write<'a>(&self, root: &'a mut R) -> Option<&'a mut Self::State> {
        self.child.write(self.parent.write(root)?)
    }
}

/// Looks up a retained domain key, without retaining a collection or insertion stamp.
///
/// An equal key can later identify a replacement. The caller's frozen authority,
/// not the key alone, decides whether a mounted operation can still run.
pub(in crate::store) struct KeyedPath<R, K, S, H> {
    /// Selects the collection before shared key lookup.
    read: for<'a> fn(&'a R) -> &'a KeyedState<K, S>,
    /// Selects the collection before exclusive key lookup.
    write: for<'a> fn(&'a mut R) -> &'a mut KeyedState<K, S>,
    /// Key owner, shared by `Arc` or `Rc` at the current construction sites.
    key: H,
}

impl<R, K, S, H: Clone> Clone for KeyedPath<R, K, S, H> {
    fn clone(&self) -> Self {
        Self {
            read: self.read,
            write: self.write,
            key: self.key.clone(),
        }
    }
}

impl<R, K, S, H> KeyedPath<R, K, S, H> {
    /// Retains a key and selectors without accessing the collection.
    pub(in crate::store) fn new(
        read: for<'a> fn(&'a R) -> &'a KeyedState<K, S>,
        write: for<'a> fn(&'a mut R) -> &'a mut KeyedState<K, S>,
        key: H,
    ) -> Self {
        Self { read, write, key }
    }
}

impl<R, K: Eq + Hash + 'static, S, H: Clone + Deref<Target = K>> BorrowPath<R>
    for KeyedPath<R, K, S, H>
{
    type State = S;

    fn read<'a>(&self, root: &'a R) -> Option<&'a S> {
        (self.read)(root).get(&*self.key)
    }

    fn read_structural<'a>(&self, root: &'a R, live: &dyn Fn() -> bool) -> Option<&'a S> {
        let collection = (self.read)(root);
        if !live() {
            return None;
        }
        // One admitted lookup may call Hash/Eq more than once; it is not interrupted.
        collection.get(&*self.key)
    }

    fn write<'a>(&self, root: &'a mut R) -> Option<&'a mut S> {
        (self.write)(root).get_mut(&*self.key)
    }
}
