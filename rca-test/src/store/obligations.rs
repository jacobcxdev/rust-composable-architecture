//! Owns explicitly declared, independently captured boundary values.
//!
//! Captures cover selected values only. Cloning a mutable alias does not freeze
//! history. Writes reverted between boundaries and unsynchronised external
//! writers are outside this accounting contract. No process registry, runtime
//! lifetime authority or implicit storage reset is supplied here.

use std::any::{Any, TypeId, type_name};
use std::fmt;
use std::marker::PhantomData;
use std::panic::{AssertUnwindSafe, Location};
#[cfg(feature = "local")]
use std::rc::Rc;
use std::sync::Arc;

use parking_lot::Mutex;

use crate::TestStoreError;
#[cfg(feature = "local")]
use crate::error::LocalCause;
use crate::error::{CaptureCause, CaptureProblem, SharedCause, TestStoreFailure};

/// Identifies one store-local declaration and its independent captured value.
///
/// Use separate marker types for independent obligations, even when their value
/// and error types coincide. `Value` and its clones must contain independent
/// history. Captures do not discover unselected fields or prove deep copying.
///
/// Accounting covers declared values at selected boundaries only, not a journal
/// of every mutation. A write reverted between captures is not detected. Captures
/// do not provide an atomic snapshot of arbitrary unsynchronised external writers;
/// callers must coordinate those writers around the selected boundary.
pub trait CaptureKey: 'static {
    /// Owned historical value compared at selected boundaries.
    type Value: Clone + PartialEq + Send + 'static;
    /// Original capture cause, transferred without requiring Debug or Sync.
    type Error: Send + 'static;
}

#[cfg(feature = "local")]
/// Exposes the same expectation policy with owner-local values and callbacks.
pub(crate) mod local {
    use super::{CaptureFamily, ExpectationEntries, Local, update_expectation};
    use crate::error::LocalCause;
    use crate::error::local::TestStoreError;
    use std::{fmt, panic::Location};

    /// Identifies one owner-local declaration and its independent captured history.
    ///
    /// Immutable Rc history is supported. Cloning a mutable alias does not freeze
    /// history; callers must coordinate external writers around each boundary.
    pub trait CaptureKey: 'static {
        /// Owned historical value compared at selected boundaries.
        type Value: Clone + PartialEq + 'static;
        /// Original capture cause retained without formatting or transfer bounds.
        type Error: 'static;
    }

    /// Borrows one owner-local boundary's independent typed expectations.
    pub struct CaptureExpectations<'a> {
        /// Temporarily exposes staged values without owning or admitting them.
        pub(super) entries: &'a mut dyn ExpectationEntries,
    }

    impl fmt::Debug for CaptureExpectations<'_> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("CaptureExpectations")
                .field("declarations", &self.entries.len())
                .finish_non_exhaustive()
        }
    }

    impl CaptureExpectations<'_> {
        /// Supplies one declared value; omitted declarations must remain unchanged.
        ///
        /// # Errors
        /// Returns a typed failure for an undeclared or already supplied key.
        #[track_caller]
        pub fn set<K: CaptureKey>(&mut self, expected: K::Value) -> Result<(), TestStoreError> {
            self.update::<K>(|value| *value = expected)
        }

        /// Edits one independent expectation after validating its declared key.
        ///
        /// Runs once, including for a no-op. Clones must preserve independent
        /// history: an immutable Rc value can use `Rc::make_mut`, but cloning a
        /// mutable alias does not freeze history. Side effects are not rolled back.
        ///
        /// # Errors
        /// Unknown and already supplied keys fail without invoking `update`.
        ///
        /// # Panics
        /// Propagates callback panics without consuming the expectation slot.
        #[track_caller]
        pub fn update<K: CaptureKey>(
            &mut self,
            update: impl FnOnce(&mut K::Value),
        ) -> Result<(), TestStoreError> {
            update_expectation::<K, K::Value, LocalCause>(self.entries, update, Location::caller())
                .map_err(Local::error)
        }
    }
}

/// Includes the declaration key in the downcast target, not just its value type.
struct KeyedValue<K, Value> {
    /// Independent staged history edited by the expectation callback.
    value: Value,
    /// Carries type identity without owning a key or inheriting its auto-traits.
    key: PhantomData<fn() -> K>,
}

/// Selects erased ownership and error envelopes for Shared and Local hosts.
pub(super) trait CaptureFamily: Sized + 'static {
    /// Retains the original capture error for typed recovery.
    type Cause: CaptureCause;
    /// Public error facade for this host family.
    type Error;
    /// Erased verified history, with Send only for transferable hosts.
    type Declared<State>: DeclaredValue<State, Self> + ?Sized;
    /// Erased expected history prepared before production admission.
    type Expected<State>: ExpectedValue<State, Self> + ?Sized;
    /// Erased expected/actual pair awaiting complete batch comparison.
    type Captured<State>: CapturedValue<State, Self> + ?Sized;
    /// Wraps a failure without inspecting or formatting its original cause.
    fn error(failure: TestStoreFailure<Self::Cause>) -> Self::Error;
}

/// Uses Send-erased values and atomically shared capture callbacks.
pub(super) struct Shared;
impl CaptureFamily for Shared {
    type Cause = SharedCause;
    type Error = TestStoreError;
    type Declared<State> = dyn DeclaredValue<State, Self> + Send;
    type Expected<State> = dyn ExpectedValue<State, Self> + Send;
    type Captured<State> = dyn CapturedValue<State, Self> + Send;
    fn error(failure: TestStoreFailure<Self::Cause>) -> Self::Error {
        TestStoreError { failure }
    }
}

#[cfg(feature = "local")]
/// Keeps values and capture callbacks on their owner without transfer bounds.
pub(super) struct Local;
#[cfg(feature = "local")]
impl CaptureFamily for Local {
    type Cause = LocalCause;
    type Error = crate::error::local::TestStoreError;
    type Declared<State> = dyn DeclaredValue<State, Self>;
    type Expected<State> = dyn ExpectedValue<State, Self>;
    type Captured<State> = dyn CapturedValue<State, Self>;
    fn error(failure: TestStoreFailure<Self::Cause>) -> Self::Error {
        crate::error::local::TestStoreError { failure }
    }
}

