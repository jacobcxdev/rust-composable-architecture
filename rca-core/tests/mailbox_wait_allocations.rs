#![cfg(feature = "allocation-measurement")]
//! Measures warmed actor commits and mailbox waits, excluding admission and replies.

#[path = "../src/allocation_test/child.rs"]
mod allocation_test;

#[expect(
    dead_code,
    reason = "this test polls the actor without executor threads"
)]
mod support;

use std::task::{Context, Poll, Waker};

use rca_core::{Effect, ReduceContext, Reducer, Store};

#[global_allocator]
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

/// Counts the accepted actions before ordered state extraction.
struct Counter;

impl Reducer for Counter {
    type State = u64;
    type Action = ();

    fn reduce(&mut self, state: &mut u64, (): &(), _: &mut ReduceContext<'_>) -> Effect<()> {
        *state += 1;
        Effect::none()
    }
}

/// Checks that warmed commits and mailbox waits require no allocations.
fn warmed_actor_commit_and_mailbox_wait_do_not_allocate() {
    if allocation_test::run_in_child("warmed_actor_commit_and_mailbox_wait_do_not_allocate") {
        return;
    }

    let runtime = support::ManualRuntime::new();
    let store = Store::builder(0, Counter, runtime.runtime())
        .mailbox_capacity(1)
        .build()
        .expect("valid store");
    let mut actor = runtime.take_only_queued_task();
    let mut context = Context::from_waker(Waker::noop());
    assert!(actor.as_mut().poll(&mut context).is_pending());

    let profiler = dhat::Profiler::builder().testing().build();
    let mut blocks = 0;
    let mut bytes = 0;
    for iteration in 0..120 {
        let send = store.send(());
        let mut send = std::pin::pin!(send);
        assert!(send.as_mut().poll(&mut context).is_pending());
        // Admission and reply storage are outside this window. The first twenty
        // operations warm the actor before measuring commit and the next wait.
        let before = dhat::HeapStats::get();
        assert!(actor.as_mut().poll(&mut context).is_pending());
        let after = dhat::HeapStats::get();
        if iteration >= 20 {
            blocks += after.total_blocks - before.total_blocks;
            bytes += after.total_bytes - before.total_bytes;
        }
        let Poll::Ready(receipt) = send.as_mut().poll(&mut context) else {
            panic!("actor must acknowledge the committed action");
        };
        assert!(receipt.expect("committed action").is_finished());
    }
    drop(profiler);

    let shutdown = store.shutdown_into_state();
    let mut shutdown = std::pin::pin!(shutdown);
    assert!(shutdown.as_mut().poll(&mut context).is_pending());
    assert!(actor.as_mut().poll(&mut context).is_ready());
    let Poll::Ready(state) = shutdown.as_mut().poll(&mut context) else {
        panic!("completed actor must acknowledge extraction");
    };
    assert_eq!(state.expect("extracted state"), 120);
    assert_eq!(
        blocks, 0,
        "100 warmed actor commits and waits allocated {blocks} blocks / {bytes} bytes"
    );
}

/// Preserves the exact names of the isolated allocation cases.
const CASES: &[(&str, fn())] = &[(
    "warmed_actor_commit_and_mailbox_wait_do_not_allocate",
    warmed_actor_commit_and_mailbox_wait_do_not_allocate,
)];

include!("support/allocation_main.rs");
