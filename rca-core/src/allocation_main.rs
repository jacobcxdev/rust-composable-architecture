#![deny(rustdoc::broken_intra_doc_links)]
#![forbid(unsafe_code)]

#[cfg(test)]
macro_rules! ordinary_test_items {
    ($($items:tt)*) => {};
}

// Public modules mirror the library root.
#[cfg(feature = "diagnostics")]
pub mod diagnostics;
#[cfg(feature = "local")]
pub mod local;

include!("crate_items.rs");

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let keyed_child = std::env::var_os("RCA_KEYED_ALLOCATION_CHILD").is_some();
    let shared_child = std::env::var_os("RCA_ALLOCATION_TEST_CHILD");
    let scope_child = std::env::var_os("RCA_SCOPE_ALLOCATION_CHILD").is_some();
    let timer_child = std::env::var_os("RCA_TIMER_REPLY_ALLOCATION_CHILD").is_some();
    let direct_future_child = std::env::var_os("RCA_DIRECT_FUTURE_ALLOCATION_CHILD").is_some();
    let result = allocation_test::actor_child_marker(
        shared_child.as_deref(),
        timer_child,
        direct_future_child,
    )
    .and_then(|marker| allocation_test::run(&args, keyed_child, marker, scope_child));
    match result {
        Ok(true) => std::process::ExitCode::SUCCESS,
        Ok(false) => std::process::ExitCode::FAILURE,
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::FAILURE
        }
    }
}
