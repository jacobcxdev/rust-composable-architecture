//! Checks that cached keys retain write permission and thread-transfer boundaries.

#[test]
#[cfg_attr(miri, ignore = "trybuild launches rustc, which Miri isolation forbids")]
fn key_cache_preserves_write_and_transport_boundaries() {
    let tests = trybuild::TestCases::new();
    tests.compile_fail("tests/ui/shared_key_cache_read_only.rs");
    tests.compile_fail("tests/ui/shared_key_cache_non_send.rs");
}
