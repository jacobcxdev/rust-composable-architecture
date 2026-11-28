//! Measures added nested-task costs separately from existing zero-or-one-name paths.

#![cfg(feature = "allocation-measurement")]

#[path = "../src/allocation_test/child.rs"]
mod allocation_test;

#[expect(
    dead_code,
    reason = "the allocation target uses only independently captured manual tasks"
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

/// Measures process-wide allocation without a concurrent libtest receiver thread.
#[global_allocator]
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

/// Polls only the selected future and never advances another owned task.
fn poll<T>(future: Pin<&mut (impl Future<Output = T> + ?Sized)>) -> Poll<T> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}

/// Keeps both carrier assays on the same finite registration and cancellation schedule.
macro_rules! nested_case {
    () => {
        use super::{Arc, AtomicBool, Ordering, Poll, poll};
        use rca_core::OperationPolicy;

        /// Declares a pending body inside two or three nested operation groups.
        struct NestedWork {
            /// Proves that cancellation, not ordinary completion, ends the body.
            done: Arc<AtomicBool>,
            /// Selects two names or the deeper declaration path.
            depth: i32,
        }

        impl Feature for NestedWork {
            type State = ();
            type Action = ();

            fn update(&mut self, (): &mut (), (): &(), context: &mut FeatureContext<'_, (), ()>) {
                let done = Arc::clone(&self.done);
                let mut task = Task::run(move |_| {
                    std::future::poll_fn(move |_| {
                        if done.load(Ordering::SeqCst) {
                            Poll::Ready(())
                        } else {
                            Poll::Pending
                        }
                    })
                });
                for key in 0..self.depth {
                    task = task.named(key, OperationPolicy::Coexist);
                }
                context.task(task);
            }
        }

        /// Processes an ordered request while every runtime body remains separately retained.
        fn ordered<T>(actor: &mut RuntimeFuture, request: impl Future<Output = T>) -> T {
            let mut request = std::pin::pin!(request);
            assert!(poll(request.as_mut()).is_pending());
            assert!(poll(actor.as_mut()).is_pending());
            match poll(request.as_mut()) {
                Poll::Ready(value) => value,
                Poll::Pending => panic!("nested task request has no actor reply"),
            }
        }

        /// Records added registration costs and requires allocation-free repeated polls.
        pub(super) fn measure() {
            for depth in [2, 3] {
                let host = host();
                let done = Arc::new(AtomicBool::new(false));
                let feature = NestedWork {
                    done: Arc::clone(&done),
                    depth,
                };
                let profiler = dhat::Profiler::builder().testing().build();
                let before = dhat::HeapStats::get();
                let store = Store::new((), feature, runtime(&host));
                let mut actor = take(&host);
                assert!(poll(actor.as_mut()).is_pending());
                let constructed = dhat::HeapStats::get();
                let receipt = store.try_send(()).unwrap();
                assert!(poll(actor.as_mut()).is_pending());
                let mut body = take(&host);
                let registered = dhat::HeapStats::get();
                assert!(poll(body.as_mut()).is_pending());
                let started = dhat::HeapStats::get();
                for _ in 0..16 {
                    assert!(poll(body.as_mut()).is_pending());
                }
                let repeated = dhat::HeapStats::get();
                assert_eq!(repeated.total_blocks, started.total_blocks);
                assert_eq!(repeated.total_bytes, started.total_bytes);

                for key in 0..depth {
                    let operation = store.operation(key);
                    assert_eq!(ordered(&mut actor, operation.status()).unwrap().active_count(), 1);
                }
                let operation = store.operation(depth - 1);
                // Unlike the single-name test, this capture count includes the test's Box::pin.
                let capture_before = dhat::HeapStats::get();
                let mut captured = Box::pin(ordered(&mut actor, operation.capture()).unwrap());
                let capture_after = dhat::HeapStats::get();
                assert!(poll(captured.as_mut()).is_pending());
                let wait_started = dhat::HeapStats::get();
                for _ in 0..16 {
                    assert!(poll(captured.as_mut()).is_pending());
                }
                let wait_repeated = dhat::HeapStats::get();
                assert_eq!(wait_repeated.total_blocks, wait_started.total_blocks);
                assert_eq!(wait_repeated.total_bytes, wait_started.total_bytes);

                // At depth three this addresses the third declaration, not either inner name.
                let cancel_before = dhat::HeapStats::get();
                ordered(&mut actor, operation.cancel()).unwrap();
                let cancel_after = dhat::HeapStats::get();
                assert!(poll(captured.as_mut()).is_pending());
                assert_eq!(ordered(&mut actor, operation.status()).unwrap().active_count(), 1);
                assert!(poll(body.as_mut()).is_ready());
                assert!(!done.load(Ordering::SeqCst));
                assert_eq!(poll(captured.as_mut()), Poll::Ready(Ok(())));
                for key in 0..depth {
                    assert_eq!(ordered(&mut actor, store.operation(key).status()).unwrap().active_count(), 0);
                }

                drop((captured, body, operation, receipt, store));
                assert!(poll(actor.as_mut()).is_ready());
                drop((actor, host));
                let finished = dhat::HeapStats::get();
                drop(profiler);
                eprintln!(
                    "nested_typed_task domain={} depth={depth} owner_bytes={} construction_blocks={} construction_bytes={} register_blocks={} register_bytes={} first_poll_blocks={} first_poll_bytes={} repeated_poll_blocks={} repeated_poll_bytes={} capture_blocks={} capture_bytes={} cancel_blocks={} cancel_bytes={} live_after_finish={}/{}",
                    module_path!(),
                    std::mem::size_of::<Task<(), ()>>(),
                    constructed.total_blocks - before.total_blocks,
                    constructed.total_bytes - before.total_bytes,
                    registered.total_blocks - constructed.total_blocks,
                    registered.total_bytes - constructed.total_bytes,
                    started.total_blocks - registered.total_blocks,
                    started.total_bytes - registered.total_bytes,
                    repeated.total_blocks - started.total_blocks,
                    repeated.total_bytes - started.total_bytes,
                    capture_after.total_blocks - capture_before.total_blocks,
                    capture_after.total_bytes - capture_before.total_bytes,
                    cancel_after.total_blocks - cancel_before.total_blocks,
                    cancel_after.total_bytes - cancel_before.total_bytes,
                    finished.curr_blocks,
                    finished.curr_bytes,
                );
            }
        }
    };
}