/// Boxes concrete stages using only the ownership bounds required by each host.
///
/// Staging, comparison and debt policy stay in the common implementation.
trait CaptureStorage<State, Value, Error>: CaptureFamily {
    /// Shared ownership of a repeatedly invoked capture callback.
    type Callback: Clone;
    /// Invokes the selected capture once on actor-owned state.
    fn call(capture: &Self::Callback, state: &State) -> Result<Value, Error>;
    /// Erases verified history while preserving its key and concrete value type.
    fn declaration<K: 'static>(
        value: TypedDeclaration<State, K, Value, Error, Self>,
    ) -> Box<Self::Declared<State>>;
    /// Erases a staged expectation without cloning its history again.
    fn expected<K: 'static>(
        value: TypedExpected<State, K, Value, Error, Self>,
    ) -> Box<Self::Expected<State>>;
    /// Erases an expected/actual pair before any batch comparisons run.
    fn captured<K: 'static>(
        value: TypedCaptured<State, K, Value, Error, Self>,
    ) -> Box<Self::Captured<State>>;
    /// Retains the original error with its declaration's downcast identity.
    fn cause<K: 'static>(cause: Error) -> Self::Cause;
}

impl<State: 'static, Value: Clone + PartialEq + Send + 'static, Error: Send + 'static>
    CaptureStorage<State, Value, Error> for Shared
{
    type Callback = Arc<dyn Fn(&State) -> Result<Value, Error> + Send + Sync>;
    fn call(capture: &Self::Callback, state: &State) -> Result<Value, Error> {
        capture(state)
    }
    fn declaration<K: 'static>(
        value: TypedDeclaration<State, K, Value, Error, Self>,
    ) -> Box<Self::Declared<State>> {
        Box::new(value)
    }
    fn expected<K: 'static>(
        value: TypedExpected<State, K, Value, Error, Self>,
    ) -> Box<Self::Expected<State>> {
        Box::new(value)
    }
    fn captured<K: 'static>(
        value: TypedCaptured<State, K, Value, Error, Self>,
    ) -> Box<Self::Captured<State>> {
        Box::new(value)
    }
    fn cause<K: 'static>(cause: Error) -> Self::Cause {
        SharedCause::new::<K, _>(cause)
    }
}

#[cfg(feature = "local")]
impl<State: 'static, Value: Clone + PartialEq + 'static, Error: 'static>
    CaptureStorage<State, Value, Error> for Local
{
    type Callback = Rc<dyn Fn(&State) -> Result<Value, Error>>;
    fn call(capture: &Self::Callback, state: &State) -> Result<Value, Error> {
        capture(state)
    }
    fn declaration<K: 'static>(
        value: TypedDeclaration<State, K, Value, Error, Self>,
    ) -> Box<Self::Declared<State>> {
        Box::new(value)
    }
    fn expected<K: 'static>(
        value: TypedExpected<State, K, Value, Error, Self>,
    ) -> Box<Self::Expected<State>> {
        Box::new(value)
    }
    fn captured<K: 'static>(
        value: TypedCaptured<State, K, Value, Error, Self>,
    ) -> Box<Self::Captured<State>> {
        Box::new(value)
    }
    fn cause<K: 'static>(cause: Error) -> Self::Cause {
        LocalCause::new::<K, _>(cause)
    }
}

/// Owns one verified historical value between assertion boundaries.
pub(super) trait DeclaredValue<State, F: CaptureFamily> {
    /// Clones independent history exactly once for the next expected value.
    fn prepare(&mut self) -> Box<F::Expected<State>>;
    /// Drops the previous expectation before the admitted debt marker is cleared.
    fn clear_retired(&mut self);
}

/// Owns one editable expectation before and during its actor-side capture.
pub(super) trait ExpectedValue<State, F: CaptureFamily> {
    /// Borrows the key-tagged value for type-safe expectation edits.
    fn value_mut(&mut self) -> &mut dyn Any;
    /// Captures once, preserving the original cause and operation on failure.
    fn capture(
        self: Box<Self>,
        state: &State,
        origin: &'static Location<'static>,
        operation: &'static str,
    ) -> Result<Box<F::Captured<State>>, TestStoreFailure<F::Cause>>;
}

/// Owns both histories until the whole boundary has compared successfully.
pub(super) trait CapturedValue<State, F: CaptureFamily> {
    /// Runs the value's authoritative equality once when reached in batch order.
    fn equal(&self) -> bool;
    /// Installs actual history and retains the expected value for later cleanup.
    fn into_declaration(self: Box<Self>) -> Box<F::Declared<State>>;
}

/// Records type identity without owning the parameters or imposing auto-traits.
type CaptureMarker<State, K, Error> = PhantomData<fn() -> (State, K, Error)>;

/// Retains verified history and any expectation awaiting post-installation Drop.
struct TypedDeclaration<State, K, Value, Error, F: CaptureStorage<State, Value, Error>> {
    /// Initial capture, or actual value installed after the complete batch matched.
    baseline: Value,
    /// Reusable actor-side selector, owned according to the host family.
    capture: F::Callback,
    /// Matched expected value whose destructor must run before clearing debt.
    retired: Option<Value>,
    /// Associates the stored histories with their state, key and error types.
    marker: CaptureMarker<State, K, Error>,
}

/// Transfers an independent expected value into one actor-side capture.
struct TypedExpected<State, K, Value, Error, F: CaptureStorage<State, Value, Error>> {
    /// Key-tagged historical clone, possibly edited during public preflight.
    expected: KeyedValue<K, Value>,
    /// Callback shared with the retained declaration, not the production state.
    capture: F::Callback,
    /// Tracks callback types without owning state or an error value.
    marker: PhantomData<fn() -> (State, Error)>,
}

/// Keeps a captured pair separate from verified history until all keys match.
struct TypedCaptured<State, K, Value, Error, F: CaptureStorage<State, Value, Error>> {
    /// Independently prepared history required at this boundary.
    expected: Value,
    /// Fresh actor-side capture to compare and eventually install.
    actual: Value,
    /// Callback retained for the next boundary without re-registration.
    capture: F::Callback,
    /// Preserves the declaration's concrete type association through erasure.
    marker: CaptureMarker<State, K, Error>,
}

impl<State, K: 'static, Value: Clone, Error, F: CaptureStorage<State, Value, Error>>
    DeclaredValue<State, F> for TypedDeclaration<State, K, Value, Error, F>
{
    fn prepare(&mut self) -> Box<F::Expected<State>> {
        F::expected(TypedExpected::<State, K, Value, Error, F> {
            expected: KeyedValue {
                value: self.baseline.clone(),
                key: PhantomData,
            },
            capture: self.capture.clone(),
            marker: PhantomData,
        })
    }

    fn clear_retired(&mut self) {
        drop(self.retired.take());
    }
}

