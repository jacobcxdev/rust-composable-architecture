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

#[cfg(feature = "local")]
#[expect(
    dead_code,
    reason = "allocation assays take individual tasks instead of polling the whole host queue"
)]
#[path = "fold/local_driver.rs"]
mod local_driver;

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Waker};

use rca_core::{
    Effect, Feature, FeatureContext, OperationPolicy, OperationWait, Reduce, RuntimeFuture, Store,
    Task,
};

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

/// Declares one typed body using the existing unnamed or single-name API.
struct TypedWork {
    /// Keeps the runtime body pending until the measured polls finish.
    done: Arc<AtomicBool>,
    /// Selects the existing single-name path without changing the body.
    named: bool,
}

impl Feature for TypedWork {
    type State = ();
    type Action = ();

    fn update(&mut self, (): &mut (), (): &(), context: &mut FeatureContext<'_, (), ()>) {
        let done = Arc::clone(&self.done);
        let run = move |_| {
            std::future::poll_fn(move |_| {
                if done.load(Ordering::SeqCst) {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            })
        };
        context.task(if self.named {
            Task::run_named(0, OperationPolicy::Coexist, run)
        } else {
            Task::run(run)
        });
    }
}

/// Separates typed-task registration and polling costs for both existing cheap paths.
fn typed_task_zero_and_one_name_costs() {
    if allocation_test::run_in_child("typed_task_zero_and_one_name_costs") {
        return;
    }

    for named in [false, true] {
        let host = support::ManualRuntime::new();
        let done = Arc::new(AtomicBool::new(false));
        let feature = TypedWork {
            done: Arc::clone(&done),
            named,
        };
        let profiler = dhat::Profiler::builder().testing().build();
        let before = dhat::HeapStats::get();
        let store = Store::new((), feature, host.runtime());
        let mut actor = host.take_only_queued_task();
        assert!(poll(actor.as_mut()).is_pending());
        let constructed = dhat::HeapStats::get();
        let receipt = store.try_send(()).unwrap();
        assert!(poll(actor.as_mut()).is_pending());
        let mut body = host.take_only_queued_task();
        let registered = dhat::HeapStats::get();
        assert!(poll(body.as_mut()).is_pending());
        let started = dhat::HeapStats::get();
        for _ in 0..16 {
            assert!(poll(body.as_mut()).is_pending());
        }
        let repeated = dhat::HeapStats::get();
        assert_eq!(repeated.total_blocks, started.total_blocks);
        assert_eq!(repeated.total_bytes, started.total_bytes);

        done.store(true, Ordering::SeqCst);
        assert!(poll(body.as_mut()).is_ready());
        drop(body);
        assert!(poll(actor.as_mut()).is_pending());
        drop((receipt, store));
        assert!(poll(actor.as_mut()).is_ready());
        drop(actor);
        let finished = dhat::HeapStats::get();
        drop(profiler);
        eprintln!(
            "typed_task named={named} owner_bytes={} construction_blocks={} construction_bytes={} register_blocks={} register_bytes={} first_poll_blocks={} first_poll_bytes={} repeated_poll_blocks={} repeated_poll_bytes={} live_after_finish={}/{}",
            std::mem::size_of::<Task<(), ()>>(),
            constructed.total_blocks - before.total_blocks,
            constructed.total_bytes - before.total_bytes,
            registered.total_blocks - constructed.total_blocks,
            registered.total_bytes - constructed.total_bytes,
            started.total_blocks - registered.total_blocks,
            started.total_bytes - registered.total_bytes,
            repeated.total_blocks - started.total_blocks,
            repeated.total_bytes - started.total_bytes,
            finished.curr_blocks,
            finished.curr_bytes,
        );
    }
}

#[cfg(feature = "local")]
mod local_costs {
    //! Measures the existing Local task APIs without introducing transferable captures.

    use super::{OperationPolicy, allocation_test, local_driver, poll};
    use rca_core::local::{Feature, FeatureContext, Runtime, Store, Task};
    use std::cell::Cell;
    use std::rc::Rc;
    use std::task::Poll;

