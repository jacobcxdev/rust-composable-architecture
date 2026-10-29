#![cfg(feature = "allocation-measurement")]

#[path = "../src/allocation_test/child.rs"]
mod allocation_test;

#[expect(
    dead_code,
    reason = "this manual polling test does not start executor threads"
)]
mod support;

use std::task::{Context, Poll, Waker};

use rca_core::{Effect, ReduceContext, Reducer, RuntimeFuture, Store};

#[global_allocator]
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

struct Counter;

impl Reducer for Counter {
    type State = u64;
    type Action = ();

    fn reduce(&mut self, state: &mut u64, (): &(), _context: &mut ReduceContext<'_>) -> Effect<()> {
        *state += 1;
        Effect::none()
    }
}

fn commit(store: &Store<u64, ()>, actor: &mut RuntimeFuture, context: &mut Context<'_>) {
    let send = store.send(());
    let mut send = std::pin::pin!(send);
    assert!(send.as_mut().poll(context).is_pending());
    assert!(actor.as_mut().poll(context).is_pending());
    let Poll::Ready(receipt) = send.as_mut().poll(context) else {
        panic!("manually polled actor must acknowledge the action");
    };
    drop(receipt.expect("committed action"));
}

#[cfg(feature = "test-util")]
#[expect(
    clippy::too_many_lines,
    reason = "fixed polls keep allocation windows separate from gate transitions"
)]
fn assertion_gate_wait_cost_and_lifecycle() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Wake;

    use futures_util::FutureExt as _;
    use rca_core::Reduce;
    use rca_core::testing::AssertionGate;

    #[derive(Default)]
    struct WakeCount(AtomicUsize);

    impl Wake for WakeCount {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    let runtime = support::ManualRuntime::new();
    let gate = AssertionGate::default();
    let reducer = Reduce::<u64, u8, _>::new(|state, action, _| {
        *state += u64::from(*action);
        if *action == 1 {
            Effect::send(2)
        } else {
            Effect::none()
        }
    });
    let store = Store::builder(0, reducer, runtime.runtime())
        .assertion_gate(gate.clone())
        .build()
        .expect("valid gated store");
    let mut actor = runtime.take_only_queued_task();
    let mut context = Context::from_waker(Waker::noop());
    assert!(actor.as_mut().poll(&mut context).is_pending());

    // Initialise the event before measuring one cancellable registration.
    {
        let mut warm = std::pin::pin!(gate.wait_for_pending());
        assert!(warm.as_mut().poll(&mut context).is_pending());
    };
    let notifications = Arc::new(WakeCount::default());
    let waker = Waker::from(Arc::clone(&notifications));
    let mut waiting_context = Context::from_waker(&waker);
    let before = dhat::HeapStats::get();
    let (registered, repolled, frame_bytes) = {
        let waiting = gate.wait_for_pending();
        let frame_bytes = std::mem::size_of_val(&waiting);
        let mut waiting = std::pin::pin!(waiting);
        assert!(waiting.as_mut().poll(&mut waiting_context).is_pending());
        let registered = dhat::HeapStats::get();
        assert!(waiting.as_mut().poll(&mut waiting_context).is_pending());
        (registered, dhat::HeapStats::get(), frame_bytes)
    };
    let cancelled = dhat::HeapStats::get();
    let blocks = registered.total_blocks - before.total_blocks;
    let bytes = registered.total_bytes - before.total_bytes;
    assert_eq!(blocks, 0, "warm registration must not allocate a listener");
    assert_eq!(bytes, 0);
    assert_eq!(repolled.total_blocks, registered.total_blocks);
    assert_eq!(repolled.total_bytes, registered.total_bytes);
    assert_eq!(cancelled.curr_blocks, before.curr_blocks);
    assert_eq!(cancelled.curr_bytes, before.curr_bytes);
    assert_eq!(
        Arc::strong_count(&notifications),
        2,
        "cancelled wait released its waker"
    );
    assert_eq!(notifications.0.load(Ordering::SeqCst), 0);

    let mut waiting = std::pin::pin!(gate.wait_for_pending());
    assert!(waiting.as_mut().poll(&mut waiting_context).is_pending());
    let (task, mut effect) = {
        let mut cancelled_after_wake = std::pin::pin!(gate.wait_for_pending());
        assert!(
            cancelled_after_wake
                .as_mut()
                .poll(&mut waiting_context)
                .is_pending()
        );
        let start = store.try_send(1).expect("start action fits");
        assert!(actor.as_mut().poll(&mut context).is_pending());
        let task = start
            .finish()
            .now_or_never()
            .expect("start reply ready")
            .expect("start committed");
        let mut effect = runtime.take_only_queued_task();
        assert!(effect.as_mut().poll(&mut context).is_pending());
        assert!(actor.as_mut().poll(&mut context).is_pending());
        assert_eq!(notifications.0.load(Ordering::SeqCst), 2);
        // Drop a notified waiter without polling it again.
        (task, effect)
    };
    assert_eq!(waiting.as_mut().poll(&mut waiting_context), Poll::Ready(()));
    assert_eq!(gate.inspect_pending(|action| *action), Some(2));
    assert!(
        !task.is_finished(),
        "waiting never releases the gated action"
    );

    let before_ready = dhat::HeapStats::get();
    {
        let mut ready = std::pin::pin!(gate.wait_for_pending());
        assert_eq!(ready.as_mut().poll(&mut waiting_context), Poll::Ready(()));
    };
    let after_ready = dhat::HeapStats::get();
    assert_eq!(after_ready.total_blocks, before_ready.total_blocks);
    assert_eq!(after_ready.total_bytes, before_ready.total_bytes);
    assert_eq!(
        Arc::strong_count(&notifications),
        2,
        "finished waits released their wakers"
    );

    let release = gate
        .release_pending_with_receipt()
        .expect("explicit release");
    assert!(actor.as_mut().poll(&mut context).is_pending());
    let released = release
        .finish()
        .now_or_never()
        .expect("release reply ready")
        .expect("response committed");
    assert!(effect.as_mut().poll(&mut context).is_ready());
    assert!(actor.as_mut().poll(&mut context).is_pending());
    assert!(task.is_finished());
    assert!(released.is_finished());
    assert!(!gate.has_pending());
    assert_eq!(
        notifications.0.load(Ordering::SeqCst),
        2,
        "no stale waiter was woken"
    );

    let mut shutdown = std::pin::pin!(store.shutdown_into_state());
    assert!(shutdown.as_mut().poll(&mut context).is_pending());
    assert!(actor.as_mut().poll(&mut context).is_ready());
    assert!(matches!(
        shutdown.as_mut().poll(&mut context),
        Poll::Ready(Ok(3))
    ));
    eprintln!(
        "assertion gate wait: frame_bytes={frame_bytes} warm_registration_blocks={blocks} warm_registration_bytes={bytes} repoll_blocks=0 cancellation_retained_blocks=0 ready_blocks=0",
    );
}

