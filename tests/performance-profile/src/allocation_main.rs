//! Runs measured controls without a concurrent libtest receiver thread.

#![expect(
    dead_code,
    unused_imports,
    reason = "The allocation host shares workload support with ordinary semantic and CLI tests."
)]

/// Reuses the established synchronous selector and exact child validation.
mod allocation_runner {
    const CASES: &[(&str, fn())] = crate::allocation_cases::CASES;

    include!("../../../rca-core/src/allocation_test/runner.rs");

    /// Checks both child-marker families before running a selected control.
    pub(super) fn main() -> std::process::ExitCode {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let child = std::env::var_os("RCA_ALLOCATION_TEST_CHILD");
        let file_child = std::env::var_os("RCA_PERFORMANCE_FILE_CHILD");
        if let Some(marker) = file_child.as_deref() {
            let valid = [
                "measurement::tests::early_error_preserves_caller_file_and_releases_session",
                "measurement::tests::unwind_preserves_caller_file_and_releases_session",
            ]
            .iter()
            .any(|name| marker == std::ffi::OsStr::new(name));
            if child.is_some() || !valid {
                eprintln!("allocation file child marker does not match a file-safety case");
                return std::process::ExitCode::FAILURE;
            }
        }
        match run(
            &args,
            false,
            child.as_deref().or(file_child.as_deref()),
            false,
        ) {
            Ok(true) => std::process::ExitCode::SUCCESS,
            Ok(false) => std::process::ExitCode::FAILURE,
            Err(error) => {
                eprintln!("{error}");
                std::process::ExitCode::FAILURE
            }
        }
    }
}

fn main() -> std::process::ExitCode {
    allocation_runner::main()
}

include!("profile_items.rs");
