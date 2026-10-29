//! Wraps production causal tasks with deterministic cleanup.

use std::fmt;
use std::ops::Deref;
use std::panic::Location;
use std::sync::Arc;

use rca_core::testing::{ActorDiagnostics, TransactionGate};
use rca_core::{ActionTask, MonotonicInstant};

use crate::{DeterministicRuntime, DriveError, DriveReport, PendingSleep, TestTaskError};

/// Retains the gate for failure diagnostics without making the task state-generic.
type SnapshotQuery = Arc<dyn Fn() -> Option<ActorDiagnostics> + Send + Sync + 'static>;

/// Shares task policy between thread-safe and owner-local wrappers.
#[derive(Clone)]
struct TaskCore<R, Q> {
    /// Production causal lineage, not a separate test scheduler task.
    task: ActionTask,
    /// Deterministic host driven explicitly by cancellation and completion checks.
    runtime: R,
    /// User call site that created the causal work.
    origin: &'static Location<'static>,
    /// Captures current actor diagnostics only when completion fails.
    snapshot: Q,
}

impl<R, Q> TaskCore<R, Q>
where
    Q: Deref,
    Q::Target: Fn() -> Option<ActorDiagnostics>,
{
    /// Requests production cancellation, then drives ready cleanup without advancing time.
    fn cancel(
        &self,
        drive: impl FnOnce(&R) -> Result<DriveReport, DriveError>,
    ) -> Result<(), DriveError> {
        self.task.cancel();
        drive(&self.runtime)?;
        Ok(())
    }

    /// Drives first, then checks completion or captures an owned failure report.
    ///
    /// Driving still reports scheduler failure when this causal task has finished.
    /// A blocked report snapshots diagnostics but neither cancels nor releases work.
    fn finish(
        &self,
        drive: impl FnOnce(&R) -> Result<DriveReport, DriveError>,
        blocked: impl FnOnce(&R) -> (Vec<u64>, Vec<PendingSleep>, MonotonicInstant),
    ) -> Result<(), TestTaskError> {
        drive(&self.runtime)
            .map_err(|error| TestTaskError::drive(error, (self.snapshot)(), self.origin))?;
        if self.task.is_finished() {
            return Ok(());
        }
        let (ready_task_ids, pending_sleeps, now) = blocked(&self.runtime);
        Err(TestTaskError::blocked(
            self.origin,
            ready_task_ids,
            pending_sleeps,
            now,
            (self.snapshot)(),
        ))
    }
}

impl<R, Q> fmt::Debug for TaskCore<R, Q> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TestActionTask")
            .field("task", &self.task)
            .field("origin", &self.origin)
            .finish_non_exhaustive()
    }
}

/// Generates concrete ownership wrappers without duplicating causal-task policy.
macro_rules! task_wrapper {
    ($runtime:ty, $snapshot:ty $(, $send:ident)?) => {
        /// Tracks causal work created by one deterministic test action.
        ///
        /// Clones refer to the same production work. Dropping a handle does not
        /// cancel that work. Cancellation and completion futures act only when
        /// polled, and neither operation advances virtual time.
        #[derive(Clone)]
        #[must_use = "dropping a test action task does not cancel causal work"]
        pub struct TestActionTask {
            /// Owns the task handle, host and diagnostics query for this carrier.
            core: TaskCore<$runtime, $snapshot>,
        }

        impl TestActionTask {
            /// Requests cancellation and drives deterministic cleanup without advancing time.
            ///
            /// This drives all ready work on the shared host, not only this task.
            /// Use [`Self::finish`] to require completion after cancellation.
            ///
            /// # Errors
            ///
            /// Returns [`DriveError`](crate::DriveError) when the deterministic
            /// poll budget is exhausted.
            pub fn cancel(
                &self,
            ) -> impl Future<Output = Result<(), crate::DriveError>> $(+ $send)? + '_ {
                async move { self.core.cancel(|runtime| runtime.run_until_stalled()) }
            }

            /// Returns whether all causal work finished.
            #[must_use]
            pub fn is_finished(&self) -> bool {
                self.core.task.is_finished()
            }

            /// Returns whether causal work was cancelled.
            #[must_use]
            pub fn is_cancelled(&self) -> bool {
                self.core.task.is_cancelled()
            }

            /// Drives immediately ready work and verifies causal completion without advancing time.
            ///
            /// This drives all ready work on the host before checking this task.
            /// A blocked result preserves the work for a later expectation or
            /// explicit clock advance.
            ///
            /// # Errors
            ///
            /// Returns [`TestTaskError`](crate::TestTaskError) when driving fails
            /// or causal work remains blocked.
            pub fn finish(
                &self,
            ) -> impl Future<Output = Result<(), crate::TestTaskError>> $(+ $send)? + '_ {
                async move {
                    self.core.finish(
                        |runtime| runtime.run_until_stalled(),
                        |runtime| {
                            (
                                runtime.ready_task_ids(),
                                runtime.pending_sleeps(),
                                runtime.now(),
                            )
                        },
                    )
                }
            }

            /// Returns the test call site that created this task.
            #[must_use]
            pub const fn origin(&self) -> &'static std::panic::Location<'static> {
                self.core.origin
            }
        }

        impl std::fmt::Debug for TestActionTask {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                std::fmt::Debug::fmt(&self.core, f)
            }
        }
    };
}

task_wrapper!(DeterministicRuntime, SnapshotQuery, Send);

impl TestActionTask {
    /// Retains a typed gate behind a Send/Sync diagnostics query.
    pub(crate) fn new<State, Action>(
        task: ActionTask,
        runtime: DeterministicRuntime,
        origin: &'static Location<'static>,
        gate: TransactionGate<State, Action>,
    ) -> Self
    where
        Action: Send + 'static,
        State: Send + 'static,
    {
        Self {
            core: TaskCore {
                task,
                runtime,
                origin,
                snapshot: Arc::new(move || gate.latest_diagnostics()),
            },
        }
    }
}

#[cfg(feature = "local")]
pub(crate) mod local {
    //! Keeps diagnostics captures and deterministic driving on the owner thread.
    use std::panic::Location;
    use std::rc::Rc;

    use rca_core::ActionTask;
    use rca_core::testing::ActorDiagnostics;
    use rca_core::testing::local::TransactionGate;

    use super::TaskCore;
    use crate::local::DeterministicRuntime;

    /// Keeps an owner-local gate alive without requiring Send on its domain.
    type SnapshotQuery = Rc<dyn Fn() -> Option<ActorDiagnostics> + 'static>;

    task_wrapper!(DeterministicRuntime, SnapshotQuery);

    impl TestActionTask {
        /// Retains the local gate without adding cross-thread bounds.
        pub(crate) fn new<State: 'static, Action: 'static>(
            task: ActionTask,
            runtime: DeterministicRuntime,
            origin: &'static Location<'static>,
            gate: TransactionGate<State, Action>,
        ) -> Self {
            Self {
                core: TaskCore {
                    task,
                    runtime,
                    origin,
                    snapshot: Rc::new(move || gate.latest_diagnostics()),
                },
            }
        }
    }
}

#[cfg(all(test, feature = "local"))]
mod tests;
