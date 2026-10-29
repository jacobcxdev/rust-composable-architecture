//! Retains deterministic failures and their captured evidence.
//!
//! Drive failures own scheduler snapshots. Store and task failures add the
//! initiating call site and, when available, the latest actor diagnostics.
//! These snapshots do not change when a later operation makes progress.
//! Capture causes stay opaque to formatting and can be recovered only with
//! their exact declared key; other typed causes use [`std::error::Error::source`].

use std::any::{Any, type_name};
use std::backtrace::Backtrace;
use std::fmt;
use std::marker::PhantomData;
use std::panic::{AssertUnwindSafe, Location};

use parking_lot::Mutex;

use rca_core::testing::{ActorDiagnostics, RuntimeSnapshot};
use rca_core::{Issue, MonotonicInstant, StoreStopped};

/// Identifies one live cancellation entry without retaining its domain value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CancellationIdentityDiagnostic {
    /// Store-local identity assigned while the cancellation entry is live.
    diagnostic_id: u64,
    /// Structural reducer scope that owns the entry.
    scope_id: u64,
    /// Type metadata only; the domain cancellation value is not retained.
    type_name: &'static str,
}

impl CancellationIdentityDiagnostic {
    /// Returns the Store-local identifier, stable while the entry remains live.
    #[must_use]
    pub const fn diagnostic_id(&self) -> u64 {
        self.diagnostic_id
    }

    /// Returns the structural reducer scope identifier.
    #[must_use]
    pub const fn scope_id(&self) -> u64 {
        self.scope_id
    }

    /// Returns the concrete Rust type name of the cancellation value.
    #[must_use]
    pub const fn type_name(&self) -> &'static str {
        self.type_name
    }
}

/// Owns actor evidence without retaining its gate or domain cancellation values.
#[derive(Clone, Debug, Eq, PartialEq)]
struct CapturedRuntimeDiagnostics {
    /// Counters and task identifiers from one actor boundary.
    runtime: RuntimeSnapshot,
    /// Identity metadata from that same boundary, in diagnostic order.
    cancellation_identities: Vec<CancellationIdentityDiagnostic>,
}

impl From<ActorDiagnostics> for CapturedRuntimeDiagnostics {
    fn from(diagnostics: ActorDiagnostics) -> Self {
        let cancellation_identities = diagnostics
            .cancellation_identities()
            .iter()
            .map(|identity| CancellationIdentityDiagnostic {
                diagnostic_id: identity.diagnostic_id,
                scope_id: identity.scope_id,
                type_name: identity.type_name,
            })
            .collect();
        Self {
            runtime: diagnostics.into_runtime(),
            cancellation_identities,
        }
    }
}

/// Describes one registered virtual sleep.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PendingSleep {
    /// Virtual instant at which the registered sleep becomes ready.
    deadline: MonotonicInstant,
    /// Stable registration identifier, distinguishing equal deadlines.
    id: u64,
}

impl PendingSleep {
    /// Copies inert registration metadata out of the scheduler.
    pub(crate) const fn new(deadline: MonotonicInstant, id: u64) -> Self {
        Self { deadline, id }
    }

    /// Returns the virtual deadline.
    #[must_use]
    pub const fn deadline(self) -> MonotonicInstant {
        self.deadline
    }

    /// Returns the stable sleep registration identifier.
    #[must_use]
    pub const fn id(self) -> u64 {
        self.id
    }
}

/// Reports bounded deterministic driving or temporal assertion failures.
///
/// Classification methods distinguish exhausted budgets from blocked work,
/// a closed host and rejected time operations. Evidence is captured at failure;
/// inspecting this error does not drive tasks or advance virtual time.
pub struct DriveError {
    /// Keeps the public error small while owning all failure evidence.
    inner: Box<DriveErrorInner>,
}

/// Scheduler evidence captured together with the original failure backtrace.
struct DriveErrorInner {
    /// Reason the operation could not meet its contract.
    failure: DriveFailure,
    /// Task polls consumed by the failed operation.
    polls: usize,
    /// Maximum task polls allowed for that operation.
    step_budget: usize,
    /// Equal-deadline cohorts processed by explicit deadline draining.
    deadline_batches: usize,
    /// Explicit draining limit, absent for other operations.
    deadline_budget: Option<usize>,
    /// Live scheduler tasks at failure, including tasks that are not ready.
    live_task_ids: Vec<u64>,
    /// Unfinished virtual-time advance retained by the scheduler.
    active_target: Option<MonotonicInstant>,
    /// Scheduler tasks ready to be polled at failure.
    ready_task_ids: Vec<u64>,
    /// Registered sleeps ordered by deadline and registration.
    pending_sleeps: Vec<PendingSleep>,
    /// Virtual time reached before the operation failed.
    current_time: MonotonicInstant,
    /// Captured once when the failure is constructed, not when it is formatted.
    backtrace: Backtrace,
}

/// Distinguishes drive limits from assertions and host lifecycle failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DriveFailure {
    /// Ready work used every permitted task poll.
    StepBudget,
    /// More timer cohorts remain than the drain may process.
    DeadlineBudget,
    /// Live tasks have neither ready work nor registered timers.
    Blocked,
    /// The last host owner closed the scheduler.
    Closed,
    /// An unfinished advance prevents starting a deadline drain.
    PartialAdvance,
    /// Registered timers violate a no-pending-time assertion.
    PendingTime,
}

impl DriveError {
    /// Owns the scheduler snapshot and captures the failure's backtrace once.
    pub(crate) fn exhausted(
        polls: usize,
        step_budget: usize,
        live_task_ids: Vec<u64>,
        ready_task_ids: Vec<u64>,
        pending_sleeps: Vec<PendingSleep>,
        current_time: MonotonicInstant,
    ) -> Self {
        Self {
            inner: Box::new(DriveErrorInner {
                failure: DriveFailure::StepBudget,
                polls,
                step_budget,
                deadline_batches: 0,
                deadline_budget: None,
                live_task_ids,
                active_target: None,
                ready_task_ids,
                pending_sleeps,
                current_time,
                backtrace: Backtrace::capture(),
            }),
        }
    }

    /// Adds time-operation evidence without recapturing or replacing the snapshot.
    pub(crate) fn with_temporal_context(
        mut self,
        failure: DriveFailure,
        deadline_batches: usize,
        deadline_budget: Option<usize>,
        active_target: Option<MonotonicInstant>,
    ) -> Self {
        self.inner.failure = failure;
        self.inner.deadline_batches = deadline_batches;
        self.inner.deadline_budget = deadline_budget;
        self.inner.active_target = active_target;
        self
    }

    /// Returns whether ready work exhausted the task-poll budget.
    #[must_use]
    pub const fn is_step_budget_exhausted(&self) -> bool {
        matches!(self.inner.failure, DriveFailure::StepBudget)
    }

    /// Returns whether timer cohorts exhausted the deadline-batch budget.
    #[must_use]
    pub const fn is_deadline_budget_exhausted(&self) -> bool {
        matches!(self.inner.failure, DriveFailure::DeadlineBudget)
    }

    /// Returns whether live tasks stalled without registered timers or ready work.
    #[must_use]
    pub const fn is_blocked(&self) -> bool {
        matches!(self.inner.failure, DriveFailure::Blocked)
    }

    /// Returns whether the final deterministic host closed the scheduler.
    #[must_use]
    pub const fn is_host_closed(&self) -> bool {
        matches!(self.inner.failure, DriveFailure::Closed)
    }

    /// Returns whether an existing virtual-time advance prevented deadline draining.
    #[must_use]
    pub const fn is_partial_advance(&self) -> bool {
        matches!(self.inner.failure, DriveFailure::PartialAdvance)
    }

