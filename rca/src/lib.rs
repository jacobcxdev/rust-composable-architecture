#![deny(rustdoc::broken_intra_doc_links)]
#![forbid(unsafe_code)]

//! Builds headless applications from composable state, actions and effects.
//!
//! Shared APIs use root imports such as [`Store`], [`Feature`] and [`Runtime`].
//! A [`Reducer`] changes state synchronously and returns [`Effect`] values.
//! [`combine`], [`scope`] and [`for_each`] compose reducers, including keyed child instances.
//! [`Feature`] also supports typed [`Task`] values and mounted child features.
//! The caller supplies the runtime. RCA installs no executor or UI framework.
//!
//! # Features
//!
//! Default features are empty. Each optional feature adds a separate capability.
//!
//! | Feature | Capability |
//! | --- | --- |
//! | `local` | Owner-thread types under `local` for non-`Send` values. |
//! | `diagnostics` | Reducer tracing and lifecycle callbacks under `diagnostics`. |
//! | `futures-stream` | Stream interoperability for observations. |
//! | `macros` | Reducer, path, snapshot and dependency macros. |
//! | `test-util` | `TestStore` and deterministic test tools. |
//! | `tokio` | `TokioRuntime` for an existing Tokio runtime. |
//!
//! Reducer tracing runs only in debug builds. Lifecycle callbacks remain available
//! in release builds. Clocks and issue reporters do not require a particular runtime.
//!
//! With `test-util`, `TestStore` compares state after every send and receive.
//! Move-only state can use independent projected snapshots. Unchanged steps use
//! `send` or `receive`. Changed steps use `send_with` or `receive_with`.
//! Explicit `finish` and normal teardown both reject unfinished test obligations.
//!
//! Controlled futures, streams and gates come directly from `rca-test`.
//! Persistence comes from `rca-persistence`, with explicit codec and backend selection.
//! Its receipts acknowledge published persistence results. If saved state is
//! required, close the document before Store shutdown. Before restart, wait for
//! the backend to stop all earlier writes.
//!
//! # Examples
//!
//! ```
//! use rca::{Effect, Reduce, Runtime, Store};
//!
//! // Runtime selection belongs to the application, not the feature.
//! fn counter(runtime: Runtime) -> Store<usize, ()> {
//!     let reducer = Reduce::new(|count: &mut usize, _: &(), _| {
//!         *count += 1;
//!         Effect::none()
//!     });
//!
//!     Store::new(0, reducer, runtime)
//! }
//! ```

#[cfg(feature = "local")]
#[doc(inline)]
pub use rca_core::local;

#[cfg(feature = "diagnostics")]
#[doc(inline)]
pub use rca_core::diagnostics;

#[doc(inline)]
pub use rca_core::{
    ActionEmbedding, ActionPath, ActionReceipt, ActionReceiptFuture, ActionTask,
    AggregateActionPath, AggregateCase, AggregateStatePath, ApplicationIds, CaseStatePath,
    CivilData, Clock, ClockCapabilities, ClockDriver, ClockTimer, CloneActionPath, Combine,
    ComposedActionEmbedding, ComposedActionPath, ComputedPath, DEFAULT_MAILBOX_CAPACITY,
    DependencyDefaultPolicy, DependencyDefaults, DependencyKey, DependencyMockMismatch,
    DependencyPreparationError, DependencyReducer, DependencyRegistrationError, DependencyValues,
    Dismiss, DuplicateId, Effect, EffectError, EffectSendError, EffectSender, ExternalObservation,
    ForEach, ForEachShared, IfCase, IfCaseIdentified, IfSome, IfSomeIdentified,
    InvalidTimerInterval, Issue, IssueCallback, IssueContext, IssueDelivery, IssueDeliveryFailure,
    IssueReportError, IssueReporter, IssueReporters, IssueSeverity, KeyedAction, KeyedActionPath,
    KeyedActionPayload, KeyedDecodeError, KeyedMergeDecision, KeyedMutationError,
    KeyedPositionError, KeyedReserveError, KeyedState, KeyedStatePath, KeyedStatePayload,
    LiftedActionEmbedding, LiftedActionPath, MissingChildPolicy, MonotonicInstant,
    MutableAggregateCase, Observation, ObservationError, ObservationProducer, OnChange,
    OperationReport, OptionalStatePath, OptionalStatePayload, OwnedCaseMismatch, OwnedCasePath,
    PredicateWait, PredicateWaitOutcome, Presentation, PresentationAction, RandomBytes, Reduce,
    ReduceContext, Reducer, ReducerExt, Runtime, RuntimeDriver, RuntimeFuture, Scope,
    ScopeIdentified, ScopedStore, ScopedStoreEnded, SharedReducer, SharedScope, ShutdownError,
    StatePath, Store, StoreBuildError, StoreBuilder, StoreStopped, StoredActionPath,
    TryScopedSendError, TrySendError, UnsupportedClockTolerance, WallTime, combine, constant_path,
    err_path, for_each, for_each_shared, identity_action_path, identity_keyed_action_path,
    identity_keyed_state_path, identity_state_path, if_case, if_case_identified, if_some,
    if_some_identified, none_path, ok_path, on_change, presentation, scope, scope_identified,
    scope_shared, some_path, try_dependency_mock, unit_path, with_dependency,
};

