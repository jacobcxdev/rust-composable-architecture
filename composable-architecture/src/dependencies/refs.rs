//! Helper reference-or-owned return type.
//!
//! Some `Dependency<T>` APIs can return either a borrowed dependency value (when present) or an
//! owned fallback value (when absent). This enum allows callers to treat both cases uniformly via
//! [`Deref`], [`AsRef`], and [`Borrow`].
//!
//! # Example
//! ```rust
//! use composable::dependencies::{Dependency, Ref};
//!
//! #[derive(Default)]
//! struct Foo(u32);
//!
//! let dep = Dependency::<Foo>::get();
//! let value: Ref<'_, Foo> = dep.unwrap_or(Foo(42));
//! assert_eq!(value.0, 42);
//! ```

use std::borrow::Borrow;
use std::ops::Deref;

/// Used to return a reference to a dependency or a separate owned value.
///
/// Used by:
/// - [`unwrap_or`][`super::Dependency::unwrap_or`]
/// - [`unwrap_or_else`][`super::Dependency::unwrap_or_else`]
/// - [`unwrap_or_default`][`super::Dependency::unwrap_or_default`]
/// - [`or`][`super::Dependency::or`]
/// - [`or_else`][`super::Dependency::or_else`]
/// - [`xor`][`super::Dependency::xor`]
pub enum Ref<'a, T: 'a> {
    /// a reference of type T
    Borrowed(&'a T),
    /// a value of type T
    Owned(T),
}

impl<T> Deref for Ref<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        match self {
            Ref::Borrowed(reference) => reference,
            Ref::Owned(value) => value,
        }
    }
}

impl<T> AsRef<T> for Ref<'_, T> {
    fn as_ref(&self) -> &T {
        self.deref()
    }
}

impl<T> Borrow<T> for Ref<'_, T> {
    fn borrow(&self) -> &T {
        self.deref()
    }
}