impl<State, K: 'static, Value: 'static, Error, F: CaptureStorage<State, Value, Error>>
    ExpectedValue<State, F> for TypedExpected<State, K, Value, Error, F>
{
    fn value_mut(&mut self) -> &mut dyn Any {
        &mut self.expected
    }

    fn capture(
        self: Box<Self>,
        state: &State,
        origin: &'static Location<'static>,
        operation: &'static str,
    ) -> Result<Box<F::Captured<State>>, TestStoreFailure<F::Cause>> {
        let actual = F::call(&self.capture, state).map_err(|cause| {
            TestStoreFailure::capture_failed::<K>(F::cause::<K>(cause), origin, operation)
        });
        let actual = match actual {
            Ok(actual) => actual,
            Err(error) => {
                // Keep the original cause local while owned expected history
                // and its callback share run their destructors.
                drop(self);
                return Err(error);
            }
        };
        Ok(F::captured(TypedCaptured::<State, K, Value, Error, F> {
            expected: self.expected.value,
            actual,
            capture: self.capture,
            marker: PhantomData,
        }))
    }
}

impl<State, K: 'static, Value: PartialEq, Error, F: CaptureStorage<State, Value, Error>>
    CapturedValue<State, F> for TypedCaptured<State, K, Value, Error, F>
{
    fn equal(&self) -> bool {
        self.expected == self.actual
    }

    fn into_declaration(self: Box<Self>) -> Box<F::Declared<State>> {
        F::declaration(TypedDeclaration::<State, K, Value, Error, F> {
            baseline: self.actual,
            capture: self.capture,
            retired: Some(self.expected),
            marker: PhantomData,
        })
    }
}

/// Stores one key's verified history in declaration order.
pub(super) struct Declaration<State, F: CaptureFamily = Shared> {
    /// Distinguishes declarations even when their value types match.
    key: TypeId,
    /// Static diagnostic name; formatting never inspects captured user values.
    name: &'static str,
    /// Supplies Sync for Send-only history without locking around user code.
    ///
    /// Exclusive access uses `get_mut`, including Clone, equality and destruction.
    value: AssertUnwindSafe<Mutex<Box<F::Declared<State>>>>,
}

/// Owns verified declarations and any incomplete boundary's persistent marker.
pub(super) struct DeclaredCaptures<State, F: CaptureFamily = Shared> {
    /// Verified histories in their original declaration order.
    entries: Vec<Declaration<State, F>>,
    /// Survives movement of capture payloads into actor commands and their failure.
    pending: AssertUnwindSafe<Mutex<Option<PendingCapture<State, F>>>>,
}

/// Retains failure provenance after an actor command takes its expected values.
struct PendingCapture<State, F: CaptureFamily> {
    /// Staged payload, emptied when the batch moves into an actor command.
    expected: Vec<ExpectedCapture<State, F>>,
    /// Original public operation location used by later debt errors.
    origin: &'static Location<'static>,
    /// Operation that admitted this boundary, not the later attempted operation.
    operation: &'static str,
}

/// Owns the ordered expectations transferred to a single actor-side inspection.
pub(super) struct CaptureBatch<State, F: CaptureFamily = Shared> {
    /// Values captured in order, stopping at the first original capture failure.
    expected: Vec<ExpectedCapture<State, F>>,
}

/// Holds a complete capture batch without partially installing any new baseline.
pub(super) struct CapturedValues<State, F: CaptureFamily = Shared> {
    /// Expected/actual pairs in declaration order for deterministic comparison.
    entries: Vec<CapturedEntry<State, F>>,
    /// Public caller location attached to a mismatch in any entry.
    origin: &'static Location<'static>,
    /// Operation name attached to a mismatch in any entry.
    operation: &'static str,
}

/// Associates a successfully captured pair with its declaration's identity.
struct CapturedEntry<State, F: CaptureFamily> {
    /// Key restored when the matched actual value becomes verified history.
    key: TypeId,
    /// Static diagnostic name used without formatting private values.
    name: &'static str,
    /// Concrete expected/actual pair behind the family-specific ownership bounds.
    value: Box<F::Captured<State>>,
}

impl<State, F: CaptureFamily> CaptureBatch<State, F> {
    /// Captures all entries before equality, retaining only the first failure.
    pub(super) fn capture(
        self,
        state: &State,
        origin: &'static Location<'static>,
        operation: &'static str,
    ) -> Result<CapturedValues<State, F>, F::Error> {
        let mut failure = None;
        let entries = self
            .expected
            .into_iter()
            .map_while(
                |entry| match entry.value.capture(state, origin, operation) {
                    Ok(value) => Some(CapturedEntry {
                        key: entry.key,
                        name: entry.name,
                        value,
                    }),
                    Err(error) => {
                        failure = Some(F::error(error));
                        None
                    }
                },
            )
            .collect::<Vec<_>>();
        if let Some(error) = failure {
            // Collection has already disposed of unvisited expectations. Keep
            // the first cause local while earlier captured values also drop.
            drop(entries);
            return Err(error);
        }
        Ok(CapturedValues {
            entries,
            origin,
            operation,
        })
    }
}

impl<State, F: CaptureFamily> Default for DeclaredCaptures<State, F> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            pending: AssertUnwindSafe(Mutex::new(None)),
        }
    }
}

/// Tracks one staged value and whether the caller already edited its expectation.
pub(super) struct ExpectedCapture<State, F: CaptureFamily = Shared> {
    /// Declaration identity preserved when the actor produces a captured pair.
    key: TypeId,
    /// Diagnostic name carried through the boundary without allocating a string.
    name: &'static str,
    /// Key-tagged editable history behind the host's transfer bounds.
    value: Box<F::Expected<State>>,
    /// Becomes true only after the caller's first edit returns successfully.
    supplied: bool,
}

/// Borrows one boundary's typed expectations without exposing a persistent queue.
pub struct CaptureExpectations<'a> {
    /// Borrows staged entries without exposing a persistent mutation capability.
    entries: &'a mut (dyn ExpectationEntries + Send),
}

/// Borrows erased expectations without exposing state or host-family parameters.
trait ExpectationEntries {
    /// Returns the fixed number of declarations available to this edit callback.
    fn len(&self) -> usize;
    /// Borrows one key-tagged value and its successful-edit marker by valid index.
    fn entry_mut(&mut self, index: usize) -> (&mut dyn Any, &mut bool);
}

impl<State, F: CaptureFamily> ExpectationEntries for Vec<ExpectedCapture<State, F>> {
    fn len(&self) -> usize {
        Vec::len(self)
    }

