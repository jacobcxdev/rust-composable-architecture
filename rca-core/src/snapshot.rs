//! Captures independent historical values without retaining live state authority.
//!
//! [`Snapshot`] is an explicit value projection, not a blanket deep-clone promise.
//! Shared or cyclic graphs require an application-defined finite owned projection.
//! Capturing does not register observers or run implicitly at production commits.

use crate::{KeyedState, MonotonicInstant};

/// Captures values independent of later source and expected-history mutations.
///
/// Implementations must preserve independence in both directions: later writes to
/// the source or its shared aliases must not alter captured history, and editing
/// history must not mutate the source, other aliases, subscriptions or lifetime
/// authority. Capturing reads values; it must not retain an operation handle.
/// Rust cannot prove this semantic contract for arbitrary custom implementations.
/// Consumers that clone history also require those clones to remain independent.
///
/// The trait imposes no consumer-specific `Clone`, `Debug`, equality, `Send`,
/// `Sync` or `'static` bounds. A manual implementation may capture a borrowed or
/// local source into an owned value. Fallible reads and consistency across
/// independently changing resources remain the caller's explicit policy; this
/// interface does not create an atomic transaction or hide capture errors.
///
/// Scalar implementations cover primitive numbers, `bool`, `char`, `()`,
/// [`Duration`](std::time::Duration) and [`MonotonicInstant`]. `String` copies its text.
/// `Option`, `Result`, `Vec`, arrays, `Box` and tuples of one to four elements
/// preserve their shape and capture each contained value. [`KeyedState`] captures
/// an ordered `Vec` of identifier and state values, not a live collection.
///
/// References, shared handles and interior-mutable wrappers have no implicit
/// implementation, even when they implement `Clone`. Use a manual projection to
/// read their values and explicitly choose omitted or retained information.
///
/// # Examples
///
/// ```
/// use rca_core::Snapshot;
///
/// struct Progress<'a>(&'a str);
/// impl Snapshot for Progress<'_> {
///     type Value = String;
///
///     fn snapshot(&self) -> String {
///         self.0.to_owned()
///     }
/// }
/// let history = {
///     let local = String::from("imported");
///     Progress(&local).snapshot()
/// };
/// assert_eq!(history, "imported");
/// ```
pub trait Snapshot {
    /// Stores the explicitly selected historical values.
    type Value;

    /// Captures an independent value without retaining mutation or lifetime authority.
    ///
    /// # Panics
    ///
    /// Container implementations propagate panics from element capture. During
    /// unwinding, they drop completed values instead of returning partial history.
    /// Custom capture side effects are not rolled back.
    fn snapshot(&self) -> Self::Value;
}

/// Implements capture by copying an explicit set of independent scalar values.
macro_rules! scalar_snapshots {
    ($($ty:ty),+ $(,)?) => {
        $(impl Snapshot for $ty {
            type Value = Self;

            fn snapshot(&self) -> Self::Value {
                *self
            }
        })+
    };
}

scalar_snapshots!(
    (),
    bool,
    char,
    u8,
    u16,
    u32,
    u64,
    u128,
    usize,
    i8,
    i16,
    i32,
    i64,
    i128,
    isize,
    f32,
    f64,
    std::time::Duration,
    MonotonicInstant,
);

impl Snapshot for String {
    type Value = Self;

    fn snapshot(&self) -> Self::Value {
        self.clone()
    }
}

impl<T: Snapshot> Snapshot for Option<T> {
    type Value = Option<T::Value>;

    fn snapshot(&self) -> Self::Value {
        self.as_ref().map(Snapshot::snapshot)
    }
}

impl<T: Snapshot, E: Snapshot> Snapshot for Result<T, E> {
    type Value = Result<T::Value, E::Value>;

    fn snapshot(&self) -> Self::Value {
        match self {
            Ok(value) => Ok(value.snapshot()),
            Err(error) => Err(error.snapshot()),
        }
    }
}

impl<T: Snapshot> Snapshot for Vec<T> {
    type Value = Vec<T::Value>;

    fn snapshot(&self) -> Self::Value {
        self.iter().map(Snapshot::snapshot).collect()
    }
}

impl<T: Snapshot, const N: usize> Snapshot for [T; N] {
    type Value = [T::Value; N];

    fn snapshot(&self) -> Self::Value {
        std::array::from_fn(|index| self[index].snapshot())
    }
}

impl<T: Snapshot> Snapshot for Box<T> {
    type Value = Box<T::Value>;

    fn snapshot(&self) -> Self::Value {
        Box::new((**self).snapshot())
    }
}

/// Preserves tuple positions while capturing each element without extra trait bounds.
macro_rules! tuple_snapshots {
    ($($ty:ident : $index:tt),+ $(,)?) => {
        impl<$($ty: Snapshot),+> Snapshot for ($($ty,)+) {
            type Value = ($($ty::Value,)+);

            fn snapshot(&self) -> Self::Value {
                ($(self.$index.snapshot(),)+)
            }
        }
    };
}

tuple_snapshots!(A: 0);
tuple_snapshots!(A: 0, B: 1);
tuple_snapshots!(A: 0, B: 1, C: 2);
tuple_snapshots!(A: 0, B: 1, C: 2, D: 3);

/// Captures an ordered value roster, not a reconstructable live keyed collection.
///
/// Every entry remains in order, including entries whose projected identifiers
/// coincide. No collection lineage, incarnation, structural revision or scope
/// authority is retained. Identifier and state projections must independently
/// satisfy [`Snapshot`]'s semantic contract.
impl<Id: Snapshot, State: Snapshot> Snapshot for KeyedState<Id, State> {
    type Value = Vec<(Id::Value, State::Value)>;

    fn snapshot(&self) -> Self::Value {
        self.iter()
            .map(|(id, state)| (id.snapshot(), state.snapshot()))
            .collect()
    }
}
