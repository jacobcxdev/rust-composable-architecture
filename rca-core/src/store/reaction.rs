//! Owns callback registrations and independent, synchronous transaction projections.
//!
//! Each admitted mutation captures owned values before it changes state. The
//! post-mutation roster pairs those values with surviving registration authority.
//! Selectors, equality, factories and value destructors run without callback guards.
//! These synchronous phases can use non-`Send` projections even for Shared stores.

#[cfg(test)]
mod guard_tests;

use super::carriers::Carriers;
use super::mount::MountDeclaration;
use super::scope::{ScopeAuthority, ScopeRegistry};
use super::transaction::Batch;
use crate::feature::{InitialPolicy, ReactionChange};
use crate::{ReactionContext, ReduceContext, Task};
use parking_lot::Mutex;
use std::marker::PhantomData;
use std::sync::{Arc, Weak};

/// Captures reaction values immediately before an admitted mutation.
///
/// The callback borrows state only for this synchronous call. A false result
/// means that the Store closed and the caller must not enter the mutation.
pub(crate) type BeforeMutation<'a, S> = dyn FnMut(&S, &ScopeRegistry) -> bool + 'a;

/// Keeps one projection paired with its original scope configuration.
struct Saved<S, T> {
    /// Authority captured before the mutation, including ancestor configurations.
    authority: ScopeAuthority,
    /// Owns the old value and a weak callback header until pairing or disposal.
    capture: Box<Captured<S, T>>,
}

/// Holds the independent reaction projections for one admitted mutation.
///
/// The key combines a live declaration address with its reaction ordinal.
/// Lookup does not determine callback order. The post-mutation roster does.
pub(crate) struct Frame<S, T> {
    /// Initialises the map and its hash state only after the first capture.
    captures: Option<std::collections::HashMap<(usize, usize), Saved<S, T>>>,
}

impl<S, T> Default for Frame<S, T> {
    fn default() -> Self {
        Self { captures: None }
    }
}

impl<S, T> Frame<S, T> {
    /// Saves one capture for a declaration and its reaction ordinal.
    ///
    /// # Panics
    ///
    /// Panics if the same registration already has a capture in this mutation.
    pub(super) fn insert(
        &mut self,
        declaration: usize,
        ordinal: usize,
        authority: ScopeAuthority,
        capture: Box<Captured<S, T>>,
    ) {
        let old = self
            .captures
            .get_or_insert_with(std::collections::HashMap::new)
            .insert((declaration, ordinal), Saved { authority, capture });
        assert!(
            old.is_none(),
            "one capture per admitted mutation and registration"
        );
    }

    /// Removes a capture and returns it only while its authority still matches.
    ///
    /// Rejected captures drop their owned values before this method returns.
    /// The caller checks Store liveness before the next user callback.
    pub(super) fn take(
        &mut self,
        declaration: usize,
        ordinal: usize,
        scopes: &ScopeRegistry,
    ) -> Option<Box<Captured<S, T>>> {
        let saved = self.captures.as_mut()?.remove(&(declaration, ordinal))?;
        // New incarnations run Initial hooks instead of consuming old captures.
        // Retained incarnations must still validate their captured authority.
        scopes.accepts(&saved.authority).then_some(saved.capture)
    }
}

/// Captures the current mounts in declaration order while the Store remains open.
///
/// A false result stops the caller before mutation. Captures already stored in
/// the frame remain owned by that frame until normal cleanup or unwinding.
pub(super) fn capture<R, A, C: Carriers<R, A>>(
    mounts: &[Box<C::Mount>],
    state: &R,
    scopes: &ScopeRegistry,
    frame: &mut Frame<R, Batch<R, A, C>>,
) -> bool {
    for mount in mounts {
        if !scopes.installation_is_live() {
            return false;
        }
        mount.capture_reactions(state, scopes, frame);
    }
    scopes.installation_is_live()
}

/// Wraps a returned root task without polling its future.
pub(super) fn root_batch<R, A, C: Carriers<R, A>>(task: C::Task) -> Batch<R, A, C> {
    let mut work = crate::feature::Work::default();
    work.push(crate::feature::WorkItem::Task(task));
    Batch::Root(work)
}

/// Consumes an old projection against post-mutation state without suspending.
///
/// The caller preserves the paired declaration and checks its authority before
/// entry. The capture itself does not keep callback payloads alive after retirement.
pub(super) type Captured<S, T> = dyn for<'a> FnOnce(&S, ReduceContext<'a>) -> Option<T>;