    fn entry_mut(&mut self, index: usize) -> (&mut dyn Any, &mut bool) {
        let entry = &mut self[index];
        (entry.value.value_mut(), &mut entry.supplied)
    }
}

impl fmt::Debug for CaptureExpectations<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CaptureExpectations")
            .field("declarations", &self.entries.len())
            .finish_non_exhaustive()
    }
}

impl CaptureExpectations<'_> {
    /// Supplies one declared value; omitted declarations must remain unchanged.
    ///
    /// # Errors
    /// Returns a typed failure for an undeclared or already supplied key.
    #[track_caller]
    pub fn set<K: CaptureKey>(&mut self, expected: K::Value) -> Result<(), TestStoreError> {
        self.update::<K>(|value| *value = expected)
    }

    /// Edits one independent expectation; omitted declarations must remain unchanged.
    ///
    /// Runs `update` once after validating the key. Even a no-op consumes its slot.
    /// The edit affects only this key's cloned historical value, not production state
    /// or another key with the same value type. Clones must own independent history
    /// and equality must lawfully compare the selected value. Captures account for
    /// selected boundaries only; they do not intercept arbitrary shared aliases.
    /// Use [`crate::requiring_change`] to assert an expected delta before admission.
    ///
    /// # Errors
    /// Returns a typed failure for an undeclared or already supplied key without
    /// invoking `update`.
    ///
    /// # Panics
    /// Propagates callback panics. Send admits nothing; receive retains its original
    /// gated action after successful preflight. Caller side effects are not rolled back.
    #[track_caller]
    pub fn update<K: CaptureKey>(
        &mut self,
        update: impl FnOnce(&mut K::Value),
    ) -> Result<(), TestStoreError> {
        update_expectation::<K, K::Value, SharedCause>(self.entries, update, Location::caller())
            .map_err(Shared::error)
    }
}

/// Validates key and duplicate use before invoking an edit exactly once.
///
/// The key is part of the downcast target. Equal value types cannot alias another
/// declaration; a panicking callback leaves the edit marker available for retry.
fn update_expectation<K: 'static, Value: 'static, Cause>(
    entries: &mut dyn ExpectationEntries,
    update: impl FnOnce(&mut Value),
    origin: &'static Location<'static>,
) -> Result<(), TestStoreFailure<Cause>> {
    for index in 0..entries.len() {
        let (value, supplied) = entries.entry_mut(index);
        if let Some(value) = value.downcast_mut::<KeyedValue<K, Value>>() {
            if *supplied {
                let error = TestStoreFailure::capture_problem::<K>(
                    CaptureProblem::DuplicateExpectation,
                    origin,
                );
                drop(update);
                return Err(error);
            }
            update(&mut value.value);
            *supplied = true;
            return Ok(());
        }
    }
    let error = TestStoreFailure::capture_problem::<K>(CaptureProblem::UnknownExpectation, origin);
    drop(update);
    Err(error)
}

impl<State, F: CaptureFamily> DeclaredCaptures<State, F> {
    /// Tests whether actor-side capture work can be skipped entirely.
    pub(super) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Reports the original admitted boundary even after its payload was moved.
    pub(super) fn incomplete_error(&mut self) -> Option<F::Error> {
        self.pending.get_mut().as_ref().map(|pending| {
            F::error(TestStoreFailure::incomplete_assertion(
                pending.operation,
                pending.origin,
            ))
        })
    }

    /// Moves expected values into a command without clearing the debt marker.
    pub(super) fn batch(&mut self) -> CaptureBatch<State, F> {
        CaptureBatch {
            // Moving the payload never removes the admitted marker. An empty
            // declaration set legitimately has no pending boundary.
            expected: self
                .pending
                .get_mut()
                .as_mut()
                .map(|pending| std::mem::take(&mut pending.expected))
                .unwrap_or_default(),
        }
    }

    /// Records a boundary after public preflight has ruled out earlier debt.
    pub(super) fn begin(
        &mut self,
        expected: Vec<ExpectedCapture<State, F>>,
        origin: &'static Location<'static>,
        operation: &'static str,
    ) {
        if self.is_empty() {
            return;
        }
        // Public preflight has rejected any existing debt before admission.
        let pending = self.pending.get_mut();
        *pending = Some(PendingCapture {
            expected,
            origin,
            operation,
        });
    }

    /// Compares in declaration order without changing any verified history.
    pub(super) fn compare(actual: &CapturedValues<State, F>) -> Result<(), F::Error> {
        for entry in &actual.entries {
            if !entry.value.equal() {
                return Err(F::error(TestStoreFailure::capture_mismatch(
                    entry.name,
                    actual.origin,
                    actual.operation,
                )));
            }
        }
        Ok(())
    }

    /// Replaces the matched batch before dropping old history, with debt retained.
    pub(super) fn install(&mut self, actual: CapturedValues<State, F>) {
        let entries = actual
            .entries
            .into_iter()
            .map(|entry| Declaration {
                key: entry.key,
                name: entry.name,
                value: AssertUnwindSafe(Mutex::new(entry.value.into_declaration())),
            })
            .collect::<Vec<_>>();
        let old = std::mem::replace(&mut self.entries, entries);
        // User destructors run unlocked, with boundary debt still installed.
        drop(old);
    }

    /// Clears debt only after every retired expected value has been destroyed.
    pub(super) fn complete(&mut self) {
        let pending = self.pending.get_mut();
        for entry in &mut self.entries {
            // A panicking value destructor must leave the admitted marker intact.
            entry.value.get_mut().clear_retired();
        }
        pending.take();
    }
}

