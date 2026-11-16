//! Frozen subtree authority over the same ordered fold command.
//!
//! Views retain weak Store metadata and one captured scope generation.
//! Acquisition freezes authority without evaluating child-state selectors.
//! Reads resolve that authority against the current committed roster.

use super::{Fold, FoldCall, FoldKey, Read, closed, submit, walk};
use crate::store::carriers::{Carriers, Shared};
use crate::store::command::Command;
use crate::store::lifecycle::Lifecycle;
use crate::store::mount::{Acquisition, MountSite};
use crate::store::output::OutputRegistration;
use crate::store::scope::ScopeRegistry;
use crate::store::task_context::{ReplyGuard, TaskAccessError, TaskAccessReason, TaskProgress};
use std::fmt;
use std::pin::Pin;
use std::sync::{Arc, Weak};

/// Reads one captured subtree without retaining its callbacks or Store.
///
/// Replacement, configuration refresh or ancestor retirement invalidates this
/// view. Explicitly acquire another view to read a successor incarnation.
pub struct FoldView<K: FoldKey> {
    /// Erases root types while retaining frozen authority and weak Store metadata.
    endpoint: Arc<dyn Endpoint<K> + Send + Sync>,
}

impl<K: FoldKey> Clone for FoldView<K> {
    fn clone(&self) -> Self {
        Self {
            endpoint: Arc::clone(&self.endpoint),
        }
    }
}

impl<K: FoldKey> fmt::Debug for FoldView<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FoldView").finish_non_exhaustive()
    }
}

impl<K: FoldKey> FoldView<K> {
    #[cfg(feature = "test-util")]
    /// Checks the requested root and reads through the captured diagnostic gate.
    pub(in crate::store) fn read_for_test(
        &self,
        root: Arc<()>,
    ) -> impl Future<Output = Result<K::Value, TaskAccessError>> + Send + 'static + use<K> {
        self.endpoint.read_for_test(root)
    }

    /// Reads this captured subtree at an ordered actor position.
    ///
    /// Construction admits nothing. First polling attempts ordered admission.
    /// Blocking synchronously on this future from an actor callback is unsupported.
    /// The owned result moves without `Clone` or `Sync` requirements.
    ///
    /// # Errors
    /// Rejects stale authority before selection, or reports closure and actor failure.
    ///
    /// # Panics
    /// User callbacks can panic during actor execution.
    pub fn read(
        &self,
    ) -> impl Future<Output = Result<K::Value, TaskAccessError>> + Send + 'static + use<K> {
        self.endpoint.read()
    }
}

/// Erases root types while preserving the owned result and thread-transfer bound.
trait Endpoint<K: FoldKey> {
    /// Prepares a lazy read of this captured subtree.
    fn read(&self) -> Pin<Box<dyn Future<Output = Result<K::Value, TaskAccessError>> + Send>>;

    #[cfg(feature = "test-util")]
    /// Rejects a foreign root before using the captured test-driver gate.
    fn read_for_test(
        &self,
        root: Arc<()>,
    ) -> Pin<Box<dyn Future<Output = Result<K::Value, TaskAccessError>> + Send>>;
}

/// Reserves one declaration address and freezes its incarnation authority.
pub(super) struct Target {
    /// Keeps the allocation address reserved without retaining callback payloads.
    reservation: Weak<dyn OutputRegistration>,
    /// Locates that declaration in the actor's current committed roster.
    identity: usize,
    /// Rejects retirement and configuration changes before selecting any state.
    frozen: super::super::output::FrozenRegistration,
}

impl Clone for Target {
    fn clone(&self) -> Self {
        Self {
            reservation: Weak::clone(&self.reservation),
            identity: self.identity,
            frozen: super::super::output::FrozenRegistration {
                scope: self.frozen.scope,
                authority: self.frozen.authority.clone(),
            },
        }
    }
}

impl Target {
    /// Borrows the current subtree or rejects this stale captured authority.
    pub(super) fn select<'a, R, A, C: Carriers<R, A>>(
        &self,
        scopes: &ScopeRegistry,
        mounts: &'a [Box<C::Mount>],
    ) -> Result<&'a [Box<C::Mount>], TaskAccessError> {
        let _reservation = self.reservation.upgrade().ok_or_else(ended)?;
        walk::subtree::<R, A, C>(scopes, mounts, self.identity, &self.frozen)
    }
}

