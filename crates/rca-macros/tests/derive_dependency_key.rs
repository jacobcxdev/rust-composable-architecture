//! Checks dependency factories and equivalent raw-helper validation.

use rca::DependencyKey as _;
use rca_macros::DependencyKey;

/// Provides distinct literal values for live and test factories.
struct ApiClient(u8);

/// Omits the test factory to preserve the trait's missing-test failure.
#[derive(DependencyKey)]
#[dependency(value = ApiClient, live = ApiClient(7))]
struct ApiClientKey;

#[test]
fn derive_emits_foundation_dependency_contract() {
    assert_eq!(ApiClientKey::live_value().0, 7);
}

/// Declares separate live and test factories with different values.
#[derive(DependencyKey)]
#[dependency(value = ApiClient, live = ApiClient(7), test = ApiClient(3))]
struct TestedClientKey;

#[test]
fn derive_keeps_live_and_test_factories_distinct() {
    assert_eq!(TestedClientKey::live_value().0, 7);
    assert_eq!(TestedClientKey::test_value().0, 3);
}

#[test]
fn omitted_test_default_still_rejects_live_fallback() {
    let panic = std::panic::catch_unwind(ApiClientKey::test_value)
        .err()
        .unwrap();
    let message = panic.downcast_ref::<String>().unwrap();
    assert!(message.contains("missing test dependency"));
    assert!(message.contains("ApiClientKey"));
}

/// Uses a caller const parameter in reordered factory options.
#[derive(DependencyKey)]
#[dependency(test = ApiClient(N), live = ApiClient(9), value = ApiClient)]
struct GenericClientKey<const N: u8>;

#[test]
fn test_expression_retains_caller_generic_scope_and_option_order() {
    assert_eq!(GenericClientKey::<4>::test_value().0, 4);
    assert_eq!(GenericClientKey::<4>::live_value().0, 9);
}

#[test]
fn raw_helpers_preserve_values_and_validation() {
    let tests = trybuild::TestCases::new();
    tests.pass("tests/ui/dependency_key/pass_raw_helpers.rs");
    tests.compile_fail("tests/ui/dependency_key/fail_raw_helpers.rs");
}
