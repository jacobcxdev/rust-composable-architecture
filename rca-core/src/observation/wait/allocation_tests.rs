//! Measures allocation costs without the ordinary test harness.

use super::{PredicateWait, PredicateWaitOutcome};
use crate::observation::local::PredicateWait as LocalPredicateWait;
use std::cell::Cell;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

/// Polls on the owner thread without scheduling further work.
fn poll<F: Future + Unpin>(future: &mut F) -> Poll<F::Output> {
    Pin::new(future).poll(&mut Context::from_waker(Waker::noop()))
}

/// Checks wait storage, polling and cancellation allocation costs.
pub(crate) fn wait_storage_allocates_one_box_and_poll_cancel_allocate_nothing() {
    /// Measures only the supplied operation, leaving setup and assertions outside.
    fn measure<T>(operation: impl FnOnce() -> T) -> (T, (u64, u64)) {
        let before = dhat::HeapStats::get();
        let result = operation();
        let after = dhat::HeapStats::get();
        (
            result,
            (
                after.total_blocks - before.total_blocks,
                after.total_bytes - before.total_bytes,
            ),
        )
    }
    if crate::allocation_test::run_in_child(
        "observation::wait::tests::wait_storage_allocates_one_box_and_poll_cancel_allocate_nothing",
    ) {
        return;
    }

    let _profiler = dhat::Profiler::builder().testing().build();
    let mut samples = Vec::with_capacity(4);
    for warm in [false, true] {
        // Non-zero future storage distinguishes a real allocation from Box's ZST optimisation.
        let future = std::future::ready(std::hint::black_box(PredicateWaitOutcome::Satisfied));
        let bytes = std::mem::size_of_val(&future) as u64;
        let (mut wait, creation) = measure(|| PredicateWait::new(future, None));
        assert_eq!(creation, (1, bytes));
        let (result, completion) = measure(|| poll(&mut wait));
        assert_eq!(result, Poll::Ready(PredicateWaitOutcome::Satisfied));
        assert_eq!(completion, (0, 0));
        let ((), cancelled) = measure(|| wait.cancel());
        assert_eq!(cancelled, (0, 0));
        samples.push(("shared", warm, creation, completion, cancelled));

        let capture = Rc::new(Cell::new(0));
        let future = async move {
            #[expect(
                clippy::no_effect_underscore_binding,
                reason = "retain ownership across the pending await for the cancellation allocation assay"
            )]
            let _capture = capture;
            std::future::pending().await
        };
        let bytes = std::mem::size_of_val(&future) as u64;
        let (mut wait, creation) = measure(|| LocalPredicateWait::new(future, None));
        assert_eq!(creation, (1, bytes));
        let (result, pending) = measure(|| poll(&mut wait));
        assert!(result.is_pending());
        assert_eq!(pending, (0, 0));
        let ((), cancelled) = measure(|| wait.cancel());
        assert_eq!(cancelled, (0, 0));
        samples.push(("local", warm, creation, pending, cancelled));
    }
    for sample in samples {
        println!("wait_storage {sample:?}");
    }
}