#[doc(inline)]
pub use rca_core::Snapshot;
#[doc(inline)]
pub use rca_core::{
    CancellingOperationWait, Operation, OperationPolicy, OperationStatus, OperationWait, SelfWait,
};
#[doc(inline)]
pub use rca_core::{
    CommandPort, CommandReceipt, CommandSite, Committed, EntryMount, Feature, FeatureContext,
    FeatureStoreBuilder, Fold, FoldKey, FoldView, InitialPolicy, InputGate, MountChildren,
    MountIdentity, MountSite, OptionalMount, Output, OutputDelivery, OutputError, OutputFlow,
    OutputHandlers, ReactionChange, ReactionContext, ReducerFeature, RemovalOrigin, RemovalSite,
    RemovedChild, RemovedOutputError, RootMount, Task, TaskAccessError, TaskAccessReason,
    TaskContext, TaskProgress, TaskSendError, TryCommandError,
};
#[doc(inline)]
pub use rca_core::{
    EncodedStackRoute, Stack, StackAction, StackDecodeError, StackEncoding, StackId,
    StackMutationError, StackState, stack,
};
#[doc(inline)]
pub use rca_core::{Ephemeral, WeakScopedStore, ephemeral};

#[cfg(feature = "test-util")]
#[doc(inline)]
pub use rca_core::{
    EndpointStub, ForbiddenClock, ForbiddenClockUsed, ImmediateClock, IssueCollector,
};

#[doc(inline)]
pub use rca_core::{
    Shared, SharedAccessError, SharedKey, SharedKeyCache, SharedKeyDefault, SharedKeyError,
    SharedKeyIdentity, SharedLoadContext, SharedMutationError, SharedProjection, SharedPublication,
    SharedReadProjection, SharedReader, SharedReaderKey, SharedSubscribeContext, SharedSubscriber,
};

#[cfg(feature = "macros")]
#[doc(inline)]
pub use rca_macros::{
    ActionPaths, CaseDescriptors, DependencyClient, DependencyKey, Snapshot, StatePaths, reducer,
    reducers,
};

#[cfg(feature = "test-util")]
#[doc(inline)]
pub use rca_test::{
    CancellationIdentityDiagnostic, CaptureExpectations, CaptureKey, CaseAssertionFailure,
    ChangeCheck, ComparisonFailure, DEFAULT_STEP_BUDGET, DeterministicRuntime, DriveError,
    DriveReport, Exhaustivity, FinishDiagnostics, IssueExpectation, IssueExpectationError,
    IssueExpectationMode, OmissionReporting, PendingSleep, TestActionTask, TestStore,
    TestStoreBuilder, TestStoreError, TestTaskError, check_case, check_case_with, check_change,
    check_change_async, check_equal, check_expected_changes, conformance, expect_issues,
    expect_issues_async,
};

#[cfg(feature = "tokio")]
#[doc(inline)]
pub use rca_tokio::{TokioRuntime, TokioRuntimeError};
