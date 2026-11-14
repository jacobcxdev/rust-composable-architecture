//! Caller-captured semantic checks, independent of diagnostic sinks and test runtimes.
//!
//! Captures must describe independent historical values: cloning a live shared
//! handle does not capture its former contents. Equality covers only the selected
//! snapshot. These helpers require neither `Clone`, `Debug`, `Send` nor `'static`.
//! Renderers can use caller-owned diagnostic views without changing equality.
//!
//! Required checks return typed failures; disabled or closed issue reporters
//! cannot suppress them. Inspect both fields of [`ChangeCheck`]. There is no
//! ambient assertion tracking, rollback, panic catching or implicit reporting.

use std::backtrace::Backtrace;
use std::fmt;
use std::panic::Location;

use rca_core::diagnostics::format_snapshot_mismatch;

/// Retains the operation result and independent semantic comparison.
///
/// Neither field implies success of the other. An operation error still permits
/// capture and comparison; comparison failure never replaces the typed error.
/// `Debug` shows only outcome metadata, never operation values or diagnostics,
/// and does not require `R` or `E` to implement `Debug`.
#[must_use = "inspect both the operation result and semantic comparison"]
pub struct ChangeCheck<R, E> {
    /// The original operation result, without conversion or replacement.
    pub operation_result: Result<R, E>,
    /// The independent change requirement and expected-value comparison.
    pub comparison: Result<(), ComparisonFailure>,
}

impl<R, E> fmt::Debug for ChangeCheck<R, E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChangeCheck")
            .field("operation_succeeded", &self.operation_result.is_ok())
            .field("comparison_succeeded", &self.comparison.is_ok())
            .finish_non_exhaustive()
    }
}

/// Describes unmet semantic equality or an operation's missing change.
///
/// Both conditions can fail together. Only caller-supplied rendered text and
/// messages are retained, never snapshots or an implicit `Debug` representation.
#[derive(Debug)]
pub struct ComparisonFailure {
    /// The operation left its selected historical value unchanged.
    no_change: bool,
    /// The expected update and actual snapshot differ under caller equality.
    mismatch: bool,
    /// Caller message, constructed only after a comparison failure.
    message: Option<String>,
    /// Bounded rendered mismatch, independent of semantic equality.
    excerpt: Option<String>,
    /// Public helper call site, captured before asynchronous polling.
    origin: &'static Location<'static>,
    /// Failure trace captured under the standard environment policy.
    backtrace: Backtrace,
}

impl ComparisonFailure {
    /// Returns whether an operation left the captured value unchanged.
    #[must_use]
    pub const fn is_no_change(&self) -> bool {
        self.no_change
    }

    /// Returns whether the expected and actual snapshots differed semantically.
    #[must_use]
    pub const fn is_mismatch(&self) -> bool {
        self.mismatch
    }

    /// Returns the lazily constructed caller message, if supplied.
    #[must_use]
    pub fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }

    /// Returns the rendered mismatch excerpt, absent when expected equals actual.
    #[must_use]
    pub fn excerpt(&self) -> Option<&str> {
        self.excerpt.as_deref()
    }

    /// Returns the helper's call site, including for deferred async checks.
    #[must_use]
    pub const fn location(&self) -> &'static Location<'static> {
        self.origin
    }

    /// Returns the backtrace captured when the comparison failed.
    pub const fn backtrace(&self) -> &Backtrace {
        &self.backtrace
    }
}

impl fmt::Display for ComparisonFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Semantic check failed at {}.", self.origin)?;
        if self.no_change {
            write!(f, " Operation did not change the captured value.")?;
        }
        if self.mismatch {
            write!(f, " Expected and actual values differ.")?;
        }
        if let Some(message) = &self.message {
            write!(f, "\n{message}")?;
        }
        if let Some(excerpt) = &self.excerpt {
            write!(f, "\n{excerpt}")?;
        }
        write!(f, "\n{}", self.backtrace)
    }
}

impl std::error::Error for ComparisonFailure {}

