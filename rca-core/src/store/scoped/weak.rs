//! Weak ownership of an already captured scoped route.

use std::fmt;
use std::sync::{Arc, Weak};

use super::{ScopedDriver, ScopedStore};

/// A non-owning reference to one already captured scoped Store route.
///
/// Upgrading never registers a new child or looks up a logical identity. The
/// resulting route keeps its original projection and, for registered children,
/// its original generation. That route rejects actions after its generation
/// ends. The last strong scoped owner ends upgradeability, even while another
/// owner keeps the root Store alive.
pub struct WeakScopedStore<State, Action> {
    /// Refers to the captured driver without keeping its root or child alive.
    driver: Weak<dyn ScopedDriver<State, Action>>,
}

impl<State, Action> ScopedStore<State, Action> {
    /// Borrows this exact captured route without retaining its Store driver.
    #[must_use]
    pub fn downgrade(&self) -> WeakScopedStore<State, Action> {
        WeakScopedStore {
            driver: Arc::downgrade(&self.driver),
        }
    }
}

impl<State, Action> WeakScopedStore<State, Action> {
    /// Retains the original route while at least one strong route owner lives.
    ///
    /// Success does not imply that the root or registered child lifetime is
    /// current. The Store still checks authority when accepting and committing.
    #[must_use]
    pub fn upgrade(&self) -> Option<ScopedStore<State, Action>> {
        self.driver.upgrade().map(ScopedStore::new)
    }
}

impl<State, Action> Clone for WeakScopedStore<State, Action> {
    fn clone(&self) -> Self {
        Self {
            driver: Weak::clone(&self.driver),
        }
    }
}

impl<State, Action> fmt::Debug for WeakScopedStore<State, Action> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WeakScopedStore").finish_non_exhaustive()
    }
}