/// Retains one typed source behind the root-erased public view.
struct TypedEndpoint<R, A, K: FoldKey> {
    /// Callback lookup metadata without ownership of mounted captures.
    fold: Fold<R, K>,
    /// The captured declaration and authority, never retargeted to a successor.
    target: Target,
    /// Upgraded only while admitting a command.
    ingress: Weak<super::super::ingress::CommandIngress<Command<R, A>>>,
    /// Checks owner loss without keeping the Store alive.
    lifecycle: Weak<Lifecycle>,
    #[cfg(feature = "test-util")]
    /// Present only when acquisition uses an explicit test-driver gate.
    gate: Option<crate::testing::TransactionGateCore<R, A>>,
}

impl<R: Send + 'static, A: Send + 'static, K: FoldKey> Endpoint<K> for TypedEndpoint<R, A, K>
where
    K::Value: Send,
{
    fn read(&self) -> Pin<Box<dyn Future<Output = Result<K::Value, TaskAccessError>> + Send>> {
        let fold = self.fold.clone();
        let target = self.target.clone();
        Box::pin(submit(
            Weak::clone(&self.ingress),
            Weak::clone(&self.lifecycle),
            #[cfg(feature = "test-util")]
            None,
            move |reply| {
                Box::new(Read {
                    fold,
                    target: Some(target),
                    reply,
                    #[cfg(feature = "test-util")]
                    expected_root: None,
                })
            },
        ))
    }

    #[cfg(feature = "test-util")]
    fn read_for_test(
        &self,
        root: Arc<()>,
    ) -> Pin<Box<dyn Future<Output = Result<K::Value, TaskAccessError>> + Send>> {
        let fold = self.fold.clone();
        let target = self.target.clone();
        let ingress = Weak::clone(&self.ingress);
        let lifecycle = Weak::clone(&self.lifecycle);
        let gate = self.gate.clone();
        Box::pin(async move {
            let owner = lifecycle.upgrade().ok_or_else(closed)?;
            owner.access()?;
            if !Arc::ptr_eq(&root, &owner.root_identity()) {
                return Err(ended());
            }
            drop(owner);
            let gate = gate.ok_or_else(|| {
                TaskAccessError::new(TaskAccessReason::PhaseDenied, TaskProgress::NotStarted)
            })?;
            submit(ingress, lifecycle, Some(gate), move |reply| {
                Box::new(Read {
                    fold,
                    target: Some(target),
                    reply,
                    expected_root: Some(root),
                })
            })
            .await
        })
    }
}

/// Freezes one subtree at an ordered actor position without selecting child state.
struct Acquire<R, A, S, X, K: FoldKey> {
    /// The contribution channel used by the resulting view.
    fold: Fold<R, K>,
    /// Identifies the declaration without owning its callbacks or installed feature.
    source: Weak<dyn Acquisition<R, S, X, A>>,
    /// The weak command endpoint stored by a successfully acquired view.
    ingress: Weak<super::super::ingress::CommandIngress<Command<R, A>>>,
    /// Identifies the submitting owner for cross-Store rejection.
    lifecycle: Weak<Lifecycle>,
    /// Publishes absence, a frozen view or terminal authority failure.
    reply: ReplyGuard<Option<FoldView<K>>>,
    #[cfg(feature = "test-util")]
    /// Passed to the view only for diagnostic acquisition.
    gate: Option<crate::testing::TransactionGateCore<R, A>>,
}

impl<R: Send + 'static, A: Send + 'static, S: 'static, X: 'static, K: FoldKey>
    FoldCall<R, A, Shared> for Acquire<R, A, S, X, K>
where
    K::Value: Send,
{
    fn execute(
        self: Box<Self>,
        _: &R,
        scopes: &ScopeRegistry,
        _: &[Box<<Shared as Carriers<R, A>>::Mount>],
        lifecycle: &Lifecycle,
    ) {
        let Self {
            fold,
            source,
            ingress,
            lifecycle: weak_lifecycle,
            reply,
            #[cfg(feature = "test-util")]
            gate,
        } = *self;
        let result = (|| {
            lifecycle.access()?;
            let owner = weak_lifecycle.upgrade().ok_or_else(closed)?;
            if !Arc::ptr_eq(&owner.root_identity(), &lifecycle.root_identity()) {
                return Err(ended());
            }
            drop(owner);
            let registration = source.upgrade().ok_or_else(ended)?;
            let root = registration.root_identity().ok_or_else(ended)?;
            if !Arc::ptr_eq(&root, &lifecycle.root_identity()) {
                return Err(ended());
            }
            let Some(frozen) = registration.freeze(scopes) else {
                return Ok(None);
            };
            let target = Target {
                identity: source.as_ptr().cast::<()>() as usize,
                reservation: source,
                frozen,
            };
            Ok(Some(FoldView {
                endpoint: Arc::new(TypedEndpoint::<R, A, K> {
                    fold,
                    target,
                    ingress,
                    lifecycle: weak_lifecycle,
                    #[cfg(feature = "test-util")]
                    gate,
                }),
            }))
        })();
        reply.complete(result);
    }

    fn reject(self: Box<Self>, error: TaskAccessError) {
        self.reply.complete(Err(error));
    }
}