/// Checks independently captured expected and actual values for semantic equality.
///
/// Calls `expected`, then `actual`, then `PartialEq::eq` exactly once each.
/// Success calls neither diagnostic closure. On failure it calls `message` once,
/// then `render` once for expected and once for actual, in that order. Identical
/// rendered text never overrides semantic inequality. The displayed excerpt is
/// bounded, but caller rendering work and string allocation are not bounded.
///
/// # Examples
/// ```
/// rca_test::check_equal(|| 3, || 3, i32::to_string, || None).unwrap();
/// ```
///
/// # Errors
/// Returns a failure when the selected values differ semantically.
///
/// # Panics
/// Propagates capture, equality, message, renderer and owned destructor panics.
#[track_caller]
pub fn check_equal<S: PartialEq>(
    expected: impl FnOnce() -> S,
    actual: impl FnOnce() -> S,
    render: impl FnMut(&S) -> String,
    message: impl FnOnce() -> Option<String>,
) -> Result<(), ComparisonFailure> {
    let origin = Location::caller();
    let expected = expected();
    let actual = actual();
    let mismatch = !expected.eq(&actual);
    let result = finish_comparison(false, mismatch, &expected, &actual, render, message, origin);
    drop(actual);
    drop(expected);
    result
}

/// Checks expected changes without running or requiring a changing operation.
///
/// Calls `before`, then `actual`, then `update` exactly once each. `update`
/// transforms the owned before snapshot into the expected value; equality is
/// checked exactly once afterwards. An unchanged actual value and no-op update
/// can pass. Diagnostic order and laziness match [`check_equal`].
///
/// # Examples
/// ```
/// rca_test::check_expected_changes(
///     || 1,
///     || 2,
///     |n| *n += 1,
///     i32::to_string,
///     || None,
/// )
/// .unwrap();
/// ```
///
/// # Errors
/// Returns a failure when the updated expectation differs from actual.
///
/// # Panics
/// Propagates capture, update, equality, message, renderer and owned destructor panics.
#[track_caller]
pub fn check_expected_changes<S: PartialEq>(
    before: impl FnOnce() -> S,
    actual: impl FnOnce() -> S,
    update: impl FnOnce(&mut S),
    render: impl FnMut(&S) -> String,
    message: impl FnOnce() -> Option<String>,
) -> Result<(), ComparisonFailure> {
    let origin = Location::caller();
    let mut expected = before();
    let actual = actual();
    update(&mut expected);
    let mismatch = !expected.eq(&actual);
    let result = finish_comparison(false, mismatch, &expected, &actual, render, message, origin);
    drop(actual);
    drop(expected);
    result
}

/// Runs an operation and checks its required change against an expected update.
///
/// Order: capture before, operation, capture actual, compare before with actual,
/// update before into expected, compare expected with actual. Capture runs twice;
/// operation and update run once; equality runs twice, including when unchanged
/// or when the operation returns `Err`. These counts assume callbacks return
/// normally. Both results are retained independently, without success/error masking.
///
/// An unchanged operation fails even if expected equals actual. Any comparison
/// failure calls `message` once. Only an expected/actual mismatch calls `render`,
/// once for expected and then once for actual; see [`check_equal`] for rendering
/// limits. The operation result is retained even when comparison fails.
///
/// # Examples
/// ```
/// let mut count = 0;
/// let checked = rca_test::check_change(
///     &mut count,
///     |n| *n,
///     |n| {
///         *n += 1;
///         Ok::<_, ()>("done")
///     },
///     |n| *n += 1,
///     i32::to_string,
///     || None,
/// );
/// assert_eq!(checked.operation_result, Ok("done"));
/// checked.comparison.unwrap();
/// ```
///
/// # Errors
/// `comparison` fails for no actual change or an expected/actual mismatch.
/// `operation_result` retains any original typed operation error.
///
/// # Panics
/// Propagates callback and owned destructor panics, stopping subsequent work
/// without rollback.
#[track_caller]
pub fn check_change<C: ?Sized, S: PartialEq, R, E>(
    context: &mut C,
    mut capture: impl FnMut(&C) -> S,
    operation: impl FnOnce(&mut C) -> Result<R, E>,
    update: impl FnOnce(&mut S),
    render: impl FnMut(&S) -> String,
    message: impl FnOnce() -> Option<String>,
) -> ChangeCheck<R, E> {
    let origin = Location::caller();
    let before = capture(context);
    let operation_result = operation(context);
    let actual = capture(context);
    let comparison = compare_change(before, &actual, update, render, message, origin);
    // Keep both outcomes local while caller-owned snapshots and captures drop.
    drop(actual);
    drop(capture);
    ChangeCheck {
        operation_result,
        comparison,
    }
}