/// Erases projection and factory types without imposing bounds on owned history.
pub(crate) trait Hook<S, T> {
    /// Reports whether a fresh incarnation runs the initial factory.
    fn runs_initial(&self) -> bool;

    /// Selects the old value and retains it only if the Store remains open.
    fn capture(&self, state: &S, live: &dyn Fn() -> bool) -> Option<Box<Captured<S, T>>>;

    /// Selects the current value and creates initial work while the Store is open.
    fn initial(&self, state: &S, context: ReduceContext<'_>) -> Option<T>;
}

/// Transfers callback ownership with Shared stores, without requiring `Sync` state.
pub(super) type SharedHook<S, A> = Box<dyn Hook<S, Task<S, A>> + Send>;

/// Keeps Local callbacks and their tasks on the owner thread.
#[cfg(feature = "local")]
pub(super) type LocalHook<S, A> = Box<dyn Hook<S, crate::local::Task<S, A>>>;

/// Detaches actor-owned callbacks before any user code runs.
///
/// Guards protect storage access only. Callers retain exclusive serial execution
/// of the detached payload, including its destruction on failure or retirement.
trait Cell<P>: Clone + 'static {
    /// Keeps the storage header reserved without retaining its payload.
    type Weak: 'static;

    /// Reserves this header for an old projection without owning its callbacks.
    fn downgrade(&self) -> Self::Weak;

    /// Acquires a strong header reference while an owner remains.
    fn upgrade(weak: &Self::Weak) -> Option<Self>;

    /// Removes the payload and releases the guard before returning it.
    fn take(&self) -> Option<P>;

    /// Swaps storage with the caller's slot without dropping either payload.
    ///
    /// Any displaced payload remains in the slot for destruction outside the guard.
    fn restore(&self, payload: &mut Option<P>);
}

impl<P: 'static> Cell<P> for Arc<Mutex<Option<P>>> {
    type Weak = Weak<Mutex<Option<P>>>;

    fn downgrade(&self) -> Self::Weak {
        Arc::downgrade(self)
    }

    fn upgrade(weak: &Self::Weak) -> Option<Self> {
        weak.upgrade()
    }

    fn take(&self) -> Option<P> {
        self.lock().take()
    }

    fn restore(&self, payload: &mut Option<P>) {
        std::mem::swap(&mut *self.lock(), payload);
    }
}

#[cfg(feature = "local")]
impl<P: 'static> Cell<P> for std::rc::Rc<std::cell::RefCell<Option<P>>> {
    type Weak = std::rc::Weak<std::cell::RefCell<Option<P>>>;

    fn downgrade(&self) -> Self::Weak {
        std::rc::Rc::downgrade(self)
    }

    fn upgrade(weak: &Self::Weak) -> Option<Self> {
        weak.upgrade()
    }

    fn take(&self) -> Option<P> {
        self.borrow_mut().take()
    }

    fn restore(&self, payload: &mut Option<P>) {
        std::mem::swap(&mut *self.borrow_mut(), payload);
    }
}

/// Runs user code with detached callbacks and restores them on normal return.
///
/// If user code panics, unwinding drops the payload outside its storage guard.
/// The actor then terminates. This function does not restore or resume callbacks
/// after a panic, and it does not support recursive entry into the same registration.
fn with_callbacks<P, C: Cell<P>, T>(cell: &C, apply: impl FnOnce(&mut P) -> T) -> T {
    let mut payload = cell.take().expect("serial actor owns reaction callbacks");
    let result = apply(&mut payload);
    let mut restoring = Some(payload);
    cell.restore(&mut restoring);
    drop(restoring);
    result
}

/// Owns one selector and factory until its declaration retires.
struct Registration<S, T, V, P, F, C: Cell<(P, F)>> {
    /// Holds both callbacks between serial actor calls.
    callbacks: C,
    /// Controls fresh-incarnation delivery independently of changed-value delivery.
    initial: InitialPolicy,
    /// Relates callback types without owning state, tasks or selected values.
    #[expect(
        clippy::type_complexity,
        reason = "non-owning marker preserves contravariant State and covariant task, value and callback parameters"
    )]
    domain: PhantomData<fn(&S) -> (T, V, P, F)>,
}

impl<S, T, V, P, F, C: Cell<(P, F)>> Drop for Registration<S, T, V, P, F, C> {
    fn drop(&mut self) {
        // Captures retain only Weak headers. Retirement destroys callbacks here,
        // even if a temporary header upgrade remains alive.
        let callbacks = self.callbacks.take();
        drop(callbacks);
    }
}

impl<S: 'static, T: 'static, V: PartialEq + 'static, P, F, C> Hook<S, T>
    for Registration<S, T, V, P, F, C>
