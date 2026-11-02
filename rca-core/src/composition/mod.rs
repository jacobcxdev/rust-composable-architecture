//! Provides paths and reducer composition.
//!
//! Paths select borrowed state and actions without cloning the parent domain.
//! [`combine()`] runs reducers in declaration order. [`scope()`] adapts one required
//! child; optional, enum-case, keyed and stack adapters also track child lifetimes.
//! Their returned effects retain those lifetimes when embedded into parent actions.
//! Identified adapters distinguish semantic identity from a value's address.
//! These reducers do not create another actor, mailbox or UI layer.
//!
//! Required child reducers can be composed by hand without macros:
//!
//! ```
//! use rca_core::{ActionPath, Effect, ReduceContext, Reducer, StatePath, Store, combine, scope};
//! # mod support {
//! #     include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/mod.rs"));
//! # }
//! # let host = support::ManualRuntime::new();
//! # let runtime = host.runtime();
//!
//! struct CounterState {
//!     count: usize,
//! }
//!
//! struct AppState {
//!     counter: CounterState,
//!     counter_actions: usize,
//! }
//!
//! enum CounterAction {
//!     Increment,
//! }
//!
//! enum AppAction {
//!     Counter(CounterAction),
//! }
//!
//! struct Counter;
//!
//! impl Reducer for Counter {
//!     type State = CounterState;
//!     type Action = CounterAction;
//!
//!     fn reduce(
//!         &mut self,
//!         state: &mut Self::State,
//!         action: &Self::Action,
//!         _context: &mut ReduceContext<'_>,
//!     ) -> Effect<Self::Action> {
//!         match action {
//!             CounterAction::Increment => state.count += 1,
//!         }
//!         Effect::none()
//!     }
//! }
//!
//! struct App;
//!
//! impl Reducer for App {
//!     type State = AppState;
//!     type Action = AppAction;
//!
//!     fn reduce(
//!         &mut self,
//!         state: &mut Self::State,
//!         action: &Self::Action,
//!         _context: &mut ReduceContext<'_>,
//!     ) -> Effect<Self::Action> {
//!         match action {
//!             AppAction::Counter(_) => state.counter_actions += 1,
//!         }
//!         Effect::none()
//!     }
//! }
//!
//! let counter_state = StatePath::new(
//!     |state: &AppState| &state.counter,
//!     |state: &mut AppState| &mut state.counter,
//! );
//! let counter_action = ActionPath::new(
//!     |action: &AppAction| match action {
//!         AppAction::Counter(action) => Some(action),
//!     },
//!     AppAction::Counter,
//! );
//! let state = AppState {
//!     counter: CounterState { count: 0 },
//!     counter_actions: 0,
//! };
//! assert_eq!(counter_state.get(&state).count, 0);
//! let action = counter_action.embed(CounterAction::Increment);
//! let reducer = combine((scope(counter_state, counter_action, Counter), App));
//! let _: &dyn Reducer<State = AppState, Action = AppAction> = &reducer;
//! let store = Store::new(state, reducer, runtime);
//! # host.start_all();
//! # futures_executor::block_on(async {
//! store.send(action).await.unwrap().finish().await;
//! let state = store.shutdown_into_state().await.unwrap();
//! assert_eq!(state.counter.count, 1);
//! assert_eq!(state.counter_actions, 1);
//! # });
//! # for result in host.join_all() { result.unwrap(); }
//! ```

mod aggregate_case;
mod case_descriptor;
mod combine;
mod dependency;
mod ephemeral;
mod keyed;
pub(crate) use keyed::{CollectionStamp, EntryStamp};
mod lifecycle;
mod missing_child;
mod on_change;
mod path;
mod presentation;
mod scope;
mod shared;
mod stack;

