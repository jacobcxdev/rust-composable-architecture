//! Challenges the destruction oracle used only by shutdown memory benchmarks.

#[path = "../benches/qualification/shutdown.rs"]
mod shutdown;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Observes whether the payload drops before its destruction report is visible.
struct Payload(Arc<AtomicUsize>);

impl Drop for Payload {
    fn drop(&mut self) {
        assert_ne!(
            shutdown::unfinished(),
            0,
            "payload destruction must precede its final report"
        );
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
/// Rejects premature reports, lost unpolled captures and an enlarged future wrapper.
fn held_futures_remain_unfinished_until_their_payloads_drop() {
    assert_eq!(
        size_of::<shutdown::TrackedFuture>(),
        size_of::<rca_core::RuntimeFuture>()
    );
    assert_eq!(
        align_of::<shutdown::TrackedFuture>(),
        align_of::<rca_core::RuntimeFuture>()
    );
    let drops = Arc::new(AtomicUsize::new(0));
    let mut held = Vec::new();
    // Two samples and two warm-ups, with eight simultaneously owned futures.
    for completed in 0..4 {
        for _ in 0..8 {
            let payload = Payload(Arc::clone(&drops));
            held.push(shutdown::TrackedFuture::new(Box::pin(async move {
                std::future::pending::<()>().await;
                drop(payload);
            })));
        }
        assert_eq!(drops.load(Ordering::SeqCst), completed * 8);
        assert_eq!(shutdown::unfinished(), 8);
        while let Some(future) = held.pop() {
            let before = drops.load(Ordering::SeqCst);
            drop(future);
            shutdown::wait_until(held.len());
            assert_eq!(drops.load(Ordering::SeqCst), before + 1);
        }
        shutdown::finish_batch();
    }
    let lifetime = shutdown::lifetime(8, 4);
    assert_eq!(lifetime.spawned, 32);
    assert_eq!(lifetime.dropped, 32);
    assert_eq!(lifetime.live_limit, 8);
    assert_eq!(lifetime.maximum_unfinished, 8);
    assert_eq!(lifetime.unfinished, 0);
    assert_eq!(lifetime.maximum_batch_end_unfinished, 0);
    assert_eq!(lifetime.batches, 4);
}