    /// Returns whether registered timers failed a no-pending-time assertion.
    #[must_use]
    pub const fn is_pending_time(&self) -> bool {
        matches!(self.inner.failure, DriveFailure::PendingTime)
    }

    /// Returns deadline cohorts processed by explicit draining before failure.
    ///
    /// Other drive errors report zero, even if an advance processed deadlines.
    #[must_use]
    pub const fn deadline_batches(&self) -> usize {
        self.inner.deadline_batches
    }

    /// Returns the deadline-batch budget, when explicitly draining deadlines.
    #[must_use]
    pub const fn deadline_budget(&self) -> Option<usize> {
        self.inner.deadline_budget
    }

    /// Returns live scheduler task identifiers at failure.
    #[must_use]
    pub fn live_task_ids(&self) -> &[u64] {
        &self.inner.live_task_ids
    }

    /// Returns an existing partial advance target, without resuming or clearing it.
    #[must_use]
    pub const fn active_target(&self) -> Option<MonotonicInstant> {
        self.inner.active_target
    }

    /// Returns scheduler polls consumed by the operation.
    #[must_use]
    pub const fn polls(&self) -> usize {
        self.inner.polls
    }

    /// Returns the configured scheduler poll budget.
    #[must_use]
    pub const fn step_budget(&self) -> usize {
        self.inner.step_budget
    }

    /// Returns ready task identifiers at failure.
    #[must_use]
    pub fn ready_task_ids(&self) -> &[u64] {
        &self.inner.ready_task_ids
    }

    /// Returns registered virtual sleeps at failure.
    #[must_use]
    pub fn pending_sleeps(&self) -> &[PendingSleep] {
        &self.inner.pending_sleeps
    }

    /// Returns virtual time at failure.
    #[must_use]
    pub const fn current_time(&self) -> MonotonicInstant {
        self.inner.current_time
    }

    /// Returns the captured failure backtrace.
    pub const fn backtrace(&self) -> &Backtrace {
        &self.inner.backtrace
    }
}

impl fmt::Debug for DriveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DriveError")
            .field("failure", &self.inner.failure)
            .field("polls", &self.inner.polls)
            .field("step_budget", &self.inner.step_budget)
            .field("deadline_batches", &self.inner.deadline_batches)
            .field("deadline_budget", &self.inner.deadline_budget)
            .field("live_task_ids", &self.inner.live_task_ids)
            .field("active_target", &self.inner.active_target)
            .field("ready_task_ids", &self.inner.ready_task_ids)
            .field("pending_sleeps", &self.inner.pending_sleeps)
            .field("current_time", &self.inner.current_time)
            .field("backtrace", &self.inner.backtrace)
            .finish()
    }
}

impl fmt::Display for DriveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.inner.failure != DriveFailure::StepBudget || self.inner.deadline_budget.is_some() {
            let reason = match self.inner.failure {
                DriveFailure::DeadlineBudget => "exhausted deadline-batch budget",
                DriveFailure::Blocked => "blocked on unfinished tasks without registered timers",
                DriveFailure::Closed => "host closed",
                DriveFailure::PartialAdvance => "existing partial virtual-time advance",
                DriveFailure::PendingTime => "registered timers remain",
                DriveFailure::StepBudget => "exhausted task-poll budget",
            };
            return write!(
                f,
                "deterministic scheduler {reason} after {} polls and {} deadline batches at {:?}; deadline budget: {:?}; live tasks: {:?}; ready tasks: {:?}; pending sleeps: {:?}; active target: {:?}\n{}",
                self.inner.polls,
                self.inner.deadline_batches,
                self.inner.current_time,
                self.inner.deadline_budget,
                self.inner.live_task_ids,
                self.inner.ready_task_ids,
                self.inner.pending_sleeps,
                self.inner.active_target,
                self.inner.backtrace,
            );
        }
        write!(
            f,
            "deterministic scheduler exhausted {} polls at {:?}; ready tasks: {:?}; pending sleeps: {:?}\n{}",
            self.inner.polls,
            self.inner.current_time,
            self.inner.ready_task_ids,
            self.inner.pending_sleeps,
            self.inner.backtrace
        )
    }
}

impl std::error::Error for DriveError {}

/// Reports a failed exhaustive Store assertion operation.
///
/// The call site and captured evidence remain available after the Store recovers
/// or stops. Formatting never inspects an original capture cause. Recover that
/// cause with [`Self::into_capture_error`]; production access, shutdown and drive
/// failures instead expose their typed cause through [`std::error::Error::source`].
pub struct TestStoreError {
    /// Shares failure policy with the owner-local facade.
    pub(crate) failure: TestStoreFailure<SharedCause>,
}

/// Makes the declared key part of the downcast target, not a separately trusted tag.
struct KeyedCause<K, E> {
    /// Original capture error, never cloned or implicitly formatted.
    value: E,
    /// Nominal key identity without transferring or sharing a value of `K`.
    key: PhantomData<fn() -> K>,
}

/// Recovers an opaque cause while preserving ownership on a nominal-key mismatch.
pub(crate) trait CaptureCause: Sized {
    /// Extracts the exact key/error pair or returns the original envelope intact.
    fn recover<K: 'static, E: 'static>(self) -> Result<E, Self>;
}

/// Keeps a transferable cause opaque while preserving `Sync` for the public error.
///
/// These envelopes are only constructed, mutably borrowed, consumed or dropped.
/// No guard or shared payload access escapes. The unwind annotation preserves
/// the error's unwind traits without requiring them from the private cause.
pub(crate) struct SharedCause(AssertUnwindSafe<Mutex<Box<dyn Any + Send>>>);

impl SharedCause {
    /// Erases a transferable cause while retaining its nominal capture key.
    pub(crate) fn new<K: 'static, E: Send + 'static>(value: E) -> Self {
        Self(AssertUnwindSafe(Mutex::new(Box::new(KeyedCause::<K, E> {
            value,
            key: PhantomData,
        }))))
    }
}

impl CaptureCause for SharedCause {
    fn recover<K: 'static, E: 'static>(self) -> Result<E, Self> {
        self.0
            .0
            .into_inner()
            .downcast::<KeyedCause<K, E>>()
            .map(|cause| cause.value)
            .map_err(|cause| Self(AssertUnwindSafe(Mutex::new(cause))))
    }
}

#[cfg(feature = "local")]
/// Keeps an owner-local cause opaque without adding `Send` or `Sync` bounds.
pub(crate) struct LocalCause(AssertUnwindSafe<Mutex<Box<dyn Any>>>);

#[cfg(feature = "local")]
impl LocalCause {
    /// Erases an owner-local cause while retaining its nominal capture key.
    pub(crate) fn new<K: 'static, E: 'static>(value: E) -> Self {
        Self(AssertUnwindSafe(Mutex::new(Box::new(KeyedCause::<K, E> {
            value,
            key: PhantomData,
        }))))
    }
}

#[cfg(feature = "local")]
impl CaptureCause for LocalCause {
    fn recover<K: 'static, E: 'static>(self) -> Result<E, Self> {
        self.0
            .0
            .into_inner()
            .downcast::<KeyedCause<K, E>>()
            .map(|cause| cause.value)
            .map_err(|cause| Self(AssertUnwindSafe(Mutex::new(cause))))
    }
}

#[cfg(feature = "local")]
/// Defines the nominal owner-local facade over the shared failure policy.
pub(crate) mod local {
    /// Reports a failed owner-local exhaustive Store assertion operation.
    ///
    /// Original capture causes remain on their owner; they need not be Send,
    /// Sync, Debug, Clone or Error. Formatting never inspects those causes.
    pub struct TestStoreError {
        /// Preserves local cause ownership through common classification and recovery.
        pub(crate) failure: super::TestStoreFailure<super::LocalCause>,
    }
}

