//! Measures ordered reads and repeated observation takes without combining their cost windows.

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

/// Advances the selected state once per committed action without creating effects.
struct Counter;

impl Reducer for Counter {
    type State = u64;
    type Action = ();

    fn reduce(&mut self, state: &mut u64, (): &(), _: &mut ReduceContext<'_>) -> Effect<()> {
        *state += 1;
        Effect::none()
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one profiling session verifies read costs, registration retention and two take round trips"
)]
fn committed_observation_takes_reuse_operation_allocation() {
    // Exclude the support module's tests from this process-wide allocation count.
    if allocation_test::run_in_child("committed_observation_takes_reuse_operation_allocation") {
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
    let profiler;
    let read_costs = {
        let mut read_once = || {
            let mut read = std::pin::pin!(store.read(|state| *state));
            assert!(read.as_mut().poll(&mut context).is_pending());
            assert!(actor.as_mut().poll(&mut context).is_pending());
            assert!(matches!(
                read.as_mut().poll(&mut context),
                Poll::Ready(Ok(0))
            ));
        };
        read_once();

        // Keep one profiler for this binary. Warm actor and mailbox storage first.
        profiler = dhat::Profiler::builder().testing().build();
        let before = dhat::HeapStats::get();
        for _ in 0..32 {
            read_once();
        }
        let after = dhat::HeapStats::get();
        (
            after.total_blocks - before.total_blocks,
            after.total_bytes - before.total_bytes,
            after.curr_blocks - before.curr_blocks,
            after.curr_bytes - before.curr_bytes,
        )
    };

    // Include registration-owned storage so transient savings cannot hide
    // increased idle retention. Actor construction stays outside this session.
    let mut values = {
        let registration = store.observe(|state| *state);
        let mut registration = std::pin::pin!(registration);
        assert!(registration.as_mut().poll(&mut context).is_pending());
        assert!(actor.as_mut().poll(&mut context).is_pending());
        let Poll::Ready(values) = registration.as_mut().poll(&mut context) else {
            panic!("actor must acknowledge registration");
        };
        values.expect("registered observation")
    };
    assert_eq!(futures_executor::block_on(values.next()), Some(0));
    let registered = dhat::HeapStats::get();
    let task = {
        let send = store.send(());
        let mut send = std::pin::pin!(send);
        assert!(send.as_mut().poll(&mut context).is_pending());
        assert!(actor.as_mut().poll(&mut context).is_pending());
        let Poll::Ready(task) = send.as_mut().poll(&mut context) else {
            panic!("actor must acknowledge committed update");
        };
        task.expect("committed update")
    };
    drop(task);

    // Isolate request creation from actor bookkeeping, then complete the take.
    let before = dhat::HeapStats::get();
    let requested = {
        let next = values.next();
        let mut next = std::pin::pin!(next);
        assert!(next.as_mut().poll(&mut context).is_pending());
        let requested = dhat::HeapStats::get();
        assert!(actor.as_mut().poll(&mut context).is_pending());
        assert_eq!(next.as_mut().poll(&mut context), Poll::Ready(Some(1)));
        requested
    };
    let completed = dhat::HeapStats::get();
    let second_task = {
        let send = store.send(());
        let mut send = std::pin::pin!(send);
        assert!(send.as_mut().poll(&mut context).is_pending());
        assert!(actor.as_mut().poll(&mut context).is_pending());
        let Poll::Ready(task) = send.as_mut().poll(&mut context) else {
            panic!("actor must acknowledge the second committed update");
        };
        task.expect("second committed update")
    };
    drop(second_task);
    let before_reuse = dhat::HeapStats::get();
    let reused = {
        let next = values.next();
        let mut next = std::pin::pin!(next);
        assert!(next.as_mut().poll(&mut context).is_pending());
        let requested = dhat::HeapStats::get();
        assert!(actor.as_mut().poll(&mut context).is_pending());
        assert_eq!(next.as_mut().poll(&mut context), Poll::Ready(Some(2)));
        requested
    };
    let completed_reuse = dhat::HeapStats::get();
    drop(profiler);
    drop(values);
    let shutdown = store.shutdown_into_state();
    let mut shutdown = std::pin::pin!(shutdown);
    assert!(shutdown.as_mut().poll(&mut context).is_pending());
    assert!(actor.as_mut().poll(&mut context).is_ready());
    assert!(matches!(
        shutdown.as_mut().poll(&mut context),
        Poll::Ready(Ok(2))
    ));

    // Reuse the operation allocation; only the single reply slot is new.
    // Avoid platform-specific byte or private structure-size assertions.
    eprintln!(
        "32 ordered reads: blocks={} bytes={} retained_blocks={} retained_bytes={}",
        read_costs.0, read_costs.1, read_costs.2, read_costs.3
    );
    assert_eq!((read_costs.2, read_costs.3), (0, 0));
    assert!(
        read_costs.0 <= 64,
        "each read needs only its command and one reply slot"
    );
    let blocks = requested.total_blocks - before.total_blocks;
    eprintln!(
        "take request: blocks={blocks} bytes={} registered_live={}/{} completed_live={}/{} handle_bytes={}",
        requested.total_bytes - before.total_bytes,
        registered.curr_blocks,
        registered.curr_bytes,
        completed.curr_blocks,
        completed.curr_bytes,
        std::mem::size_of::<rca_core::Observation<std::cell::Cell<u64>>>(),
    );
    eprintln!(
        "reused take: blocks={} bytes={} completed_live={}/{}",
        reused.total_blocks - before_reuse.total_blocks,
        reused.total_bytes - before_reuse.total_bytes,
        completed_reuse.curr_blocks,
        completed_reuse.curr_bytes,
    );
    assert!(
        blocks <= 1,
        "take request allocated {blocks} blocks; budget is 1"
    );
    // Completing the take and waiting for another command add no allocations.
    let total_blocks = completed.total_blocks - before.total_blocks;
    assert!(
        total_blocks <= 1,
        "completed take allocated {total_blocks} blocks; budget is 1"
    );
    let reused_blocks = completed_reuse.total_blocks - before_reuse.total_blocks;
    assert!(
        reused_blocks <= 1,
        "reused take allocated {reused_blocks} blocks; budget is 1"
    );
}
