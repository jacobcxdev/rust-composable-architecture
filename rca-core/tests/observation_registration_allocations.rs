//! Measures observation request creation separately from actor registration and cleanup.

#![cfg(feature = "allocation-measurement")]

#[path = "../src/allocation_test/child.rs"]
mod allocation_test;

#[expect(dead_code, reason = "this allocation test manually polls the actor")]
mod support;

use std::task::{Context, Poll, Waker};

use rca_core::{Effect, ReduceContext, Reducer, Store};

/// Records the isolated test process's allocation events during profiling.
#[global_allocator]
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

/// Supplies a zero-sized reducer while the assay registers the initial value.
struct Counter;

impl Reducer for Counter {
    type State = u64;
    type Action = ();

    fn reduce(&mut self, state: &mut u64, (): &(), _: &mut ReduceContext<'_>) -> Effect<()> {
        *state += 1;
        Effect::none()
    }
}

/// Checks that observation registration avoids a separate reply channel.
fn registration_request_avoids_general_purpose_reply_channel() {
    // Exclude the support module's tests from this process-wide allocation count.
    if allocation_test::run_in_child("registration_request_avoids_general_purpose_reply_channel") {
        return;
    }

    let runtime = support::ManualRuntime::new();
    let store = Store::builder(7, Counter, runtime.runtime())
        .mailbox_capacity(1)
        .build()
        .expect("valid store");
    let mut actor = runtime.take_only_queued_task();
    let mut context = Context::from_waker(Waker::noop());
    assert!(actor.as_mut().poll(&mut context).is_pending());

    // Initialise mailbox notification storage outside the registration window.
    let initial = {
        let read = store.read(|state| *state);
        let mut read = std::pin::pin!(read);
        assert!(read.as_mut().poll(&mut context).is_pending());
        assert!(actor.as_mut().poll(&mut context).is_pending());
        read.as_mut().poll(&mut context)
    };
    assert!(matches!(initial, Poll::Ready(Ok(7))));

    // Measure request creation, then verify the actual actor registration,
    // initial projection, unregistration and state extraction.
    let profiler = dhat::Profiler::builder().testing().build();
    let before = dhat::HeapStats::get();
    let (mut values, requested) = {
        let registration = store.observe(|state| *state);
        let mut registration = std::pin::pin!(registration);
        assert!(registration.as_mut().poll(&mut context).is_pending());
        let requested = dhat::HeapStats::get();
        assert!(actor.as_mut().poll(&mut context).is_pending());
        let Poll::Ready(values) = registration.as_mut().poll(&mut context) else {
            panic!("actor must acknowledge registration");
        };
        (values.expect("registered observation"), requested)
    };
    drop(profiler);
    assert_eq!(futures_executor::block_on(values.next()), Some(7));
    drop(values);
    let shutdown = store.shutdown_into_state();
    let mut shutdown = std::pin::pin!(shutdown);
    assert!(shutdown.as_mut().poll(&mut context).is_pending());
    assert!(actor.as_mut().poll(&mut context).is_ready());
    assert!(matches!(
        shutdown.as_mut().poll(&mut context),
        Poll::Ready(Ok(7))
    ));

    // One signal, one command and one single-response slot; the selector is ZST.
    let blocks = requested.total_blocks - before.total_blocks;
    assert!(
        blocks <= 3,
        "registration request allocated {blocks} blocks; budget is 3"
    );
}

/// Preserves the exact names of the isolated allocation cases.
const CASES: &[(&str, fn())] = &[(
    "registration_request_avoids_general_purpose_reply_channel",
    registration_request_avoids_general_purpose_reply_channel,
)];

include!("support/allocation_main.rs");
