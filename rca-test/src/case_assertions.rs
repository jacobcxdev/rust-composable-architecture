//! Checks matching cases independently of diagnostic sinks and test runtimes.
//!
//! Extraction and payload checks run once. Missing cases return mandatory typed
//! failures in every build; no reporting configuration suppresses the result.
//! Payloads are borrowed, never implicitly cloned, compared or rendered. User
//! check errors remain distinct from case failures. Callback panics propagate.

use std::backtrace::Backtrace;
use std::fmt;
use std::panic::Location;

/// Records a required case that did not match.
#[derive(Debug)]
pub struct CaseAssertionFailure {
    /// Public helper call site, preserved through the nested payload-check helper.
    origin: &'static Location<'static>,
    /// Failure trace captured under the standard environment policy.
    backtrace: Backtrace,
}

impl CaseAssertionFailure {
    /// Returns the caller location of the failed case check.
    #[must_use]
    pub const fn location(&self) -> &'static Location<'static> {
        self.origin
    }

    /// Returns the trace captured when the case check failed.
    pub const fn backtrace(&self) -> &Backtrace {
        &self.backtrace
    }
}

impl fmt::Display for CaseAssertionFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Required case did not match at {}.", self.origin)?;
        fmt::Display::fmt(&self.backtrace, f)
    }
}

impl std::error::Error for CaseAssertionFailure {}

/// Requires a matching case and returns its borrowed payload.
///
/// Calls `extract` exactly once, without implicit rendering or domain bounds.
/// An extractor from a typed state/action path can be supplied directly through
/// a closure. Missing optional or keyed child values also fail this check.
///
/// # Examples
/// ```
/// let root = Some(String::from("value"));
/// assert_eq!(rca_test::check_case(&root, Option::as_ref).unwrap(), "value");
/// ```
///
/// # Errors
/// Returns a case failure when extraction returns `None`, regardless of sinks.
///
/// # Panics
/// Propagates extraction panics.
#[track_caller]
pub fn check_case<'a, Root: ?Sized, Value: ?Sized>(
    root: &'a Root,
    extract: impl FnOnce(&'a Root) -> Option<&'a Value>,
) -> Result<&'a Value, CaseAssertionFailure> {
    let origin = Location::caller();
    extract(root).ok_or_else(|| CaseAssertionFailure {
        origin,
        backtrace: Backtrace::capture(),
    })
}

/// Requires a matching case before running a typed payload check.
///
/// Calls extraction once and, on a match only, calls `check` once. The outer
/// result reports case matching; the inner result retains the check's original
/// typed result. Neither domain nor user error requires `Debug`, `Clone`, `Send`
/// or `'static`. Semantic checks such as [`crate::check_equal`] can check the payload.
///
/// # Examples
/// ```
/// let checked = rca_test::check_case_with(
///     &Some(3),
///     Option::as_ref,
///     |value| Ok::<_, ()>(*value + 1),
/// );
/// assert_eq!(checked.unwrap(), Ok(4));
/// ```
///
/// # Errors
/// The outer result fails for a missing case; the inner retains any check error.
///
/// # Panics
/// Propagates extraction, payload-check and owned callback-destructor panics.
#[track_caller]
pub fn check_case_with<'a, Root: ?Sized, Value: ?Sized + 'a, R, E>(
    root: &'a Root,
    extract: impl FnOnce(&'a Root) -> Option<&'a Value>,
    check: impl FnOnce(&'a Value) -> Result<R, E>,
) -> Result<Result<R, E>, CaseAssertionFailure> {
    match check_case(root, extract) {
        Ok(value) => Ok(check(value)),
        Err(error) => {
            // Retain the captured failure while destroying the unused callback.
            drop(check);
            Err(error)
        }
    }
}