#[cfg(feature = "test-util")]
fn typed_inspection_preserves_frontier_without_allocating() {
    use rca_core::{Reduce, testing::TransactionGate};

    let runtime = support::ManualRuntime::new();
    let gate = TransactionGate::new();
    let reducer = Reduce::<u64, u8, _>::new(|state, action, _| {
        *state += u64::from(*action);
        Effect::none()
    });
    let store = Store::builder(0, reducer, runtime.runtime())
        .transaction_gate(gate.clone())
        .mailbox_capacity(1)
        .build()
        .expect("valid typed gated store");
    let mut actor = runtime.take_only_queued_task();
    let mut context = Context::from_waker(Waker::noop());
    assert!(actor.as_mut().poll(&mut context).is_pending());
    let receipt = store.try_send(7).expect("one action fits");
    assert!(actor.as_mut().poll(&mut context).is_pending());
    let boundary = gate.boundary().expect("action waits at typed frontier");
    assert_eq!(gate.inspect_pending(|action| *action == 7), Some(true));

    // Measure only borrowing and restoring the same payload, without actor work.
    let before = dhat::HeapStats::get();
    for _ in 0..100 {
        assert_eq!(gate.inspect_pending(|action| *action == 7), Some(true));
    }
    let after = dhat::HeapStats::get();
    assert_eq!(after.total_blocks - before.total_blocks, 0);
    assert_eq!(after.total_bytes - before.total_bytes, 0);
    assert_eq!(gate.boundary(), Some(boundary));
    assert!(gate.has_pending());
    assert_eq!(gate.inspect_pending(|action| *action), Some(7));
    let mut committed = std::pin::pin!(receipt.finish());
    assert!(committed.as_mut().poll(&mut context).is_pending());

    let release = gate
        .release_pending_with_receipt()
        .expect("inspection left the action releasable");
    assert!(actor.as_mut().poll(&mut context).is_pending());
    let Poll::Ready(Ok(task)) = committed.as_mut().poll(&mut context) else {
        panic!("released action must publish its actual commit");
    };
    assert!(task.is_finished());
    let mut released = std::pin::pin!(release.finish());
    let Poll::Ready(Ok(released_task)) = released.as_mut().poll(&mut context) else {
        panic!("release must publish its actual commit receipt");
    };
    assert!(released_task.is_finished());
    assert!(!gate.has_pending());
    assert_eq!(gate.boundary(), Some(boundary));
    assert!(gate.acknowledge(boundary));
    assert!(gate.boundary().is_none());
    assert!(actor.as_mut().poll(&mut context).is_pending());

    let mut shutdown = std::pin::pin!(store.shutdown_into_state());
    assert!(shutdown.as_mut().poll(&mut context).is_pending());
    assert!(actor.as_mut().poll(&mut context).is_ready());
    assert!(matches!(
        shutdown.as_mut().poll(&mut context),
        Poll::Ready(Ok(7))
    ));
}

