//! Separates warmed observation-wait allocations from terminal owner retention.

#![cfg(feature = "allocation-measurement")]

#[path = "../src/allocation_test/child.rs"]
mod allocation_test;

#[expect(dead_code, reason = "this allocation test manually polls the actor")]
mod support;

use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use rca_core::{Effect, Observation, ReduceContext, Reducer, Store};

/// Records the isolated test process's allocation events during profiling.
#[global_allocator]
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

/// Produces distinct selected values without spawning additional effect tasks.
struct Counter;

impl Reducer for Counter {
    type State = u64;
    type Action = ();

    fn reduce(&mut self, state: &mut u64, (): &(), _: &mut ReduceContext<'_>) -> Effect<()> {
        *state += 1;
        Effect::none()
    }
}

/// Counts real notifications after cancelled demand and before terminal delivery.
#[derive(Default)]
struct WakeCount(AtomicUsize);

impl Wake for WakeCount {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::AcqRel);
    }
}

#[test]
fn waiting_observation_avoids_a_second_listener_allocation() {
    // Exclude the support module's tests from this process-wide allocation count.
    if allocation_test::run_in_child("waiting_observation_avoids_a_second_listener_allocation") {
        return;
    }

    // One process-wide session also tracks the reusable take operation created
    // during registration. Waiting costs still use a warmed, isolated delta.
    let profiler = dhat::Profiler::builder().testing().build();
    let runtime = support::ManualRuntime::new();
    let store = Store::builder(0, Counter, runtime.runtime())
        .mailbox_capacity(1)
        .build()
        .expect("valid store");
    let mut actor = runtime.take_only_queued_task();
    let mut actor_context = Context::from_waker(Waker::noop());
    assert!(actor.as_mut().poll(&mut actor_context).is_pending());
    let mut values = {
        let registration = store.observe(|state| Cell::new(*state));
        let mut registration = std::pin::pin!(registration);
        assert!(registration.as_mut().poll(&mut actor_context).is_pending());
        assert!(actor.as_mut().poll(&mut actor_context).is_pending());
        let Poll::Ready(values) = registration.as_mut().poll(&mut actor_context) else {
            panic!("actor must acknowledge registration");
        };
        values.expect("registered observation")
    };
    assert_eq!(futures_executor::block_on(values.next()).unwrap().get(), 0);
    let notifications = Arc::new(WakeCount::default());
    let waker = Waker::from(Arc::clone(&notifications));
    let mut observation_context = Context::from_waker(&waker);

    // The first round warms waiting and extraction; only the second round's
    // entry into waiting is the waiting-allocation oracle.
    let mut waiting_blocks = 0;
    let mut waiting_bytes = 0;
    let mut waiting_retained_blocks = 0;
    let mut waiting_retained_bytes = 0;
    for expected in [1, 2] {
        let before = dhat::HeapStats::get();
        let waiting = {
            let next = values.next();
            let mut next = std::pin::pin!(next);
            assert!(next.as_mut().poll(&mut observation_context).is_pending());
            dhat::HeapStats::get()
        };
        if expected == 2 {
            waiting_blocks = waiting.total_blocks - before.total_blocks;
            waiting_bytes = waiting.total_bytes - before.total_bytes;
            waiting_retained_blocks = waiting.curr_blocks as i128 - before.curr_blocks as i128;
            waiting_retained_bytes = waiting.curr_bytes as i128 - before.curr_bytes as i128;
        }

        // Cancelled next() leaves its listener registered inside Observation.
        // No actor or update work ran inside the measured waiting window.
        let before_wakes = notifications.0.load(Ordering::Acquire);
        let receipt = store.try_send(()).expect("update fits");
        assert!(actor.as_mut().poll(&mut actor_context).is_pending());
        let task = futures_executor::block_on(receipt.finish()).expect("update committed");
        assert!(task.is_finished());
        assert!(notifications.0.load(Ordering::Acquire) > before_wakes);

        let resumed = values.next();
        let mut resumed = std::pin::pin!(resumed);
        assert!(resumed.as_mut().poll(&mut observation_context).is_pending());
        assert!(actor.as_mut().poll(&mut actor_context).is_pending());
        let Poll::Ready(Some(value)) = resumed.as_mut().poll(&mut observation_context) else {
            panic!("resumed observation must deliver the committed update");
        };
        assert_eq!(value.get(), expected);
    }
    let closed = {
        let pending = values.next();
        let mut pending = std::pin::pin!(pending);
        assert!(pending.as_mut().poll(&mut observation_context).is_pending());
        let before_close_wakes = notifications.0.load(Ordering::Acquire);
        let shutdown = store.shutdown_into_state();
        let mut shutdown = std::pin::pin!(shutdown);
        assert!(shutdown.as_mut().poll(&mut actor_context).is_pending());
        assert!(actor.as_mut().poll(&mut actor_context).is_ready());
        assert!(matches!(
            shutdown.as_mut().poll(&mut actor_context),
            Poll::Ready(Ok(2))
        ));
        assert!(notifications.0.load(Ordering::Acquire) > before_close_wakes);
        pending.as_mut().poll(&mut observation_context)
    };
    assert!(matches!(closed, Poll::Ready(None)));
    let closed_owner = dhat::HeapStats::get();
    drop(values);
    let released_owner = dhat::HeapStats::get();
    let closed_owner_blocks = closed_owner.curr_blocks - released_owner.curr_blocks;
    let closed_owner_bytes = closed_owner.curr_bytes - released_owner.curr_bytes;
    drop(profiler);

    eprintln!(
        "observation waiting: blocks={waiting_blocks} bytes={waiting_bytes} retained_blocks_delta={waiting_retained_blocks} retained_bytes_delta={waiting_retained_bytes} observation_cell_size={}",
        std::mem::size_of::<Observation<Cell<u64>>>(),
    );
    eprintln!("closed observation owner: blocks={closed_owner_blocks} bytes={closed_owner_bytes}");
    assert!(
        waiting_blocks <= 1,
        "entering observation waiting allocated {waiting_blocks} blocks; budget is 1",
    );
    assert!(
        closed_owner_blocks <= 4,
        "closed observation retained {closed_owner_blocks} blocks; signal metadata budget is 4",
    );
}
