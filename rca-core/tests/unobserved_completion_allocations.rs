//! Allocation cost of harvesting a task with no completion observer.

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
/// Measures unobserved harvest in one process without unrelated support tests.
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

#[test]
fn unobserved_completion_does_not_allocate_notification_storage() {
    if allocation_test::run_in_child("unobserved_completion_does_not_allocate_notification_storage")
    {
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
    let mut effect = runtime.take_only_queued_task();
    assert!(effect.as_mut().poll(&mut context).is_ready());
    drop(effect);
    assert!(!task.is_finished(), "actor has not harvested completion");

    // No finish future has been polled: completion must not initialise an
    // unused notification event. This binary owns its only global profiler.
    let profiler = dhat::Profiler::builder().testing().build();
    let before = dhat::HeapStats::get();
    assert!(actor.as_mut().poll(&mut context).is_pending());
    let after = dhat::HeapStats::get();
    assert!(task.is_finished());
    let finish = task.finish();
    let mut finish = std::pin::pin!(finish);
    assert!(finish.as_mut().poll(&mut context).is_ready());
    let after_finish = dhat::HeapStats::get();
    drop(profiler);

    let shutdown = store.shutdown_into_state();
    let mut shutdown = std::pin::pin!(shutdown);
    assert!(shutdown.as_mut().poll(&mut context).is_pending());
    assert!(actor.as_mut().poll(&mut context).is_ready());
    let Poll::Ready(state) = shutdown.as_mut().poll(&mut context) else {
        panic!("completed actor must acknowledge extraction");
    };
    assert_eq!(state.expect("extracted state"), 1);

    let blocks = after.total_blocks - before.total_blocks;
    // Only the mailbox receive listener may allocate.
    assert!(
        blocks <= 1,
        "unobserved harvest allocated {blocks} blocks; budget is 1"
    );
    assert_eq!(after_finish.total_blocks, after.total_blocks);
    assert_eq!(after_finish.total_bytes, after.total_bytes);
}
