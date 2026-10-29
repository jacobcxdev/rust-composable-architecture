//! Keyed child reducers.
//!
//! Many applications contain *dynamic* collections of child features—for example, a set of open
//! tabs, a list of downloads, or a map of in-flight jobs. In these cases a parent reducer often
//! needs to route a child action to **one specific child instance**.
//!
//! This module provides two small building blocks:
//!
//! - [`Keyed<K, A>`] wraps a child action `A` with a key `K` identifying which child should handle it.
//! - [`KeyedState<K, V>`] is a wrapper around a map-like collection (by default a `HashMap`) used to
//!   store child state keyed by `K`.
//!
//! ## Routing contract
//!
//! A typical parent action enum will include a keyed child action variant:
//!
//!     use composable::{Effects, Keyed};
//!
//!     #[derive(Clone, Debug, PartialEq, Eq, Hash)]
//!     struct Id(u32);
//!
//!     #[derive(Clone, Debug, PartialEq)]
//!     enum ChildAction {
//!         Ping,
//!     }
//!
//!     #[derive(Clone, Debug, PartialEq)]
//!     enum ParentAction {
//!         Child(Keyed<Id, ChildAction>),
//!     }
//!
//!     impl From<Keyed<Id, ChildAction>> for ParentAction {
//!         fn from(value: Keyed<Id, ChildAction>) -> Self {
//!             ParentAction::Child(value)
//!         }
//!     }
//!
//!     fn child(send: impl Effects<ChildAction>) {
//!         send.action(ChildAction::Ping);
//!     }
//!
//!     fn parent(send: impl Effects<ParentAction>) {
//!         // Any follow-up actions emitted by `child` are wrapped back into `ParentAction::Child`
//!         // with the same key.
//!         child(send.scope_keyed(Id(1)));
//!     }
//!
//! When used with `#[derive(RecursiveReducer)]`, keyed routing relies on `TryInto`/`From` conversions:
//! the derive macro attempts to `try_into()` a `Keyed<K, ChildAction>` from the parent action, then
//! uses the key to select the child state and `scope_keyed(key)` to route effects back.

use std::collections::{BTreeMap, HashMap};
use std::hash::Hash;
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};

/// A keyed wrapper around an action `A` for a particular child identified by `K`.
///
/// This is typically embedded inside a parent `Action` enum to represent the “child actions”
/// of a keyed collection.
///
/// Prefer using a newtype for `K` (as in `struct Id(u32);`) to avoid accidental key mixing and
/// to keep multiple keyed collections unambiguous at the type level.
#[derive(Clone, Debug, PartialEq)]
pub struct Keyed<K, A> {
    pub key: K,
    pub action: A,
}

impl<K, A> Keyed<K, A> {
    /// Create a new keyed action payload.
    pub fn new(key: K, action: A) -> Self {
        Self { key, action }
    }
}

/// A keyed collection of child `State`s.
///
/// This wrapper type exists so derive macros can reliably detect “keyed child state” fields.
///
/// - Use it when a parent reducer owns a dynamic collection of child reducers.
/// - Pair it with [`Keyed`] actions and `Effects::scope_keyed(…)`.
///
/// ## Map types
///
/// The default map type is a `HashMap<K, V>`, but you can supply any map-like type you control:
///
///     use composable::KeyedState;
///     use std::collections::BTreeMap;
///
///     #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
///     struct Id(u32);
///
///     #[derive(Clone, Debug, Default, PartialEq)]
///     struct ChildState;
///
///     let children: KeyedState<Id, ChildState, BTreeMap<Id, ChildState>> = Default::default();
///     let _ = children;
///
/// # Note
/// If you need multiple keyed collections of the same child action type under one parent,
/// prefer using distinct *key types* (newtype keys) so the routed action payload types differ.
#[derive(Clone, Debug, PartialEq)]
pub struct KeyedState<K, V, Map = HashMap<K, V>>(
    /// The underlying map storage.
    pub Map,
    /// Ensures `K` and `V` are treated as “used” by the type system even when `Map` is a type
    /// parameter. The `fn() -> (K, V)` form avoids implying ownership or drop order.
    PhantomData<fn() -> (K, V)>,
);

impl<K, V, Map> KeyedState<K, V, Map> {
    /// Wrap an existing map as a keyed state collection.
    pub fn new(map: Map) -> Self {
        Self(map, PhantomData)
    }
}

impl<K, V, Map> Default for KeyedState<K, V, Map>
where
    Map: Default,
{
    fn default() -> Self {
        Self(Map::default(), PhantomData)
    }
}

impl<K, V, Map> Deref for KeyedState<K, V, Map> {
    type Target = Map;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<K, V, Map> DerefMut for KeyedState<K, V, Map> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<K, V, Map> From<Map> for KeyedState<K, V, Map> {
    fn from(value: Map) -> Self {
        Self(value, PhantomData)
    }
}

impl<K, V, Map> KeyedState<K, V, Map>
where
    Map: KeyedMap<K, V>,
{
    /// Map-agnostic `get_mut`, delegated through [`KeyedMap`].
    pub fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        Map::get_mut(&mut self.0, key)
    }
}

/// A small abstraction over “map-like” keyed storage used by [`KeyedState`].
///
/// This is intentionally minimal: the derive macro only needs `get_mut` in order to route a child
/// action to a selected child state.
pub trait KeyedMap<K, V> {
    fn get_mut<'a>(this: &'a mut Self, key: &K) -> Option<&'a mut V>;
}

impl<K, V> KeyedMap<K, V> for HashMap<K, V>
where
    K: Eq + Hash,
{
    fn get_mut<'a>(this: &'a mut Self, key: &K) -> Option<&'a mut V> {
        HashMap::get_mut(this, key)
    }
}

impl<K, V> KeyedMap<K, V> for BTreeMap<K, V>
where
    K: Ord,
{
    fn get_mut<'a>(this: &'a mut Self, key: &K) -> Option<&'a mut V> {
        BTreeMap::get_mut(this, key)
    }
}