impl<R: Send + 'static, A: Send + 'static> super::super::Store<R, A> {
    /// Captures one installed subtree without evaluating contributions or selectors.
    ///
    /// Returns `None` for an absent installation in this Store. The view keeps
    /// fixed authority and cannot retarget a later incarnation.
    /// Construction is inert. First polling attempts ordered admission.
    ///
    /// # Errors
    /// Rejects another root, pre-readiness access, retired declarations and closed actors.
    pub fn acquire_fold<K: FoldKey, S: 'static, X: 'static>(
        &self,
        fold: &Fold<R, K>,
        site: &MountSite<R, S, X, A>,
    ) -> impl Future<Output = Result<Option<FoldView<K>>, TaskAccessError>>
    + Send
    + 'static
    + use<R, A, K, S, X>
    where
        K::Value: Send,
    {
        let fold = fold.clone();
        let source = site.fold_registration();
        let ingress = Arc::downgrade(&self.ingress);
        let lifecycle = Arc::downgrade(&self.lifecycle);
        submit(
            Weak::clone(&ingress),
            Weak::clone(&lifecycle),
            #[cfg(feature = "test-util")]
            None,
            move |reply| {
                Box::new(Acquire {
                    fold,
                    source,
                    ingress,
                    lifecycle,
                    reply,
                    #[cfg(feature = "test-util")]
                    gate: None,
                })
            },
        )
    }

    #[cfg(feature = "test-util")]
    /// Acquires through this test driver's gate and retains it for diagnostic reads.
    pub(crate) fn acquire_fold_for_test<K: FoldKey, S: 'static, X: 'static>(
        &self,
        fold: &Fold<R, K>,
        site: &MountSite<R, S, X, A>,
        gate: crate::testing::TransactionGateCore<R, A>,
    ) -> impl Future<Output = Result<Option<FoldView<K>>, TaskAccessError>>
    + Send
    + 'static
    + use<R, A, K, S, X>
    where
        K::Value: Send,
    {
        let fold = fold.clone();
        let source = site.fold_registration();
        let ingress = Arc::downgrade(&self.ingress);
        let lifecycle = Arc::downgrade(&self.lifecycle);
        submit(
            Weak::clone(&ingress),
            Weak::clone(&lifecycle),
            Some(gate.clone()),
            move |reply| {
                Box::new(Acquire {
                    fold,
                    source,
                    ingress,
                    lifecycle,
                    reply,
                    gate: Some(gate),
                })
            },
        )
    }

    #[cfg(feature = "test-util")]
    /// Reads a captured diagnostic view with this Store's root identity.
    pub(crate) fn fold_view_for_test<K: FoldKey>(
        &self,
        view: &FoldView<K>,
    ) -> impl Future<Output = Result<K::Value, TaskAccessError>> + Send + 'static + use<R, A, K>
    {
        view.read_for_test(self.lifecycle.root_identity())
    }
}

/// Reports stale or foreign subtree authority before any user callback starts.
fn ended() -> TaskAccessError {
    TaskAccessError::new(TaskAccessReason::ScopeEnded, TaskProgress::NotStarted)
}

#[cfg(test)]
ordinary_test_items! {
    // Prints a layout diagnostic, without asserting behavioural correctness.
    #[test]
    fn shared_endpoint_layout_records_diagnostic_metadata() {
        struct Key;

        impl FoldKey for Key {
            type Value = ();

            fn identity() {}

            fn reduce((): (), (): ()) -> std::ops::ControlFlow<(), ()> {
                std::ops::ControlFlow::Continue(())
            }
        }

        eprintln!(
            "Shared Fold typed endpoint bytes={} test_util={}",
            std::mem::size_of::<TypedEndpoint<(), (), Key>>(),
            cfg!(feature = "test-util")
        );
    }
}
