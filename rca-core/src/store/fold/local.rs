//! Owner-local callback storage for the common committed-roster fold.
//!
//! Local entries use `Rc` and `RefCell`, without `Send` or `Sync` requirements.
//! The walker borrows callback payloads without holding metadata guards.
//! The same ordered traversal and authority checks apply to both domains.

#[cfg(test)]
ordinary_test_items! {
    mod borrowed_tests {
        use crate::store::mount::local::{Acquisition, commands::Registration as CommandRegistration};
        use crate::store::output::local::{HandlerRegistration, OutputRegistration};
        use std::rc::Rc as Strong;
        use std::cell::RefCell as Storage;

        type Route = Box<dyn crate::store::output::local::HandlerRoute<usize, (), usize, ()>>;
        type CommandRoute = Strong<dyn crate::store::mount::local::commands::Dispatch<usize, (), ()>>;
        type AcquireFuture = std::pin::Pin<
            Box<
                dyn std::future::Future<
                    Output = Result<Option<crate::local::TaskContext<usize, ()>>, crate::TaskAccessError>,
                >,
            >,
        >;
        #[cfg(feature = "test-util")]
        type Gate = crate::testing::TransactionGateCore<usize, (), super::Local>;

        fn retain(registration: &Strong<Registration>) {
            *registration.held.borrow_mut() = Some(Strong::clone(registration));
        }

        fn release(registration: &Registration) {
            drop(registration.held.borrow_mut().take());
        }

        fn inspect_storage<T, R>(storage: &Storage<T>, inspect: impl FnOnce(&mut T) -> R) -> R {
            let mut guard = storage.try_borrow_mut().expect("metadata borrow released");
            inspect(&mut guard)
        }

        super::super::borrowed_tests::cases!();
    }
}

use super::{CallbackLease, FoldCall, FoldKey, PureEntry, closed, walk};
use crate::store::carriers::Carriers;
use crate::store::command::Command;
use crate::store::ingress::CommandIngress;
use crate::store::lifecycle::Lifecycle;
use crate::store::local::Local;
use crate::store::mount::{
    MountDeclaration,
    local::{Acquisition, LocalMountSite},
};
use crate::store::output::{FrozenRegistration, local::OutputRegistration};
use crate::store::scope::{ScopeRegistry, ScopeToken, ScopeTransitionBatch};
use crate::store::task_context::{ReplyGuard, TaskAccessError, TaskAccessReason, TaskProgress};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt;
use std::pin::Pin;
use std::rc::{Rc, Weak};
use std::sync::{Arc, Weak as SharedWeak};

/// Names owner-local typed contributions without retaining callbacks or Store.
///
/// The common actor's committed mount roster supplies membership and order.
/// A channel can name contributions in several local Stores. Each read visits
/// only its own Store's roster. Cloning the channel does not clone callbacks.
pub struct Fold<R, K: FoldKey> {
    /// Weak callback locations, keyed by reserved declaration allocation addresses.
    index: Rc<RefCell<BTreeMap<usize, Dispatch<R, K>>>>,
}

impl<R, K: FoldKey> Fold<R, K> {
    /// Creates an empty owner-local contribution channel.
    #[must_use]
    pub fn new() -> Self {
        Self {
            index: Rc::new(RefCell::new(BTreeMap::new())),
        }
    }
}

impl<R, K: FoldKey> Default for Fold<R, K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<R, K: FoldKey> Clone for Fold<R, K> {
    fn clone(&self) -> Self {
        Self {
            index: Rc::clone(&self.index),
        }
    }
}

impl<R, K: FoldKey> fmt::Debug for Fold<R, K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Fold").finish_non_exhaustive()
    }
}

/// Locates callbacks without owning their captures or reusing a live address.
struct Dispatch<R, K: FoldKey> {
    /// Reserves the declaration allocation address until this entry is removed.
    _source: Weak<dyn OutputRegistration>,
    /// Upgrades while the declaration or a callback lease owns the cell.
    cell: Weak<EntryCell<R, K>>,
}

