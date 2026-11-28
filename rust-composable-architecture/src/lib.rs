#![forbid(unsafe_code)]

//! Re-exports the Rust Composable Architecture API from [`rca`].

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
