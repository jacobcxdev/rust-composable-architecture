//! Observes caller-labelled synchronous operations without altering their results.

use core::fmt;

/// Identifies whether the observed operation returned success or an error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CheckpointOutcome {
    /// The operation returned `Ok`.
    Success,
    /// The operation returned `Err`.
    Error,
}

/// Borrows independent snapshots and metadata during one synchronous callback.
///
/// Debug includes only outcome and change metadata, never the caller's label
/// or domain snapshots. Neither successful commit nor effect completion is implied.
pub struct Checkpoint<'a, Snapshot> {
    /// Caller-owned operation label, excluded from implicit Debug output.
    label: &'a str,
    /// Independent value captured before the operation.
    before: &'a Snapshot,
    /// Independent value captured after the operation returns a typed result.
    after: &'a Snapshot,
    /// Cached semantic comparison, not a comparison of rendered output.
    changed: bool,
    /// Result category without retaining or exposing the operation's payload.
    outcome: CheckpointOutcome,
}

impl<'a, Snapshot> Checkpoint<'a, Snapshot> {
    /// Returns the caller's explicit operation label.
    #[must_use]
    pub const fn label(&self) -> &'a str {
        self.label
    }

    /// Returns the independent snapshot captured before the operation.
    #[must_use]
    pub const fn before(&self) -> &'a Snapshot {
        self.before
    }

    /// Returns the independent snapshot captured after the operation.
    #[must_use]
    pub const fn after(&self) -> &'a Snapshot {
        self.after
    }

    /// Reports semantic inequality without another comparison.
    #[must_use]
    pub const fn is_changed(&self) -> bool {
        self.changed
    }

    /// Reports success or error without exposing the operation's typed result.
    #[must_use]
    pub const fn outcome(&self) -> CheckpointOutcome {
        self.outcome
    }
}

impl<Snapshot> fmt::Debug for Checkpoint<'_, Snapshot> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Checkpoint")
            .field("is_changed", &self.changed)
            .field("outcome", &self.outcome)
            .finish_non_exhaustive()
    }
}

/// Observes a synchronous operation through debug-only snapshots and outcome metadata.
///
/// The order is capture-before, operation, capture-after, comparison, sink. Capture
/// independent values: shared interior-mutable handles do not freeze prior state.
/// Explicit nested labels deliver in lexical callback order, with inner sinks
/// before outer sinks. No ambient registry or durable event history is installed.
/// Without debug assertions, only `operation` runs, exactly once. Required
/// assertions belong in the operation, not in capture or sink callbacks.
/// Callback captures are still dropped before return. Their destructors are
/// not disabled with diagnostic observation.
///
/// # Examples
///
/// ```
/// use rca_core::diagnostics::{CheckpointOutcome, checkpoint};
/// let mut count = 0;
/// let result = checkpoint(&mut count, "increment", |state| *state, |state| {
///     *state += 1;
///     Ok::<_, ()>(*state)
/// }, |event| {
///     assert_eq!(event.outcome(), CheckpointOutcome::Success);
///     assert_eq!((*event.before(), *event.after()), (0, 1));
/// });
/// assert_eq!(result, Ok(1));
/// ```
///
/// # Errors
///
/// Returns the operation's original typed error unchanged, after observation.
/// Diagnostic panics are never converted into operation success or error.
///
/// # Panics
///
/// Before-capture panic prevents the operation. Operation panic skips after
/// capture and the sink. After-capture, comparison, and sink panics propagate
/// after the operation has run. State mutations are never rolled back. This
/// observes neither Store commit acknowledgement nor effect completion.
/// Snapshot or callback destructors can also panic before the result reaches
/// the caller, including after the sink returns normally.
pub fn checkpoint<State, Snapshot, T, E>(
    state: &mut State,
    label: &str,
    capture: impl Fn(&State) -> Snapshot,
    operation: impl FnOnce(&mut State) -> Result<T, E>,
    mut sink: impl for<'a> FnMut(Checkpoint<'a, Snapshot>),
) -> Result<T, E>
where
    Snapshot: PartialEq,
{
    #[cfg(debug_assertions)]
    let before = capture(state);
    let result = operation(state);
    #[cfg(debug_assertions)]
    {
        let after = capture(state);
        let changed = before != after;
        let outcome = if result.is_ok() {
            CheckpointOutcome::Success
        } else {
            CheckpointOutcome::Error
        };
        sink(Checkpoint {
            label,
            before: &before,
            after: &after,
            changed,
            outcome,
        });
        // Keep the result local until owned observation cleanup succeeds.
        drop(after);
        drop(before);
        drop(sink);
        drop(capture);
        result
    }
    #[cfg(not(debug_assertions))]
    {
        let _ = (label, capture, &mut sink);
        drop(sink);
        result
    }
}

/// Executes an explicitly unobserved operation without snapshot or callback bounds.
///
/// Calls the operation exactly once and returns its original typed result. State
/// need not implement equality, cloning, Debug, Send, or any lifetime bound.
/// Required assertions remain the operation's responsibility in every profile.
///
/// # Examples
///
/// ```
/// use rca_core::diagnostics::checkpoint_disabled;
/// let mut count = 0;
/// let result = checkpoint_disabled(&mut count, |state| {
///     *state += 1;
///     Ok::<_, ()>(*state)
/// });
/// assert_eq!(result, Ok(1));
/// ```
///
/// # Errors
///
/// Returns the operation's original error unchanged.
///
/// # Panics
///
/// Propagates operation panics without rolling back state mutations.
pub fn checkpoint_disabled<State, T, E>(
    state: &mut State,
    operation: impl FnOnce(&mut State) -> Result<T, E>,
) -> Result<T, E> {
    operation(state)
}
