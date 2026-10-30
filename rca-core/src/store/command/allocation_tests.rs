//! Measures allocation costs without the ordinary test harness.

use super::{Command, TerminalReply};

/// Checks terminal callbacks retain their single payload-box allocation.
pub(crate) fn terminal_callbacks_allocate_one_existing_payload_box() {
    fn measure<T>(make: impl FnOnce() -> T) -> (u64, u64) {
        let before = dhat::HeapStats::get();
        let value = std::hint::black_box(make());
        let after = dhat::HeapStats::get();
        drop(value);
        (
            after.total_blocks - before.total_blocks,
            after.total_bytes - before.total_bytes,
        )
    }
    type SharedCallback =
        <super::super::carriers::Shared as super::super::carriers::Carriers<u64, ()>>::Terminal;
    type LocalCallback =
        <super::super::local::Local as super::super::carriers::Carriers<u64, ()>>::Terminal;
    if crate::allocation_test::run_in_child(
        "store::command::reply_contract_tests::terminal_callbacks_allocate_one_existing_payload_box",
    ) {
        return;
    }

    let _profiler = dhat::Profiler::builder().testing().build();
    let shared = measure(|| {
        let captured = std::hint::black_box(7_u64);
        Command::<u64, ()>::Shutdown(TerminalReply::<SharedCallback>::State(Box::new(
            move |state| {
                std::hint::black_box(captured + state);
            },
        )))
    });
    let local = measure(|| {
        let captured = std::hint::black_box(7_u64);
        Command::<u64, (), super::super::local::Local>::Shutdown(
            TerminalReply::<LocalCallback>::State(Box::new(move |state| {
                std::hint::black_box(captured + state);
            })),
        )
    });
    assert_eq!(shared, (1, 8));
    assert_eq!(local, (1, 8));
    println!("terminal_callback_allocations shared={shared:?} local={local:?}");
}
