#![cfg(feature = "allocation-measurement")]
//! Measures the storage difference between two usable mailbox capacities.

#[cfg(target_pointer_width = "64")]
#[path = "../src/allocation_test/child.rs"]
mod allocation_test;

#[cfg(target_pointer_width = "64")]
#[expect(
    dead_code,
    reason = "this test captures actors without executor threads"
)]
mod support;

#[cfg(target_pointer_width = "64")]
use std::task::{Context, Poll, Waker};

#[cfg(target_pointer_width = "64")]
use rca_core::{Effect, ReduceContext, Reducer, Store};

#[cfg(target_pointer_width = "64")]
#[global_allocator]
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

/// Counts every accepted action during the ordered shutdown drain.
#[cfg(target_pointer_width = "64")]
struct Counter;

#[cfg(target_pointer_width = "64")]
impl Reducer for Counter {
    type State = u64;
    type Action = ();

    fn reduce(&mut self, state: &mut u64, (): &(), _: &mut ReduceContext<'_>) -> Effect<()> {
        *state += 1;
        Effect::none()
    }
}

/// Checks the storage budget for additional usable mailbox slots.
#[cfg(target_pointer_width = "64")]
fn additional_mailbox_capacity_has_bounded_storage_cost() {
    if allocation_test::run_in_child("additional_mailbox_capacity_has_bounded_storage_cost") {
        return;
    }

    let profiler = dhat::Profiler::builder().testing().build();
    let mut measurements = Vec::new();
    for capacity in [128, 256] {
        let runtime = support::ManualRuntime::new();
        let before = dhat::HeapStats::get();
        let store = Store::builder(0, Counter, runtime.runtime())
            .mailbox_capacity(capacity)
            .build()
            .expect("valid store");
        let after = dhat::HeapStats::get();
        measurements.push(after.total_bytes - before.total_bytes);
        for _ in 0..capacity {
            drop(store.try_send(()).expect("each advertised slot is usable"));
        }
        assert!(store.try_send(()).expect_err("bounded capacity").is_full());
        let mut actor = runtime.take_only_queued_task();
        let mut context = Context::from_waker(Waker::noop());
        let shutdown = store.shutdown_into_state();
        let mut shutdown = std::pin::pin!(shutdown);
        assert!(shutdown.as_mut().poll(&mut context).is_pending());
        let mut state = None;
        // The actor cooperatively yields while draining. Bound this diagnostic
        // generously above the batches needed for 256 accepted commands.
        for _ in 0..32 {
            let _ = actor.as_mut().poll(&mut context);
            if let Poll::Ready(result) = shutdown.as_mut().poll(&mut context) {
                state = Some(result.expect("ordered extraction"));
                break;
            }
        }
        assert_eq!(state, Some(capacity as u64));
    }
    drop(profiler);
    let additional = measurements[1] - measurements[0];
    println!("additional 128 slots: {additional} bytes");
    // Each slot in this 64-bit case holds the request and tracks the work it
    // starts without a separate box per send. The 152-byte limit applies to
    // this action type. It does not specify the layout for other action types.
    assert!(
        additional <= 128 * 152,
        "additional mailbox slots exceeded storage budget: {additional} bytes"
    );
}

/// Preserves the exact names of the isolated allocation cases.
const CASES: &[(&str, fn())] = &[
    #[cfg(target_pointer_width = "64")]
    (
        "additional_mailbox_capacity_has_bounded_storage_cost",
        additional_mailbox_capacity_has_bounded_storage_cost,
    ),
];

include!("support/allocation_main.rs");
