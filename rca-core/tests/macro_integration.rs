//! Checks derive crate lookup from the core package's integration-test target.

use rca_core::DependencyKey as _;
use rca_macros::{DependencyKey, StatePaths};

/// Supplies a literal default through the generated dependency-key implementation.
#[derive(DependencyKey)]
#[dependency(value = u8, live = 7)]
struct CountKey;

/// Requires the generated namespace to resolve the core state-path type.
#[derive(StatePaths)]
struct MacroState {
    child: u8,
}

#[test]
fn core_library_target_resolves_for_integration_derives() {
    assert_eq!(CountKey::live_value(), 7);
    let _ = MacroStatePaths::child();
}
