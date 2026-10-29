//! Checks dependency-key rejection diagnostics in the top-level UI fixtures.
//!
//! Accepted declarations run separately in `derive_dependency_key`.

#[test]
fn dependency_key_rejects_unsupported_declarations() {
    let tests = trybuild::TestCases::new();
    tests.compile_fail("tests/ui/*.rs");
}
