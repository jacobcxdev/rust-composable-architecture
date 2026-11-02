//! Opt-in metadata for the actual root, without another installed scope.

use crate::effect::leaves::EffectValue as _;
use crate::feature::Work;
use crate::store::actor::Launches;
use crate::store::carriers::Shared;
use crate::store::mounted::{InstalledNode, MountedWork, WorkOrigin};
use crate::store::output::{
    FrozenRegistration, HandlerRegistration, HandlerRoute, OutputRegistration,
};
use crate::store::scope::{ScopeRegistry, ScopeToken, ScopeTransitionBatch};
use crate::store::transaction::TransactionParts;
use crate::store::{ingress::Ingress, lifecycle::Lifecycle, mount::MountDeclaration};
use crate::{Effect, Task};
use parking_lot::Mutex;
use std::sync::{Arc, OnceLock, Weak};

/// Records root authority without retaining the store or installing another scope.
pub(in crate::store) struct RootRegistration<R, A> {
    /// Weak transport bound once when the owning declaration enters a store.
    binding: OnceLock<Weak<Ingress<R, A>>>,
    /// Root identity and token, present only while the declaration remains installed.
    installed: Mutex<Option<(Arc<()>, ScopeToken)>>,
}

impl<R, A> RootRegistration<R, A> {
    /// Creates unbound metadata for the root declaration and its public sites.
    pub(in crate::store) fn new() -> Arc<Self> {
        Arc::new(Self {
            binding: OnceLock::new(),
            installed: Mutex::new(None),
        })
    }

    /// Returns the bound transport, or an empty weak handle before installation.
    pub(super) fn ingress(&self) -> Weak<Ingress<R, A>> {
        self.binding.get().map_or_else(Weak::new, Weak::clone)
    }
}

/// Clears installed authority when the actor releases its root declaration.
pub(in crate::store) struct RootOwner<R, A>(pub(in crate::store) Arc<RootRegistration<R, A>>);

impl<R, A> Drop for RootOwner<R, A> {
    fn drop(&mut self) {
        *self.0.installed.lock() = None;
    }
}

impl<R: Send + 'static, A: Send + 'static> OutputRegistration for RootRegistration<R, A> {
    fn installed(&self) -> Option<(Arc<()>, ScopeToken)> {
        self.installed.lock().clone()
    }
}

impl<R: Send + 'static, A: Send + 'static> HandlerRegistration<R, R, A, A>
    for RootRegistration<R, A>
{
    fn freeze_handler_route(
        &self,
        scopes: &ScopeRegistry,
    ) -> Option<Box<dyn HandlerRoute<R, A, R, A> + Send>> {
        // Removal validation already matched this actor-owned root registration.
        Some(Box::new(RootRoute(FrozenRegistration {
            scope: scopes.root(),
            authority: scopes.authority(scopes.root()),
        })))
    }
}

/// Selects the entire root using authority frozen for one removal turn.
struct RootRoute(FrozenRegistration);

impl<R: Send + 'static, A: Send + 'static> HandlerRoute<R, A, R, A> for RootRoute {
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
    ) -> Box<dyn MountedWork<R, A, Shared> + Send> {
        Box::new(RootWork(work))
    }
}

/// Registers removal follow-up work under the surviving root's authority.
struct RootWork<R, A>(Work<Effect<A>, Task<R, A>>);

impl<R: Send + 'static, A: Send + 'static> MountedWork<R, A, Shared> for RootWork<R, A> {
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn prune_scopes(&mut self, current: &dyn Fn(ScopeToken) -> bool) {
        self.0.for_each_effect(|effect| {
            effect.prune_scopes(&current);
        });
    }

    fn register(
        self: Box<Self>,
        parts: &mut TransactionParts<'_, R, A>,
        origin: WorkOrigin<'_>,
        launches: &mut Launches<crate::RuntimeFuture>,
    ) {
        super::super::transaction::register_root_work(self.0, parts, origin, launches);
    }
}

impl<R: Send + 'static, A: Send + 'static> MountDeclaration<R, A> for RootOwner<R, A> {
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
        _: &mut Vec<Box<dyn InstalledNode + Send>>,
        _: ScopeToken,
        _: &mut Vec<Box<dyn MountDeclaration<R, A> + Send>>,
        _: &mut crate::store::mount::nested::Descendants<'_, R, A, Shared>,
        #[cfg(feature = "diagnostics")] _: Option<crate::diagnostics::LifecycleCall<'_>>,
    ) {
        self.0
            .installed
            .lock()
            .get_or_insert_with(|| (scopes.root_identity(), scopes.root()));
    }
}