impl<State: 'static, F: CaptureFamily> DeclaredCaptures<State, F> {
    /// Rejects duplicate keys before capture and stores only a successful baseline.
    fn declare_typed<K: 'static, Value, Error, Capture>(
        &mut self,
        state: &State,
        capture: Capture,
        origin: &'static Location<'static>,
        own: impl FnOnce(Capture) -> F::Callback,
    ) -> Result<(), F::Error>
    where
        F: CaptureStorage<State, Value, Error>,
        Capture: Fn(&State) -> Result<Value, Error>,
    {
        if self
            .entries
            .iter()
            .any(|entry| entry.key == TypeId::of::<K>())
        {
            let error = F::error(TestStoreFailure::capture_problem::<K>(
                CaptureProblem::DuplicateDeclaration,
                origin,
            ));
            drop(own);
            drop(capture);
            return Err(error);
        }
        let baseline = capture(state).map_err(|cause| {
            F::error(TestStoreFailure::capture_failed::<K>(
                F::cause::<K>(cause),
                origin,
                "declare",
            ))
        });
        let baseline = match baseline {
            Ok(baseline) => baseline,
            Err(error) => {
                drop(own);
                drop(capture);
                return Err(error);
            }
        };
        self.entries.push(Declaration {
            key: TypeId::of::<K>(),
            name: type_name::<K>(),
            value: AssertUnwindSafe(Mutex::new(F::declaration(TypedDeclaration::<
                State,
                K,
                Value,
                Error,
                F,
            > {
                baseline,
                capture: own(capture),
                retired: None,
                marker: PhantomData,
            }))),
        });
        Ok(())
    }

    /// Clones each verified history once, preserving declaration order.
    pub(super) fn unchanged(&mut self) -> Vec<ExpectedCapture<State, F>> {
        self.entries
            .iter_mut()
            .map(|entry| ExpectedCapture {
                key: entry.key,
                name: entry.name,
                value: entry.value.get_mut().prepare(),
                supplied: false,
            })
            .collect()
    }
}

impl<State: 'static> DeclaredCaptures<State> {
    /// Registers a transferable declaration without imposing Debug or Sync on values.
    pub(super) fn declare<K: CaptureKey>(
        &mut self,
        state: &State,
        capture: impl Fn(&State) -> Result<K::Value, K::Error> + Send + Sync + 'static,
        origin: &'static Location<'static>,
    ) -> Result<(), TestStoreError> {
        self.declare_typed::<K, K::Value, K::Error, _>(state, capture, origin, |capture| {
            Arc::new(capture)
        })
    }

    /// Stages transferable expectations and runs the caller's edits before admission.
    pub(super) fn expected(
        &mut self,
        update: impl FnOnce(&mut CaptureExpectations<'_>) -> Result<(), TestStoreError>,
    ) -> Result<Vec<ExpectedCapture<State>>, TestStoreError> {
        let mut entries = self.unchanged();
        let result = update(&mut CaptureExpectations {
            entries: &mut entries,
        });
        if let Err(error) = result {
            drop(entries);
            return Err(error);
        }
        Ok(entries)
    }
}

#[cfg(feature = "local")]
impl<State: 'static> DeclaredCaptures<State, Local> {
    /// Registers owner-local history and preserves an original non-Send cause.
    pub(super) fn declare<K: local::CaptureKey>(
        &mut self,
        state: &State,
        capture: impl Fn(&State) -> Result<K::Value, K::Error> + 'static,
        origin: &'static Location<'static>,
    ) -> Result<(), crate::error::local::TestStoreError> {
        self.declare_typed::<K, K::Value, K::Error, _>(state, capture, origin, |capture| {
            Rc::new(capture)
        })
    }

    /// Stages owner-local expectations and runs the caller's edits before admission.
    pub(super) fn expected(
        &mut self,
        update: impl FnOnce(
            &mut local::CaptureExpectations<'_>,
        ) -> Result<(), crate::error::local::TestStoreError>,
    ) -> Result<Vec<ExpectedCapture<State, Local>>, crate::error::local::TestStoreError> {
        let mut entries = self.unchanged();
        let result = update(&mut local::CaptureExpectations {
            entries: &mut entries,
        });
        if let Err(error) = result {
            drop(entries);
            return Err(error);
        }
        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::panic::catch_unwind;

    use futures_executor::block_on;
    use rca_core::{Effect, ReduceContext, Reducer};

    use super::*;
    use crate::TestStore;

    struct First;
    impl CaptureKey for First {
        type Value = u64;
        type Error = Infallible;
    }
    struct Second;
    impl CaptureKey for Second {
        type Value = u64;
        type Error = Infallible;
    }
    struct Increment;
    impl Reducer for Increment {
        type State = u64;
        type Action = ();
        fn reduce(&mut self, state: &mut u64, (): &(), _: &mut ReduceContext<'_>) -> Effect<()> {
            *state += 1;
            Effect::none()
        }
    }

    #[test]
    fn later_declaration_mismatch_installs_no_partial_baselines() {
        let mut store = TestStore::builder(0, Increment)
            .with_capture::<First>(|state| Ok(*state))
            .unwrap()
            .with_capture::<Second>(|state| Ok(*state))
            .unwrap()
            .build();
        let failure = block_on(store.send_with_captures(
            (),
            |state| *state = 1,
            |expected| expected.set::<First>(1),
        ))
        .unwrap_err();
        assert!(failure.is_capture_mismatch());
        assert_eq!(*store.assertions.baseline.get_mut(), 0);
        store
            .assertions
            .declared
            .expected(|values| {
                values.update::<First>(|value| assert_eq!(*value, 0))?;
                values.update::<Second>(|value| assert_eq!(*value, 0))
            })
            .unwrap();
        assert!(
            block_on(store.finish())
                .unwrap_err()
                .is_incomplete_assertion()
        );
        assert!(catch_unwind(AssertUnwindSafe(|| drop(store))).is_err());
    }
}

#[cfg(all(test, feature = "local"))]
mod local_storage_tests {
    use super::*;
    use std::cell::Cell;
    use std::convert::Infallible;
    use std::panic::catch_unwind;

    struct History;
    impl local::CaptureKey for History {
        type Value = Rc<Vec<u64>>;
        type Error = Infallible;
    }

    struct OtherHistory;
    impl local::CaptureKey for OtherHistory {
        type Value = Rc<Vec<u64>>;
        type Error = Infallible;
    }

    struct Number;
    impl CaptureKey for Number {
        type Value = u64;
        type Error = Infallible;
    }
    impl local::CaptureKey for Number {
        type Value = u64;
        type Error = Infallible;
    }

    fn common_staging_policy<F: CaptureFamily>(mut values: DeclaredCaptures<u64, F>)
    where
        F::Error: fmt::Debug,
    {
        let expected = values.unchanged();
        values.begin(expected, Location::caller(), "check_captures");
        let actual = values
            .batch()
            .capture(&1, Location::caller(), "check_captures")
            .unwrap();
        DeclaredCaptures::<_, F>::compare(&actual).unwrap();
        values.install(actual);
        values.complete();
        assert!(values.incomplete_error().is_none());
        let expected = values.unchanged();
        values.begin(expected, Location::caller(), "check_captures");
        let actual = values
            .batch()
            .capture(&2, Location::caller(), "check_captures")
            .unwrap();
        assert!(DeclaredCaptures::<_, F>::compare(&actual).is_err());
        assert!(values.incomplete_error().is_some());
        let mut retained = values.unchanged();
        let value = retained[0]
            .value
            .value_mut()
            .downcast_mut::<KeyedValue<Number, u64>>()
            .unwrap();
        assert_eq!(value.value, 1);
    }