mod shared {
    //! Uses the existing Shared manual host without worker threads.

    use rca_core::{Feature, FeatureContext, Runtime, RuntimeFuture, Store, Task};

    /// Creates the existing manually polled Shared host.
    fn host() -> super::support::ManualRuntime {
        super::support::ManualRuntime::new()
    }

    /// Shares that host with the Store without starting background workers.
    fn runtime(host: &super::support::ManualRuntime) -> Runtime {
        host.runtime()
    }

    /// Separates the actor or body from the host queue before measurement.
    fn take(host: &super::support::ManualRuntime) -> RuntimeFuture {
        host.take_only_queued_task()
    }

    nested_case!();
}

#[cfg(feature = "local")]
mod local {
    //! Uses the existing Local manual host on its owner thread.

    use rca_core::local::{Feature, FeatureContext, Runtime, RuntimeFuture, Store, Task};

    /// Creates the existing manually polled owner-thread host.
    fn host() -> super::local_driver::Driver {
        super::local_driver::Driver::default()
    }

    /// Shares that host with the Store while preserving its owner thread.
    fn runtime(host: &super::local_driver::Driver) -> Runtime {
        Runtime::new(host.clone())
    }

    /// Separates the actor or body from the local queue before measurement.
    fn take(host: &super::local_driver::Driver) -> RuntimeFuture {
        host.take()
    }

    nested_case!();
}

/// Runs the Shared nested-name assay in its isolated allocation process.
fn shared_nested_task_costs() {
    if !allocation_test::run_in_child("shared_nested_task_costs") {
        shared::measure();
    }
}

/// Runs the Local nested-name assay in its isolated allocation process.
#[cfg(feature = "local")]
fn local_nested_task_costs() {
    if !allocation_test::run_in_child("local_nested_task_costs") {
        local::measure();
    }
}

/// Names every allocation case for the existing synchronous test runner.
const CASES: &[(&str, fn())] = &[
    ("shared_nested_task_costs", shared_nested_task_costs),
    #[cfg(feature = "local")]
    ("local_nested_task_costs", local_nested_task_costs),
];

include!("support/allocation_main.rs");
