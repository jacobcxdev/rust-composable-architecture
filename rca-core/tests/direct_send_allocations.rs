//! Direct effect-send costs with admission, actor commit and completion driven explicitly.

#![cfg(feature = "allocation-measurement")]

#[path = "../src/allocation_test/child.rs"]
mod allocation_test;

#[expect(dead_code, reason = "this test manually polls captured runtime tasks")]
mod support;

use std::task::{Context, Poll, Waker};

use rca_core::{Effect, ReduceContext, Reducer, Store};

#[global_allocator]
/// Measures one warmed direct-send schedule in an isolated child.
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

/// Emits one follow-up action, which alone increments the final state.
struct Counter;

impl Reducer for Counter {
    type State = u64;
    type Action = bool;

    fn reduce(
        &mut self,
        state: &mut u64,
        action: &bool,
        _: &mut ReduceContext<'_>,
    ) -> Effect<bool> {
        if *action {
            Effect::send(false)
        } else {
            *state += 1;
            Effect::none()
        }
    }
}

#[test]
fn direct_send_avoids_private_erased_sender_allocation() {
    if allocation_test::run_in_child("direct_send_avoids_private_erased_sender_allocation") {
        return;
    }

    let runtime = support::ManualRuntime::new();
    let store = Store::builder(0, Counter, runtime.runtime())
        .mailbox_capacity(1)
        .build()
        .unwrap();
    let mut actor = runtime.take_only_queued_task();
    let mut cx = Context::from_waker(Waker::noop());
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    let mut blocks = 0;
    for measured in [false, true] {
        let receipt = store.try_send(true).unwrap();
        let profiler = measured.then(|| dhat::Profiler::builder().testing().build());
        let before = profiler.as_ref().map(|_| dhat::HeapStats::get());
        assert!(actor.as_mut().poll(&mut cx).is_pending());
        let task = futures_executor::block_on(receipt.finish()).unwrap();
        let mut effect = runtime.take_only_queued_task();
        assert!(!task.is_finished());
        assert!(effect.as_mut().poll(&mut cx).is_pending());
        assert!(!task.is_finished(), "admission is not commit");
        assert!(actor.as_mut().poll(&mut cx).is_pending());
        assert!(effect.as_mut().poll(&mut cx).is_ready());
        drop(effect);
        assert!(actor.as_mut().poll(&mut cx).is_pending());
        assert!(task.is_finished());
        drop(task);
        if let Some(before) = before {
            let after = dhat::HeapStats::get();
            blocks = after.total_blocks - before.total_blocks;
            eprintln!(
                "direct send: {blocks} blocks, {} requested bytes, {} peak bytes, {} retained bytes",
                after.total_bytes - before.total_bytes,
                after.max_bytes,
                after.curr_bytes,
            );
        }
        drop(profiler);
    }
    let shutdown = store.shutdown_into_state();
    let mut shutdown = std::pin::pin!(shutdown);
    assert!(shutdown.as_mut().poll(&mut cx).is_pending());
    assert!(actor.as_mut().poll(&mut cx).is_ready());
    let Poll::Ready(state) = shutdown.as_mut().poll(&mut cx) else {
        panic!("completed actor must acknowledge extraction");
    };
    assert_eq!(state.unwrap(), 2);
    assert!(
        blocks <= 5,
        "direct send allocated {blocks} blocks; budget is 5"
    );
}