/// Awaits an operation and checks its required change against an expected update.
///
/// Captures the caller location at construction. No supplied closure or operation
/// factory runs until the returned future is polled. Once polled, the order and
/// counts are exactly [`check_change`], awaiting the operation between captures.
/// An operation `Err` still triggers the second capture and both comparisons.
///
/// Dropping an unpolled future performs no check. Dropping a pending operation
/// performs no subsequent capture, update or comparison and returns no completed
/// result. There is no ambient pending assertion obligation or rollback. Local
/// borrowing futures need no `Send` or `'static` bound.
///
/// # Examples
/// ```
/// # futures_executor::block_on(async {
///     let mut count = 0;
///     let checked = rca_test::check_change_async(
///         &mut count,
///         |n| *n,
///         async |n| {
///             *n += 1;
///             Ok::<_, ()>(())
///         },
///         |n| *n += 1,
///         i32::to_string,
///         || None,
///     )
///     .await;
///     checked.operation_result.unwrap();
///     checked.comparison.unwrap();
/// # });
/// ```
///
/// # Errors
/// Returns both outcomes described by [`check_change`], without masking either.
///
/// # Panics
/// Propagates callback, operation-future and owned destructor panics during polling.
#[track_caller]
pub fn check_change_async<C: ?Sized, S: PartialEq, R, E>(
    context: &mut C,
    mut capture: impl FnMut(&C) -> S,
    operation: impl AsyncFnOnce(&mut C) -> Result<R, E>,
    update: impl FnOnce(&mut S),
    render: impl FnMut(&S) -> String,
    message: impl FnOnce() -> Option<String>,
) -> impl Future<Output = ChangeCheck<R, E>> {
    let origin = Location::caller();
    async move {
        let before = capture(context);
        let operation_result = operation(context).await;
        let actual = capture(context);
        let comparison = compare_change(before, &actual, update, render, message, origin);
        drop(actual);
        drop(capture);
        ChangeCheck {
            operation_result,
            comparison,
        }
    }
}

/// Checks required change before editing the owned expected snapshot.
///
/// Both equality calls run even when the operation made no change.
fn compare_change<S: PartialEq>(
    mut expected: S,
    actual: &S,
    update: impl FnOnce(&mut S),
    render: impl FnMut(&S) -> String,
    message: impl FnOnce() -> Option<String>,
    origin: &'static Location<'static>,
) -> Result<(), ComparisonFailure> {
    let no_change = expected.eq(actual);
    update(&mut expected);
    let mismatch = !expected.eq(actual);
    let result = finish_comparison(
        no_change, mismatch, &expected, actual, render, message, origin,
    );
    drop(expected);
    result
}

/// Builds diagnostics only for an unmet semantic requirement.
///
/// Messages precede rendering. A missing change alone does not render snapshots.
fn finish_comparison<S>(
    no_change: bool,
    mismatch: bool,
    expected: &S,
    actual: &S,
    mut render: impl FnMut(&S) -> String,
    message: impl FnOnce() -> Option<String>,
    origin: &'static Location<'static>,
) -> Result<(), ComparisonFailure> {
    if !no_change && !mismatch {
        return Ok(());
    }
    let message = message();
    let excerpt = mismatch.then(|| {
        let expected = render(expected);
        let actual = render(actual);
        format_snapshot_mismatch(&expected, &actual)
    });
    let result = Err(ComparisonFailure {
        no_change,
        mismatch,
        message,
        excerpt,
        origin,
        backtrace: Backtrace::capture(),
    });
    // Rendering captures can own destructors; keep the failure local until they drop.
    drop(render);
    result
}
