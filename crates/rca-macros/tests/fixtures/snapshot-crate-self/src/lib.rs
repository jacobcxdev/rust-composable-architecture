//! Exercises derive paths when the consumer itself has the facade package name.

pub use rca_core::Snapshot;
use rca_macros::Snapshot;
include!("../../snapshot-consumer-common.rs");
