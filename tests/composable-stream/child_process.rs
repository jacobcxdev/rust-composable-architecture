// Keep the child watchdog, bounded pipe drains, and exact harness oracle in
// one existing implementation across the three standalone fixture workspaces.
include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../rca-core/src/allocation_test/child.rs"
));

pub(super) fn capture_bounded(command: &mut Command, timeout: Duration) -> Result<Output, String> {
    capture(command, timeout)
}

pub(super) fn passed_exactly_one(stdout: &str) -> bool {
    passed_one_test(stdout)
}
