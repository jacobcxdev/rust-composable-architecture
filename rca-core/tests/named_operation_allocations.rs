//! Separates named-work construction, polling, observation and cancellation costs.

#![cfg(feature = "allocation-measurement")]

#[path = "../src/allocation_test/child.rs"]
mod allocation_test;

#[expect(
    dead_code,
    reason = "only manual scheduling is used in this allocation target"
)]
#[path = "support/mod.rs"]
mod support;

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Waker};

use rca_core::{Effect, OperationPolicy, OperationWait, Reduce, RuntimeFuture, Store};

/// Counts heap work only while the single assay's profiler is active.
#[global_allocator]
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

/// Polls one selected future without advancing the actor or its body.
fn poll<T>(future: Pin<&mut (impl Future<Output = T> + ?Sized)>) -> Poll<T> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}

/// Uses a stack-pinned request so the helper adds no request-box allocation.
fn ordered<T>(actor: &mut RuntimeFuture, request: impl Future<Output = T>) -> T {
    let mut request = std::pin::pin!(request);
    assert!(poll(request.as_mut()).is_pending());
    assert!(poll(actor.as_mut()).is_pending());
    match poll(request.as_mut()) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("missing reply"),
    }
}

/// Measures only guard destruction, after capture creation and guard construction.
fn cancel_capture_without_allocating(wait: OperationWait) {
    let cancelling = wait.finish_cancelling_on_drop();
    let before = dhat::HeapStats::get();
    drop(cancelling);
    let after = dhat::HeapStats::get();
    assert_eq!(
        after.total_blocks, before.total_blocks,
        "cancelling a captured set on drop must not allocate"
    );
    assert_eq!(
        after.total_bytes, before.total_bytes,
        "cancellation cleanup must not allocate heap storage"
    );
}

/// Adds zero to three named ancestors around one independently controlled body.
///
/// Keys stay `i32`, matching the endpoint's unsuffixed `0` in the assay.
fn named_pending_work(done: Arc<AtomicBool>, depth: i32) -> Effect<()> {
    let mut effect = Effect::run(move |_| {
        std::future::poll_fn(move |_| {
            if done.load(Ordering::SeqCst) {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
    });
    for key in 0..depth {
        effect = effect.named(key, OperationPolicy::Coexist);
    }
    effect
}

/// Requires allocation-free repeated polls and cancellation, with separate setup totals.
/// Measures unused named-operation and wait costs separately.
fn unused_named_and_wait_costs_are_measured_separately() {
    if allocation_test::run_in_child("unused_named_and_wait_costs_are_measured_separately") {
        return;
    }

    for depth in 0..=3 {
        let host = support::ManualRuntime::new();
        let done = Arc::new(AtomicBool::new(false));
        let release = Arc::clone(&done);
        let reducer = Reduce::<(), (), _>::new(move |(), (), _| {
            named_pending_work(Arc::clone(&release), depth)
        });
        let profiler = dhat::Profiler::builder().testing().build();
        let construction_before = dhat::HeapStats::get();
        let store = Store::new((), reducer, host.runtime());
        let mut actor = host.take_only_queued_task();
        assert!(poll(actor.as_mut()).is_pending());
        let constructed = dhat::HeapStats::get();
        let before = dhat::HeapStats::get();
        let receipt = store.try_send(()).unwrap();
        assert!(poll(actor.as_mut()).is_pending());
        let mut body = host.take_only_queued_task();
        assert!(poll(body.as_mut()).is_pending());
        let installed = dhat::HeapStats::get();
        for _ in 0..16 {
            assert!(poll(body.as_mut()).is_pending());
        }
        let polled = dhat::HeapStats::get();
        assert_eq!(
            polled.total_blocks, installed.total_blocks,
            "managed ancestry installation must not allocate per poll, including empty ancestry"
        );
        assert_eq!(polled.total_bytes, installed.total_bytes);

        let endpoint_before = dhat::HeapStats::get();
        let operation = store.operation(0);
        let endpoint = dhat::HeapStats::get();
        let status_before = dhat::HeapStats::get();
        let status = ordered(&mut actor, operation.status()).unwrap();
        assert_eq!(status.active_count(), usize::from(depth != 0));
        let status_after = dhat::HeapStats::get();
        let capture_before = dhat::HeapStats::get();
        let wait = ordered(&mut actor, operation.capture()).unwrap();
        let capture_after = dhat::HeapStats::get();
        let mut wait = std::pin::pin!(wait);
        let first_poll_before = dhat::HeapStats::get();
        let first = poll(wait.as_mut());
        assert_eq!(first.is_ready(), depth == 0);
        let first_poll_after = dhat::HeapStats::get();
        for _ in 0..16 {
            assert_eq!(poll(wait.as_mut()), first);
        }
        let repeated = dhat::HeapStats::get();
        assert_eq!(
            repeated.total_blocks, first_poll_after.total_blocks,
            "repeated wait polls reuse the single registered listener"
        );
        assert_eq!(repeated.total_bytes, first_poll_after.total_bytes);

        cancel_capture_without_allocating(ordered(&mut actor, operation.capture()).unwrap());

        // Apply the drop's control request before any explicit cancellation.
        // Named work must stop while its ordinary completion flag is still false.
        assert!(poll(actor.as_mut()).is_pending());
        assert_eq!(poll(body.as_mut()).is_ready(), depth != 0);
        assert!(!done.load(Ordering::SeqCst));

        let cancel_before = dhat::HeapStats::get();
        ordered(&mut actor, operation.cancel()).unwrap();
        let cancel_after = dhat::HeapStats::get();
        done.store(true, Ordering::SeqCst);
        if depth == 0 {
            assert!(poll(body.as_mut()).is_ready());
        }
        assert_eq!(poll(wait.as_mut()), Poll::Ready(Ok(())));
        assert!(poll(actor.as_mut()).is_pending());
        let finished = dhat::HeapStats::get();
        drop(profiler);
        eprintln!(
            "depth={depth} construction_blocks={} construction_bytes={} install_blocks={} install_bytes={} poll_blocks={} endpoint_blocks={} endpoint_bytes={} status_blocks={} status_bytes={} capture_blocks={} capture_bytes={} cancel_blocks={} cancel_bytes={} first_wait_blocks={} first_wait_bytes={} live_after_finish={}/{}",
            constructed.total_blocks - construction_before.total_blocks,
            constructed.total_bytes - construction_before.total_bytes,
            installed.total_blocks - before.total_blocks,
            installed.total_bytes - before.total_bytes,
            polled.total_blocks - installed.total_blocks,
            endpoint.total_blocks - endpoint_before.total_blocks,
            endpoint.total_bytes - endpoint_before.total_bytes,
            status_after.total_blocks - status_before.total_blocks,
            status_after.total_bytes - status_before.total_bytes,
            capture_after.total_blocks - capture_before.total_blocks,
            capture_after.total_bytes - capture_before.total_bytes,
            cancel_after.total_blocks - cancel_before.total_blocks,
            cancel_after.total_bytes - cancel_before.total_bytes,
            first_poll_after.total_blocks - first_poll_before.total_blocks,
            first_poll_after.total_bytes - first_poll_before.total_bytes,
            finished.curr_blocks,
            finished.curr_bytes
        );
        drop((operation, receipt, store));
        assert!(poll(actor.as_mut()).is_ready());
    }
}

/// Preserves the exact names of the isolated allocation cases.
const CASES: &[(&str, fn())] = &[(
    "unused_named_and_wait_costs_are_measured_separately",
    unused_named_and_wait_costs_are_measured_separately,
)];

include!("support/allocation_main.rs");
