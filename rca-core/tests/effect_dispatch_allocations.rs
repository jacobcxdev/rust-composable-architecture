//! Counts actor registration allocations without running the effect factory in that window.

#![cfg(feature = "allocation-measurement")]

#[path = "../src/allocation_test/child.rs"]
mod allocation_test;

#[expect(
    dead_code,
    reason = "this manual polling test does not start executor threads"
)]
mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};

use rca_core::{Effect, ReduceContext, Reducer, Store};

#[global_allocator]
/// Counts allocations in the isolated registration assay.
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

/// Separates the committed action count from deferred factory calls.
struct ReadyEffect(Arc<AtomicUsize>);

impl Reducer for ReadyEffect {
    type State = u64;
    type Action = ();

    fn reduce(&mut self, state: &mut u64, (): &(), _context: &mut ReduceContext<'_>) -> Effect<()> {
        *state += 1;
        let calls = Arc::clone(&self.0);
        Effect::run(move |_sender| {
            calls.fetch_add(1, Ordering::Relaxed);
            std::future::ready(())
        })
    }
}

/// Checks that single-task registration avoids separate dispatch and launch boxes.
fn single_task_registration_avoids_dispatch_and_launch_boxes() {
    if allocation_test::run_in_child("single_task_registration_avoids_dispatch_and_launch_boxes") {
        return;
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let runtime = support::ManualRuntime::new();
    let store = Store::builder(0, ReadyEffect(Arc::clone(&calls)), runtime.runtime())
        .mailbox_capacity(1)
        .build()
        .expect("valid store");
    let mut actor = runtime.take_only_queued_task();
    let mut context = Context::from_waker(Waker::noop());
    assert!(actor.as_mut().poll(&mut context).is_pending());

    // First operation warms registry and host queue storage. The second measures
    // real actor registration, not an already-boxed stand-in future.
    let mut measured_blocks = 0;
    for previous_calls in 0..2 {
        let task = {
            let send = store.send(());
            let mut send = std::pin::pin!(send);
            assert!(send.as_mut().poll(&mut context).is_pending());
            let profiler =
                (previous_calls == 1).then(|| dhat::Profiler::builder().testing().build());
            let before = profiler.as_ref().map(|_| dhat::HeapStats::get());
            assert!(actor.as_mut().poll(&mut context).is_pending());
            if let Some(before) = before {
                measured_blocks = dhat::HeapStats::get().total_blocks - before.total_blocks;
            }
            drop(profiler);
            assert_eq!(calls.load(Ordering::Relaxed), previous_calls);
            let Poll::Ready(task) = send.as_mut().poll(&mut context) else {
                panic!("actor must acknowledge registration without invoking factory");
            };
            task.expect("committed action")
        };
        let mut effect = runtime.take_only_queued_task();
        assert!(effect.as_mut().poll(&mut context).is_ready());
        assert_eq!(calls.load(Ordering::Relaxed), previous_calls + 1);
        drop(effect);
        assert!(actor.as_mut().poll(&mut context).is_pending());
        assert!(task.is_finished());
        assert!(
            std::pin::pin!(task.finish())
                .as_mut()
                .poll(&mut context)
                .is_ready()
        );
    }

    let shutdown = store.shutdown_into_state();
    let mut shutdown = std::pin::pin!(shutdown);
    assert!(shutdown.as_mut().poll(&mut context).is_pending());
    assert!(actor.as_mut().poll(&mut context).is_ready());
    let Poll::Ready(state) = shutdown.as_mut().poll(&mut context) else {
        panic!("completed actor must acknowledge extraction");
    };
    assert_eq!(state.expect("extracted state"), 2);

    // Factory, lineage, completion, abort, sender and final runtime wrapper.
    // Dispatch, singleton launch and mailbox wait storage need no boxes.
    assert!(
        measured_blocks <= 6,
        "registration allocated {measured_blocks} blocks; budget is 6"
    );
}

/// Preserves the exact names of the isolated allocation cases.
const CASES: &[(&str, fn())] = &[(
    "single_task_registration_avoids_dispatch_and_launch_boxes",
    single_task_registration_avoids_dispatch_and_launch_boxes,
)];

include!("support/allocation_main.rs");
