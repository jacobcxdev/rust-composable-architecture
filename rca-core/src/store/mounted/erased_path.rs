//! Opt-in shared ownership for nested route metadata.

use super::path::BorrowPath;
use std::sync::Arc;

/// Erases route types while each owning wrapper handles cloning.
///
/// The owner handles cloning. These methods preserve the underlying path's
/// selection and structural liveness boundaries without adding another check.
trait StateRoute<R, S> {
    /// Forwards ordinary shared selection for the root borrow.
    fn read<'a>(&self, root: &'a R) -> Option<&'a S>;

    /// Forwards structural selection and its between-operation liveness checks.
    fn read_structural<'a>(&self, root: &'a R, live: &dyn Fn() -> bool) -> Option<&'a S>;

    /// Forwards exclusive selection without structural liveness checks.
    fn write<'a>(&self, root: &'a mut R) -> Option<&'a mut S>;
}

impl<R, S, P: BorrowPath<R, State = S>> StateRoute<R, S> for P {
    fn read<'a>(&self, root: &'a R) -> Option<&'a S> {
        BorrowPath::read(self, root)
    }

    fn read_structural<'a>(&self, root: &'a R, live: &dyn Fn() -> bool) -> Option<&'a S> {
        BorrowPath::read_structural(self, root, live)
    }

    fn write<'a>(&self, root: &'a mut R) -> Option<&'a mut S> {
        BorrowPath::write(self, root)
    }
}

/// Shares one route allocation across endpoints without cloning its payload.
///
/// Each access still borrows the supplied root. Sharing metadata does not acquire
/// a Store or installed Feature. The wrapped path determines key ownership.
/// Thread-safety bounds apply to that path, not to `R` or `S` here.
pub(in crate::store) struct SharedPath<R, S>(
    /// Metadata owner, released when the last cloned path is dropped.
    Arc<dyn StateRoute<R, S> + Send + Sync>,
);

impl<R, S> SharedPath<R, S> {
    /// Allocates one metadata owner without selecting state or cloning the path.
    pub(in crate::store) fn new<P: BorrowPath<R, State = S> + Send + Sync + 'static>(
        path: P,
    ) -> Self {
        Self(Arc::new(path))
    }
}

impl<R, S> Clone for SharedPath<R, S> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<R, S> BorrowPath<R> for SharedPath<R, S> {
    type State = S;

    fn read<'a>(&self, root: &'a R) -> Option<&'a S> {
        self.0.read(root)
    }

    fn read_structural<'a>(&self, root: &'a R, live: &dyn Fn() -> bool) -> Option<&'a S> {
        self.0.read_structural(root, live)
    }

    fn write<'a>(&self, root: &'a mut R) -> Option<&'a mut S> {
        self.0.write(root)
    }
}

/// Shares route metadata on one thread without `Send` or `Sync` bounds.
///
/// Clones retain the same payload, not the selected state or installed Feature.
/// The final path owner releases that payload through ordinary `Rc` destruction.
#[cfg(feature = "local")]
pub(in crate::store) struct LocalPath<R, S>(
    /// Non-atomic metadata owner for local endpoints and owned requests.
    std::rc::Rc<dyn StateRoute<R, S>>,
);

#[cfg(feature = "local")]
impl<R, S> LocalPath<R, S> {
    /// Allocates one local metadata owner without selecting state or cloning the path.
    pub(in crate::store) fn new<P: BorrowPath<R, State = S> + 'static>(path: P) -> Self {
        Self(std::rc::Rc::new(path))
    }
}

#[cfg(feature = "local")]
impl<R, S> Clone for LocalPath<R, S> {
    fn clone(&self) -> Self {
        Self(std::rc::Rc::clone(&self.0))
    }
}

#[cfg(feature = "local")]
impl<R, S> BorrowPath<R> for LocalPath<R, S> {
    type State = S;

    fn read<'a>(&self, root: &'a R) -> Option<&'a S> {
        self.0.read(root)
    }

    fn read_structural<'a>(&self, root: &'a R, live: &dyn Fn() -> bool) -> Option<&'a S> {
        self.0.read_structural(root, live)
    }

    fn write<'a>(&self, root: &'a mut R) -> Option<&'a mut S> {
        self.0.write(root)
    }
}