where
    P: FnMut(&S) -> V + 'static,
    F: for<'a> FnMut(ReactionChange<V>, ReduceContext<'a>) -> T + 'static,
    C: Cell<(P, F)>,
{
    fn runs_initial(&self) -> bool {
        self.initial == InitialPolicy::Run
    }

    fn capture(&self, state: &S, live: &dyn Fn() -> bool) -> Option<Box<Captured<S, T>>> {
        let previous = with_callbacks(&self.callbacks, |(select, _)| select(state));
        if !live() {
            return None;
        }
        let weak = self.callbacks.downgrade();
        Some(Box::new(move |state: &S, context: ReduceContext<'_>| {
            let cell = C::upgrade(&weak).expect("paired declaration owns its callbacks");
            with_callbacks(&cell, |(select, factory)| {
                let current = select(state);
                if !context.installation_is_live() {
                    return None;
                }
                let changed = previous != current;
                if !context.installation_is_live() || !changed {
                    return None;
                }
                Some(factory(
                    ReactionChange::Changed { previous, current },
                    context,
                ))
            })
        }))
    }

    fn initial(&self, state: &S, context: ReduceContext<'_>) -> Option<T> {
        with_callbacks(&self.callbacks, |(select, factory)| {
            let current = select(state);
            if !context.installation_is_live() {
                return None;
            }
            Some(factory(ReactionChange::Initial { current }, context))
        })
    }
}

/// Creates a transferable registration with synchronous, potentially non-`Send` projections.
pub(super) fn shared<S: 'static, A: 'static, V: PartialEq + 'static>(
    select: impl FnMut(&S) -> V + Send + 'static,
    initial: InitialPolicy,
    mut factory: impl for<'a> FnMut(ReactionChange<V>, ReactionContext<'a, S, A>) -> Task<S, A>
    + Send
    + 'static,
) -> SharedHook<S, A> {
    let factory = move |change: ReactionChange<V>, context: ReduceContext<'_>| {
        factory(change, ReactionContext::new(context))
    };
    Box::new(Registration {
        callbacks: Arc::new(Mutex::new(Some((select, factory)))),
        initial,
        domain: PhantomData,
    })
}

/// Creates a thread-confined registration without `Send` callback or task bounds.
#[cfg(feature = "local")]
pub(super) fn local<S: 'static, A: 'static, V: PartialEq + 'static>(
    select: impl FnMut(&S) -> V + 'static,
    initial: InitialPolicy,
    mut factory: impl for<'a> FnMut(
        ReactionChange<V>,
        ReactionContext<'a, S, A>,
    ) -> crate::local::Task<S, A>
    + 'static,
) -> LocalHook<S, A> {
    let factory = move |change: ReactionChange<V>, context: ReduceContext<'_>| {
        factory(change, ReactionContext::new(context))
    };
    Box::new(Registration {
        callbacks: std::rc::Rc::new(std::cell::RefCell::new(Some((select, factory)))),
        initial,
        domain: PhantomData,
    })
}

#[cfg(test)]
mod frame_tests {
    ordinary_test_items! {
        use super::{Frame, ScopeRegistry};
        use std::rc::Rc;

        #[test]
        fn frame_takes_each_capture_once_and_returns_none_when_empty() {
            let (_lifecycle, scopes) = ScopeRegistry::for_test();
            let scope = scopes.root();
            let mut frame = Frame::<(), ()>::default();
            assert!(frame.take(7, 3, &scopes).is_none());

            let value = Rc::new(());
            let retained = Rc::downgrade(&value);
            frame.insert(
                7,
                3,
                scopes.authority(scope),
                Box::new(move |(), _| {
                    drop(value);
                    Some(())
                }),
            );
            assert!(frame.take(7, 4, &scopes).is_none());
            let captured = frame.take(7, 3, &scopes).unwrap();
            assert!(frame.take(7, 3, &scopes).is_none());
            drop(frame);
            assert!(retained.upgrade().is_some());
            drop(captured);
            assert!(retained.upgrade().is_none());
        }

        #[test]
        fn frame_drops_untaken_capture_values() {
            let (_lifecycle, scopes) = ScopeRegistry::for_test();
            let scope = scopes.root();
            let value = Rc::new(());
            let retained = Rc::downgrade(&value);
            let mut frame = Frame::<(), ()>::default();
            frame.insert(
                7,
                3,
                scopes.authority(scope),
                Box::new(move |(), _| {
                    drop(value);
                    Some(())
                }),
            );
            drop(frame);
            assert!(retained.upgrade().is_none());
        }
    }
}