    /// Keeps one owner-local body pending under the existing zero-or-one-name API.
    struct TypedWork {
        /// Controls physical task completion without a cross-thread flag.
        done: Rc<Cell<bool>>,
        /// Selects the existing single-name path without changing the body.
        named: bool,
    }

    impl Feature for TypedWork {
        type State = ();
        type Action = ();

        fn update(&mut self, (): &mut (), (): &(), context: &mut FeatureContext<'_, (), ()>) {
            let done = Rc::clone(&self.done);
            let run = move |_| {
                std::future::poll_fn(move |_| {
                    if done.get() {
                        Poll::Ready(())
                    } else {
                        Poll::Pending
                    }
                })
            };
            context.task(if self.named {
                Task::run_named(0, OperationPolicy::Coexist, run)
            } else {
                Task::run(run)
            });
        }
    }

    /// Separates Local construction and first-poll costs from allocation-free repeated polls.
    pub(super) fn typed_task_zero_and_one_name_costs() {
        if allocation_test::run_in_child("local_typed_task_zero_and_one_name_costs") {
            return;
        }

        for named in [false, true] {
            let host = local_driver::Driver::default();
            let done = Rc::new(Cell::new(false));
            let feature = TypedWork {
                done: Rc::clone(&done),
                named,
            };
            let profiler = dhat::Profiler::builder().testing().build();
            let before = dhat::HeapStats::get();
            let store = Store::new((), feature, Runtime::new(host.clone()));
            let mut actor = host.take();
            assert!(poll(actor.as_mut()).is_pending());
            let constructed = dhat::HeapStats::get();
            let receipt = store.try_send(()).unwrap();
            assert!(poll(actor.as_mut()).is_pending());
            let mut body = host.take();
            let registered = dhat::HeapStats::get();
            assert!(poll(body.as_mut()).is_pending());
            let started = dhat::HeapStats::get();
            for _ in 0..16 {
                assert!(poll(body.as_mut()).is_pending());
            }
            let repeated = dhat::HeapStats::get();
            assert_eq!(repeated.total_blocks, started.total_blocks);
            assert_eq!(repeated.total_bytes, started.total_bytes);

            done.set(true);
            assert!(poll(body.as_mut()).is_ready());
            drop(body);
            assert!(poll(actor.as_mut()).is_pending());
            drop((receipt, store));
            assert!(poll(actor.as_mut()).is_ready());
            drop(actor);
            let finished = dhat::HeapStats::get();
            drop(profiler);
            eprintln!(
                "local_typed_task named={named} owner_bytes={} construction_blocks={} construction_bytes={} register_blocks={} register_bytes={} first_poll_blocks={} first_poll_bytes={} repeated_poll_blocks={} repeated_poll_bytes={} live_after_finish={}/{}",
                std::mem::size_of::<Task<(), ()>>(),
                constructed.total_blocks - before.total_blocks,
                constructed.total_bytes - before.total_bytes,
                registered.total_blocks - constructed.total_blocks,
                registered.total_bytes - constructed.total_bytes,
                started.total_blocks - registered.total_blocks,
                started.total_bytes - registered.total_bytes,
                repeated.total_blocks - started.total_blocks,
                repeated.total_bytes - started.total_bytes,
                finished.curr_blocks,
                finished.curr_bytes,
            );
        }
    }
}

/// Preserves the exact names of the isolated allocation cases.
const CASES: &[(&str, fn())] = &[
    (
        "unused_named_and_wait_costs_are_measured_separately",
        unused_named_and_wait_costs_are_measured_separately,
    ),
    (
        "typed_task_zero_and_one_name_costs",
        typed_task_zero_and_one_name_costs,
    ),
    #[cfg(feature = "local")]
    (
        "local_typed_task_zero_and_one_name_costs",
        local_costs::typed_task_zero_and_one_name_costs,
    ),
];

include!("support/allocation_main.rs");