#[cfg(feature = "test-util")]
fn typed_snapshots_copy_only_runtime_metadata() {
    use futures_util::FutureExt as _;
    use rca_core::{Reduce, testing::TransactionGate};

    let runtime = support::ManualRuntime::new();
    let gate = TransactionGate::new();
    let reducer = Reduce::<u64, u8, _>::new(|state, action, _| {
        *state += u64::from(*action);
        if *action == 1 {
            Effect::run(|_| std::future::pending()).cancellable("snapshot work")
        } else {
            Effect::none()
        }
    });
    let store = Store::builder(0, reducer, runtime.runtime())
        .transaction_gate(gate.clone())
        .build()
        .expect("valid typed gated store");
    let mut actor = runtime.take_only_queued_task();
    let mut context = Context::from_waker(Waker::noop());
    assert!(actor.as_mut().poll(&mut context).is_pending());

    assert!(gate.reserve_root());
    let start = store.try_send(1).expect("start action fits");
    assert!(actor.as_mut().poll(&mut context).is_pending());
    let task = start
        .finish()
        .now_or_never()
        .expect("start reply ready")
        .expect("start committed");
    assert!(gate.acknowledge(gate.boundary().expect("committed boundary")));
    let mut effect = runtime.take_only_queued_task();
    assert!(effect.as_mut().poll(&mut context).is_pending());

    let pending = store.try_send(2).expect("next action fits");
    assert!(actor.as_mut().poll(&mut context).is_pending());
    assert!(gate.has_pending());
    let diagnostics = gate.pending_diagnostics().expect("pending diagnostics");
    assert_eq!(diagnostics.cancellation_identities().len(), 1);
    assert_eq!(diagnostics.runtime().cancellation_count, 1);
    assert_eq!(diagnostics.runtime().live_task_ids.len(), 1);
    assert_eq!(gate.latest_diagnostics().as_ref(), Some(&diagnostics));

    // Compare identical runtime vectors. Full diagnostics and actor work stay
    // outside the measured window, including its non-empty identity allocation.
    let before = dhat::HeapStats::get();
    let expected = diagnostics.runtime().clone();
    let after = dhat::HeapStats::get();
    let expected_cost = (
        after.total_blocks - before.total_blocks,
        after.total_bytes - before.total_bytes,
    );
    for latest in [false, true] {
        let before = dhat::HeapStats::get();
        let snapshot = if latest {
            gate.latest_snapshot()
        } else {
            gate.pending_snapshot()
        };
        let after = dhat::HeapStats::get();
        let actual_cost = (
            after.total_blocks - before.total_blocks,
            after.total_bytes - before.total_bytes,
        );
        assert_eq!(snapshot.as_ref(), Some(&expected));
        eprintln!(
            "typed snapshot latest={latest}: actual={actual_cost:?} runtime_only={expected_cost:?}"
        );
        assert_eq!(
            actual_cost, expected_cost,
            "runtime snapshots must not clone cancellation identities: latest={latest}"
        );
    }

    assert!(gate.skip_pending());
    assert!(actor.as_mut().poll(&mut context).is_pending());
    drop(pending);
    task.cancel();
    // The actor applies the request before the runtime observes the abort.
    assert!(actor.as_mut().poll(&mut context).is_pending());
    assert!(effect.as_mut().poll(&mut context).is_ready());
    assert!(actor.as_mut().poll(&mut context).is_pending());
    assert!(task.is_finished());
    let mut shutdown = std::pin::pin!(store.shutdown_into_state());
    assert!(shutdown.as_mut().poll(&mut context).is_pending());
    assert!(actor.as_mut().poll(&mut context).is_ready());
    assert!(matches!(
        shutdown.as_mut().poll(&mut context),
        Poll::Ready(Ok(1))
    ));
}

