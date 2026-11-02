//! Supplies cooperative virtual time to integration-test runtimes without optional features.

use std::future::{pending, poll_fn};
use std::sync::{Arc, Mutex};
use std::task::Poll;

use super::{MonotonicInstant, RuntimeFuture};

/// Shares monotonic virtual time and advances finite waits when first polled.
///
/// Elapsed waits are immediately ready. Future waits advance time, then yield once.
/// Maximum deadlines remain pending without advancing time. Dropping an unpolled
/// wait does not change time. Driver clones share the same clock.
///
/// # Panics
///
/// Reading or advancing time panics if the time mutex is poisoned.
#[derive(Clone, Debug)]
pub(crate) struct ImmediateClock {
    /// Retains the greatest finite deadline reached by any clone.
    now: Arc<Mutex<MonotonicInstant>>,
}

impl ImmediateClock {
    /// Creates an independent clock at zero.
    pub(crate) fn new() -> Self {
        Self {
            now: Arc::new(Mutex::new(MonotonicInstant::ZERO)),
        }
    }

    /// Returns the shared virtual time.
    pub(crate) fn now(&self) -> MonotonicInstant {
        *self
            .now
            .lock()
            .expect("test clock lock must not be poisoned")
    }

    /// Creates a wait without advancing time until its first poll.
    pub(crate) fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
        if deadline == MonotonicInstant::MAX {
            return Box::pin(pending());
        }

        let now = Arc::clone(&self.now);
        let mut advanced = false;
        Box::pin(poll_fn(move |context| {
            if advanced {
                return Poll::Ready(());
            }

            let elapsed = {
                let mut now = now.lock().expect("test clock lock must not be poisoned");
                let elapsed = deadline <= *now;
                *now = (*now).max(deadline);
                elapsed
            };
            if elapsed {
                return Poll::Ready(());
            }

            advanced = true;
            // A caller's wake callback must not run while the time mutex is held.
            context.waker().wake_by_ref();
            Poll::Pending
        }))
    }
}
