//! Runs the shared command-line checks against the RCA counter.

const BINARY: &str = env!("CARGO_BIN_EXE_rca-counter-contracts");

include!("../../cli_tests.rs");