/// Separates callback ownership from the index's temporary borrow.
struct EntryCell<R, K: FoldKey> {
    /// Absent during a callback lease or after declaration retirement.
    payload: RefCell<Option<Box<dyn PureEntry<R, K>>>>,
}

/// Borrows child state and returns an owned, possibly non-`Send` value.
type Contribution<S, V> = Box<dyn Fn(&S) -> V>;

/// Consumes one declared subtree's owned accumulator on the owner thread.
type Transform<V> = Box<dyn Fn(V) -> V>;

/// Keeps owner-local child projection and callbacks typed until traversal.
struct TypedEntry<R, A, S, X, K: FoldKey> {
    /// The actor's committed roster keeps this projection alive during the walk.
    registration: Weak<dyn Acquisition<R, S, X, A>>,
    /// Absent for a transform-only group, which adds no identity contribution.
    contribution: Option<Contribution<S, K::Value>>,
    /// Present only for a declared subtree group.
    transform: Option<Transform<K::Value>>,
}

impl<R, A, S, X, K: FoldKey> PureEntry<R, K> for TypedEntry<R, A, S, X, K> {
    fn contribution(
        &self,
        root: &R,
        lifecycle: &Lifecycle,
    ) -> Result<Option<K::Value>, TaskAccessError> {
        let Some(contribute) = &self.contribution else {
            return Ok(None);
        };
        let state = {
            let site = self
                .registration
                .upgrade()
                .expect("fold roster owns the committed declaration");
            site.read_committed(root, &|| lifecycle.access().is_ok())
        };
        walk::live(lifecycle)?;
        let state = state.ok_or_else(|| {
            TaskAccessError::new(TaskAccessReason::ScopeEnded, TaskProgress::MayHaveRun)
        })?;
        Ok(Some(contribute(state)))
    }

    fn has_transform(&self) -> bool {
        self.transform.is_some()
    }

    fn transform(&self, value: K::Value) -> K::Value {
        self.transform.as_ref().expect("explicit transform")(value)
    }
}

/// Owns one incarnation's callback bundle and its index entry.
struct EntryOwner<R, K: FoldKey> {
    /// Owns captures except while the synchronous walker borrows them.
    cell: Rc<EntryCell<R, K>>,
    /// Does not keep a discarded fold channel alive.
    index: Weak<RefCell<BTreeMap<usize, Dispatch<R, K>>>>,
    /// The declaration address reserved by the weak index registration.
    identity: usize,
}

impl<R, K: FoldKey> EntryOwner<R, K> {
    /// Removes dispatch authority before dropping captures outside metadata borrows.
    fn drain(&self) {
        let payload = self.cell.payload.borrow_mut().take();
        if let Some(index) = self.index.upgrade() {
            // This sole owner removes its reserved entry once, before user captures drop.
            index.borrow_mut().remove(&self.identity);
        }
        drop(payload);
    }
}

impl<R, K: FoldKey> Drop for EntryOwner<R, K> {
    fn drop(&mut self) {
        self.drain();
    }
}

impl<R, A, K: FoldKey> MountDeclaration<R, A, Local> for EntryOwner<R, K> {
    // This owner participates in declaration teardown only. Its mounted site
    // supplies scope, identity and preorder position to the common walker.
    fn bind(&self, _: &Arc<CommandIngress<Command<R, A, Local>>>, _: &Arc<Lifecycle>) {}
    fn reconcile(
        &self,
        _: &R,
        _: &mut ScopeRegistry,
        _: &mut ScopeTransitionBatch,
        _: &mut Vec<Box<<Local as Carriers<R, A>>::Node>>,
        _: ScopeToken,
        _: &mut Vec<Box<<Local as Carriers<R, A>>::Mount>>,
        _: &mut crate::store::mount::nested::Descendants<'_, R, A, Local>,
        #[cfg(feature = "diagnostics")] _: Option<crate::diagnostics::LifecycleCall<'_>>,
    ) {
    }
}