    #[test]
    fn shared_and_local_use_the_same_success_and_mismatch_staging_policy() {
        // A family-dependent comparison/install/debt policy breaks one run of
        // this same harness; the two constructors only select lawful storage.
        let mut shared = DeclaredCaptures::<u64>::default();
        shared
            .declare::<Number>(&1, |state| Ok(*state), Location::caller())
            .unwrap();
        common_staging_policy(shared);
        let mut local = DeclaredCaptures::<u64, Local>::default();
        local
            .declare::<Number>(&1, |state| Ok(*state), Location::caller())
            .unwrap();
        common_staging_policy(local);
        println!(
            "capture_stage_layout declaration_shared={} declaration_local={} pending_shared={} pending_local={} owner_shared={} owner_local={}",
            std::mem::size_of::<Declaration<u64>>(),
            std::mem::size_of::<Declaration<u64, Local>>(),
            std::mem::size_of::<PendingCapture<u64, Shared>>(),
            std::mem::size_of::<PendingCapture<u64, Local>>(),
            std::mem::size_of::<DeclaredCaptures<u64>>(),
            std::mem::size_of::<DeclaredCaptures<u64, Local>>()
        );
    }

    #[test]
    fn local_expectation_validation_precedes_mutation_and_panic_keeps_slot_available() {
        // Consuming a slot before callback success or identifying by Value alone
        // would reject the retry or edit the wrong same-value-type declaration.
        let mut values = DeclaredCaptures::<Rc<Vec<u64>>, Local>::default();
        let state = Rc::new(vec![1]);
        values
            .declare::<History>(&state, |state| Ok(Rc::clone(state)), Location::caller())
            .unwrap();
        let calls = Cell::new(0);
        let Err(error) = values
            .expected(|expected| expected.update::<OtherHistory>(|_| calls.set(calls.get() + 1)))
        else {
            panic!("unknown expectation must propagate without installing a boundary");
        };
        assert!(error.to_string().contains("UnknownExpectation"));
        assert_eq!(calls.get(), 0);
        assert!(values.incomplete_error().is_none());
        values
            .expected(|expected| {
                assert!(format!("{expected:?}").contains("declarations: 1"));
                let error = expected
                    .update::<OtherHistory>(|_| calls.set(calls.get() + 1))
                    .unwrap_err();
                assert!(error.to_string().contains("UnknownExpectation"));
                catch_unwind(AssertUnwindSafe(|| {
                    expected.update::<History>(|_| panic!("caller mutation"))
                }))
                .expect_err("caller mutation must panic");
                expected.update::<History>(|value| {
                    assert_eq!(&**value, &[1]);
                    calls.set(calls.get() + 1);
                })?;
                let error = expected
                    .update::<History>(|_| calls.set(calls.get() + 1))
                    .unwrap_err();
                assert!(error.to_string().contains("DuplicateExpectation"));
                Ok(())
            })
            .unwrap();
        assert_eq!(calls.get(), 1);
        assert!(values.incomplete_error().is_none());
        let calls = Rc::new(Cell::new(0));
        let observed = Rc::clone(&calls);
        let error = values
            .declare::<History>(
                &state,
                move |_| {
                    observed.set(observed.get() + 1);
                    Ok(Rc::new(vec![9]))
                },
                Location::caller(),
            )
            .unwrap_err();
        assert!(error.to_string().contains("DuplicateDeclaration"));
        assert_eq!(calls.get(), 0);
    }

    struct CauseKey;
    impl local::CaptureKey for CauseKey {
        type Value = Rc<Vec<u64>>;
        type Error = Rc<Cell<u64>>;
    }

    #[test]
    fn local_later_key_mismatch_keeps_both_original_histories_and_debt() {
        // Matching keys by value type or installing the first successful entry
        // would erase the omitted second key's obligation or move one baseline.
        let mut values = DeclaredCaptures::<Rc<Vec<u64>>, Local>::default();
        let state = Rc::new(vec![1]);
        values
            .declare::<History>(&state, |state| Ok(Rc::clone(state)), Location::caller())
            .unwrap();
        values
            .declare::<OtherHistory>(&state, |state| Ok(Rc::clone(state)), Location::caller())
            .unwrap();
        let expected = values
            .expected(|expected| expected.set::<History>(Rc::new(vec![1, 2])))
            .unwrap();
        values.begin(expected, Location::caller(), "send");
        let actual = values
            .batch()
            .capture(&Rc::new(vec![1, 2]), Location::caller(), "send")
            .unwrap();
        let error = DeclaredCaptures::<_, Local>::compare(&actual).unwrap_err();
        assert!(error.is_capture_mismatch());
        assert!(error.to_string().contains("OtherHistory"));
        drop(actual);
        assert!(values.incomplete_error().unwrap().is_incomplete_assertion());
        values
            .expected(|expected| {
                expected.update::<History>(|value| assert_eq!(&**value, &[1]))?;
                expected.update::<OtherHistory>(|value| assert_eq!(&**value, &[1]))
            })
            .unwrap();
    }

