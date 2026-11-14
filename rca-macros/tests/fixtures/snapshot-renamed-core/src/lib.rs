//! Exercises warning-free derives through a renamed core dependency.

#![deny(warnings)]

use architecture::Snapshot;
use rca_macros::Snapshot;
include!("../../snapshot-consumer-common.rs");