/// Forwards nominal facades to one classification, formatting and recovery policy.
macro_rules! error_facade {
    ($error:ty, $key:path) => {
        impl $error {
            /// Returns the exact production task-access cause, when authority rejected an operation.
            #[must_use]
            pub fn task_access_error(&self) -> Option<&rca_core::TaskAccessError> {
                self.failure.task_access_error()
            }

            /// Returns the operation's call site.
            #[must_use]
            pub const fn origin(&self) -> &'static Location<'static> {
                self.failure.origin()
            }

            /// Returns whether no issue was available for expectation.
            #[must_use]
            pub fn is_no_issue(&self) -> bool {
                self.failure.is_no_issue()
            }

            /// Returns whether the oldest issue failed its expectation matcher.
            #[must_use]
            pub fn is_issue_mismatch(&self) -> bool {
                self.failure.is_issue_mismatch()
            }

            /// Returns evidence for a rejected oldest-issue matcher.
            #[must_use]
            pub fn mismatched_issue(&self) -> Option<&Issue> {
                self.failure.mismatched_issue()
            }

            /// Returns issues that prevented exhaustive completion.
            #[must_use]
            pub fn unconsumed_issues(&self) -> &[Issue] {
                self.failure.unconsumed_issues()
            }

            /// Returns whether snapshot, terminal or failed-match assertion debt remains.
            #[must_use]
            pub fn is_incomplete_assertion(&self) -> bool {
                self.failure.is_incomplete_assertion()
            }

            /// Returns whether an effect action required receive first.
            #[must_use]
            pub fn is_unreceived_action(&self) -> bool {
                self.failure.is_unreceived_action()
            }

            /// Returns whether no effect action awaited receive.
            #[must_use]
            pub fn is_no_received_action(&self) -> bool {
                self.failure.is_no_received_action()
            }

            /// Returns whether the receive matcher rejected the pending action.
            #[must_use]
            pub fn is_action_mismatch(&self) -> bool {
                self.failure.is_action_mismatch()
            }

            /// Returns whether explicit skip was requested in exhaustive mode.
            #[must_use]
            pub fn is_skip_requires_non_exhaustive(&self) -> bool {
                self.failure.is_skip_requires_non_exhaustive()
            }

            /// Returns unfinished-work diagnostics when finish was not exhaustive.
            #[must_use]
            pub fn finish_diagnostics(&self) -> Option<&FinishDiagnostics> {
                self.failure.finish_diagnostics()
            }

            /// Returns the underlying deterministic drive failure, when present.
            #[must_use]
            pub fn drive_error(&self) -> Option<&DriveError> {
                self.failure.drive_error()
            }

            /// Returns actor diagnostics captured before driving or exhaustive finish failed.
            #[must_use]
            pub fn runtime_snapshot(&self) -> Option<&RuntimeSnapshot> {
                self.failure.runtime_snapshot()
            }

            /// Returns cancellation identities captured before operation failure.
            #[must_use]
            pub fn cancellation_identities(&self) -> &[CancellationIdentityDiagnostic] {
                self.failure.cancellation_identities()
            }

            /// Returns whether a declared independent value differed from its expectation.
            #[must_use]
            pub fn is_capture_mismatch(&self) -> bool {
                self.failure.is_capture_mismatch()
            }

            /// Recovers the original cause only for its exact declared capture key.
            ///
            /// Wrong keys return the original error unchanged. Extraction consumes
            /// this error without cloning or formatting its original cause.
            ///
            /// # Errors
            ///
            /// Returns this error when it has no cause for the selected key.
            pub fn into_capture_error<K: $key>(self) -> Result<K::Error, Self> {
                self.failure
                    .into_capture_error::<K, K::Error>()
                    .map_err(|failure| Self { failure })
            }
        }

        impl fmt::Debug for $error {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Debug::fmt(&self.failure, f)
            }
        }

        impl fmt::Display for $error {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.failure, f)
            }
        }

        impl std::error::Error for $error {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                self.failure.source()
            }
        }
    };
}

error_facade!(TestStoreError, crate::CaptureKey);
#[cfg(feature = "local")]
error_facade!(
    local::TestStoreError,
    crate::store::local_captures::CaptureKey
);

/// Gives both nominal facades the same internal failure constructors.
macro_rules! error_constructors {
    ($error:ty) => {
        impl $error {
            /// Records assertion debt without consuming the operation that owes it.
            pub(crate) fn incomplete_assertion(
                operation: &'static str,
                origin: &'static Location<'static>,
            ) -> Self {
                Self {
                    failure: TestStoreFailure::incomplete_assertion(operation, origin),
                }
            }

            /// Retains the exact production authority failure and test call site.
            pub(crate) fn task_access(
                error: rca_core::TaskAccessError,
                origin: &'static Location<'static>,
            ) -> Self {
                Self {
                    failure: TestStoreFailure::new(TestStoreErrorKind::TaskAccess(error), origin),
                }
            }

            /// Reports an empty issue queue at the expectation boundary.
            pub(crate) fn no_issue(origin: &'static Location<'static>) -> Self {
                Self {
                    failure: TestStoreFailure::no_issue(origin),
                }
            }

            /// Owns the oldest issue rejected by its expectation matcher.
            pub(crate) fn issue_mismatch(issue: Issue, origin: &'static Location<'static>) -> Self {
                Self {
                    failure: TestStoreFailure::issue_mismatch(issue, origin),
                }
            }

            /// Owns issues that prevented exhaustive completion.
            pub(crate) fn unconsumed(
                issues: Vec<Issue>,
                origin: &'static Location<'static>,
            ) -> Self {
                Self {
                    failure: TestStoreFailure::unconsumed(issues, origin),
                }
            }

            /// Reports an effect action that must be received before sending.
            pub(crate) fn unreceived_action(origin: &'static Location<'static>) -> Self {
                Self {
                    failure: TestStoreFailure::unreceived_action(origin),
                }
            }

            /// Reports that no effect action is available for receiving.
            pub(crate) fn no_received_action(origin: &'static Location<'static>) -> Self {
                Self {
                    failure: TestStoreFailure::no_received_action(origin),
                }
            }

            /// Records a rejected receive matcher without exposing the action.
            pub(crate) fn action_mismatch(origin: &'static Location<'static>) -> Self {
                Self {
                    failure: TestStoreFailure::action_mismatch(origin),
                }
            }

            /// Rejects action skipping while exhaustive assertions are required.
            pub(crate) fn skip_requires_non_exhaustive(origin: &'static Location<'static>) -> Self {
                Self {
                    failure: TestStoreFailure::skip_requires_non_exhaustive(origin),
                }
            }

            /// Reports mailbox capacity rejection before action admission.
            pub(crate) fn admission_full(origin: &'static Location<'static>) -> Self {
                Self {
                    failure: TestStoreFailure::admission_full(origin),
                }
            }

            /// Records unavailable admission when no typed shutdown cause exists.
            pub(crate) fn store_stopped_from_admission(origin: &'static Location<'static>) -> Self {
                Self {
                    failure: TestStoreFailure::store_stopped_from_admission(origin),
                }
            }

            /// Records unavailable shutdown when no typed shutdown cause exists.
            pub(crate) fn store_stopped_from_shutdown(origin: &'static Location<'static>) -> Self {
                Self {
                    failure: TestStoreFailure::store_stopped_from_shutdown(origin),
                }
            }

            /// Retains the original typed production shutdown cause.
            pub(crate) fn store_stopped(
                error: StoreStopped,
                origin: &'static Location<'static>,
            ) -> Self {
                Self {
                    failure: TestStoreFailure::store_stopped(error, origin),
                }
            }

            /// Adds actor evidence without replacing the scheduler's failure.
            pub(crate) fn drive_with_snapshot(
                error: DriveError,
                diagnostics: Option<ActorDiagnostics>,
                origin: &'static Location<'static>,
            ) -> Self {
                Self {
                    failure: TestStoreFailure::drive_with_snapshot(error, diagnostics, origin),
                }
            }

            /// Owns the work snapshot that prevented exhaustive completion.
            pub(crate) fn unfinished(
                diagnostics: FinishDiagnostics,
                origin: &'static Location<'static>,
            ) -> Self {
                Self {
                    failure: TestStoreFailure::unfinished(diagnostics, origin),
                }
            }
        }
    };
}
error_constructors!(TestStoreError);
#[cfg(feature = "local")]
error_constructors!(local::TestStoreError);

