//! Exercises warning-free derives through a renamed facade dependency.

#![deny(warnings)]

use architecture::Snapshot;
include!("../../snapshot-consumer-common.rs");
