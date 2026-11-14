//! Typed, incarnation-owned contributions over the committed mount roster.
//!
//! The index locates callbacks, but does not define membership or order.
//! The actor lends each callback to the walker without a metadata lock held.
//! Declaration retirement removes the index entry before it drops captures.

mod view;
mod walk;
#[cfg(test)]
ordinary_test_items! { mod borrowed_tests; }
#[cfg(feature = "local")]
pub(super) mod local;
pub use view::FoldView;

use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::fmt;
use std::ops::ControlFlow;
use std::sync::{Arc, Weak};

use super::carriers::{Carriers, Shared};
use super::command::Command;
use super::lifecycle::Lifecycle;
use super::mount::{Acquisition, MountDeclaration, MountSite};
use super::output::OutputRegistration;
use super::scope::{ScopeRegistry, ScopeToken, ScopeTransitionBatch};
use super::task_context::{ReplyGuard, TaskAccessError, TaskAccessReason, TaskProgress};

/// Defines an owned, ordered fold with explicit short-circuiting.
///
/// Ordinary nodes contribute in preorder without grouping their descendants.
/// A transform groups only its declared subtree. `Break` stops that group.
/// Its transform still runs and contributes once to the enclosing group.
/// Values move without `Clone` or `Sync` requirements. Shared reads require `Send`.
/// Identity and reduction callbacks run synchronously on the actor.
/// Reduction order is observable. Reduction need not be associative or commutative.
pub trait FoldKey: 'static {
    /// The owned accumulator and contribution type.
    type Value: 'static;

    /// Creates the initial accumulator of a fold or transformed subtree.
    fn identity() -> Self::Value;

    /// Consumes one contribution, optionally ending the current group.
    fn reduce(accumulator: Self::Value, next: Self::Value)
    -> ControlFlow<Self::Value, Self::Value>;
}

/// Names typed contributions without retaining their callbacks or Store.
///
/// Membership and order always come from the actor's committed mount roster.
/// Declare callbacks on their own incarnation using `MountChildren::contribute`.
/// One channel can name contributions in several Stores. Each read visits only
/// its own Store's roster. Cloning the channel does not clone callbacks.
pub struct Fold<R, K: FoldKey> {
    /// Weak callback locations, keyed by allocation addresses reserved by declarations.
    index: Arc<Mutex<BTreeMap<usize, Dispatch<R, K>>>>,
}

impl<R, K: FoldKey> Fold<R, K> {
    /// Creates an empty typed contribution channel.
    #[must_use]
    pub fn new() -> Self {
        Self {
            index: Arc::new(Mutex::new(BTreeMap::new())),
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
            index: Arc::clone(&self.index),
        }
    }
}

impl<R, K: FoldKey> fmt::Debug for Fold<R, K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Fold").finish_non_exhaustive()
    }
}

/// Locates a declaration without owning its callback captures.
struct Dispatch<R, K: FoldKey> {
    /// Keeps the declaration address reserved even after its payload retires.
    _source: Weak<dyn OutputRegistration>,
    /// Upgrades only while the declaration or an active callback lease owns it.
    cell: Weak<EntryCell<R, K>>,
}

/// Separates callback ownership from the index's lookup lock.
struct EntryCell<R, K: FoldKey> {
    /// Absent while leased or after retirement. User code runs without this guard.
    payload: Mutex<Option<Box<dyn PureEntry<R, K> + Send>>>,
}

/// Erases child state while preserving the fold's owned value type.
pub(super) trait PureEntry<R, K: FoldKey> {
    /// Selects committed child state, then runs its optional contribution callback.
    fn contribution(
        &self,
        root: &R,
        lifecycle: &Lifecycle,
    ) -> Result<Option<K::Value>, TaskAccessError>;

    /// Reports whether the walker must open a subtree accumulator.
    fn has_transform(&self) -> bool;

    /// Completes a subtree after `has_transform` returned true for this entry.
    fn transform(&self, value: K::Value) -> K::Value;
}

/// Borrows child state without cloning it or requiring a thread-shared callback.
type Contribution<S, V> = Box<dyn Fn(&S) -> V + Send>;

/// Consumes the owned accumulator for one declared subtree.
type Transform<V> = Box<dyn Fn(V) -> V + Send>;