/// Implements common failure policy while preserving the carrier's cause bounds.
pub(crate) struct TestStoreFailure<Cause> {
    /// One allocation keeps both public error facades pointer-sized.
    inner: Box<TestStoreErrorInner<Cause>>,
}

/// Couples owned failure evidence with the initiating test operation.
struct TestStoreErrorInner<Cause> {
    /// Failure category and any original cause or captured evidence.
    kind: TestStoreErrorKind<Cause>,
    /// User call site, never replaced by an internal helper's location.
    origin: &'static Location<'static>,
}

/// Separates assertion mistakes, actor failures and unfinished work.
enum TestStoreErrorKind<Cause> {
    /// Production authority rejected access before the operation could run.
    TaskAccess(rca_core::TaskAccessError),
    /// A declared capture failed or violated its assertion contract.
    Capture {
        /// Declared key's Rust type name, not a rendered domain value.
        name: &'static str,
        /// Registration, evaluation or equality failure.
        problem: CaptureProblem,
        /// Original opaque cause, present only when the capture itself failed.
        cause: Option<Cause>,
        /// Assertion phase that encountered the failure.
        operation: &'static str,
    },
    /// No issue was available for the expectation.
    NoIssue,
    /// The oldest issue was rejected by its matcher.
    IssueMismatch(Issue),
    /// Exhaustive completion found unasserted issues.
    UnconsumedIssues(Vec<Issue>),
    /// A prior operation still owes its terminal or snapshot assertion.
    IncompleteAssertion {
        /// Operation that created the outstanding debt.
        operation: &'static str,
    },
    /// An effect action must be received before another external action is sent.
    UnreceivedAction,
    /// No effect action is waiting at the receive boundary.
    NoReceivedAction,
    /// The pending action failed its receive matcher.
    ActionMismatch,
    /// Exhaustive mode does not permit explicitly skipping effect actions.
    SkipRequiresNonExhaustive,
    /// The production mailbox rejected admission because it was full.
    AdmissionFull,
    /// Production communication ended without a typed shutdown cause.
    StoreUnavailable,
    /// Production returned its original typed shutdown cause.
    StoreStopped(StoreStopped),
    /// Deterministic driving failed before the assertion could complete.
    Drive {
        /// Original scheduler failure, including its backtrace.
        error: DriveError,
        /// Latest actor evidence, absent if no snapshot was available.
        diagnostics: Option<CapturedRuntimeDiagnostics>,
    },
    /// A finish assertion found RCA-owned work still outstanding.
    Unfinished(FinishDiagnostics),
}

/// Distinguishes capture registration errors from evaluation and equality failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CaptureProblem {
    /// The same nominal capture key was declared more than once.
    DuplicateDeclaration,
    /// An expectation named a capture key that was not declared.
    UnknownExpectation,
    /// A capture key received more than one expectation in the same operation.
    DuplicateExpectation,
    /// Evaluating the capture returned its typed error.
    Failed,
    /// The selected independent value differed from its expectation.
    Mismatch,
}

/// Snapshots unfinished RCA-owned work at `TestStore` finish.
///
/// This owned evidence does not change when work later completes. It holds no
/// live handles and cannot cancel work, release actions or advance virtual time.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FinishDiagnostics {
    /// Actor counters and task identifiers at the failed finish boundary.
    runtime: RuntimeSnapshot,
    /// Inert cancellation metadata captured with the actor counters.
    cancellation_identities: Vec<CancellationIdentityDiagnostic>,
    /// Sleeps registered with the deterministic scheduler.
    pending_sleeps: Vec<PendingSleep>,
    /// Virtual time when finish inspected the scheduler.
    current_time: MonotonicInstant,
    /// Whether an effect action still awaited an assertion decision.
    pending_action: bool,
}

impl FinishDiagnostics {
    /// Combines already captured actor and scheduler evidence without driving work.
    pub(crate) fn new(
        diagnostics: ActorDiagnostics,
        pending_sleeps: Vec<PendingSleep>,
        current_time: MonotonicInstant,
        pending_action: bool,
    ) -> Self {
        let diagnostics = CapturedRuntimeDiagnostics::from(diagnostics);
        Self {
            runtime: diagnostics.runtime,
            cancellation_identities: diagnostics.cancellation_identities,
            pending_sleeps,
            current_time,
            pending_action,
        }
    }

    /// Returns the actor-owned runtime snapshot captured at failure.
    #[must_use]
    pub const fn runtime(&self) -> &RuntimeSnapshot {
        &self.runtime
    }

    /// Returns live cancellation identities in Store-local diagnostic order.
    #[must_use]
    pub fn cancellation_identities(&self) -> &[CancellationIdentityDiagnostic] {
        &self.cancellation_identities
    }

    /// Returns deterministic driver sleeps in deadline order.
    #[must_use]
    pub fn pending_sleeps(&self) -> &[PendingSleep] {
        &self.pending_sleeps
    }

    /// Returns the captured virtual monotonic time.
    #[must_use]
    pub const fn current_time(&self) -> MonotonicInstant {
        self.current_time
    }

    /// Returns whether an effect action remained at the assertion gate.
    #[must_use]
    pub const fn has_pending_action(&self) -> bool {
        self.pending_action
    }
}

impl<Cause> TestStoreFailure<Cause> {
    /// Borrows the original production authority cause without changing its identity.
    fn task_access_error(&self) -> Option<&rca_core::TaskAccessError> {
        match &self.inner.kind {
            TestStoreErrorKind::TaskAccess(error) => Some(error),
            _ => None,
        }
    }

    /// Records that the issue queue was empty at the expectation boundary.
    pub(crate) fn no_issue(origin: &'static Location<'static>) -> Self {
        Self::new(TestStoreErrorKind::NoIssue, origin)
    }

    /// Owns the oldest issue rejected by its expectation matcher.
    pub(crate) fn issue_mismatch(issue: Issue, origin: &'static Location<'static>) -> Self {
        Self::new(TestStoreErrorKind::IssueMismatch(issue), origin)
    }

    /// Owns the unasserted issues that prevented exhaustive completion.
    pub(crate) fn unconsumed(issues: Vec<Issue>, origin: &'static Location<'static>) -> Self {
        Self::new(TestStoreErrorKind::UnconsumedIssues(issues), origin)
    }

    /// Returns whether no issue was available for expectation.
    #[must_use]
    pub fn is_no_issue(&self) -> bool {
        matches!(self.inner.kind, TestStoreErrorKind::NoIssue)
    }

    /// Returns whether the oldest issue failed its expectation matcher.
    #[must_use]
    pub fn is_issue_mismatch(&self) -> bool {
        matches!(self.inner.kind, TestStoreErrorKind::IssueMismatch(_))
    }

    /// Returns evidence for a rejected oldest-issue matcher.
    #[must_use]
    pub fn mismatched_issue(&self) -> Option<&Issue> {
        match &self.inner.kind {
            TestStoreErrorKind::IssueMismatch(issue) => Some(issue),
            _ => None,
        }
    }

