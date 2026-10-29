//! Opt-in root metadata and work leaves for owner-local removal.

use super::Ingress;
use crate::feature::Work;
use crate::local::{Effect, Task};
use crate::store::actor::Launches;
use crate::store::local::Local;
use crate::store::mounted::{InstalledNode, MountedWork, WorkOrigin};
use crate::store::output::{
    FrozenRegistration,
    local::{HandlerRegistration, HandlerRoute, OutputRegistration},
};
use crate::store::scope::{ScopeRegistry, ScopeToken, ScopeTransitionBatch};
use crate::store::transaction::TransactionParts;
use crate::store::{lifecycle::Lifecycle, mount::MountDeclaration};
use std::cell::{OnceCell, RefCell};
use std::rc::Rc;
use std::sync::{Arc, Weak};

/// Records root authority without retaining the store or installing another scope.
pub(in crate::store) struct RootRegistration<R, A> {
    /// Weak transport bound once when the owning declaration enters a store.
    binding: OnceCell<Weak<Ingress<R, A>>>,
    /// Root identity and token, present only while the declaration remains installed.
    installed: RefCell<Option<(Arc<()>, ScopeToken)>>,
}

impl<R, A> RootRegistration<R, A> {
    /// Creates unbound metadata for the root declaration and its public sites.
    pub(in crate::store) fn new() -> Rc<Self> {
        Rc::new(Self {
            binding: OnceCell::new(),
            installed: RefCell::new(None),
        })
    }

    /// Returns the bound transport, or an empty weak handle before installation.
    pub(super) fn ingress(&self) -> Weak<Ingress<R, A>> {
        self.binding.get().map_or_else(Weak::new, Weak::clone)
    }
}

/// Clears installed authority when the actor releases its root declaration.
pub(in crate::store) struct RootOwner<R, A>(pub(in crate::store) Rc<RootRegistration<R, A>>);

impl<R, A> Drop for RootOwner<R, A> {
    fn drop(&mut self) {
        *self.0.installed.borrow_mut() = None;
    }
}

impl<R: 'static, A: 'static> OutputRegistration for RootRegistration<R, A> {
    fn installed(&self) -> Option<(Arc<()>, ScopeToken)> {
        self.installed.borrow().clone()
    }
}

impl<R: 'static, A: 'static> HandlerRegistration<R, R, A, A> for RootRegistration<R, A> {
    fn freeze_handler_route(
        &self,
        scopes: &ScopeRegistry,
    ) -> Option<Box<dyn HandlerRoute<R, A, R, A>>> {
        // Removal validation already matched this actor-owned root registration.
        Some(Box::new(RootRoute(FrozenRegistration {
            scope: scopes.root(),
            authority: scopes.authority(scopes.root()),
        })))
    }
}

/// Selects the entire root using authority frozen for one removal turn.
struct RootRoute(FrozenRegistration);

impl<R: 'static, A: 'static> HandlerRoute<R, A, R, A> for RootRoute {
    fn registration(&self) -> &FrozenRegistration {
        &self.0
    }

    #[cfg(all(test, feature = "test-util"))]
    fn read<'a>(&self, root: &'a R) -> Option<&'a R> {
        Some(root)
    }

    fn select<'a>(&self, root: &'a mut R) -> Option<&'a mut R> {
        Some(root)
    }

    fn into_work(
        self: Box<Self>,
        work: Work<Effect<A>, Task<R, A>>,
    ) -> Box<dyn MountedWork<R, A, Local>> {
        Box::new(RootWork(work))
    }
}

/// Registers owner-local follow-up work under the surviving root's authority.
struct RootWork<R, A>(Work<Effect<A>, Task<R, A>>);

impl<R: 'static, A: 'static> MountedWork<R, A, Local> for RootWork<R, A> {
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn prune_scopes(&mut self, _: &dyn Fn(ScopeToken) -> bool) {
        // Local effects have no intrinsic scopes; the surviving root owns this work.
    }

    fn register(
        self: Box<Self>,
        parts: &mut TransactionParts<'_, R, A, Local>,
        origin: WorkOrigin<'_>,
        launches: &mut Launches<crate::local::RuntimeFuture>,
    ) {
        crate::store::transaction::register_root_work(self.0, parts, origin, launches);
    }
}

impl<R: 'static, A: 'static> MountDeclaration<R, A, Local> for RootOwner<R, A> {
    fn bind(&self, ingress: &Arc<Ingress<R, A>>, _: &Arc<Lifecycle>) {
        assert!(
            self.0.binding.set(Arc::downgrade(ingress)).is_ok(),
            "root metadata binds once"
        );
    }

    fn reconcile(
        &self,
        _: &R,
        scopes: &mut ScopeRegistry,
        _: &mut ScopeTransitionBatch,
        _: &mut Vec<Box<dyn InstalledNode>>,
        _: ScopeToken,
        _: &mut Vec<Box<dyn MountDeclaration<R, A, Local>>>,
        _: &mut crate::store::mount::nested::Descendants<'_, R, A, Local>,
        #[cfg(feature = "diagnostics")] _: Option<crate::diagnostics::LifecycleCall<'_>>,
    ) {
        self.0
            .installed
            .borrow_mut()
            .get_or_insert_with(|| (scopes.root_identity(), scopes.root()));
    }
}
