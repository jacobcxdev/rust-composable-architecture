#![forbid(unsafe_code)]

//! Re-exports the same types and optional features as the matching `rca` package.
//!
//! Start with [`Store`] to send an action and read its committed state.
//! This package has no default features. To install a published release with the `rca` import, run
//! `cargo add rust-composable-architecture --rename rca`.
//!
//! Once published, both packages share the [canonical API reference for this version].
//!
#![doc = concat!(
    "[canonical API reference for this version]: https://docs.rs/rca/",
    env!("CARGO_PKG_VERSION"),
    "/rca/"
)]

#[cfg(feature = "macros")]
extern crate self as rust_composable_architecture;

#[doc(inline)]
pub use rca::*;

#[cfg(feature = "macros")]
#[doc(inline)]
pub use rca::__long_facade_macros::{
    ActionPaths, DependencyKey, Snapshot, StatePaths, reducer, reducers,
};

#[cfg(all(test, feature = "macros"))]
mod facade_macros;

#[cfg(test)]
mod tests {
    #[test]
    fn shares_types_with_rca() {
        let effect: super::Effect<()> = rca::Effect::none();
        let _: rca::Effect<()> = effect;
    }
}