    /// Returns issues that prevented exhaustive completion.
    #[must_use]
    pub fn unconsumed_issues(&self) -> &[Issue] {
        match &self.inner.kind {
            TestStoreErrorKind::UnconsumedIssues(issues) => issues,
            _ => &[],
        }
    }

    /// Records the operation whose assertion debt must be settled first.
    pub(crate) fn incomplete_assertion(
        operation: &'static str,
        origin: &'static Location<'static>,
    ) -> Self {
        Self::new(
            TestStoreErrorKind::IncompleteAssertion { operation },
            origin,
        )
    }

    /// Requires a pending effect action to be received before another send.
    pub(crate) fn unreceived_action(origin: &'static Location<'static>) -> Self {
        Self::new(TestStoreErrorKind::UnreceivedAction, origin)
    }

    /// Reports that receive has no pending effect action to inspect.
    pub(crate) fn no_received_action(origin: &'static Location<'static>) -> Self {
        Self::new(TestStoreErrorKind::NoReceivedAction, origin)
    }

    /// Rejects the receive matcher without retaining the domain action.
    pub(crate) fn action_mismatch(origin: &'static Location<'static>) -> Self {
        Self::new(TestStoreErrorKind::ActionMismatch, origin)
    }

    /// Rejects effect-action skipping under exhaustive policy.
    pub(crate) fn skip_requires_non_exhaustive(origin: &'static Location<'static>) -> Self {
        Self::new(TestStoreErrorKind::SkipRequiresNonExhaustive, origin)
    }

    /// Records capacity rejection before the production actor admits the action.
    pub(crate) fn admission_full(origin: &'static Location<'static>) -> Self {
        Self::new(TestStoreErrorKind::AdmissionFull, origin)
    }

    /// Records unavailable admission without inventing a typed shutdown cause.
    pub(crate) fn store_stopped_from_admission(origin: &'static Location<'static>) -> Self {
        Self::new(TestStoreErrorKind::StoreUnavailable, origin)
    }

    /// Records unavailable shutdown without inventing a typed shutdown cause.
    pub(crate) fn store_stopped_from_shutdown(origin: &'static Location<'static>) -> Self {
        Self::new(TestStoreErrorKind::StoreUnavailable, origin)
    }

    /// Owns the exact shutdown cause returned by production.
    pub(crate) fn store_stopped(error: StoreStopped, origin: &'static Location<'static>) -> Self {
        Self::new(TestStoreErrorKind::StoreStopped(error), origin)
    }

    /// Attaches actor evidence to the original scheduler failure.
    pub(crate) fn drive_with_snapshot(
        error: DriveError,
        diagnostics: Option<ActorDiagnostics>,
        origin: &'static Location<'static>,
    ) -> Self {
        Self::new(
            TestStoreErrorKind::Drive {
                error,
                diagnostics: diagnostics.map(CapturedRuntimeDiagnostics::from),
            },
            origin,
        )
    }

    /// Owns the work snapshot that prevented exhaustive completion.
    pub(crate) fn unfinished(
        diagnostics: FinishDiagnostics,
        origin: &'static Location<'static>,
    ) -> Self {
        Self::new(TestStoreErrorKind::Unfinished(diagnostics), origin)
    }

    /// Allocates the common envelope without recapturing any underlying cause.
    fn new(kind: TestStoreErrorKind<Cause>, origin: &'static Location<'static>) -> Self {
        Self {
            inner: Box::new(TestStoreErrorInner { kind, origin }),
        }
    }

    /// Returns the operation's call site.
    #[must_use]
    pub const fn origin(&self) -> &'static Location<'static> {
        self.inner.origin
    }

    /// Returns whether snapshot, terminal or failed-match assertion debt remains.
    #[must_use]
    pub fn is_incomplete_assertion(&self) -> bool {
        matches!(
            self.inner.kind,
            TestStoreErrorKind::IncompleteAssertion { .. }
        )
    }

    /// Returns whether an effect action required receive first.
    #[must_use]
    pub fn is_unreceived_action(&self) -> bool {
        matches!(self.inner.kind, TestStoreErrorKind::UnreceivedAction)
    }

    /// Returns whether no effect action awaited receive.
    #[must_use]
    pub fn is_no_received_action(&self) -> bool {
        matches!(self.inner.kind, TestStoreErrorKind::NoReceivedAction)
    }

    /// Returns whether the receive matcher rejected the pending action.
    #[must_use]
    pub fn is_action_mismatch(&self) -> bool {
        matches!(self.inner.kind, TestStoreErrorKind::ActionMismatch)
    }

    /// Returns whether explicit skip was requested in exhaustive mode.
    #[must_use]
    pub fn is_skip_requires_non_exhaustive(&self) -> bool {
        matches!(
            self.inner.kind,
            TestStoreErrorKind::SkipRequiresNonExhaustive
        )
    }

    /// Returns unfinished-work diagnostics when finish was not exhaustive.
    #[must_use]
    pub fn finish_diagnostics(&self) -> Option<&FinishDiagnostics> {
        match &self.inner.kind {
            TestStoreErrorKind::Unfinished(diagnostics) => Some(diagnostics),
            _ => None,
        }
    }

    /// Returns the underlying deterministic drive failure, when present.
    #[must_use]
    pub fn drive_error(&self) -> Option<&DriveError> {
        match &self.inner.kind {
            TestStoreErrorKind::Drive { error, .. } => Some(error),
            _ => None,
        }
    }

    /// Returns actor diagnostics captured before driving or exhaustive finish failed.
    #[must_use]
    pub fn runtime_snapshot(&self) -> Option<&RuntimeSnapshot> {
        match &self.inner.kind {
            TestStoreErrorKind::Drive { diagnostics, .. } => {
                diagnostics.as_ref().map(|diagnostics| &diagnostics.runtime)
            }
            TestStoreErrorKind::Unfinished(diagnostics) => Some(diagnostics.runtime()),
            _ => None,
        }
    }

    /// Returns cancellation identities captured before operation failure.
    #[must_use]
    pub fn cancellation_identities(&self) -> &[CancellationIdentityDiagnostic] {
        match &self.inner.kind {
            TestStoreErrorKind::Drive { diagnostics, .. } => diagnostics
                .as_ref()
                .map_or(&[], |diagnostics| &diagnostics.cancellation_identities),
            TestStoreErrorKind::Unfinished(diagnostics) => diagnostics.cancellation_identities(),
            _ => &[],
        }
    }
}

impl<Cause> TestStoreFailure<Cause> {
    /// Records a declaration or expectation problem without inventing a capture cause.
    pub(crate) fn capture_problem<K: 'static>(
        problem: CaptureProblem,
        origin: &'static Location<'static>,
    ) -> Self {
        Self::new(
            TestStoreErrorKind::Capture {
                name: type_name::<K>(),
                problem,
                cause: None,
                operation: "declare/expect",
            },
            origin,
        )
    }

    /// Owns a failed capture's opaque cause together with its nominal key name.
    pub(crate) fn capture_failed<K: 'static>(
        cause: Cause,
        origin: &'static Location<'static>,
        operation: &'static str,
    ) -> Self {
        Self::new(
            TestStoreErrorKind::Capture {
                name: type_name::<K>(),
                problem: CaptureProblem::Failed,
                cause: Some(cause),
                operation,
            },
            origin,
        )
    }

    /// Records unequal selected values without retaining or rendering those values.
    pub(crate) fn capture_mismatch(
        name: &'static str,
        origin: &'static Location<'static>,
        operation: &'static str,
    ) -> Self {
        Self::new(
            TestStoreErrorKind::Capture {
                name,
                problem: CaptureProblem::Mismatch,
                cause: None,
                operation,
            },
            origin,
        )
    }

    /// Returns whether a declared independent value differed from its expectation.
    #[must_use]
    pub fn is_capture_mismatch(&self) -> bool {
        matches!(
            self.inner.kind,
            TestStoreErrorKind::Capture {
                problem: CaptureProblem::Mismatch,
                ..
            }
        )
    }

    /// Recovers the original cause only for its exact declared capture key.
    ///
    /// Wrong keys return the original error unchanged. The cause need not be
    /// Debug, Error or Sync; extraction consumes the enclosing error.
    ///
    /// # Errors
    ///
    /// Returns this error when it has no cause for the selected key.
    pub(crate) fn into_capture_error<K: 'static, E: 'static>(mut self) -> Result<E, Self>
    where
        Cause: CaptureCause,
    {
        if let TestStoreErrorKind::Capture { cause, .. } = &mut self.inner.kind
            && let Some(stored) = cause.take()
        {
            match stored.recover::<K, E>() {
                Ok(value) => return Ok(value),
                Err(original) => *cause = Some(original),
            }
        }
        Err(self)
    }
}

