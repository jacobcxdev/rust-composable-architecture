//! Runs public conformance suites and checks failed clock advancement preserves queued work.

use rca_test::conformance::{DeterministicFactory, run_all, run_composition};

#[test]
fn deterministic_conformance() {
    let factory = DeterministicFactory::new();
    let scheduler = factory.scheduler();

    let conformance = run_all(factory);
    eprintln!(
        "conformance run_all: frame_bytes={}",
        std::mem::size_of_val(&conformance),
    );
    futures_executor::block_on(scheduler.run(conformance))
        .expect("deterministic conformance drive");
}

#[test]
fn composition_deterministic() {
    let factory = DeterministicFactory::new();
    let scheduler = factory.scheduler();

    futures_executor::block_on(scheduler.run(run_composition(factory)))
        .expect("deterministic composition drive");
}

#[test]
fn failed_factory_advance_retains_work_without_committing_requested_time() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Poll;
    use std::time::Duration;

    use futures_util::FutureExt as _;
    use rca_core::MonotonicInstant;
    use rca_test::conformance::RuntimeFactory as _;

    let factory = DeterministicFactory::default();
    let polls = Arc::new(AtomicUsize::new(0));
    let task_polls = Arc::clone(&polls);
    factory.spawn_probe(Box::pin(std::future::poll_fn(move |context| {
        // One more than the public factory's default budget keeps the witness finite.
        if task_polls.fetch_add(1, Ordering::SeqCst) + 1 == 100_001 {
            Poll::Ready(())
        } else {
            context.waker().wake_by_ref();
            Poll::Pending
        }
    })));
    assert_eq!(polls.load(Ordering::SeqCst), 0);

    let error = factory
        .advance(Duration::from_secs(5))
        .now_or_never()
        .expect("factory advance is bounded without an external wake")
        .expect_err("finite ready work exceeds the default task-poll budget");
    let diagnostic = error.to_string();
    assert!(diagnostic.starts_with("deterministic scheduler exhausted 100000 polls at "));
    assert!(diagnostic.contains("ready tasks: [0]; pending sleeps: []"));
    assert_eq!(polls.load(Ordering::SeqCst), 100_000);
    assert_eq!(
        Arc::strong_count(&polls),
        2,
        "queued work still owns its capture"
    );
    assert_eq!(factory.now_probe(), MonotonicInstant::ZERO);

    factory
        .advance(Duration::from_secs(2))
        .now_or_never()
        .expect("retry is bounded")
        .expect("the retained final poll completes before the new target");
    assert_eq!(polls.load(Ordering::SeqCst), 100_001);
    assert_eq!(
        Arc::strong_count(&polls),
        1,
        "completed work releases its capture"
    );
    assert_eq!(
        factory.now_probe(),
        MonotonicInstant::from_elapsed(Duration::from_secs(2))
    );
    assert_eq!(
        error.to_string(),
        diagnostic,
        "failure evidence remains owned"
    );
}
