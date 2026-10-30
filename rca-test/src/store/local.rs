//! Owner-local nominal API for the common test-store orchestration.

use std::fmt;
use std::panic::Location;
use std::rc::Rc as CaptureOwner;
use std::sync::Mutex;
use std::time::Duration;

use rca_core::local as core_api;
#[cfg(test)]
use rca_core::local::Reducer;
use rca_core::local::{Clock, Store, StoreBuilder};
use rca_core::testing::IssueRecorder;
use rca_core::testing::local::TransactionGate;
// Dependency keys and issue services deliberately retain portable semantics.
use super::assertions::{self, Coverage};
use super::{Exhaustivity, OmissionReporting, has_unfinished_work};
use crate::local::{
    CaptureExpectations, CaptureKey, DeterministicRuntime, TestActionTask, TestStoreError,
};
use crate::{DEFAULT_STEP_BUDGET, DriveReport, FinishDiagnostics};
use rca_core::{DependencyKey, DependencyValues, Issue, MonotonicInstant};

/// Keeps snapshot callbacks and values on the owning thread without `Send` bounds.
type Assertions<S, T> = super::assertions::Assertions<S, T, super::obligations::Local>;

/// Uses owner-local capture values and causes with the common obligation policy.
type DeclaredCaptures<S> = super::obligations::DeclaredCaptures<S, super::obligations::Local>;

/// Stages owner-local declaration values for one focused transaction.
type ExpectedCaptures<S> = Vec<super::obligations::ExpectedCapture<S, super::obligations::Local>>;

super::api::test_store_api!(
    core_api,
    [],
    [],
    r#"
```
use std::{cell::OnceCell, rc::Rc};
use rca_core::{DependencyKey, ReduceContext};
use rca_core::local::{Clock, Effect, Reducer};
use rca_test::local::TestStore;
// Dependency values remain portable; the local factory may own Rc captures.
struct Configured;
impl DependencyKey for Configured {
    type Value = ();
    fn live_value() { panic!("configure the local client clock explicitly") }
}
struct Feature;
impl Reducer for Feature {
    type State = ();
    type Action = ();
    fn reduce(&mut self, _: &mut (), _: &(), _: &mut ReduceContext<'_>) -> Effect<()> {
        Effect::none()
    }
}
let clock = Rc::new(OnceCell::<Clock>::new());
let configured = Rc::clone(&clock);
let store = TestStore::builder((), Feature)
    .with_clock_dependency::<Configured>(move |clock| { configured.set(clock).unwrap(); })
    .build();
assert!(clock.get().is_some());
```
"#,
    r#"
```
use std::rc::Rc;
use rca_core::ReduceContext;
use rca_core::local::{Effect, Reducer};
use rca_test::local::TestStore;
# struct Feature;
# impl Reducer for Feature {
# type State = (u32, Rc<str>);
# type Action = ();
# fn reduce(&mut self, _: &mut Self::State, _: &(), _: &mut ReduceContext<'_>) -> Effect<()> { Effect::none() }
# }
let label = Rc::new("count");
let store = TestStore::builder((0, Rc::<str>::from("private")), Feature)
    .with_snapshot_renderer(move |snapshot| format!("{}: {}\nsecret: [redacted]", label, snapshot.0))
    .build();
# drop(store);
```
"#
);

#[cfg(test)]
mod tests;