impl<Cause> fmt::Debug for TestStoreFailure<Cause> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = f.debug_struct("TestStoreError");
        debug.field("origin", &self.inner.origin);
        match &self.inner.kind {
            TestStoreErrorKind::Capture {
                name,
                problem,
                operation,
                ..
            } => debug
                .field("kind", problem)
                .field("capture", name)
                .field("operation", operation),
            TestStoreErrorKind::TaskAccess(error) => {
                debug.field("kind", &"TaskAccess").field("error", error)
            }
            TestStoreErrorKind::NoIssue => debug.field("kind", &"NoIssue"),
            TestStoreErrorKind::IssueMismatch(issue) => {
                debug.field("kind", &"IssueMismatch").field("issue", issue)
            }
            TestStoreErrorKind::UnconsumedIssues(issues) => debug
                .field("kind", &"UnconsumedIssues")
                .field("issues", issues),
            TestStoreErrorKind::IncompleteAssertion { operation } => debug
                .field("kind", &"IncompleteAssertion")
                .field("operation", operation),
            TestStoreErrorKind::UnreceivedAction => debug.field("kind", &"UnreceivedAction"),
            TestStoreErrorKind::NoReceivedAction => debug.field("kind", &"NoReceivedAction"),
            TestStoreErrorKind::ActionMismatch => debug.field("kind", &"ActionMismatch"),
            TestStoreErrorKind::SkipRequiresNonExhaustive => {
                debug.field("kind", &"SkipRequiresNonExhaustive")
            }
            TestStoreErrorKind::AdmissionFull => debug.field("kind", &"AdmissionFull"),
            TestStoreErrorKind::StoreUnavailable => debug.field("kind", &"StoreUnavailable"),
            TestStoreErrorKind::StoreStopped(error) => debug.field("kind", error),
            TestStoreErrorKind::Drive { error, diagnostics } => {
                debug.field("kind", error).field("diagnostics", diagnostics)
            }
            TestStoreErrorKind::Unfinished(diagnostics) => debug.field("kind", diagnostics),
        };
        debug.finish()
    }
}

impl<Cause> fmt::Display for TestStoreFailure<Cause> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match &self.inner.kind {
            TestStoreErrorKind::Capture {
                name,
                problem,
                operation,
                ..
            } => {
                return write!(
                    f,
                    "TestStore capture {name} {problem:?} during {operation} at {}",
                    self.inner.origin
                );
            }
            TestStoreErrorKind::TaskAccess(error) => {
                return write!(f, "task access rejected: {error}");
            }
            TestStoreErrorKind::NoIssue => "no issue awaits expectation",
            TestStoreErrorKind::IssueMismatch(issue) => {
                return write!(
                    f,
                    "oldest issue did not match expectation at {}: {issue:?}",
                    self.inner.origin
                );
            }
            TestStoreErrorKind::UnconsumedIssues(issues) => {
                return write!(
                    f,
                    "TestStore finished with unconsumed issues at {}: {issues:?}",
                    self.inner.origin
                );
            }
            TestStoreErrorKind::IncompleteAssertion { operation } => {
                return write!(
                    f,
                    "TestStore incomplete assertion for {operation} at {}",
                    self.inner.origin
                );
            }
            TestStoreErrorKind::UnreceivedAction => {
                "receive pending effect action before sending external action"
            }
            TestStoreErrorKind::NoReceivedAction => "no effect action awaits receive",
            TestStoreErrorKind::ActionMismatch => "pending effect action did not match receive",
            TestStoreErrorKind::SkipRequiresNonExhaustive => {
                "effect actions may be skipped only in non-exhaustive mode"
            }
            TestStoreErrorKind::AdmissionFull => "TestStore mailbox is full",
            TestStoreErrorKind::StoreUnavailable => {
                "production Store unavailable during test operation"
            }
            TestStoreErrorKind::StoreStopped(_) => "production Store stopped during test operation",
            TestStoreErrorKind::Drive { .. } => "deterministic scheduler drive failed",
            TestStoreErrorKind::Unfinished(_) => "TestStore finished with RCA-owned work pending",
        };
        write!(f, "{message} at {}", self.inner.origin)
    }
}

impl<Cause> std::error::Error for TestStoreFailure<Cause> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.inner.kind {
            TestStoreErrorKind::TaskAccess(error) => Some(error),
            TestStoreErrorKind::StoreStopped(error) => Some(error),
            TestStoreErrorKind::Drive { error, .. } => Some(error),
            _ => None,
        }
    }
}

#[cfg(all(test, feature = "local"))]
mod capture_storage_tests {
    use super::*;
    use std::cell::Cell;
    use std::panic::{RefUnwindSafe, UnwindSafe};
    use std::rc::Rc;

    struct First;
    struct Second;

    impl crate::local::CaptureKey for First {
        type Value = ();
        type Error = Rc<OpaqueCause>;
    }
    impl crate::local::CaptureKey for Second {
        type Value = ();
        type Error = Rc<OpaqueCause>;
    }

    struct OpaqueCause {
        owner: std::thread::ThreadId,
        drops: Rc<Cell<usize>>,
    }

    impl Drop for OpaqueCause {
        fn drop(&mut self) {
            assert_eq!(self.owner, std::thread::current().id());
            self.drops.set(self.drops.get() + 1);
        }
    }

    fn cause_address(failure: &mut TestStoreFailure<LocalCause>) -> *const () {
        let TestStoreErrorKind::Capture {
            cause: Some(cause), ..
        } = &mut failure.inner.kind
        else {
            panic!("fixture must retain its original capture cause");
        };
        std::ptr::from_ref(cause.0.0.get_mut().as_ref()).cast::<()>()
    }

    #[test]
    fn local_cause_survives_repeated_wrong_keys_without_replacement_or_clone() {
        // A bare-E downcast would let Second steal First's cause. Reboxing or
        // cloning on a miss would also violate retained original ownership.
        let drops = Rc::new(Cell::new(0));
        let cause = Rc::new(OpaqueCause {
            owner: std::thread::current().id(),
            drops: Rc::clone(&drops),
        });
        let pointer = Rc::as_ptr(&cause);
        let origin = Location::caller();
        let mut failure = TestStoreFailure::capture_failed::<First>(
            LocalCause::new::<First, _>(cause),
            origin,
            "declare",
        );
        let inner = std::ptr::from_ref(failure.inner.as_ref());
        let allocation = cause_address(&mut failure);
        let display = failure.to_string();
        let debug = format!("{failure:?}");
        for _ in 0..8 {
            failure = match failure.into_capture_error::<Second, Rc<OpaqueCause>>() {
                Ok(_) => panic!("a different key recovered the original cause"),
                Err(failure) => failure,
            };
            assert_eq!(std::ptr::from_ref(failure.inner.as_ref()), inner);
            assert_eq!(cause_address(&mut failure), allocation);
            assert_eq!(failure.origin(), origin);
            assert_eq!(failure.to_string(), display);
            assert_eq!(format!("{failure:?}"), debug);
            assert_eq!(drops.get(), 0);
        }
        let recovered = failure
            .into_capture_error::<First, Rc<OpaqueCause>>()
            .unwrap();
        assert_eq!(Rc::as_ptr(&recovered), pointer);
        assert_eq!(Rc::strong_count(&recovered), 1);
        drop(recovered);
        assert_eq!(drops.get(), 1);
    }

