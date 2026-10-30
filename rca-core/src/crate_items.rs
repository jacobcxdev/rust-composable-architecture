#[cfg(all(test, feature = "allocation-measurement"))]
mod allocation_test;

mod composition;
mod dependency;
mod effect;
mod error;
mod feature;
mod issue;
mod observation;
mod reducer;
mod runtime;
mod sharing;
mod snapshot;
mod store;
#[cfg(feature = "test-util")]
#[doc(hidden)]
pub mod testing;

#[doc(inline)]
pub use composition::{
    ActionEmbedding, ActionPath, AggregateActionPath, AggregateCase, AggregateStatePath,
    CaseStatePath, CloneActionPath, Combine, ComposedActionEmbedding, ComposedActionPath,
    ComputedPath, DependencyReducer, Dismiss, DuplicateId, ForEach, ForEachShared, IfCase,
    IfCaseIdentified, IfSome, IfSomeIdentified, KeyedAction, KeyedActionPath, KeyedActionPayload,
    KeyedDecodeError, KeyedMergeDecision, KeyedMutationError, KeyedPositionError,
    KeyedReserveError, KeyedState, KeyedStatePath, KeyedStatePayload, LiftedActionEmbedding,
    LiftedActionPath, MissingChildPolicy, MutableAggregateCase, OnChange, OptionalStatePath,
    OptionalStatePayload, OwnedCaseMismatch, OwnedCasePath, Presentation, PresentationAction,
    ReducerExt, Scope, ScopeIdentified, SharedReducer, SharedScope, StatePath, StoredActionPath,
    combine, constant_path, err_path, for_each, for_each_shared, identity_action_path,
    identity_keyed_action_path, identity_keyed_state_path, identity_state_path, if_case,
    if_case_identified, if_some, if_some_identified, none_path, ok_path, on_change, presentation,
    scope, scope_identified, scope_shared, some_path, unit_path, with_dependency,
};
#[doc(inline)]
pub use composition::{
    EncodedStackRoute, Stack, StackAction, StackDecodeError, StackEncoding, StackId,
    StackMutationError, StackState, stack,
};
#[doc(inline)]
pub use composition::{Ephemeral, ephemeral};
#[doc(inline)]
pub use dependency::{
    ApplicationIds, CivilData, DependencyDefaultPolicy, DependencyDefaults, DependencyKey,
    DependencyMockMismatch, DependencyPreparationError, DependencyRegistrationError,
    DependencyValues, RandomBytes, ReduceContext, WallTime, try_dependency_mock,
};
#[doc(inline)]
pub use effect::{
    CancellingOperationWait, Effect, EffectError, EffectSender, OperationPolicy, OperationStatus,
    OperationWait, SelfWait,
};
#[doc(inline)]
pub use error::{
    EffectSendError, ScopedStoreEnded, ShutdownError, StoreBuildError, StoreStopped,
    TryScopedSendError, TrySendError,
};
#[doc(inline)]
pub use feature::{Feature, FeatureContext, InitialPolicy, ReactionChange, ReactionContext};
#[cfg(feature = "test-util")]
#[doc(inline)]
pub use issue::{EndpointStub, IssueCollector};
#[doc(inline)]
pub use issue::{
    Issue, IssueCallback, IssueContext, IssueDelivery, IssueDeliveryFailure, IssueReportError,
    IssueReporter, IssueReporters, IssueSeverity, OperationReport,
};
#[doc(inline)]
pub use observation::{
    ExternalObservation, Observation, ObservationError, ObservationProducer, PredicateWait,
    PredicateWaitOutcome,
};
#[doc(inline)]
pub use reducer::{Reduce, Reducer};
#[doc(inline)]
pub use runtime::{
    Clock, ClockCapabilities, ClockDriver, ClockTimer, InvalidTimerInterval, MonotonicInstant,
    Runtime, RuntimeDriver, RuntimeFuture, UnsupportedClockTolerance,
};
#[cfg(feature = "test-util")]
#[doc(inline)]
pub use runtime::{ForbiddenClock, ForbiddenClockUsed, ImmediateClock};
#[doc(inline)]
pub use sharing::cache::{
    SharedKey, SharedKeyCache, SharedKeyDefault, SharedKeyError, SharedKeyIdentity,
    SharedLoadContext, SharedPublication, SharedReaderKey, SharedSubscribeContext,
    SharedSubscriber,
};
#[doc(inline)]
pub use sharing::{
    Shared, SharedAccessError, SharedMutationError, SharedProjection, SharedReadProjection,
    SharedReader,
};
#[doc(inline)]
pub use snapshot::Snapshot;
#[doc(inline)]
pub use store::task_context::{
    Committed, Task, TaskAccessError, TaskAccessReason, TaskContext, TaskProgress, TaskSendError,
};
#[doc(inline)]
pub use store::{
    ActionReceipt, ActionReceiptFuture, ActionTask, CommandPort, CommandReceipt, CommandSite,
    DEFAULT_MAILBOX_CAPACITY, EntryMount, Fold, FoldKey, FoldView, InputGate, MountChildren,
    MountIdentity, MountSite, Operation, OptionalMount, Output, OutputDelivery, OutputError,
    OutputFlow, OutputHandlers, RemovalOrigin, RemovalSite, RemovedChild, RemovedOutputError,
    RootMount, ScopedStore, Store, StoreBuilder, TryCommandError, WeakScopedStore,
};
