//! Reply allocation budgets and backtrace cleanup after rejected-command destruction.
//!
//! Each assay runs in an exact-name child, away from other profilers and controls.
//! Backtrace settings are supplied before the child initialises capture state.

#![cfg(feature = "allocation-measurement")]

#[expect(
    dead_code,
    reason = "Shared support tests run in the ordinary libtest targets."
)]
mod support;

#[path = "../src/allocation_test/child.rs"]
mod allocation_test;

use rca_core::{Effect, ReduceContext, Reducer, Store};
use std::backtrace::{Backtrace, BacktraceStatus};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[global_allocator]
/// Measures the selected child assay, including its explicitly driven actor work.
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

/// Counts committed actions independently of allocation statistics.
struct Counter;

impl Reducer for Counter {
    type State = u64;
    type Action = ();

    fn reduce(&mut self, state: &mut u64, (): &(), _context: &mut ReduceContext<'_>) -> Effect<()> {
        *state += 1;
        Effect::none()
    }
}

/// Checks the allocation budget for warmed committed-action replies.
fn warmed_committed_actions_do_not_allocate_general_purpose_reply_channels() {
    if allocation_test::run_in_child(
        "warmed_committed_actions_do_not_allocate_general_purpose_reply_channels",
    ) {
        return;
    }
    let runtime = support::ManualRuntime::new();
    let store = Store::builder(0, Counter, runtime.runtime())
        .mailbox_capacity(1)
        .build()
        .expect("valid store");
    runtime.start_all();
    assert_eq!(runtime.spawn_count(), 1);
    for _ in 0..20 {
        drop(futures_executor::block_on(store.send(())).expect("warm-up commit"));
    }

    // The child isolates this profiler from the other allocation test.
    let profiler = dhat::Profiler::builder().testing().build();
    let before = dhat::HeapStats::get();
    for _ in 0..100 {
        drop(futures_executor::block_on(store.send(())).expect("checked commit"));
    }
    let after = dhat::HeapStats::get();
    drop(profiler);

    // Allow one reply and one spare event per action for scheduling variation.
    // Actor waits no longer allocate. Do not freeze platform-specific bytes.
    assert!(
        after.total_blocks - before.total_blocks <= 200,
        "100 committed actions allocated {} blocks; reply lifecycle budget is 200",
        after.total_blocks - before.total_blocks,
    );
    assert_eq!(
        futures_executor::block_on(store.shutdown_into_state()).unwrap(),
        120
    );
    assert!(runtime.join_all().into_iter().all(|result| result.is_ok()));
}

/// Counts rejected-payload destruction before raising a recognisable panic.
struct PanicOnDrop(Arc<AtomicUsize>);

impl Drop for PanicOnDrop {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
        panic!("rejected command destructor");
    }
}

/// Provides action types for a store whose actor is dropped before use.
struct Noop;

impl Reducer for Noop {
    type State = ();
    type Action = PanicOnDrop;

    fn reduce(
        &mut self,
        _state: &mut (),
        _action: &PanicOnDrop,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<PanicOnDrop> {
        Effect::none()
    }
}

#[derive(Clone, Copy, Debug)]
/// Selects the rejected command path that owns a panicking payload or capture.
enum RejectedCommand {
    Send,
    Read,
    Observe,
}

/// Requires the intended payload panic without allowing user callbacks to run.
fn reject_with_panicking_capture(
    command: RejectedCommand,
    store: &Store<(), PanicOnDrop>,
    drops: &Arc<AtomicUsize>,
    calls: &Arc<AtomicUsize>,
) {
    let probe = PanicOnDrop(Arc::clone(drops));
    let calls = Arc::clone(calls);
    let panic = catch_unwind(AssertUnwindSafe(|| match command {
        RejectedCommand::Send => drop(futures_executor::block_on(store.send(probe))),
        RejectedCommand::Read => drop(futures_executor::block_on(store.read(move |()| {
            std::hint::black_box(&probe);
            calls.fetch_add(1, Ordering::Relaxed);
        }))),
        RejectedCommand::Observe => drop(futures_executor::block_on(store.observe(move |()| {
            std::hint::black_box(&probe);
            calls.fetch_add(1, Ordering::Relaxed);
        }))),
    }))
    .expect_err("rejected command must destroy its user payload");
    assert_eq!(
        panic.downcast_ref::<&str>(),
        Some(&"rejected command destructor"),
    );
}

/// Checks that rejected commands release their captured backtraces.
fn rejected_command_destructors_do_not_leak_captured_backtraces() {
    if allocation_test::run_in_child_with_env(
        "rejected_command_destructors_do_not_leak_captured_backtraces",
        &[("RUST_LIB_BACKTRACE", "1"), ("RUST_BACKTRACE", "0")],
    ) {
        return;
    }
    assert_eq!(Backtrace::capture().status(), BacktraceStatus::Captured);
    let runtime = support::ManualRuntime::new();
    let store = Store::new((), Noop, runtime.runtime());
    runtime.drop_queued();
    assert!(store.is_closed());
    let drops = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let commands = [
        RejectedCommand::Send,
        RejectedCommand::Read,
        RejectedCommand::Observe,
    ];
    // Warm executor, panic and backtrace machinery before measuring retained memory.
    for command in commands {
        reject_with_panicking_capture(command, &store, &drops, &calls);
    }
    assert_eq!(drops.load(Ordering::Relaxed), 3);
    assert_eq!(calls.load(Ordering::Relaxed), 0);

    let profiler = dhat::Profiler::builder().testing().build();
    let measurements = commands.map(|command| {
        let before = dhat::HeapStats::get();
        reject_with_panicking_capture(command, &store, &drops, &calls);
        let after = dhat::HeapStats::get();
        (
            after.curr_blocks - before.curr_blocks,
            after.curr_bytes - before.curr_bytes,
        )
    });
    drop(profiler);

    assert_eq!(drops.load(Ordering::Relaxed), 6);
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    assert_eq!(
        measurements,
        [(0, 0); 3],
        "rejected {commands:?} commands retained blocks/bytes after payload panic",
    );
}

/// Preserves the exact names of the isolated allocation cases.
const CASES: &[(&str, fn())] = &[
    (
        "warmed_committed_actions_do_not_allocate_general_purpose_reply_channels",
        warmed_committed_actions_do_not_allocate_general_purpose_reply_channels,
    ),
    (
        "rejected_command_destructors_do_not_leak_captured_backtraces",
        rejected_command_destructors_do_not_leak_captured_backtraces,
    ),
];

include!("support/allocation_main.rs");