    #[test]
    fn root_error_retains_transfer_and_unwind_traits() {
        fn traits<T: Send + Sync + UnwindSafe + RefUnwindSafe>() {}
        traits::<TestStoreError>();
    }

    #[test]
    fn local_nominal_error_drops_unrecovered_cause_once_on_owner() {
        let drops = Rc::new(Cell::new(0));
        let cause = Rc::new(OpaqueCause {
            owner: std::thread::current().id(),
            drops: Rc::clone(&drops),
        });
        let failure = TestStoreFailure::capture_failed::<First>(
            LocalCause::new::<First, _>(cause),
            Location::caller(),
            "receive",
        );
        let error = local::TestStoreError { failure };
        assert!(!error.is_capture_mismatch());
        assert!(std::error::Error::source(&error).is_none());
        assert!(error.to_string().contains("during receive"));
        assert!(format!("{error:?}").contains("Failed"));
        let Err(error) = error.into_capture_error::<Second>() else {
            panic!("wrong key recovered cause");
        };
        assert_eq!(drops.get(), 0);
        drop(error);
        assert_eq!(drops.get(), 1);
    }

    #[test]
    fn local_recovery_miss_keeps_no_cause_and_non_capture_metadata() {
        for failure in [
            TestStoreFailure::<LocalCause>::no_issue(Location::caller()),
            TestStoreFailure::capture_problem::<First>(
                CaptureProblem::UnknownExpectation,
                Location::caller(),
            ),
        ] {
            let error = local::TestStoreError { failure };
            let origin = error.origin();
            let display = error.to_string();
            let debug = format!("{error:?}");
            let no_issue = error.is_no_issue();
            let Err(error) = error.into_capture_error::<First>() else {
                panic!("error without cause recovered one");
            };
            assert_eq!(error.origin(), origin);
            assert_eq!(error.to_string(), display);
            assert_eq!(format!("{error:?}"), debug);
            assert_eq!(error.is_no_issue(), no_issue);
            assert!(!error.is_issue_mismatch());
            assert!(error.mismatched_issue().is_none());
            assert!(error.unconsumed_issues().is_empty());
            assert!(!error.is_incomplete_assertion());
            assert!(!error.is_unreceived_action());
            assert!(!error.is_no_received_action());
            assert!(!error.is_action_mismatch());
            assert!(!error.is_skip_requires_non_exhaustive());
            assert!(error.finish_diagnostics().is_none());
            assert!(error.drive_error().is_none());
            assert!(error.runtime_snapshot().is_none());
            assert!(error.cancellation_identities().is_empty());
            assert!(std::error::Error::source(&error).is_none());
        }
    }

    #[test]
    fn local_non_capture_recovery_preserves_original_typed_sources() {
        // Reconstructing unrelated errors on a capture miss would replace the
        // original source allocation or metadata despite having no capture cause.
        struct Unchanged;
        impl rca_core::Reducer for Unchanged {
            type State = ();
            type Action = ();
            fn reduce(
                &mut self,
                (): &mut (),
                (): &(),
                _: &mut rca_core::ReduceContext<'_>,
            ) -> rca_core::Effect<()> {
                rca_core::Effect::none()
            }
        }
        let mut store = crate::TestStore::new((), Unchanged);
        futures_executor::block_on(store.finish()).unwrap();
        let stopped = futures_executor::block_on(store.assert_state_eq(())).unwrap_err();
        let TestStoreErrorKind::StoreStopped(cause) = stopped.failure.inner.kind else {
            panic!("finished Store inspection must retain StoreStopped");
        };
        let stopped = TestStoreFailure::<LocalCause>::store_stopped(cause, Location::caller());
        let host = crate::local::DeterministicRuntime::with_step_budget(1);
        host.spawn_task(Box::pin(std::future::poll_fn(|context| {
            context.waker().wake_by_ref();
            std::task::Poll::Pending
        })));
        let drive = host.run_until_stalled().unwrap_err();
        let drive = TestStoreFailure::drive_with_snapshot(drive, None, Location::caller());
        for failure in [stopped, drive] {
            let error = local::TestStoreError { failure };
            let origin = error.origin();
            let display = error.to_string();
            let debug = format!("{error:?}");
            let source = std::error::Error::source(&error).unwrap();
            let pointer = std::ptr::from_ref(source).cast::<()>();
            assert!(source.is::<StoreStopped>() || source.is::<DriveError>());
            let Err(error) = error.into_capture_error::<First>() else {
                panic!("non-capture error recovered a cause");
            };
            assert_eq!(error.origin(), origin);
            assert_eq!(error.to_string(), display);
            assert_eq!(format!("{error:?}"), debug);
            assert_eq!(
                std::ptr::from_ref(std::error::Error::source(&error).unwrap()).cast::<()>(),
                pointer
            );
            if let Some(drive) = error.drive_error() {
                assert_eq!(drive.polls(), 1);
                assert!(drive.is_step_budget_exhausted());
            }
        }
    }

    #[test]
    fn capture_envelopes_report_padded_sizes_without_changing_wrapper_shape() {
        use std::mem::{align_of, size_of};
        assert_eq!(size_of::<TestStoreError>(), size_of::<usize>());
        assert_eq!(size_of::<local::TestStoreError>(), size_of::<usize>());
        println!(
            "capture_layout shared_cause={} local_cause={} cause_option={} cause_align={} inner={} local_inner={}",
            size_of::<SharedCause>(),
            size_of::<LocalCause>(),
            size_of::<Option<SharedCause>>(),
            align_of::<SharedCause>(),
            size_of::<TestStoreErrorInner<SharedCause>>(),
            size_of::<TestStoreErrorInner<LocalCause>>()
        );
    }
}

/// Reports failed driving or causal work that remains unfinished after driving.
///
/// [`Self::drive_error`] and [`std::error::Error::source`] retain the scheduler's
/// cause when driving failed. Otherwise the task remained unfinished after ready
/// work stalled. Both cases retain the action's call site and available evidence;
/// holding the error does not keep that causal work alive or prevent recovery.
pub struct TestTaskError {
    /// Keeps the public wrapper pointer-sized while owning failure evidence.
    inner: Box<TestTaskErrorInner>,
}

/// Couples task-completion evidence with the action that created the task.
struct TestTaskErrorInner {
    /// Scheduler failure or unfinished causal work after a successful drive.
    kind: TestTaskErrorKind,
    /// Original action call site, not the later finish call.
    origin: &'static Location<'static>,
}

/// Separates a failed scheduler drive from work that stalled normally.
enum TestTaskErrorKind {
    /// Driving failed before causal completion could be checked.
    Drive {
        /// Original scheduler failure, including its captured backtrace.
        error: DriveError,
        /// Latest stable actor evidence when one was available.
        diagnostics: Option<CapturedRuntimeDiagnostics>,
    },
    /// Ready work stalled but the causal task had not finished.
    Blocked {
        /// Scheduler readiness captured after the drive completed.
        ready_task_ids: Vec<u64>,
        /// Virtual sleeps still registered at that boundary.
        pending_sleeps: Vec<PendingSleep>,
        /// Virtual time when completion was checked.
        current_time: MonotonicInstant,
        /// Latest stable actor evidence when one was available.
        diagnostics: Option<CapturedRuntimeDiagnostics>,
    },
}

