#![deny(rustdoc::broken_intra_doc_links)]
#![forbid(unsafe_code)]

//! Provides runtime-neutral RCA foundations.
//!
//! Shared APIs use root imports such as [`Store`], [`Feature`] and [`Runtime`].
//! [`Reducer`] handles synchronous state changes and returns [`Effect`] values.
//! [`Feature`] can also declare typed [`Task`] values through [`FeatureContext`].
//! [`RootMount`] and child mounts connect features to their parent state.
//! Mount reactions receive owned [`ReactionChange`] values and a [`ReactionContext`].
//!
//! [`combine`], [`scope`] and [`for_each`] compose reducers. [`DependencyValues`]
//! supplies their dependencies. [`Observation`] delivers selected committed state,
//! and [`Shared`] gives multiple owners access to shared values.
//!
//! The `local` feature adds owner-thread types under `local` for non-`Send` values.
//! The `diagnostics` feature adds optional inspection under `diagnostics`.
//! The `futures-stream` feature adds stream interoperability to observations.
//! The `test-util` feature adds deterministic test controls, not another runtime.
//! RCA does not install an executor or require a UI framework.

/// Keeps ordinary unit-test items separate from the allocation-test host.
#[cfg(test)]
macro_rules! ordinary_test_items {
    ($($items:tt)*) => { $($items)* };
}

/// Shares the manual host between internal observation and scoped-store tests.
#[cfg(all(test, not(loom)))]
#[expect(
    dead_code,
    reason = "internal tests use only the manual actor controls"
)]
mod test_support {
    use crate as rca_core;
    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/mod.rs"));
}

// Rustdoc links cross-crate module re-exports to these declarations.
// Keep them in the emitted crate-root source.
#[cfg(feature = "diagnostics")]
pub mod diagnostics;
#[cfg(feature = "local")]
pub mod local;

// The allocation-test binary shares these declarations without ordinary test items.
include!("crate_items.rs");
