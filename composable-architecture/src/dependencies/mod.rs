#![doc = include_str!("README.md")]

//! Dependency scoping and access.
//!
//! This module provides dynamically scoped dependencies:
//! values are supplied for a closure using [`with_dependency`] / [`with_dependencies`], and retrieved
//! at use-sites with [`Dependency<T>`].
//!
//! The implementation uses per-thread storage with stack-like semantics:
//! inner scopes shadow outer scopes for the same dependency type.

pub use refs::Ref;
pub use values::{Dependency, DependencyDefault};

pub(crate) mod guard;
mod refs;
mod values;

/// Supplies a tuple of dependencies for the duration of `f`.
///
/// The tuple can contain heterogeneous dependency values; each value is scoped by its concrete type.
/// Inner scopes shadow outer scopes.
///
/// For a single dependency value, prefer [`with_dependency`].
pub fn with_dependencies<T: Tuple, F: FnOnce() -> R, R>(with: T, f: F) -> R {
    let _guards = with.guards();
    f()
}

/// Supplies a single dependency value for the duration of `f`.
///
/// A convenience function that just forwards to [`with_dependencies`].
pub fn with_dependency<T: 'static, F: FnOnce() -> R, R>(with: T, f: F) -> R {
    with_dependencies((with,), f)
}

#[doc(hidden)]
/// A [`tuple`] of up to twenty-five values.
///
/// Used by [`with_dependencies`] to set the current [`Dependency`] values for its closure.
///
/// This is an internal mechanism: users generally only interact with [`with_dependencies`].
pub trait Tuple {
    #[doc(hidden)]
    type Output;

    #[doc(hidden)]
    fn guards(self) -> Self::Output;
}

macro_rules! tuple_impl {
    ( $($val:ident)+ ) => {
        #[doc(hidden)]
        #[allow(dead_code)]
        #[allow(non_snake_case)]
        impl<$($val: 'static),+> Tuple for ( $($val,)+ ) {
            type Output = ( $(guard::Guard<$val>,)+ );

            fn guards(self) -> Self::Output {
                let ( $($val,)+ ) = self;
                ( $(guard::Guard::new($val),)+ )
            }
        }
    };
}

tuple_impl! { A }
tuple_impl! { A B }
tuple_impl! { A B C }
tuple_impl! { A B C D }
tuple_impl! { A B C D E }
tuple_impl! { A B C D E F }
tuple_impl! { A B C D E F G }
tuple_impl! { A B C D E F G H }
tuple_impl! { A B C D E F G H I }
tuple_impl! { A B C D E F G H I J }
tuple_impl! { A B C D E F G H I J K }
tuple_impl! { A B C D E F G H I J K L }
tuple_impl! { A B C D E F G H I J K L M }
tuple_impl! { A B C D E F G H I J K L M N }
tuple_impl! { A B C D E F G H I J K L M N O }
tuple_impl! { A B C D E F G H I J K L M N O P }
tuple_impl! { A B C D E F G H I J K L M N O P Q }
tuple_impl! { A B C D E F G H I J K L M N O P Q R }
tuple_impl! { A B C D E F G H I J K L M N O P Q R S }
tuple_impl! { A B C D E F G H I J K L M N O P Q R S T }
tuple_impl! { A B C D E F G H I J K L M N O P Q R S T U }
tuple_impl! { A B C D E F G H I J K L M N O P Q R S T U V }
tuple_impl! { A B C D E F G H I J K L M N O P Q R S T U V W }
tuple_impl! { A B C D E F G H I J K L M N O P Q R S T U V W X }
tuple_impl! { A B C D E F G H I J K L M N O P Q R S T U V W X Y }
// up to 25 dependencies are supported