/// Keeps child projection and callbacks typed until fold traversal.
struct TypedEntry<R, A, S, X, K: FoldKey> {
    /// The committed roster owns this declaration throughout a synchronous walk.
    registration: Weak<dyn Acquisition<R, S, X, A>>,
    /// Absent for a transform-only group, which contributes no synthetic identity.
    contribution: Option<Contribution<S, K::Value>>,
    /// Groups descendants only when explicitly declared.
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
        // Temporary path and registration destructors run before this checkpoint.
        // Closure outranks a missing path and must suppress the next callback.
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

/// Gives one mounted incarnation sole ownership of a callback entry.
struct EntryOwner<R, K: FoldKey> {
    /// Owns captures except while the synchronous walker borrows them.
    cell: Arc<EntryCell<R, K>>,
    /// Does not keep a discarded fold channel alive.
    index: Weak<Mutex<BTreeMap<usize, Dispatch<R, K>>>>,
    /// The declaration address reserved by the index's weak registration.
    identity: usize,
}

impl<R, K: FoldKey> EntryOwner<R, K> {
    /// Removes dispatch authority before dropping captures outside metadata guards.
    fn drain(&self) {
        let payload = self.cell.payload.lock().take();
        if let Some(index) = self.index.upgrade() {
            // This sole owner removes its reserved entry once, before user captures drop.
            index.lock().remove(&self.identity);
        }
        // Capture destructors never run under either metadata guard.
        drop(payload);
    }
}

impl<R, K: FoldKey> Drop for EntryOwner<R, K> {
    fn drop(&mut self) {
        self.drain();
    }
}

impl<R, A, K: FoldKey> MountDeclaration<R, A> for EntryOwner<R, K> {
    // This declaration owns teardown only. Its mounted site supplies identity,
    // scope and preorder position to the common walker.
    fn bind(&self, _: &Arc<super::ingress::CommandIngress<Command<R, A>>>, _: &Arc<Lifecycle>) {}
    fn reconcile(
        &self,
        _: &R,
        _: &mut ScopeRegistry,
        _: &mut ScopeTransitionBatch,
        _: &mut Vec<Box<<Shared as Carriers<R, A>>::Node>>,
        _: ScopeToken,
        _: &mut Vec<Box<<Shared as Carriers<R, A>>::Mount>>,
        _: &mut super::mount::nested::Descendants<'_, R, A, Shared>,
        #[cfg(feature = "diagnostics")] _: Option<crate::diagnostics::LifecycleCall<'_>>,
    ) {
    }
}

impl<R: 'static, K: FoldKey> Fold<R, K> {
    /// Installs one callback bundle for this channel and declaration address.
    ///
    /// Duplicate declarations panic after the index guard is released.
    pub(super) fn attach<A: 'static, S: 'static, X: 'static>(
        &self,
        site: &MountSite<R, S, X, A>,
        contribution: Option<Contribution<S, K::Value>>,
        transform: Option<Transform<K::Value>>,
    ) -> impl MountDeclaration<R, A> + Send + 'static {
        let registration = site.fold_registration();
        let identity = registration.as_ptr().cast::<()>() as usize;
        let cell = Arc::new(EntryCell {
            payload: Mutex::new(Some(Box::new(TypedEntry::<_, _, _, _, K> {
                registration,
                contribution,
                transform,
            }))),
        });
        {
            let mut index = self.index.lock();
            if index.contains_key(&identity) {
                drop(index);
                panic!("one fold declaration per channel and incarnation");
            }
            index.insert(
                identity,
                Dispatch {
                    _source: site.output_registration(),
                    cell: Arc::downgrade(&cell),
                },
            )
        };
        EntryOwner {
            cell,
            index: Arc::downgrade(&self.index),
            identity,
        }
    }

    /// Detaches callbacks for a serial walk that already owns the committed roster.
    fn acquire(&self, identity: usize) -> Option<EntryLease<R, K>> {
        // EntryOwner removes its index entry before releasing the strong cell.
        // Keep the index guard through upgrade so retirement cannot overtake it.
        let cell = self
            .index
            .lock()
            .get(&identity)?
            .cell
            .upgrade()
            .expect("indexed fold entry has a live owner");
        let payload = cell
            .payload
            .lock()
            .take()
            .expect("serial fold cannot re-enter a leased callback");
        Some(EntryLease {
            cell,
            payload: Some(payload),
        })
    }
}

/// Holds callbacks outside metadata guards until normal return or unwind.
struct EntryLease<R, K: FoldKey> {
    /// Keeps storage alive until its detached callbacks return.
    cell: Arc<EntryCell<R, K>>,
    /// Present until drop restores it, without cloning user captures.
    payload: Option<Box<dyn PureEntry<R, K> + Send>>,
}

/// Gives the common walker access to Shared or Local callback ownership.
pub(super) trait CallbackLease<R, K: FoldKey> {
    /// Borrows the detached payload for the lifetime of this lease.
    fn entry(&self) -> &dyn PureEntry<R, K>;
}

impl<R, K: FoldKey> CallbackLease<R, K> for EntryLease<R, K> {
    fn entry(&self) -> &dyn PureEntry<R, K> {
        self.payload.as_deref().unwrap()
    }
}

impl<R, K: FoldKey> Drop for EntryLease<R, K> {
    fn drop(&mut self) {
        // Restore before actor teardown can drain the owner. If retirement
        // already removed that owner, this lease releases the final strong cell.
        let mut destination = self.cell.payload.lock();
        std::mem::swap(&mut *destination, &mut self.payload);
        drop(destination);
        drop(self.payload.take());
    }
}

/// Owns an admitted fold read or subtree acquisition and its terminal reply.
pub(crate) trait FoldCall<R, A, C: Carriers<R, A>> {
    /// Checks authority and runs against the actor's committed mount roster.
    fn execute(
        self: Box<Self>,
        root: &R,
        scopes: &ScopeRegistry,
        mounts: &[Box<C::Mount>],
        lifecycle: &Lifecycle,
    );