#[doc(inline)]
pub use aggregate_case::{
    AggregateActionPath, AggregateCase, AggregateStatePath, MutableAggregateCase,
};
#[doc(inline)]
pub use case_descriptor::{OwnedCaseMismatch, OwnedCasePath};
#[doc(inline)]
pub use combine::{Combine, combine};
#[doc(inline)]
pub use dependency::{DependencyReducer, with_dependency};
#[doc(inline)]
pub use ephemeral::{Ephemeral, ephemeral};
#[doc(inline)]
pub use keyed::{
    DuplicateId, ForEach, KeyedAction, KeyedActionPayload, KeyedDecodeError, KeyedMergeDecision,
    KeyedMutationError, KeyedPositionError, KeyedReserveError, KeyedState, for_each,
};
#[doc(inline)]
pub use lifecycle::{
    IfCase, IfCaseIdentified, IfSome, IfSomeIdentified, ReducerExt, if_case, if_case_identified,
    if_some, if_some_identified,
};
#[doc(inline)]
pub use missing_child::MissingChildPolicy;
#[doc(inline)]
pub use on_change::{OnChange, on_change};
#[doc(inline)]
pub use path::{
    ActionEmbedding, ActionPath, CaseStatePath, CloneActionPath, ComposedActionEmbedding,
    ComposedActionPath, ComputedPath, KeyedActionPath, KeyedStatePath, KeyedStatePayload,
    LiftedActionEmbedding, LiftedActionPath, OptionalStatePath, OptionalStatePayload, StatePath,
    StoredActionPath, constant_path, err_path, identity_action_path, identity_keyed_action_path,
    identity_keyed_state_path, identity_state_path, none_path, ok_path, some_path, unit_path,
};
#[doc(inline)]
pub use presentation::{Dismiss, Presentation, PresentationAction, presentation};
pub(crate) use presentation::{DismissTarget, report_unavailable_dismiss};
#[doc(inline)]
pub use scope::{Scope, ScopeIdentified, scope, scope_identified};
#[doc(inline)]
pub use shared::{ForEachShared, SharedReducer, SharedScope, for_each_shared, scope_shared};
#[doc(inline)]
pub use stack::{
    EncodedStackRoute, Stack, StackAction, StackDecodeError, StackEncoding, StackId,
    StackMutationError, StackState, stack,
};

#[cfg(all(test, feature = "allocation-measurement"))]
pub(crate) use keyed::{
    cleared_shrink_releases_backing_storage_but_retains_lineage,
    merge_result_capacity_tracks_conflicts_instead_of_all_inputs,
    moves_bound_permutation_allocations, reserved_keyed_collection_operations_do_not_allocate,
    sort_reuses_completed_scratch_storage, unused_empty_collections_do_not_allocate,
    warmed_alternating_store_and_parent_contexts_do_not_allocate,
    warmed_keyed_reconciliation_does_not_allocate,
};

#[cfg(all(test, feature = "allocation-measurement"))]
pub(crate) use scope::scope_none_paths_do_not_allocate_after_warm_up;

#[cfg(all(test, feature = "allocation-measurement"))]
pub(crate) use shared::{
    warmed_shared_empty_effect_matches_ordinary_scope_allocations,
    warmed_shared_keyed_paths_do_not_allocate_or_remount,
};

#[cfg(all(test, feature = "allocation-measurement"))]
pub(crate) use dependency::dependency_overlays_do_not_allocate_during_reduction;

#[cfg(all(test, feature = "allocation-measurement"))]
pub(crate) use lifecycle::{
    case_lifecycle_hot_paths_do_not_allocate_after_warm_up,
    optional_lifecycle_hot_paths_do_not_allocate_after_warm_up,
};

#[cfg(all(test, feature = "allocation-measurement"))]
pub(crate) use stack::pop_does_not_allocate;

#[cfg(all(test, feature = "allocation-measurement"))]
pub(crate) use combine::{
    combine_all_none_widths_do_not_allocate, vector_all_none_reduction_does_not_allocate,
};

#[cfg(all(test, feature = "allocation-measurement"))]
pub(crate) use path::keyed_path_operations_do_not_allocate;
