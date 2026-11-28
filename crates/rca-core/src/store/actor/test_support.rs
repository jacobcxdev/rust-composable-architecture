//! Shares real actor setup between semantic tests and isolated allocation measurements.

use std::fmt;
use std::future::pending;

use super::*;
use crate::{MonotonicInstant, Reducer, RuntimeDriver};

/// Transfers one prebuilt effect into a real actor transaction without cloning it.
struct OnceEffect(Option<Effect<()>>);

impl Reducer for OnceEffect {
    type State = ();
    type Action = ();

    fn reduce(&mut self, (): &mut (), (): &(), _: &mut ReduceContext<'_>) -> Effect<()> {
        self.0.take().unwrap()
    }
}

/// Retains unpolled futures and can fail one spawn after earlier ownership transfers.
///
/// Time stays at zero. Elapsed sleeps complete immediately, while yields
/// offer one pending poll before completion.
pub(super) struct LaunchDriver {
    /// Futures already accepted by the host, not by the actor's launch buffer.
    pub(super) tasks: Arc<std::sync::Mutex<Vec<RuntimeFuture>>>,
    /// Number of accepted futures after which spawn deliberately panics.
    pub(super) panic_after: usize,
}

impl fmt::Debug for LaunchDriver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LaunchDriver").finish_non_exhaustive()
    }
}

impl RuntimeDriver for LaunchDriver {
    fn spawn(&self, task: RuntimeFuture) {
        let mut tasks = self.tasks.lock().unwrap();
        if tasks.len() == self.panic_after {
            drop(tasks);
            panic!("host spawn failure");
        }
        tasks.push(task);
    }

    fn now(&self) -> MonotonicInstant {
        MonotonicInstant::ZERO
    }
    fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
        if deadline <= self.now() {
            Box::pin(std::future::ready(()))
        } else {
            Box::pin(pending())
        }
    }
    fn yield_now(&self) -> RuntimeFuture {
        let mut yielded = false;
        Box::pin(std::future::poll_fn(move |context| {
            if yielded {
                std::task::Poll::Ready(())
            } else {
                yielded = true;
                context.waker().wake_by_ref();
                std::task::Poll::Pending
            }
        }))
    }
}

/// Keeps ingress alive while tests drive the concrete actor and its prepared work.
pub(super) fn launch_actor(
    effect: Effect<()>,
    runtime: Runtime,
) -> (Actor<(), ()>, Arc<Ingress<(), ()>>) {
    let (sender, receiver) = crate::store::mailbox::bounded(
        std::num::NonZeroUsize::new(8).expect("test capacity is nonzero"),
    );
    let ingress = Arc::new(Ingress::new(sender, ControlWake::new()));
    let actor = Actor::new(
        (),
        Box::new(OnceEffect(Some(effect))),
        DependencyValues::new(),
        crate::IssueReporter::default(),
        receiver,
        &ingress,
        Arc::new(Lifecycle::new()),
        runtime,
        #[cfg(feature = "test-util")]
        None,
    );
    (actor, ingress)
}