#[test]
fn actor_control_wait_does_not_allocate_a_listener_box_per_commit() {
    if allocation_test::run_in_child(
        "actor_control_wait_does_not_allocate_a_listener_box_per_commit",
    ) {
        return;
    }

    let runtime = support::ManualRuntime::new();
    let store = Store::builder(0, Counter, runtime.runtime())
        .mailbox_capacity(1)
        .build()
        .expect("valid store");
    let mut actor = runtime.take_only_queued_task();
    let frame_bytes = std::mem::size_of_val(actor.as_ref().get_ref());
    let mut context = Context::from_waker(Waker::noop());
    assert!(actor.as_mut().poll(&mut context).is_pending());
    for _ in 0..20 {
        commit(&store, &mut actor, &mut context);
    }

    // One test in this integration binary owns the process-wide profiler. Both
    // receipt and actor are polled synchronously: no executor scheduling noise.
    let profiler = dhat::Profiler::builder().testing().build();
    let before = dhat::HeapStats::get();
    for _ in 0..100 {
        commit(&store, &mut actor, &mut context);
    }
    let after = dhat::HeapStats::get();
    #[cfg(feature = "test-util")]
    assertion_gate_wait_cost_and_lifecycle();
    #[cfg(feature = "test-util")]
    typed_inspection_preserves_frontier_without_allocating();
    #[cfg(feature = "test-util")]
    typed_snapshots_copy_only_runtime_metadata();
    drop(profiler);

    let shutdown = store.shutdown_into_state();
    let mut shutdown = std::pin::pin!(shutdown);
    assert!(shutdown.as_mut().poll(&mut context).is_pending());
    assert!(actor.as_mut().poll(&mut context).is_ready());
    let Poll::Ready(state) = shutdown.as_mut().poll(&mut context) else {
        panic!("completed actor must acknowledge extraction");
    };
    assert_eq!(state.expect("extracted state"), 120);

    let blocks = after.total_blocks - before.total_blocks;
    // Each action owns one commit reply. The shared mailbox/control listener
    // belongs inline in the actor's suspended future.
    assert!(
        blocks <= 100,
        "100 committed actions allocated {blocks} blocks; budget is 100 (actor frame {frame_bytes} bytes)",
    );
}
