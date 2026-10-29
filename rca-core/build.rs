//! Registers the Loom configuration used by concurrency checks.

/// Declares the custom configuration and this script's only rebuild input.
fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rustc-check-cfg=cfg(loom)");
}