impl<R: 'static, K: FoldKey> Fold<R, K> {
    /// Installs one owner-local callback bundle per channel and declaration.
    ///
    /// Duplicate declarations panic after the index borrow is released.
    pub(in crate::store) fn attach<A: 'static, S: 'static, X: 'static>(
        &self,
        site: &LocalMountSite<R, S, X, A>,
        contribution: Option<Contribution<S, K::Value>>,
        transform: Option<Transform<K::Value>>,
    ) -> impl MountDeclaration<R, A, Local> + 'static {
        let registration = site.fold_registration();
        let identity = registration.as_ptr().cast::<()>() as usize;
        let cell = Rc::new(EntryCell {
            payload: RefCell::new(Some(Box::new(TypedEntry::<_, _, _, _, K> {
                registration,
                contribution,
                transform,
            }))),
        });
        {
            let mut index = self.index.borrow_mut();
            if index.contains_key(&identity) {
                drop(index);
                panic!("one fold declaration per channel and incarnation");
            }
            index.insert(
                identity,
                Dispatch {
                    _source: site.output_registration(),
                    cell: Rc::downgrade(&cell),
                },
            )
        };
        EntryOwner {
            cell,
            index: Rc::downgrade(&self.index),
            identity,
        }
    }

    /// Detaches callbacks for a serial walk that already owns the committed roster.
    fn acquire(&self, identity: usize) -> Option<EntryLease<R, K>> {
        // EntryOwner removes its index entry before releasing the strong cell.
        let cell = self
            .index
            .borrow()
            .get(&identity)?
            .cell
            .upgrade()
            .expect("indexed fold entry has a live owner");
        let payload = cell
            .payload
            .borrow_mut()
            .take()
            .expect("serial fold cannot re-enter a leased callback");
        Some(EntryLease {
            cell,
            payload: Some(payload),
        })
    }
}

/// Holds callbacks outside metadata borrows until normal return or unwind.
struct EntryLease<R, K: FoldKey> {
    /// Keeps storage alive until the lease returns its callbacks.
    cell: Rc<EntryCell<R, K>>,
    /// Present until drop restores the original callback bundle.
    payload: Option<Box<dyn PureEntry<R, K>>>,
}

impl<R, K: FoldKey> CallbackLease<R, K> for EntryLease<R, K> {
    fn entry(&self) -> &dyn PureEntry<R, K> {
        self.payload.as_deref().unwrap()
    }
}

impl<R, K: FoldKey> Drop for EntryLease<R, K> {
    fn drop(&mut self) {
        // Restore before actor teardown drains the owner. If retirement already
        // removed that owner, this lease releases the final strong cell.
        let mut destination = self.cell.payload.borrow_mut();
        std::mem::swap(&mut *destination, &mut self.payload);
        drop(destination);
        drop(self.payload.take());
    }
}

/// Carries a whole-root or captured-subtree read on the owner thread.
struct Read<R, K: FoldKey> {
    /// Callback lookup metadata without ownership of mounted captures.
    fold: Fold<R, K>,
    /// Absent for the root, frozen to one incarnation for a subtree.
    target: Option<Target>,
    /// Publishes a conservative terminal error if execution unwinds.
    reply: ReplyGuard<K::Value>,
    #[cfg(feature = "test-util")]
    /// Rejects commands submitted through a different Store's test gate.
    expected_root: Option<Arc<()>>,
}

