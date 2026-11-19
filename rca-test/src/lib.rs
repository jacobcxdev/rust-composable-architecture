//! Provides deterministic testing for RCA features.
//!
//! [`TestStore`] drives the production reducer and compares an independent snapshot
//! after each action commits. [`TestStore::send`] and [`TestStore::receive`] assert
//! unchanged state. [`TestStore::send_with`] and [`TestStore::receive_with`] describe
//! expected changes. Effect actions wait for an explicit receive.
//! [`TestStore::assert_state`] adds inspection without replacing the automatic comparison.
//!
//! # Choose a test tool
//!
//! - [`TestStore`] checks reducer behaviour, effects and unfinished work.
//! - [`DeterministicRuntime`] controls task polling and virtual time without sleeping.
//! - [`controlled_future`], [`controlled_stream`] and [`controlled_gate`] control external inputs.
//! - [`snapshots`] captures named values and compares them with caller-owned references.
//! - [`conformance`] checks a runtime adapter against shared behavioural tests.
//!
//! The `local` feature adds owner-thread tools under `local` for non-`Send` values.
//! This crate contains test tools only. Applications can depend on it as a dev-dependency.
//!
//! # Full-state coverage
//!
//! Full-state constructors require `State: Clone + Debug + PartialEq + Send + 'static`.
//! The clone must be independent and equality must include every relevant field.
//! These are test-construction bounds, not production `Store` or `Reducer` bounds.
//!
//! ```
//! use rca_core::{Effect, ReduceContext, Reducer};
//! use rca_test::TestStore;
//!
//! struct Counter;
//!
//! impl Reducer for Counter {
//!     type State = usize;
//!     type Action = ();
//!
//!     fn reduce(&mut self, state: &mut usize, _: &(), _: &mut ReduceContext<'_>) -> Effect<()> {
//!         *state += 1;
//!         Effect::none()
//!     }
//! }
//!
//! futures_executor::block_on(async {
//!     let mut store = TestStore::new(0, Counter);
//!     let _task = store.send_with((), |count| *count = 1).await.unwrap();
//!     store.finish().await.unwrap();
//! });
//! ```
//!
//! # Projected coverage for move-only domains
//!
//! A projection is exhaustive only over its selected snapshot, not omitted fields.
//! It runs synchronously on actor-owned state and returns an independent comparison
//! value. `Snapshot` needs `Clone + Debug + PartialEq + Send + 'static`, not `Sync`.
//! Actions need neither `Clone`, `Debug` nor `PartialEq` in either construction path.
//!
//! ```
//! use rca_core::{Effect, ReduceContext, Reducer};
//! use rca_test::TestStore;
//!
//! struct State {
//!     count: usize,
//! }
//!
//! struct Increment;
//! struct Counter;
//!
//! #[derive(Clone, Debug, PartialEq)]
//! struct Snapshot {
//!     count: usize,
//! }
//!
//! fn snapshot(state: &State) -> Snapshot {
//!     Snapshot { count: state.count }
//! }
//!
//! impl Reducer for Counter {
//!     type State = State;
//!     type Action = Increment;
//!
//!     fn reduce(
//!         &mut self,
//!         state: &mut State,
//!         _: &Increment,
//!         _: &mut ReduceContext<'_>,
//!     ) -> Effect<Increment> {
//!         state.count += 1;
//!         Effect::none()
//!     }
//! }
//!
//! futures_executor::block_on(async {
//!     let mut store = TestStore::projected(State { count: 0 }, Counter, snapshot);
//!     let _task = store.send_with(Increment, |state| state.count = 1).await.unwrap();
//!     store.finish().await.unwrap();
//! });
//! ```
//!
//! A clone that still shares mutable storage is not an independent historical snapshot.
//! Neither full-state nor projected coverage tracks shared or persistent mutations automatically.
//! The domain author controls which fields participate in equality and projection.
//!
//! # Interrupted assertions and teardown
//!
//! An unpolled operation admits no work. After admission or release, the assertion
//! remains incomplete until comparison succeeds. This also applies if its future
//! is dropped, the runtime cannot progress, or the comparison panics.
//! Later steps and `finish` reject the incomplete assertion.
//! There is no recovery or automatic baseline reset.
//!
//! A rejected root-action receive matcher leaves the exact action available for another receive.
//! Output, mounted-action and command matchers also select an exact source.
//! Their failures leave an incomplete assertion that a later match cannot repair.
//! A later match permits cleanup only.
//!
//! Call [`TestStore::finish`] to report unfinished work at an explicit point in the test.
//! It settles ready work within the poll budget, then shuts down on success.
//! It does not advance virtual time or wait for external I/O.
//!
//! Normal `Drop` checks incomplete assertions and unfinished actions, effects, timers,
//! cancellation registrations and observations. It cleans up before it panics on failure.
//! During unwinding, it only aborts work and preserves the original panic.
//! It does not poll callbacks or add a second assertion panic.
//!
//! [`Exhaustivity::NonExhaustive`] only permits explicit [`TestStore::skip_received`].
//! This operation cancels the command and checks that state remains unchanged.
//! It disables neither state comparison nor unfinished-work checks.
//! Skipped actions do not run implicitly.
//!
//! For a focused scenario, [`TestStore::receive_until`] executes earlier transactions
//! and strictly checks the selected action. [`TestStore::receive_omitting`] executes
//! one transaction without checking its changes. Both adopt the selected snapshot
//! and declared captures, rather than cancelling production work. Choose
//! [`OmissionReporting::Warn`] to retain store-local warnings for
//! [`TestStore::take_omissions`], or [`OmissionReporting::Silent`] to omit them.
//! These operations never advance time, wait for external progress, forgive an
//! earlier failed assertion, or relax [`TestStore::finish`].

mod case_assertions;
pub mod conformance;
mod controls;
mod error;
mod issue_expectations;
#[cfg(feature = "local")]
pub mod local;
mod required_change;
mod runtime;
mod semantic_checks;
pub mod snapshots;
mod store;
mod task;

#[doc(inline)]
pub use case_assertions::{CaseAssertionFailure, check_case, check_case_with};

#[doc(inline)]
pub use error::{
    CancellationIdentityDiagnostic, DriveError, FinishDiagnostics, PendingSleep, TestStoreError,
    TestTaskError,
};
#[doc(inline)]
pub use issue_expectations::{
    IssueExpectation, IssueExpectationError, IssueExpectationMode, expect_issues,
    expect_issues_async,
};
#[doc(inline)]
pub use required_change::requiring_change;
#[doc(inline)]
pub use runtime::{DEFAULT_STEP_BUDGET, DeterministicRuntime, DriveReport};
#[doc(inline)]
pub use semantic_checks::{
    ChangeCheck, ComparisonFailure, check_change, check_change_async, check_equal,
    check_expected_changes,
};
#[doc(inline)]
pub use store::{
    CaptureExpectations, CaptureKey, Exhaustivity, OmissionReporting, TestStore, TestStoreBuilder,
};
#[doc(inline)]
pub use task::TestActionTask;

#[doc(inline)]
pub use controls::{
    Completion, ControlCancelled, ControlledFuture, ControlledGate, ControlledStream, GateControl,
    GateDropped, ReceiverDropped, StreamControl, TryControlledSendError, controlled_future,
    controlled_gate, controlled_stream,
};

#[cfg(all(test, target_os = "macos", target_arch = "aarch64", not(loom)))]
#[path = "../tests/support/cold_listener_alloc.rs"]
mod cold_listener_alloc;
