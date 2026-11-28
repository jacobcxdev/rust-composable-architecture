//! Allocation cost of harvesting one ready effect and waking its observed task.

#![cfg(feature = "allocation-measurement")]

#[path = "../src/allocation_test/child.rs"]
mod allocation_test;

#[expect(
    dead_code,
    reason = "this manual polling test does not start executor threads"
)]
mod support;

use std::task::{Context, Poll, Waker};

use rca_core::{Effect, ReduceContext, Reducer, Store};

#[global_allocator]
/// Measures terminal harvest in one process without unrelated support tests.
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

/// Commits one action and starts an already-ready owned effect.
struct ReadyEffect;

impl Reducer for ReadyEffect {
    type State = u64;
    type Action = ();

    fn reduce(&mut self, state: &mut u64, (): &(), _context: &mut ReduceContext<'_>) -> Effect<()> {
        *state += 1;
        Effect::run(|_sender| std::future::ready(()))
    }
}

/// Checks that singleton effect completion needs no result buffer.
fn singleton_ready_effect_harvest_allocates_no_result_buffer() {
    if allocation_test::run_in_child("singleton_ready_effect_harvest_allocates_no_result_buffer") {
        return;
    }

    let runtime = support::ManualRuntime::new();
    let store = Store::builder(0, ReadyEffect, runtime.runtime())
        .mailbox_capacity(1)
        .build()
        .expect("valid store");
    let mut actor = runtime.take_only_queued_task();
    let mut context = Context::from_waker(Waker::noop());
    assert!(actor.as_mut().poll(&mut context).is_pending());
    let task = {
        let send = store.send(());
        let mut send = std::pin::pin!(send);
        assert!(send.as_mut().poll(&mut context).is_pending());
        assert!(actor.as_mut().poll(&mut context).is_pending());
        let Poll::Ready(task) = send.as_mut().poll(&mut context) else {
            panic!("actor must acknowledge effect registration");
        };
        task.expect("committed action")
    };
    let finish = task.finish();
    let mut finish = std::pin::pin!(finish);
    // Initialise the lineage's event/mutex outside measurement, so platform
    // lazy-lock allocations cannot obscure the terminal collection budget.
    assert!(finish.as_mut().poll(&mut context).is_pending());
    let mut effect = runtime.take_only_queued_task();
    assert!(effect.as_mut().poll(&mut context).is_ready());
    drop(effect);
    assert!(
        !task.is_finished(),
        "actor has not harvested runtime completion"
    );

    // This integration binary owns one process-wide profiler and polls both
    // real runtime futures synchronously, without executor scheduling noise.
    let profiler = dhat::Profiler::builder().testing().build();
    let before = dhat::HeapStats::get();
    assert!(actor.as_mut().poll(&mut context).is_pending());
    let after = dhat::HeapStats::get();
    drop(profiler);
    assert!(finish.as_mut().poll(&mut context).is_ready());
    assert!(task.is_finished());

    let shutdown = store.shutdown_into_state();
    let mut shutdown = std::pin::pin!(shutdown);
    assert!(shutdown.as_mut().poll(&mut context).is_pending());
    assert!(actor.as_mut().poll(&mut context).is_ready());
    let Poll::Ready(state) = shutdown.as_mut().poll(&mut context) else {
        panic!("completed actor must acknowledge extraction");
    };
    assert_eq!(state.expect("extracted state"), 1);

    let blocks = after.total_blocks - before.total_blocks;
    // Neither singleton harvesting nor the next mailbox wait needs heap storage.
    assert_eq!(blocks, 0, "singleton harvest allocated {blocks} blocks");
}

/// Preserves the exact names of the isolated allocation cases.
const CASES: &[(&str, fn())] = &[(
    "singleton_ready_effect_harvest_allocates_no_result_buffer",
    singleton_ready_effect_harvest_allocates_no_result_buffer,
)];

include!("support/allocation_main.rs");
