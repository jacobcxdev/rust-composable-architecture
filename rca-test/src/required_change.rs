//! Provides an opt-in expected-change assertion for existing callbacks.

/// Requires an edit to change its selected expected value.
///
/// Composes with [`crate::TestStore::send_with`], [`crate::TestStore::receive_with`]
/// and [`crate::CaptureExpectations::update`]. This asserts only the wrapped root
/// snapshot or capture, not an implicit change in any other scope. Ordinary
/// unchanged-expectation defaults and operation-first [`crate::check_change`]
/// remain distinct. A changed expectation must still match production after commit.
/// A later mismatch retains assertion debt.
///
/// Each invocation adds one `Clone`, one edit, one `PartialEq::eq` call and
/// destruction of the saved clone before returning. The adapter itself allocates
/// nothing, but user implementations may allocate. Clones must own independent
/// history. Equality must lawfully compare that history. The adapter does not detect aliasing.
///
/// # Examples
/// ```
/// use rca_test::requiring_change;
///
/// let mut expected = vec![1, 2];
/// requiring_change(|values: &mut Vec<i32>| values.push(3))(&mut expected);
/// assert_eq!(expected, [1, 2, 3]);
/// ```
///
/// # Panics
/// Panics without formatting values when the edit leaves the value equal.
/// Propagates clone, edit, equality and saved-clone destructor panics. In `TestStore`
/// expectation callbacks these happen before send admission or receive release:
/// after successful preflight, the original receive action remains available.
/// Existing preflight debt remains sticky. Caller side effects are not rolled back.
pub fn requiring_change<T: Clone + PartialEq>(update: impl FnOnce(&mut T)) -> impl FnOnce(&mut T) {
    move |value| {
        let before = value.clone();
        update(value);
        assert!(!before.eq(value), "expected value did not change");
        drop(before);
    }
}
