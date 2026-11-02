/// Preserves each allocation control's original name and child marker.
const CASES: &[(&str, fn())] = ready_allocations::tests::CASES;

include!("../../rca-core/src/allocation_test/runner.rs");

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let child = std::env::var_os("RCA_READY_ALLOCATION_CHILD").map(|name| {
        let mut exact = std::ffi::OsString::from("ready_allocations::tests::");
        exact.push(name);
        exact
    });
    match run(&args, false, child.as_deref(), false) {
        Ok(true) => std::process::ExitCode::SUCCESS,
        Ok(false) => std::process::ExitCode::FAILURE,
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::FAILURE
        }
    }
}