impl<R: 'static, A, K: FoldKey> FoldCall<R, A, Local> for Read<R, K> {
    fn execute(
        mut self: Box<Self>,
        root: &R,
        scopes: &ScopeRegistry,
        mounts: &[Box<<Local as Carriers<R, A>>::Mount>],
        lifecycle: &Lifecycle,
    ) {
        if let Err(error) = lifecycle.access() {
            self.reply.complete(Err(error));
            return;
        }
        #[cfg(feature = "test-util")]
        if self
            .expected_root
            .as_ref()
            .is_some_and(|root| !Arc::ptr_eq(root, &lifecycle.root_identity()))
        {
            self.reply.complete(Err(ended()));
            return;
        }
        let mounts = match self
            .target
            .as_ref()
            .map(|target| target.select::<R, A>(scopes, mounts))
            .transpose()
        {
            Ok(Some(mounts)) => mounts,
            Ok(None) => mounts,
            Err(error) => {
                self.reply.complete(Err(error));
                return;
            }
        };
        self.reply.mark_entered();
        let result = walk::walk::<R, A, K, Local, _>(root, scopes, mounts, lifecycle, |identity| {
            self.fold.acquire(identity)
        });
        self.reply.complete(result);
    }

    fn reject(self: Box<Self>, error: TaskAccessError) {
        self.reply.complete(Err(error));
    }
}

/// The owner-local actor mailbox, without a thread-transfer requirement on commands.
type Ingress<R, A> = CommandIngress<Command<R, A, Local>>;

/// Prepares a lazy whole-root read without retaining Store ownership.
pub(in crate::store) fn read<R: 'static, A: 'static, K: FoldKey>(
    fold: Fold<R, K>,
    ingress: SharedWeak<Ingress<R, A>>,
    lifecycle: SharedWeak<Lifecycle>,
) -> impl Future<Output = Result<K::Value, TaskAccessError>> + 'static {
    submit(
        ingress,
        lifecycle,
        #[cfg(feature = "test-util")]
        None,
        move |reply| {
            Box::new(Read {
                fold,
                target: None,
                reply,
                #[cfg(feature = "test-util")]
                expected_root: None,
            })
        },
    )
}

#[cfg(feature = "test-util")]
/// Prepares a gated diagnostic read tied to the supplied root identity.
pub(in crate::store) fn read_for_test<R: 'static, A: 'static, K: FoldKey>(
    fold: Fold<R, K>,
    ingress: SharedWeak<Ingress<R, A>>,
    lifecycle: SharedWeak<Lifecycle>,
    expected_root: Arc<()>,
    gate: crate::testing::TransactionGateCore<R, A, Local>,
) -> impl Future<Output = Result<K::Value, TaskAccessError>> + 'static {
    submit(ingress, lifecycle, Some(gate), move |reply| {
        Box::new(Read {
            fold,
            target: None,
            reply,
            expected_root: Some(expected_root),
        })
    })
}

/// Checks weak authority, admits a local command and awaits its guarded reply.
async fn submit<R: 'static, A: 'static, T: 'static>(
    ingress: SharedWeak<Ingress<R, A>>,
    lifecycle: SharedWeak<Lifecycle>,
    #[cfg(feature = "test-util")] gate: Option<crate::testing::TransactionGateCore<R, A, Local>>,
    make: impl FnOnce(ReplyGuard<T>) -> Box<dyn FoldCall<R, A, Local>>,
) -> Result<T, TaskAccessError> {
    let root = lifecycle.upgrade().ok_or_else(closed)?;
    root.access()?;
    drop(root);
    let (reply, receive) = ReplyGuard::channel();
    let call = make(reply);
    #[cfg(feature = "test-util")]
    if let Some((gate, boundary)) =
        gate.and_then(|gate| gate.boundary().map(|boundary| (gate, boundary)))
    {
        gate.fold(boundary, call).await;
        return receive
            .await
            .expect("owned reply guard publishes a terminal result");
    }
    // The open Store owns ingress throughout this uninterrupted owner-local
    // prefix. The constructors above only move metadata, without user callbacks.
    let ingress = ingress.upgrade().expect("open local store owns ingress");
    let command: Command<R, A, Local> = Command::Fold(call);
    if let Err(error) = ingress.send(command).await {
        error.0.close();
    }
    drop(ingress);
    receive
        .await
        .expect("owned reply guard publishes a terminal result")
}

/// Captures one owner-local subtree without retaining callbacks or Store.
///
/// Replacement, configuration refresh and ancestor retirement invalidate it.
pub struct FoldView<K: FoldKey> {
    /// Erases root types while retaining only frozen authority and weak Store metadata.
    endpoint: Rc<dyn Endpoint<K>>,
}