    #[test]
    fn local_capture_cause_retains_identity_and_failed_boundary_keeps_prior_baselines() {
        // Installing early entries during capture would change History before
        // CauseKey fails. Wrapping or formatting the cause would lose identity.
        let cause = Rc::new(Cell::new(7));
        let pointer = Rc::as_ptr(&cause);
        let retained = Rc::clone(&cause);
        let mut values = DeclaredCaptures::<Rc<Vec<u64>>, Local>::default();
        let state = Rc::new(vec![1]);
        values
            .declare::<History>(&state, |state| Ok(Rc::clone(state)), Location::caller())
            .unwrap();
        values
            .declare::<CauseKey>(
                &state,
                move |state| {
                    if state.len() == 1 {
                        Ok(Rc::clone(state))
                    } else {
                        Err(Rc::clone(&retained))
                    }
                },
                Location::caller(),
            )
            .unwrap();
        let expected = values
            .expected(|expected| {
                expected.set::<History>(Rc::new(vec![1, 2]))?;
                expected.set::<CauseKey>(Rc::new(vec![1, 2]))
            })
            .unwrap();
        values.begin(expected, Location::caller(), "receive");
        let Err(error) =
            values
                .batch()
                .capture(&Rc::new(vec![1, 2]), Location::caller(), "receive")
        else {
            panic!("capture should retain its original error");
        };
        assert!(error.to_string().contains("during receive"));
        let recovered = error.into_capture_error::<CauseKey>().unwrap();
        assert_eq!(Rc::as_ptr(&recovered), pointer);
        assert_eq!(recovered.get(), 7);
        assert!(values.incomplete_error().unwrap().is_incomplete_assertion());
        values
            .expected(|expected| {
                expected.update::<History>(|value| assert_eq!(&**value, &[1]))?;
                expected.update::<CauseKey>(|value| assert_eq!(&**value, &[1]))
            })
            .unwrap();
        let mut undeclared = DeclaredCaptures::<(), Local>::default();
        let error = undeclared
            .declare::<CauseKey>(&(), move |()| Err(Rc::clone(&cause)), Location::caller())
            .unwrap_err();
        assert!(undeclared.is_empty());
        assert!(undeclared.incomplete_error().is_none());
        assert_eq!(
            Rc::as_ptr(&error.into_capture_error::<CauseKey>().unwrap()),
            pointer
        );
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Phase {
        Baseline,
        Expected,
        Actual,
    }

    struct Observations {
        owner: std::thread::ThreadId,
        events: Arc<Mutex<Vec<(&'static str, std::thread::ThreadId)>>>,
        clones: Cell<usize>,
        captures: Cell<usize>,
        equalities: Cell<usize>,
        clone_panics: Cell<bool>,
        capture_panics: Cell<bool>,
        equality_panics: Cell<bool>,
        drop_panics: Cell<Option<Phase>>,
    }

    impl Observations {
        fn record(&self, event: &'static str) {
            let current = std::thread::current().id();
            self.events.lock().push((event, current));
            assert_eq!(self.owner, current);
        }
    }

    struct ObservedValue {
        value: u64,
        phase: Phase,
        observations: Rc<Observations>,
    }

    impl Clone for ObservedValue {
        fn clone(&self) -> Self {
            self.observations.record("clone");
            self.observations
                .clones
                .set(self.observations.clones.get() + 1);
            assert!(
                !self.observations.clone_panics.replace(false),
                "historical clone"
            );
            Self {
                value: self.value,
                phase: Phase::Expected,
                observations: Rc::clone(&self.observations),
            }
        }
    }

    impl PartialEq for ObservedValue {
        fn eq(&self, other: &Self) -> bool {
            self.observations.record("equality");
            self.observations
                .equalities
                .set(self.observations.equalities.get() + 1);
            assert!(
                !self.observations.equality_panics.replace(false),
                "historical equality"
            );
            self.value == other.value
        }
    }

    impl Drop for ObservedValue {
        fn drop(&mut self) {
            self.observations.record("drop");
            if self.observations.drop_panics.get() == Some(self.phase) {
                self.observations.drop_panics.set(None);
                panic!("historical destructor");
            }
        }
    }

    struct Observed;
    impl local::CaptureKey for Observed {
        type Value = ObservedValue;
        type Error = Infallible;
    }

    fn observed_capture() -> (DeclaredCaptures<u64, Local>, Rc<Observations>) {
        let observations = Rc::new(Observations {
            owner: std::thread::current().id(),
            events: Arc::new(Mutex::new(Vec::new())),
            clones: Cell::new(0),
            captures: Cell::new(0),
            equalities: Cell::new(0),
            clone_panics: Cell::new(false),
            capture_panics: Cell::new(false),
            equality_panics: Cell::new(false),
            drop_panics: Cell::new(None),
        });
        let recorded = Rc::clone(&observations);
        let mut values = DeclaredCaptures::<u64, Local>::default();
        values
            .declare::<Observed>(
                &1,
                move |state| {
                    recorded.record("capture");
                    let phase = if recorded.captures.get() == 0 {
                        Phase::Baseline
                    } else {
                        Phase::Actual
                    };
                    recorded.captures.set(recorded.captures.get() + 1);
                    assert!(
                        !recorded.capture_panics.replace(false),
                        "independent capture"
                    );
                    Ok(ObservedValue {
                        value: *state,
                        phase,
                        observations: Rc::clone(&recorded),
                    })
                },
                Location::caller(),
            )
            .unwrap();
        (values, observations)
    }

    #[test]
    fn local_clone_panic_precedes_admission_but_capture_and_equality_panics_leave_debt() {
        // Moving the debt marker after user capture/equality would hide an
        // admitted failed boundary. A pre-admission clone must not add debt.
        let (mut values, observations) = observed_capture();
        observations.clone_panics.set(true);
        assert!(catch_unwind(AssertUnwindSafe(|| values.unchanged())).is_err());
        assert!(values.incomplete_error().is_none());
        for capture_panics in [true, false] {
            let (mut values, observations) = observed_capture();
            let expected = values.unchanged();
            values.begin(expected, Location::caller(), "send");
            observations.capture_panics.set(capture_panics);
            observations.equality_panics.set(!capture_panics);
            assert!(
                catch_unwind(AssertUnwindSafe(|| {
                    let actual = values
                        .batch()
                        .capture(&1, Location::caller(), "send")
                        .unwrap();
                    DeclaredCaptures::<_, Local>::compare(&actual).unwrap();
                }))
                .is_err()
            );
            assert!(values.incomplete_error().unwrap().is_incomplete_assertion());
        }
    }

    #[test]
    fn local_old_and_retired_value_drop_panics_keep_boundary_debt() {
        // Clearing debt before either user destructor lets finish overlook the
        // partially completed boundary. Both drops must remain owner-thread local.
        for phase in [Phase::Baseline, Phase::Expected] {
            let (mut values, observations) = observed_capture();
            let expected = values.unchanged();
            values.begin(expected, Location::caller(), "check_captures");
            let actual = values
                .batch()
                .capture(&1, Location::caller(), "check_captures")
                .unwrap();
            DeclaredCaptures::<_, Local>::compare(&actual).unwrap();
            observations.drop_panics.set(Some(phase));
            assert!(
                catch_unwind(AssertUnwindSafe(|| {
                    values.install(actual);
                    values.complete();
                }))
                .is_err()
            );
            assert!(values.incomplete_error().unwrap().is_incomplete_assertion());
        }
    }

    struct CountedCause(Rc<Cell<usize>>);

    impl Drop for CountedCause {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    struct CountedCauseKey;

    impl local::CaptureKey for CountedCauseKey {
        type Value = u64;
        type Error = CountedCause;
    }

    #[test]
    fn local_batch_cleanup_drops_original_cause_before_propagating_value_panic() {
        for already_captured in [false, true] {
            let (mut values, observations) = observed_capture();
            let cause_drops = Rc::new(Cell::new(0));
            let captured_drops = Rc::clone(&cause_drops);
            values
                .declare::<CountedCauseKey>(
                    &1,
                    move |state| {
                        if *state == 1 {
                            Ok(0)
                        } else {
                            Err(CountedCause(Rc::clone(&captured_drops)))
                        }
                    },
                    Location::caller(),
                )
                .unwrap();
            if !already_captured {
                // Equivalent to declaring the fallible key first: its failure
                // must dispose of the unvisited expectation as well as its cause.
                values.entries.swap(0, 1);
            }
            let expected = values.unchanged();
            values.begin(expected, Location::caller(), "check_captures");
            observations.drop_panics.set(Some(if already_captured {
                Phase::Actual
            } else {
                Phase::Expected
            }));
            let result = catch_unwind(AssertUnwindSafe(|| {
                drop(
                    values
                        .batch()
                        .capture(&2, Location::caller(), "check_captures"),
                );
            }));
            let panic = result.expect_err("value cleanup must propagate its original panic");
            assert_eq!(panic.downcast_ref::<&str>(), Some(&"historical destructor"));
            assert_eq!(cause_drops.get(), 1, "already_captured={already_captured}");
            assert_eq!(
                observations.captures.get(),
                if already_captured { 2 } else { 1 }
            );
            assert_eq!(observations.equalities.get(), 0);
            assert!(values.incomplete_error().unwrap().is_incomplete_assertion());
        }
    }

    #[test]
    fn local_preflight_cleanup_drops_returned_cause_without_creating_debt() {
        let (mut values, observations) = observed_capture();
        let cause_drops = Rc::new(Cell::new(0));
        let captured_drops = Rc::clone(&cause_drops);
        let mut failed_declaration = DeclaredCaptures::<(), Local>::default();
        let error = failed_declaration
            .declare::<CountedCauseKey>(
                &(),
                move |()| Err(CountedCause(Rc::clone(&captured_drops))),
                Location::caller(),
            )
            .unwrap_err();
        observations.drop_panics.set(Some(Phase::Expected));
        let result = catch_unwind(AssertUnwindSafe(|| {
            drop(values.expected(|_| Err(error)));
        }));
        let panic = result.expect_err("preflight cleanup must propagate its original panic");
        assert_eq!(panic.downcast_ref::<&str>(), Some(&"historical destructor"));
        assert_eq!(cause_drops.get(), 1);
        assert!(values.incomplete_error().is_none());
        assert_eq!(observations.captures.get(), 1);
        assert_eq!(observations.clones.get(), 1);
    }

    #[test]
    fn local_omitted_set_and_update_each_clone_capture_and_compare_once() {
        // Re-cloning during install or comparing before all captures changes
        // the per-boundary user-code budget and can alter observable behaviour.
        for mode in 0..3 {
            let (mut values, observations) = observed_capture();
            let expected = values
                .expected(|expected| match mode {
                    0 => Ok(()),
                    1 => expected.set::<Observed>(ObservedValue {
                        value: 1,
                        phase: Phase::Expected,
                        observations: Rc::clone(&observations),
                    }),
                    _ => expected.update::<Observed>(|value| value.value = 1),
                })
                .unwrap();
            values.begin(expected, Location::caller(), "check_captures");
            let actual = values
                .batch()
                .capture(&1, Location::caller(), "check_captures")
                .unwrap();
            DeclaredCaptures::<_, Local>::compare(&actual).unwrap();
            values.install(actual);
            values.complete();
            assert_eq!(observations.clones.get(), 1);
            assert_eq!(observations.captures.get(), 2); // Initial declaration plus boundary.
            assert_eq!(observations.equalities.get(), 1);
            assert!(values.incomplete_error().is_none());
            let events = Arc::clone(&observations.events);
            let owner = observations.owner;
            drop(values);
            drop(observations);
            let events = events.lock();
            assert!(events.iter().all(|(_, thread)| *thread == owner));
            assert_eq!(
                events.iter().filter(|(event, _)| *event == "drop").count(),
                if mode == 1 { 4 } else { 3 }
            );
        }
    }

    #[test]
    fn local_empty_boundary_has_no_debt_and_dropped_nonempty_batch_keeps_debt() {
        let mut empty = DeclaredCaptures::<(), Local>::default();
        assert!(empty.is_empty());
        let expected = empty.unchanged();
        empty.begin(expected, Location::caller(), "check_captures");
        let actual = empty
            .batch()
            .capture(&(), Location::caller(), "check_captures")
            .unwrap();
        DeclaredCaptures::<_, Local>::compare(&actual).unwrap();
        empty.install(actual);
        empty.complete();
        assert!(empty.incomplete_error().is_none());
        let (mut values, _) = observed_capture();
        let expected = values.unchanged();
        values.begin(expected, Location::caller(), "receive");
        drop(values.batch());
        assert!(values.incomplete_error().unwrap().is_incomplete_assertion());
    }

    #[test]
    fn local_history_edits_do_not_alias_the_prior_baseline() {
        // Retaining Send-erased history rejects this lawful Rc key; sharing the
        // live history during expected mutation would corrupt the old baseline.
        let owner = std::thread::current().id();
        let old = Rc::new(vec![1]);
        let captures = Rc::new(Cell::new(0));
        let recorded = Rc::clone(&captures);
        let mut values = DeclaredCaptures::<Rc<Vec<u64>>, Local>::default();
        values
            .declare::<History>(
                &old,
                move |state| {
                    assert_eq!(owner, std::thread::current().id());
                    recorded.set(recorded.get() + 1);
                    Ok(Rc::clone(state))
                },
                Location::caller(),
            )
            .unwrap();
        let expected = values
            .expected(|expected| expected.update::<History>(|value| Rc::make_mut(value).push(2)))
            .unwrap();
        assert_eq!(&*old, &[1]);
        values.begin(expected, Location::caller(), "check_captures");
        let actual = values
            .batch()
            .capture(&Rc::new(vec![1, 2]), Location::caller(), "check_captures")
            .unwrap();
        DeclaredCaptures::<_, Local>::compare(&actual).unwrap();
        values.install(actual);
        values.complete();
        assert!(values.incomplete_error().is_none());
        assert_eq!(captures.get(), 2);
        values
            .expected(|expected| {
                expected.update::<History>(|value| {
                    assert_eq!(&**value, &[1, 2]);
                })
            })
            .unwrap();
    }
}
