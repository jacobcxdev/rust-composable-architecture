//! Bounds waits in managed-server tests without setting the order of events.

use std::time::Duration;

/// Runs the test on a single Tokio thread with a 15-second deadline.
pub(super) fn check(test: impl Future<Output = ()>) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            tokio::time::timeout(Duration::from_secs(15), test)
                .await
                .unwrap();
        });
}