impl<K: FoldKey> Clone for FoldView<K> {
    fn clone(&self) -> Self {
        Self {
            endpoint: Rc::clone(&self.endpoint),
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
    /// Reads through the captured diagnostic gate after checking its root identity.
    pub(in crate::store) fn read_for_test(
        &self,
        root: Arc<()>,
    ) -> impl Future<Output = Result<K::Value, TaskAccessError>> + 'static + use<K> {
        self.endpoint.read_for_test(root)
    }

    /// Reads this captured subtree at an ordered actor position.
    ///
    /// Construction admits nothing. First polling attempts ordered admission.
    /// The future and its possibly non-`Send` result stay on the owner thread.
    /// Blocking synchronously on this future from an actor callback is unsupported.
    ///
    /// # Errors
    /// Rejects stale authority before selection, or reports closure and actor failure.
    pub fn read(
        &self,
    ) -> impl Future<Output = Result<K::Value, TaskAccessError>> + 'static + use<K> {
        self.endpoint.read()
    }
}

/// Erases root types without erasing the fold's value type or local thread bound.
trait Endpoint<K: FoldKey> {
    /// Prepares a lazy read of the frozen subtree.
    fn read(&self) -> Pin<Box<dyn Future<Output = Result<K::Value, TaskAccessError>>>>;

    #[cfg(feature = "test-util")]
    /// Checks the requested root and uses the endpoint's captured test gate.
    fn read_for_test(
        &self,
        root: Arc<()>,
    ) -> Pin<Box<dyn Future<Output = Result<K::Value, TaskAccessError>>>>;
}

/// Freezes one subtree's allocation identity, incarnation and scope authority.
struct Target {
    /// Reserves the allocation address without keeping declaration payloads alive.
    reservation: Weak<dyn OutputRegistration>,
    /// Matches the declaration in the committed actor roster.
    identity: usize,
    /// Rejects retired scopes and changed configuration before callback selection.
    frozen: FrozenRegistration,
}

impl Clone for Target {
    fn clone(&self) -> Self {
        Self {
            reservation: Weak::clone(&self.reservation),
            identity: self.identity,
            frozen: FrozenRegistration {
                scope: self.frozen.scope,
                authority: self.frozen.authority.clone(),
            },
        }
    }
}

impl Target {
    /// Selects the current committed subtree or rejects stale captured authority.
    fn select<'a, R, A>(
        &self,
        scopes: &ScopeRegistry,
        mounts: &'a Mounts<R, A>,
    ) -> Result<&'a Mounts<R, A>, TaskAccessError> {
        let _reservation = self.reservation.upgrade().ok_or_else(ended)?;
        walk::subtree::<R, A, Local>(scopes, mounts, self.identity, &self.frozen)
    }
}

/// The owner-local actor's flat preorder declaration roster.
type Mounts<R, A> = [Box<<Local as Carriers<R, A>>::Mount>];

/// Retains a frozen subtree endpoint without retaining Store or callback ownership.
struct TypedEndpoint<R, A, K: FoldKey> {
    /// Weak callback lookup metadata shared with other reads of this fold.
    fold: Fold<R, K>,
    /// The captured declaration and authority, never retargeted to a successor.
    target: Target,
    /// Upgraded only while admitting a command.
    ingress: SharedWeak<Ingress<R, A>>,
    /// Rejects owner loss before admission without keeping the Store alive.
    lifecycle: SharedWeak<Lifecycle>,
    #[cfg(feature = "test-util")]
    /// The owning test driver's gate, if acquisition occurred through that driver.
    gate: Option<crate::testing::TransactionGateCore<R, A, Local>>,
}

