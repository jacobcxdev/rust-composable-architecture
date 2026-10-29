//! Measures owned diagnostic delivery independently of effect launch and outcome storage.

#![cfg(all(feature = "allocation-measurement", feature = "test-util"))]

#[path = "../src/allocation_test/child.rs"]
mod allocation_test;

#[expect(dead_code, reason = "this test manually polls actor and effect tasks")]
mod support;

use std::convert::Infallible;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Waker};

use rca_core::{
    Effect, EffectError, Issue, IssueReporter, ReduceContext, Reducer, Store,
    testing::IssueRecorder,
};
use rca_core::{IssueDelivery, OperationReport};

#[global_allocator]
/// Counts diagnostic delivery allocations in an isolated test process.
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

/// Launches one prepared effect without constructing it during reduction.
struct Once(Option<Effect<()>>);

impl Reducer for Once {
    type State = ();
    type Action = ();

    fn reduce(&mut self, (): &mut (), (): &(), _: &mut ReduceContext<'_>) -> Effect<()> {
        self.0.take().expect("one launch")
    }
}

/// Selects callback delivery, no reporter or an already closed recorder.
#[derive(Clone, Copy, Debug)]
enum Endpoint {
    Enabled,
    Disabled,
    Closed,
}

#[test]
fn reported_failure_moves_owned_diagnostics_without_cloning_their_strings() {
    if allocation_test::run_in_child(
        "reported_failure_moves_owned_diagnostics_without_cloning_their_strings",
    ) {
        return;
    }

    // The child excludes imported runtime tests. Each endpoint uses a separate session.
    let costs = [Endpoint::Enabled, Endpoint::Disabled, Endpoint::Closed]
        .map(|endpoint| (endpoint, measure_report(endpoint)));
    for (endpoint, (blocks, bytes)) in costs {
        eprintln!("{endpoint:?}: {blocks} allocations / {bytes} bytes");
    }
    for (endpoint, (blocks, bytes)) in costs {
        assert_eq!(
            blocks, 1,
            "{endpoint:?}: only the delivery vector allocates"
        );
        assert_eq!(
            bytes,
            std::mem::size_of::<IssueDelivery<Infallible>>() as u64
        );
    }
}

/// Warms the endpoint, measures delivery and checks the original cause and diagnostic.
fn measure_report(endpoint: Endpoint) -> (u64, u64) {
    let runtime = support::ManualRuntime::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let reporter = match endpoint {
        Endpoint::Enabled => {
            let calls = Arc::clone(&calls);
            IssueReporter::new(move |issue| {
                assert_eq!(issue.message, "owned message");
                assert_eq!(issue.cause.as_deref(), Some("owned cause"));
                calls.fetch_add(1, Ordering::SeqCst);
            })
        }
        Endpoint::Disabled => IssueReporter::default(),
        Endpoint::Closed => {
            let recorder = IssueRecorder::new();
            let reporter = recorder.reporter();
            let owner = Store::builder((), Once(None), runtime.runtime())
                .issue_recorder(recorder)
                .build()
                .unwrap();
            drop(runtime.take_only_queued_task());
            drop(owner);
            reporter
        }
    };
    // Initialise the endpoint's platform mutex before the measured delivery.
    let _ = reporter.report(Issue::new("failed", "owned message").with_cause("owned cause"));
    calls.store(0, Ordering::SeqCst);
    let retained = Arc::new(Mutex::new(None::<OperationReport<(), u8, Infallible>>));
    let output = Arc::clone(&retained);
    let cost = Arc::new(Mutex::new((0, 0)));
    // The measurement's mutex must not contribute its own lazy allocation.
    drop(cost.lock().unwrap());
    let before_delivery = Arc::clone(&cost);
    let after_delivery = Arc::clone(&cost);
    let diagnostic =
        Issue::new("failed", "owned message".to_owned()).with_cause("owned cause".to_owned());
    let (release, gate) = futures_channel::oneshot::channel();
    let effect = Effect::try_run_reported(
        move |_| async move {
            gate.await.unwrap();
            Err(EffectError::Failed(7))
        },
        reporter,
        move |_| {
            let before = dhat::HeapStats::get();
            *before_delivery.lock().unwrap() = (before.total_blocks, before.total_bytes);
            diagnostic
        },
        move |report| {
            let after = dhat::HeapStats::get();
            let mut cost = after_delivery.lock().unwrap();
            *cost = (after.total_blocks - cost.0, after.total_bytes - cost.1);
            *output.lock().unwrap() = Some(report);
        },
    );
    let store = Store::new((), Once(Some(effect)), runtime.runtime());
    let mut actor = runtime.take_only_queued_task();
    let receipt = store.try_send(()).unwrap();
    let mut context = Context::from_waker(Waker::noop());
    assert!(actor.as_mut().poll(&mut context).is_pending());
    let task = futures_executor::block_on(receipt.finish()).unwrap();
    let mut effect = runtime.take_only_queued_task();
    assert!(effect.as_mut().poll(&mut context).is_pending());
    release.send(()).unwrap();

    // The factory starts the window, and the outcome ends it before retaining evidence.
    // Construction, launch boxing, task completion and user callback storage are excluded.
    let profiler = dhat::Profiler::builder().testing().build();
    assert!(effect.as_mut().poll(&mut context).is_ready());
    drop(profiler);

    let report = retained.lock().unwrap().take().unwrap();
    assert_eq!(report.result, Err(7));
    match (endpoint, report.deliveries.as_slice()) {
        (Endpoint::Enabled, [IssueDelivery::Accepted]) => {
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }
        (Endpoint::Disabled, [IssueDelivery::Disabled(issue)])
        | (Endpoint::Closed, [IssueDelivery::Closed(issue)]) => {
            assert_eq!(issue.message, "owned message");
            assert_eq!(issue.cause.as_deref(), Some("owned cause"));
            assert_eq!(calls.load(Ordering::SeqCst), 0);
        }
        _ => panic!("incorrect delivery evidence"),
    }
    assert!(actor.as_mut().poll(&mut context).is_pending());
    assert!(task.is_finished());
    *cost.lock().unwrap()
}
