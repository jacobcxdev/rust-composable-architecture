//! Measures reporting costs separately from setup, shutdown and parallel helper tests.

#![cfg(feature = "allocation-measurement")]

#[expect(dead_code, reason = "manual polling does not start executor threads")]
mod support;

/// Reuses exact-test child isolation before any allocation window starts.
#[cfg(not(miri))]
mod subprocess {
    include!("../src/allocation_test/child.rs");
}

use rca_core::{Effect, Issue, IssueReporter, ReduceContext, Reducer, RuntimeFuture, Store};
use rca_core::{IssueCallback, IssueContext, IssueDeliveryFailure, IssueReporters};
use std::hint::black_box;
use std::task::{Context, Poll, Waker};

/// Counts process-wide allocations only while the assay's profiler is active.
#[global_allocator]
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

/// Captures the selected reporter without allocating an effect task.
struct Counter;

impl Reducer for Counter {
    type State = u64;
    type Action = ();

    fn reduce(&mut self, state: &mut u64, (): &(), context: &mut ReduceContext<'_>) -> Effect<()> {
        *state += 1;
        let reporter = context.issue_reporter();
        // Capturing into an unboxed future is the reporting-specific cost,
        // independent of the ordinary effect scheduler's own allocation costs.
        drop(black_box(async move {
            drop(reporter);
        }));
        Effect::none()
    }
}

/// Polls one admitted action and its commit reply without starting executor threads.
fn commit(store: &Store<u64, ()>, actor: &mut RuntimeFuture, context: &mut Context<'_>) {
    let send = store.send(());
    let mut send = std::pin::pin!(send);
    assert!(send.as_mut().poll(context).is_pending());
    assert!(actor.as_mut().poll(context).is_pending());
    let Poll::Ready(result) = send.as_mut().poll(context) else {
        panic!("committed action");
    };
    drop(result.unwrap());
}

/// Measures delivery after endpoint warm-up and owned payload construction.
fn owned_endpoint_allocations(accepted: bool, count: usize) -> u64 {
    let endpoint = if accepted {
        IssueReporter::new(|_| {})
    } else {
        IssueReporter::default()
    };
    // Exclude first-use endpoint admission costs from the clone window.
    drop(endpoint.report(Issue::new("warm", "borrowed")));
    let context = IssueReporters::new(std::iter::repeat_n(endpoint, count));
    let issue = Issue::new("owned", String::from("original message"))
        .with_cause(String::from("original cause"));
    let before = dhat::HeapStats::get();
    let outcomes = black_box(context.report(black_box(issue)));
    let after = dhat::HeapStats::get();
    drop(outcomes);
    after.total_blocks - before.total_blocks
}

/// Checks that disabled reporting and issue-free reductions require no heap work.
#[expect(
    clippy::too_many_lines,
    reason = "one profiler owns the related allocation windows and drops before assertions"
)]
fn disabled_reporting_and_issue_free_reductions_add_no_heap_allocations() {
    #[cfg(not(miri))]
    if subprocess::run_in_child(
        "disabled_reporting_and_issue_free_reductions_add_no_heap_allocations",
    ) {
        return;
    }

    let mut stores = Vec::new();
    for enabled in [false, true] {
        let runtime = support::ManualRuntime::new();
        let builder = Store::builder(0, Counter, runtime.runtime()).mailbox_capacity(1);
        let builder = if enabled {
            builder.with_issue_handler(|_| {})
        } else {
            builder
        };
        let store = builder.build().unwrap();
        let mut actor = runtime.take_only_queued_task();
        let mut context = Context::from_waker(Waker::noop());
        assert!(actor.as_mut().poll(&mut context).is_pending());
        for _ in 0..20 {
            commit(&store, &mut actor, &mut context);
        }
        stores.push((store, actor, runtime));
    }
    let profiler = dhat::Profiler::builder().testing().build();
    let before = dhat::HeapStats::get();
    for _ in 0..100 {
        let reporter = black_box(IssueReporter::default());
        let captured = reporter.clone();
        drop(black_box(async move {
            drop(captured);
        }));
        assert!(
            reporter
                .report(Issue::new("disabled", "borrowed"))
                .unwrap_err()
                .is_disabled()
        );
    }
    let disabled = dhat::HeapStats::get();
    let mut windows = Vec::with_capacity(2);
    for (store, actor, _) in &mut stores {
        let mut context = Context::from_waker(Waker::noop());
        let before = dhat::HeapStats::get();
        for _ in 0..100 {
            commit(store, actor, &mut context);
        }
        let after = dhat::HeapStats::get();
        windows.push(after.total_blocks - before.total_blocks);
    }
    let mut operation_windows = [[0; 3]; 2];
    for (row, rejected) in [false, true].into_iter().enumerate() {
        for (column, count) in [0, 1, 3].into_iter().enumerate() {
            let callback = |_: &Issue, (): &()| {
                if rejected {
                    Err(IssueDeliveryFailure::<()>::Disabled)
                } else {
                    Ok(())
                }
            };
            let reporters = [&callback as &IssueCallback<'_, (), ()>; 3];
            let context = IssueContext::new(&reporters[..count]);
            let issue = Issue::new("owned", String::from("original message"))
                .with_cause(String::from("original cause"));
            let before = dhat::HeapStats::get();
            let deliveries = black_box(context.report(black_box(issue), &()));
            let after = dhat::HeapStats::get();
            operation_windows[row][column] = after.total_blocks - before.total_blocks;
            drop(deliveries);
        }
    }
    let mut endpoint_windows = [[0; 3]; 2];
    for (row, accepted) in [false, true].into_iter().enumerate() {
        for (column, count) in [0, 1, 3].into_iter().enumerate() {
            endpoint_windows[row][column] = owned_endpoint_allocations(accepted, count);
        }
    }
    drop(profiler);
    assert_eq!(
        disabled.total_blocks - before.total_blocks,
        0,
        "disabled construction, capture and rejection must stay allocation-free"
    );
    assert_eq!(
        windows,
        [100, 100],
        "only existing per-action commit replies may allocate, with reporting disabled or enabled"
    );
    assert_eq!(
        operation_windows[0],
        [1, 1, 1],
        "one outcome vector, with no growth or issue clones"
    );
    assert_eq!(
        operation_windows[1],
        [1, 1, 5],
        "the final rejection moves its two owned strings"
    );
    assert_eq!(
        endpoint_windows,
        [[1, 1, 5]; 2],
        "only prefix endpoints copy the owned strings"
    );
}

/// Preserves the exact names of the isolated allocation cases.
const CASES: &[(&str, fn())] = &[(
    "disabled_reporting_and_issue_free_reductions_add_no_heap_allocations",
    disabled_reporting_and_issue_free_reductions_add_no_heap_allocations,
)];

include!("support/allocation_main.rs");