    /// Replies without entering user callbacks when actor authority rejects the call.
    fn reject(self: Box<Self>, error: TaskAccessError);
}

/// Carries an ordered whole-root or captured-subtree read.
struct Read<R, K: FoldKey> {
    /// Callback lookup metadata, without ownership of mounted callbacks.
    fold: Fold<R, K>,
    /// Absent for a whole-root read, frozen for a captured subtree.
    target: Option<view::Target>,
    #[cfg(feature = "test-util")]
    /// Rejects a diagnostic command submitted through another Store's gate.
    expected_root: Option<Arc<()>>,
    /// Publishes completion or a conservative failure if execution unwinds.
    reply: ReplyGuard<K::Value>,
}

impl<R: 'static, A, K: FoldKey> FoldCall<R, A, Shared> for Read<R, K> {
    fn execute(
        mut self: Box<Self>,
        root: &R,
        scopes: &ScopeRegistry,
        mounts: &[Box<<Shared as Carriers<R, A>>::Mount>],
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
            self.reply.complete(Err(TaskAccessError::new(
                TaskAccessReason::ScopeEnded,
                TaskProgress::NotStarted,
            )));
            return;
        }
        let mounts = match self
            .target
            .as_ref()
            .map(|target| target.select::<R, A, Shared>(scopes, mounts))
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
        let result =
            walk::walk::<R, A, K, Shared, _>(root, scopes, mounts, lifecycle, |identity| {
                self.fold.acquire(identity)
            });
        self.reply.complete(result);
    }

    fn reject(self: Box<Self>, error: TaskAccessError) {
        self.reply.complete(Err(error));
    }
}

impl<R: Send + 'static, A: Send + 'static> super::Store<R, A> {
    /// Folds committed contributions at an ordered, non-mutating actor position.
    ///
    /// Construction is inert. First polling attempts ordered admission.
    /// A callback can first-poll another read. That read runs after this one finishes.
    /// Blocking synchronously on that future inside the actor is unsupported.
    /// Input admission gates do not remove contributions.
    ///
    /// # Errors
    /// Returns readiness, closure or authority failures with conservative progress.
    ///
    /// # Panics
    /// User callbacks can panic. The actor then fails and publishes a terminal error.
    pub fn fold<K: FoldKey>(
        &self,
        fold: &Fold<R, K>,
    ) -> impl Future<Output = Result<K::Value, TaskAccessError>> + Send + 'static + use<R, A, K>
    where
        K::Value: Send,
    {
        let ingress = Arc::downgrade(&self.ingress);
        let lifecycle = Arc::downgrade(&self.lifecycle);
        let fold = fold.clone();
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
    /// Checks lifecycle authority before a test driver chooses its fold boundary.
    pub(crate) fn validate_fold_for_test(&self) -> Result<(), TaskAccessError> {
        self.lifecycle.access()
    }

    #[cfg(feature = "test-util")]
    /// Captures this root's identity and submits through its explicit test gate.
    pub(crate) fn fold_for_test<K: FoldKey>(
        &self,
        fold: &Fold<R, K>,
        gate: crate::testing::TransactionGateCore<R, A>,
    ) -> impl Future<Output = Result<K::Value, TaskAccessError>> + Send + 'static + use<R, A, K>
    where
        K::Value: Send,
    {
        let fold = fold.clone();
        let expected_root = Some(self.lifecycle.root_identity());
        submit(
            Arc::downgrade(&self.ingress),
            Arc::downgrade(&self.lifecycle),
            Some(gate),
            move |reply| {
                Box::new(Read {
                    fold,
                    target: None,
                    expected_root,
                    reply,
                })
            },
        )
    }
}

/// Checks weak authority, admits one command and awaits its guarded terminal reply.
async fn submit<R: Send + 'static, A: Send + 'static, T: Send + 'static>(
    ingress: Weak<super::ingress::CommandIngress<Command<R, A>>>,
    lifecycle: Weak<Lifecycle>,
    #[cfg(feature = "test-util")] gate: Option<crate::testing::TransactionGateCore<R, A>>,
    make: impl FnOnce(ReplyGuard<T>) -> Box<dyn FoldCall<R, A, Shared> + Send> + Send,
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

    #[cfg(all(test, feature = "test-util"))]
    ordinary_test_items! {
        super::mounted::mounted_admission_test_hook::before_ingress();
    }
    let ingress = ingress.upgrade().ok_or_else(closed)?;
    let command: Command<R, A> = Command::Fold(call);
    if let Err(error) = ingress.send(command).await {
        error.0.close();
    }
    drop(ingress);
    receive
        .await
        .expect("owned reply guard publishes a terminal result")
}

/// Reports owner loss before any user callback starts.
fn closed() -> TaskAccessError {
    TaskAccessError::new(TaskAccessReason::StoreClosed, TaskProgress::NotStarted)
}
