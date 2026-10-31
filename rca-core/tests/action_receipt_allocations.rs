//! Allocation-free conversion and polling of pending and buffered action receipts.

#![cfg(feature = "allocation-measurement")]

#[path = "../src/allocation_test/child.rs"]
mod allocation_test;

#[expect(dead_code, reason = "this allocation test manually polls the actor")]
mod support;

use std::cell::Cell;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use rca_core::{Effect, ReduceContext, Reducer, Store};

#[global_allocator]
/// Counts allocations in the isolated receipt-conversion assay.
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

/// Encodes committed action order in the final decimal state.
struct Append;

impl Reducer for Append {
    type State = u64;
    type Action = Cell<u64>;

    fn reduce(
        &mut self,
        state: &mut u64,
        action: &Cell<u64>,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Cell<u64>> {
        *state = *state * 10 + action.get();
        Effect::none()
    }
}

/// Checks that receipt conversion and polling reuse existing allocation storage.
fn receipt_conversion_and_polling_need_no_extra_allocation() {
    if allocation_test::run_in_child("receipt_conversion_and_polling_need_no_extra_allocation") {
        return;
    }

    let runtime = support::ManualRuntime::new();
    let store = Store::new(0, Append, runtime.runtime());
    let mut actor = runtime.take_only_queued_task();
    let mut context = Context::from_waker(Waker::noop());
    assert!(actor.as_mut().poll(&mut context).is_pending());
    let warm = store.try_send(Cell::new(1)).expect("warm admission");
    assert!(actor.as_mut().poll(&mut context).is_pending());
    assert!(
        futures_executor::block_on(async { warm.await })
            .expect("warm commit")
            .is_finished()
    );

    let profiler = dhat::Profiler::builder().testing().build();
    let mut blocks = [0; 2];
    let mut bytes = [0; 2];
    let mut future_bytes = [0; 2];
    let mut completed_retained_bytes = [0; 2];
    let mut pending_completion_cost = None;
    for (index, value) in [2, 3].into_iter().enumerate() {
        let receipt = store.try_send(Cell::new(value)).expect("admission");
        if index == 1 {
            assert!(actor.as_mut().poll(&mut context).is_pending());
        }
        // Admission and actor processing are deliberately outside this window:
        // converting an existing receipt must not introduce a second allocation.
        let before = dhat::HeapStats::get();
        let mut future = receipt.into_future();
        let poll = Pin::new(&mut future).poll(&mut context);
        let after = dhat::HeapStats::get();
        blocks[index] = after.total_blocks - before.total_blocks;
        bytes[index] = after.total_bytes - before.total_bytes;
        future_bytes[index] = std::mem::size_of_val(&future);
        let task = if index == 0 {
            assert!(poll.is_pending());
            assert!(actor.as_mut().poll(&mut context).is_pending());
            // Keep actor work outside this separate completion-poll window too.
            let before = dhat::HeapStats::get();
            let completion = Pin::new(&mut future).poll(&mut context);
            let after = dhat::HeapStats::get();
            pending_completion_cost = Some((
                after.total_blocks - before.total_blocks,
                after.total_bytes - before.total_bytes,
            ));
            match completion {
                Poll::Ready(Ok(task)) => task,
                other => panic!("expected committed task, got {other:?}"),
            }
        } else {
            match poll {
                Poll::Ready(Ok(task)) => task,
                other => panic!("expected buffered task, got {other:?}"),
            }
        };
        assert!(task.is_finished());
        let completed = dhat::HeapStats::get();
        drop(future);
        completed_retained_bytes[index] = completed.curr_bytes - dhat::HeapStats::get().curr_bytes;
    }
    drop(profiler);

    let shutdown = store.shutdown_into_state();
    let mut shutdown = std::pin::pin!(shutdown);
    assert!(shutdown.as_mut().poll(&mut context).is_pending());
    assert!(actor.as_mut().poll(&mut context).is_ready());
    assert!(matches!(
        shutdown.as_mut().poll(&mut context),
        Poll::Ready(Ok(123))
    ));
    for (index, name) in ["pending", "buffered"].into_iter().enumerate() {
        eprintln!(
            "receipt {name}: blocks={} bytes={} future_bytes={} completed_retained_bytes={}",
            blocks[index], bytes[index], future_bytes[index], completed_retained_bytes[index],
        );
    }
    assert_eq!(blocks, [0, 0], "receipt conversion allocated");
    assert_eq!(bytes, [0, 0], "receipt conversion requested heap bytes");
    assert_eq!(
        pending_completion_cost,
        Some((0, 0)),
        "pending receipt completion allocated"
    );
    assert_eq!(
        completed_retained_bytes,
        [0, 0],
        "completed receipt retained heap storage"
    );
}

/// Preserves the exact names of the isolated allocation cases.
const CASES: &[(&str, fn())] = &[(
    "receipt_conversion_and_polling_need_no_extra_allocation",
    receipt_conversion_and_polling_need_no_extra_allocation,
)];

include!("support/allocation_main.rs");