#[cfg(test)]
ordinary_test_items! {
    // Prints a layout diagnostic, without asserting behavioural correctness.
    #[test]
    fn local_endpoint_layout_records_diagnostic_metadata() {
        struct Key;

        impl FoldKey for Key {
            type Value = ();

            fn identity() {}

            fn reduce((): (), (): ()) -> std::ops::ControlFlow<(), ()> {
                std::ops::ControlFlow::Continue(())
            }
        }

        eprintln!(
            "Local Fold typed endpoint bytes={} test_util={}",
            std::mem::size_of::<TypedEndpoint<(), (), Key>>(),
            cfg!(feature = "test-util")
        );
    }
}

impl<R: 'static, A: 'static, K: FoldKey> Endpoint<K> for TypedEndpoint<R, A, K> {
    fn read(&self) -> Pin<Box<dyn Future<Output = Result<K::Value, TaskAccessError>>>> {
        let fold = self.fold.clone();
        let target = self.target.clone();
        Box::pin(submit(
            SharedWeak::clone(&self.ingress),
            SharedWeak::clone(&self.lifecycle),
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
    ) -> Pin<Box<dyn Future<Output = Result<K::Value, TaskAccessError>>>> {
        let fold = self.fold.clone();
        let target = self.target.clone();
        let ingress = SharedWeak::clone(&self.ingress);
        let lifecycle = SharedWeak::clone(&self.lifecycle);
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

/// Acquires frozen subtree metadata at an ordered actor position.
struct Acquire<R, A, S, X, K: FoldKey> {
    /// The contribution channel to use after successful acquisition.
    fold: Fold<R, K>,
    /// The declaration to freeze, without owning its callbacks or installed feature.
    source: Weak<dyn Acquisition<R, S, X, A>>,
    /// Weak command endpoint for the acquired view.
    ingress: SharedWeak<Ingress<R, A>>,
    /// Identifies the submitting owner for cross-Store rejection.
    lifecycle: SharedWeak<Lifecycle>,
    /// Returns absence, a frozen view, or a terminal authority failure.
    reply: ReplyGuard<Option<FoldView<K>>>,
    #[cfg(feature = "test-util")]
    /// Passed to the view only for a diagnostic acquisition.
    gate: Option<crate::testing::TransactionGateCore<R, A, Local>>,
}

impl<R: 'static, A: 'static, S: 'static, X: 'static, K: FoldKey> FoldCall<R, A, Local>
    for Acquire<R, A, S, X, K>
{
    fn execute(
        self: Box<Self>,
        _: &R,
        scopes: &ScopeRegistry,
        _: &[Box<<Local as Carriers<R, A>>::Mount>],
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
                endpoint: Rc::new(TypedEndpoint::<R, A, K> {
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

/// Prepares a lazy owner-local subtree acquisition with weak Store ownership.
pub(in crate::store) fn acquire<R: 'static, A: 'static, S: 'static, X: 'static, K: FoldKey>(
    fold: Fold<R, K>,
    source: Weak<dyn Acquisition<R, S, X, A>>,
    ingress: SharedWeak<Ingress<R, A>>,
    lifecycle: SharedWeak<Lifecycle>,
) -> impl Future<Output = Result<Option<FoldView<K>>, TaskAccessError>> + 'static {
    submit(
        SharedWeak::clone(&ingress),
        SharedWeak::clone(&lifecycle),
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
/// Prepares an acquisition through the explicit diagnostic gate.
pub(in crate::store) fn acquire_for_test<
    R: 'static,
    A: 'static,
    S: 'static,
    X: 'static,
    K: FoldKey,
>(
    fold: Fold<R, K>,
    source: Weak<dyn Acquisition<R, S, X, A>>,
    ingress: SharedWeak<Ingress<R, A>>,
    lifecycle: SharedWeak<Lifecycle>,
    gate: crate::testing::TransactionGateCore<R, A, Local>,
) -> impl Future<Output = Result<Option<FoldView<K>>, TaskAccessError>> + 'static {
    submit(
        SharedWeak::clone(&ingress),
        SharedWeak::clone(&lifecycle),
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

/// Reports stale or foreign subtree authority before callback entry.
fn ended() -> TaskAccessError {
    TaskAccessError::new(TaskAccessReason::ScopeEnded, TaskProgress::NotStarted)
}
