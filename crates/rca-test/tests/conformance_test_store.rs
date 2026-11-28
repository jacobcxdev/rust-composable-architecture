//! Runs the public test-store conformance suite through its ordinary integration target.

#[test]
fn test_store_conformance() {
    futures_executor::block_on(rca_test::conformance::run_test_store());
}
