//! Runs explicit owned dependency scopes and checks concrete mock interfaces.

use std::any::{Any, TypeId, type_name};
use std::backtrace::Backtrace;
use std::fmt;

use super::{DependencyKey, DependencyValues, ReduceContext};

impl DependencyValues {
    /// Runs a synchronous operation with an independently overridden environment.
    ///
    /// Clones this environment, applies `update`, then lends the child to `operation`.
    /// Nested overrides never mutate the parent's maps, including during unwinding.
    /// Cloning shares existing provider handles and constructors, not independent
    /// backend state. For independent parameterised tests or repetitions, call a
    /// fresh `Fn() -> DependencyValues` fixture constructor for each case.
    /// Borrowed child values cannot escape; clone selected owned handles explicitly.
    ///
    /// # Panics
    /// Propagates callback and dependency destructor panics.
    pub fn scope<Output>(
        &self,
        update: impl FnOnce(&mut Self),
        operation: impl FnOnce(&mut Self) -> Output,
    ) -> Output {
        let mut child = self.clone();
        update(&mut child);
        operation(&mut child)
    }

    /// Creates caller-owned asynchronous work with an explicitly inherited dependency environment.
    ///
    /// Clones this environment and invokes both callbacks immediately. `operation`
    /// takes ownership of the child; this method adds no parent borrow to its future.
    /// The operation can capture the child in that future or transfer it elsewhere.
    /// Dropping the future releases only the values that the future still owns.
    /// No work is spawned or polled here. The caller chooses the executor and owns
    /// polling, cancellation and shutdown. Local, non-`Send` futures are accepted;
    /// thread-safe transport requires the caller's ordinary Rust `Send` bounds.
    /// Existing provider handles and constructors are shared, as with [`Self::scope`].
    ///
    /// Capturing an effect sender or scoped handle retains its existing authority,
    /// not a fresh child lifetime. Old output remains revoked after removal, even
    /// when a replacement uses the same public ID. Root external input is separately
    /// authorised. Detached providers need explicit transport and a shutdown owner;
    /// dropping this future cannot stop independently spawned backend work.
    ///
    /// # Examples
    ///
    /// ```
    /// use rca_core::{DependencyKey, DependencyValues};
    /// struct Number;
    /// impl DependencyKey for Number { type Value = usize; }
    /// let work = {
    ///     let values = DependencyValues::new();
    ///     values.scope_async(
    ///         |child| child.set::<Number>(7),
    ///         |mut child| async move { *child.get_or_insert::<Number>() },
    ///     )
    /// };
    /// assert_eq!(futures_executor::block_on(work), 7);
    /// ```
    ///
    /// # Panics
    /// Propagates callback and dependency destructor panics during construction.
    pub fn scope_async<F: Future>(
        &self,
        update: impl FnOnce(&mut Self),
        operation: impl FnOnce(Self) -> F,
    ) -> F {
        let mut child = self.clone();
        update(&mut child);
        operation(child)
    }
}

impl ReduceContext<'_> {
    /// Clones one selected dependency without retaining this borrowed reduction context.
    ///
    /// Uses the same test, action, feature and store precedence as [`Self::dependency`].
    /// Only this key's value needs `Clone`; domains and other dependencies do not.
    /// Clone semantics belong to the value: handles usually share their provider.
    /// Capturing a handle neither extends nor renews its output authority.
    ///
    /// # Panics
    /// Propagates the selected default factory or value's clone panic.
    pub fn capture_dependency<K: DependencyKey>(&mut self) -> K::Value
    where
        K::Value: Clone,
    {
        self.dependency::<K>().clone()
    }
}

/// Reports a checked mock-interface type mismatch without exposing its value.
#[derive(Debug)]
pub struct DependencyMockMismatch {
    /// Identifies the concrete mock requested by the caller.
    expected_type_id: TypeId,
    /// Names the requested type without retaining or formatting the interface value.
    expected_type_name: &'static str,
    /// Identifies the concrete value behind the supplied `Any` reference.
    actual_type_id: TypeId,
    /// Captures the mismatched lookup site when backtraces are enabled.
    backtrace: Backtrace,
}

impl DependencyMockMismatch {
    /// Returns the requested concrete mock type's identity.
    #[must_use]
    pub const fn expected_type_id(&self) -> TypeId {
        self.expected_type_id
    }

    /// Returns the requested concrete mock type's Rust name.
    #[must_use]
    pub const fn expected_type_name(&self) -> &'static str {
        self.expected_type_name
    }

    /// Returns the concrete interface value's identity.
    #[must_use]
    pub const fn actual_type_id(&self) -> TypeId {
        self.actual_type_id
    }

    /// Returns the backtrace captured when the types did not match.
    pub const fn backtrace(&self) -> &Backtrace {
        &self.backtrace
    }
}

impl fmt::Display for DependencyMockMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "dependency mock is not `{}`\n{}",
            self.expected_type_name, self.backtrace
        )
    }
}

impl std::error::Error for DependencyMockMismatch {}

/// Borrows a concrete mock from an explicitly supplied type-erased interface.
///
/// Pass the interface's `Any` view, not its `Box` or `Arc` container. Traits that
/// extend `Any` can be upcast directly. Uses safe checked downcasting; a mismatch
/// leaves the original interface usable and never reinterprets its representation.
/// No provider discovery or dependency lookup is performed.
///
/// # Examples
///
/// ```
/// use rca_core::try_dependency_mock;
/// trait Service: std::any::Any {}
/// struct Mock;
/// impl Service for Mock {}
/// let service: Box<dyn Service> = Box::new(Mock);
/// let _: &Mock = try_dependency_mock(&*service).unwrap();
/// ```
///
/// # Errors
/// Returns type metadata when the interface's concrete type is not `T`.
pub fn try_dependency_mock<T: Any>(value: &dyn Any) -> Result<&T, DependencyMockMismatch> {
    value.downcast_ref().ok_or_else(|| DependencyMockMismatch {
        expected_type_id: TypeId::of::<T>(),
        expected_type_name: type_name::<T>(),
        actual_type_id: value.type_id(),
        backtrace: Backtrace::capture(),
    })
}