impl TestTaskError {
    /// Retains a drive failure without claiming the causal task itself has stalled.
    pub(crate) fn drive(
        error: DriveError,
        diagnostics: Option<ActorDiagnostics>,
        origin: &'static Location<'static>,
    ) -> Self {
        Self {
            inner: Box::new(TestTaskErrorInner {
                kind: TestTaskErrorKind::Drive {
                    error,
                    diagnostics: diagnostics.map(CapturedRuntimeDiagnostics::from),
                },
                origin,
            }),
        }
    }

    /// Captures unfinished causal work after ready work has stalled normally.
    pub(crate) fn blocked(
        origin: &'static Location<'static>,
        ready_task_ids: Vec<u64>,
        pending_sleeps: Vec<PendingSleep>,
        current_time: MonotonicInstant,
        diagnostics: Option<ActorDiagnostics>,
    ) -> Self {
        Self {
            inner: Box::new(TestTaskErrorInner {
                kind: TestTaskErrorKind::Blocked {
                    ready_task_ids,
                    pending_sleeps,
                    current_time,
                    diagnostics: diagnostics.map(CapturedRuntimeDiagnostics::from),
                },
                origin,
            }),
        }
    }

    /// Returns the action call site that created the causal task.
    #[must_use]
    pub const fn origin(&self) -> &'static Location<'static> {
        self.inner.origin
    }

    /// Returns the underlying deterministic drive failure, when present.
    #[must_use]
    pub fn drive_error(&self) -> Option<&DriveError> {
        match &self.inner.kind {
            TestTaskErrorKind::Drive { error, .. } => Some(error),
            TestTaskErrorKind::Blocked { .. } => None,
        }
    }

    /// Returns ready scheduler task identifiers captured at failure.
    #[must_use]
    pub fn ready_task_ids(&self) -> &[u64] {
        match &self.inner.kind {
            TestTaskErrorKind::Drive { error, .. } => error.ready_task_ids(),
            TestTaskErrorKind::Blocked { ready_task_ids, .. } => ready_task_ids,
        }
    }

    /// Returns registered virtual sleeps captured at failure.
    #[must_use]
    pub fn pending_sleeps(&self) -> &[PendingSleep] {
        match &self.inner.kind {
            TestTaskErrorKind::Drive { error, .. } => error.pending_sleeps(),
            TestTaskErrorKind::Blocked { pending_sleeps, .. } => pending_sleeps,
        }
    }

    /// Returns the virtual time captured at failure.
    #[must_use]
    pub const fn current_time(&self) -> MonotonicInstant {
        match &self.inner.kind {
            TestTaskErrorKind::Drive { error, .. } => error.current_time(),
            TestTaskErrorKind::Blocked { current_time, .. } => *current_time,
        }
    }

    /// Returns the latest stable actor diagnostics captured before task failure.
    #[must_use]
    pub fn runtime_snapshot(&self) -> Option<&RuntimeSnapshot> {
        match &self.inner.kind {
            TestTaskErrorKind::Drive { diagnostics, .. }
            | TestTaskErrorKind::Blocked { diagnostics, .. } => {
                diagnostics.as_ref().map(|diagnostics| &diagnostics.runtime)
            }
        }
    }

    /// Returns cancellation identities captured before task failure.
    #[must_use]
    pub fn cancellation_identities(&self) -> &[CancellationIdentityDiagnostic] {
        match &self.inner.kind {
            TestTaskErrorKind::Drive { diagnostics, .. }
            | TestTaskErrorKind::Blocked { diagnostics, .. } => diagnostics
                .as_ref()
                .map_or(&[], |diagnostics| &diagnostics.cancellation_identities),
        }
    }
}

impl fmt::Debug for TestTaskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TestTaskError")
            .field("origin", &self.inner.origin)
            .field("drive_error", &self.drive_error())
            .field("ready_task_ids", &self.ready_task_ids())
            .field("pending_sleeps", &self.pending_sleeps())
            .field("current_time", &self.current_time())
            .field("runtime", &self.runtime_snapshot())
            .field("cancellation_identities", &self.cancellation_identities())
            .finish_non_exhaustive()
    }
}

impl fmt::Display for TestTaskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(error) = self.drive_error() {
            return write!(
                f,
                "finishing causal task from {} failed while driving; RCA runtime: {:?}: {error}",
                self.inner.origin,
                self.runtime_snapshot(),
            );
        }
        write!(
            f,
            "causal task from {} remains blocked at {:?}; ready tasks: {:?}; pending sleeps: {:?}; RCA runtime: {:?}",
            self.inner.origin,
            self.current_time(),
            self.ready_task_ids(),
            self.pending_sleeps(),
            self.runtime_snapshot()
        )
    }
}

impl std::error::Error for TestTaskError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.drive_error()
            .map(|error| error as &(dyn std::error::Error + 'static))
    }
}

#[cfg(test)]
mod diagnostic_storage_tests {
    use super::*;
    use futures_executor::block_on;
    use rca_core::{Effect, ReduceContext, Reducer, Store};
    use std::time::Duration;

    struct PendingWork;

    impl Reducer for PendingWork {
        type State = ();
        type Action = ();

        fn reduce(&mut self, (): &mut (), (): &(), _: &mut ReduceContext<'_>) -> Effect<()> {
            Effect::merge([
                Effect::send_after(Duration::from_secs(5), ()),
                Effect::run(|_| std::future::pending()),
            ])
            .cancellable("pending work")
        }
    }

    #[test]
    fn diagnostic_conversion_moves_owned_runtime_buffers() {
        let host = crate::DeterministicRuntime::new();
        let store = Store::new((), PendingWork, host.runtime());
        block_on(async {
            let task = host.run(store.send(())).await.unwrap().unwrap();
            host.run_until_stalled().unwrap();
            let diagnostics = host
                .run(store.runtime_diagnostics())
                .await
                .unwrap()
                .unwrap();
            assert!(!diagnostics.runtime().live_task_ids.is_empty());
            assert!(!diagnostics.runtime().timer_deadlines.is_empty());
            assert!(!diagnostics.cancellation_identities().is_empty());
            let original_tasks = diagnostics.runtime().live_task_ids.as_ptr();
            let original_timers = diagnostics.runtime().timer_deadlines.as_ptr();
            let expected = diagnostics.runtime().clone();
            let expected_identities = diagnostics.cancellation_identities().to_vec();

            // Exercise the public consuming API separately from the error adapter.
            let direct = diagnostics.clone();
            let direct_tasks = direct.runtime().live_task_ids.as_ptr();
            let direct_timers = direct.runtime().timer_deadlines.as_ptr();
            let runtime = direct.into_runtime();
            assert_eq!(runtime, expected);
            assert_eq!(runtime.live_task_ids.as_ptr(), direct_tasks);
            assert_eq!(runtime.timer_deadlines.as_ptr(), direct_timers);

            let captured = CapturedRuntimeDiagnostics::from(diagnostics);
            assert_eq!(captured.runtime, expected);
            assert_eq!(
                captured.cancellation_identities.len(),
                expected_identities.len()
            );
            for (actual, expected) in captured
                .cancellation_identities
                .iter()
                .zip(expected_identities)
            {
                assert_eq!(actual.diagnostic_id(), expected.diagnostic_id);
                assert_eq!(actual.scope_id(), expected.scope_id);
                assert_eq!(actual.type_name(), expected.type_name);
            }
            let moved = (
                captured.runtime.live_task_ids.as_ptr() == original_tasks,
                captured.runtime.timer_deadlines.as_ptr() == original_timers,
            );
            task.cancel();
            host.run(store.shutdown()).await.unwrap().unwrap();
            assert_eq!(
                moved,
                (true, true),
                "owned runtime buffers must not be cloned"
            );
        });
    }
}
