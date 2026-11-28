#![forbid(unsafe_code)]
#![expect(
    dead_code,
    unused_imports,
    reason = "The allocation host shares private fixtures without running ordinary unit tests."
)]

//! Runs persistence allocation checks without a concurrent test-runner thread.

include!("crate_items.rs");

const CASES: &[(&str, fn())] = file::tests::ALLOCATION_CASES;

include!("allocation_runner.rs");

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let child = std::env::var_os("RCA_PERSISTENCE_ALLOCATION_TEST");
    match run(&args, false, child.as_deref(), false) {
        Ok(true) => std::process::ExitCode::SUCCESS,
        Ok(false) => std::process::ExitCode::FAILURE,
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::FAILURE
        }
    }
}
