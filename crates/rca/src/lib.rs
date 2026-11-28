#![deny(rustdoc::broken_intra_doc_links)]
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]

//! Builds headless applications from composable state, actions and effects.
//!
//! Shared APIs use root imports such as [`Store`], [`Feature`] and [`Runtime`].
//! A [`Reducer`] changes state synchronously and returns [`Effect`] values.
//! [`combine`], [`scope`] and [`for_each`] compose reducers, including keyed child instances.
//! Every reducer implements [`Feature`] automatically. Both forms support mounted children.
//! A direct `Feature` implementation can also declare typed [`Task`] values.
//! The caller supplies the runtime. RCA installs no executor or UI framework.
//!
//! Start with the [task guides][guides] or run the counter in the
//! [example applications][examples]. In an unreleased local checkout, open
//! `docs/README.md` or `examples/README.md` from that checkout.
//!
//! # Choosing an authoring form
//!
//! Start with [`Reduce::new`] or [`Reducer`] when inputs and asynchronous results
//! pass through actions. Use [`Feature`] directly when tasks need typed reads
//! or updates of current State between awaits. [`Reducer::reduce`] and
//! [`Feature::update`] run synchronously. You can pass either kind of feature
//! to [`Store::new`] or [`Store::builder`].
//! Reducer-only combinators require `Reducer`; installed children accept either form.
//! A type implements one trait explicitly, not both. See [`Feature`] for the
//! ownership and testing consequences.
//!
//! # Dependencies and shared state
//!
//! Use a dependency to replace a service, such as a clock or a network client.
//! Define a [dependency key](trait@DependencyKey) and supply its value through
//! [`StoreBuilder::with_dependency`]. [`DependencyValues`] holds the values for that Store.
//!
//! Keep data in feature state unless another owner needs access to the same value.
//! [`Shared`] gives owners read and write access. [`SharedReader`] gives an owner read access.
//! [`SharedKeyCache`] lets independent callers find the same value by key.
//! Both dependencies and shared state are part of `rca`. Neither requires a Cargo feature.
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
//! With `test-util`, `TestStore` compares state after each strict send or receive.
//! Move-only state can use independent projected snapshots. Unchanged steps use
//! `send` or `receive`. Changed steps use `send_with` or `receive_with`.
//! Explicit `finish` and normal teardown both reject unfinished test obligations.
//!
//! Controlled futures, streams and gates come directly from `rca-test`.
//! Persistence comes from `rca-persistence`, with explicit codec and backend selection.
//! Its receipts acknowledge published persistence results. If saved state is
//! required, wait for the document to load, send its close action, and check
//! the persistence receipt before Store shutdown. Before restart, wait for
//! the backend to stop all earlier writes.
//!
//! # Examples
//!
//! Give this function your application's runtime. It sends one action and reads
//! its committed result. It needs only `rca`, with no optional features.
//!
//! ```
//! use rca::{Effect, Reduce, Runtime, Store};
//!
//! async fn count_once(runtime: Runtime) -> Result<(), Box<dyn std::error::Error>> {
//!     let reducer = Reduce::new(|count: &mut usize, _: &(), _| {
//!         *count += 1;
//!         Effect::none()
//!     });
//!
//!     let store = Store::new(0, reducer, runtime);
//!     let task = store.send(()).await?;
//!     task.finish().await;
//!     assert_eq!(store.read(|count| *count).await?, 1);
//!     store.shutdown().await?;
//!     Ok(())
//! }
//! ```
#![cfg_attr(rca_docs_source, doc = concat!(
    "\n[guides]: ", env!("CARGO_PKG_REPOSITORY"), "/blob/", env!("RCA_DOCS_REVISION"), "/docs/README.md",
    "\n[examples]: ", env!("CARGO_PKG_REPOSITORY"), "/blob/", env!("RCA_DOCS_REVISION"), "/examples/README.md"
))]
#![cfg_attr(not(rca_docs_source), doc = concat!(
    "\n[guides]: ", env!("CARGO_PKG_REPOSITORY"), "/blob/v", env!("CARGO_PKG_VERSION"), "/docs/README.md",
    "\n[examples]: ", env!("CARGO_PKG_REPOSITORY"), "/blob/v", env!("CARGO_PKG_VERSION"), "/examples/README.md"
))]

#[cfg(feature = "local")]
#[cfg_attr(docsrs, doc(cfg(feature = "local")))]
#[doc(inline)]
pub use rca_core::local;

#[cfg(feature = "diagnostics")]
#[cfg_attr(docsrs, doc(cfg(feature = "diagnostics")))]
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

// Generated paths name the library both inside it and from sibling binary targets.
#[cfg(feature = "macros")]
extern crate self as rca;

#[doc(inline)]
pub use rca_core::Snapshot;
#[doc(inline)]
pub use rca_core::{
    CancellingOperationWait, Operation, OperationPolicy, OperationStatus, OperationWait, SelfWait,
};
#[doc(inline)]
pub use rca_core::{
    CommandPort, CommandReceipt, CommandSite, Committed, EntryMount, Feature, FeatureContext, Fold,
    FoldKey, FoldView, InitialPolicy, InputGate, MountChildren, MountIdentity, MountSite,
    OptionalMount, Output, OutputDelivery, OutputError, OutputFlow, OutputHandlers, ReactionChange,
    ReactionContext, RemovalOrigin, RemovalSite, RemovedChild, RemovedOutputError, RootMount, Task,
    TaskAccessError, TaskAccessReason, TaskContext, TaskProgress, TaskSendError, TryCommandError,
};
#[doc(inline)]
pub use rca_core::{
    EncodedStackRoute, Stack, StackAction, StackDecodeError, StackEncoding, StackId,
    StackMutationError, StackState, stack,
};
#[doc(inline)]
pub use rca_core::{Ephemeral, WeakScopedStore, ephemeral};

#[cfg(feature = "test-util")]
#[cfg_attr(docsrs, doc(cfg(feature = "test-util")))]
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
#[cfg_attr(docsrs, doc(cfg(feature = "macros")))]
#[doc(inline)]
pub use rca_macros::{
    __RcaShortActionPaths as ActionPaths, __RcaShortDependencyKey as DependencyKey,
    __RcaShortSnapshot as Snapshot, __RcaShortStatePaths as StatePaths, CaseDescriptors,
    DependencyClient, short_reducer as reducer, short_reducers as reducers,
};

/// Supplies the long facade's generated paths without a second public dependency.
#[cfg(feature = "macros")]
#[doc(hidden)]
pub mod __long_facade_macros {
    pub use rca_core::{DependencyKey, Snapshot};
    pub use rca_macros::{
        __RcaLongActionPaths as ActionPaths, __RcaLongDependencyKey as DependencyKey,
        __RcaLongSnapshot as Snapshot, __RcaLongStatePaths as StatePaths, long_reducer as reducer,
        long_reducers as reducers,
    };
}

#[cfg(feature = "test-util")]
#[cfg_attr(docsrs, doc(cfg(feature = "test-util")))]
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
#[cfg_attr(docsrs, doc(cfg(feature = "tokio")))]
#[doc(inline)]
pub use rca_tokio::{TokioRuntime, TokioRuntimeError};
