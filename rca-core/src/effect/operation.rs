//! Tracks named completion ownership and actual managed polling ancestry.
//!
//! Named declarations use a separate key namespace from ordinary cancellation.
//! Captured waits hold completion metadata, not work. Callers release concrete
//! payloads before the work leases and registration guards that publish completion.
//! Each managed poll temporarily installs its ancestry and restores the previous
//! scope on return or unwind. This supports self-wait rejection after task migration.

use std::cell::RefCell;
use std::fmt;
use std::hash::Hash;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use event_listener::EventListener;

use super::CancellationIdentity;
use super::lineage::{LineageState, LineageWork};
use crate::TaskAccessReason;

/// Chooses how a named declaration treats earlier matching generations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperationPolicy {
    /// Retains earlier generations alongside the new generation.
    Coexist,
    /// Requests cancellation of earlier generations before registering the new one.
    Replace,
}

/// Separates named operation identity from ordinary cancellation with an equal key.
#[derive(Hash, Eq, PartialEq)]
struct NamedKey<K>(K);

/// Retains a named identity and its registration-time replacement policy.
pub(crate) struct OperationDeclaration {
    /// Type-separated key whose scope is supplied by the registering actor.
    pub(crate) identity: CancellationIdentity,
    /// Determines whether earlier matching generations receive cancellation requests.
    pub(crate) policy: OperationPolicy,
}

impl OperationDeclaration {
    /// Hashes the owned key once and retains the requested generation policy.
    pub(crate) fn new<K>(key: K, policy: OperationPolicy) -> Self
    where
        K: Hash + Eq + Send + Sync + 'static,
    {
        Self {
            identity: Self::identity(key),
            policy,
        }
    }

    /// Creates the same named-key namespace for declarations and observation handles.
    pub(crate) fn identity<K>(key: K) -> CancellationIdentity
    where
        K: Hash + Eq + Send + Sync + 'static,
    {
        CancellationIdentity::local(NamedKey(key))
    }
}

/// Describes the incomplete generations at one ordered observation.
#[derive(Clone)]
pub struct OperationStatus {
    /// Incomplete generations at one actor position, including cancelled retained work.
    active_count: usize,
}

impl OperationStatus {
    /// Records an actor-ordered count without retaining its generations.
    pub(crate) const fn new(active_count: usize) -> Self {
        Self { active_count }
    }

    /// Returns the number of incomplete named generations.
    #[must_use]
    pub const fn active_count(&self) -> usize {
        self.active_count
    }
}

impl fmt::Debug for OperationStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OperationStatus")
            .field("active_count", &self.active_count)
            .finish_non_exhaustive()
    }
}

/// Rejects waiting for completion of the currently polled operation itself.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SelfWait;

impl fmt::Display for SelfWait {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a managed operation cannot wait for its own completion")
    }
}

impl std::error::Error for SelfWait {}

/// Passively waits for a fixed, captured set of named generations.
///
/// This observer owns no work and never includes later generations. Dropping it
/// does not cancel work. Polling within an incomplete captured ancestor returns
/// [`SelfWait`] without cancelling that ancestor.
pub struct OperationWait {
    /// Fixed completion metadata; these references add no causal work leases.
    captured: Vec<Arc<LineageState>>,
    /// At most one completion registration, reused while its generation is incomplete.
    listener: Option<EventListener>,
    /// Captured index that owns the current listener, preventing cross-lineage reuse.
    listening: Option<usize>,
    /// Terminal result retained so later polls neither register nor reconsider ancestry.
    outcome: Option<Result<(), SelfWait>>,
}

impl OperationWait {
    /// Takes a fixed generation snapshot without registering a completion listener.
    pub(crate) fn new(captured: Vec<Arc<LineageState>>) -> Self {
        Self {
            captured,
            listener: None,
            listening: None,
            outcome: None,
        }
    }

    /// Arms cancellation of this captured set when an unfinished wait is dropped.
    ///
    /// Arming is synchronous. Successful completion and [`SelfWait`] disarm the
    /// guard before unregistering its listener. Later generations are unaffected.
    #[must_use]
    pub fn finish_cancelling_on_drop(self) -> CancellingOperationWait {
        let armed = self.outcome.is_none();
        CancellingOperationWait { wait: self, armed }
    }

    /// Stores the result and disarms cancellation before listener destruction can wake.
    fn complete(
        &mut self,
        result: Result<(), SelfWait>,
        armed: &mut bool,
    ) -> Poll<Result<(), SelfWait>> {
        *armed = false;
        self.outcome = Some(result);
        self.listener = None;
        self.listening = None;
        Poll::Ready(result)
    }

    /// Rechecks actual ancestry, then waits on one incomplete generation at a time.
    fn poll_inner(&mut self, cx: &mut Context<'_>, armed: &mut bool) -> Poll<Result<(), SelfWait>> {
        if let Some(result) = self.outcome {
            *armed = false;
            return Poll::Ready(result);
        }

        // Read actual polling ancestry anew even if a different captured entry
        // already owns the single registered completion listener.
        if POLL_ANCESTRY.with(|current| current.borrow().contains_incomplete(&self.captured)) {
            return self.complete(Err(SelfWait), armed);
        }

        loop {
            let Some(index) = self
                .captured
                .iter()
                .position(|lineage| !lineage.is_finished())
            else {
                return self.complete(Ok(()), armed);
            };

            if self.listening != Some(index) {
                self.listener = None;
                self.listening = Some(index);
            }

            if self.captured[index]
                .poll_finish(&mut self.listener, cx)
                .is_pending()
            {
                return Poll::Pending;
            }
        }
    }
}

impl Future for OperationWait {
    type Output = Result<(), SelfWait>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.get_mut().poll_inner(cx, &mut false)
    }
}

impl fmt::Debug for OperationWait {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OperationWait").finish_non_exhaustive()
    }
}

/// Waits for captured generations, cancelling that set if dropped while unfinished.
///
/// If a cancellation wake panics, remaining captured generations still receive
/// cancellation during unwinding. A second panic during unwinding aborts the process.
pub struct CancellingOperationWait {
    /// Passive metadata observer shared by both polling policies.
    wait: OperationWait,
    /// Cleared before terminal listener cleanup, including self-wait rejection.
    armed: bool,
}

impl Future for CancellingOperationWait {
    type Output = Result<(), SelfWait>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        this.wait.poll_inner(cx, &mut this.armed)
    }
}

impl Drop for CancellingOperationWait {
    fn drop(&mut self) {
        /// Requests cancellation on the remaining captures if an earlier wake panics.
        struct CancelRemaining<'a> {
            /// Advanced before each cancellation so unwinding never retries its wake.
            remaining: std::slice::Iter<'a, Arc<LineageState>>,
        }

        impl Drop for CancelRemaining<'_> {
            fn drop(&mut self) {
                for lineage in self.remaining.by_ref() {
                    lineage.cancel();
                }
            }
        }

        if self.armed {
            let mut cancellations = CancelRemaining {
                remaining: self.wait.captured.iter(),
            };
            for lineage in cancellations.remaining.by_ref() {
                lineage.cancel();
            }
        }
    }
}

impl fmt::Debug for CancellingOperationWait {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CancellingOperationWait")
            .finish_non_exhaustive()
    }
}

/// Shares immutable named ancestry without allocating for ordinary unnamed work.
#[derive(Clone, Default)]
pub(crate) struct OperationAncestry(Option<Arc<[Arc<LineageState>]>>);

impl OperationAncestry {
    /// Identifies the unnamed representation, which retains no ancestry allocation.
    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_none()
    }

    /// Appends one generation while preserving the parent's immutable ancestry.
    pub(crate) fn with_generation(&self, generation: Arc<LineageState>) -> Self {
        let ancestors = self.0.as_deref().unwrap_or_default();
        let mut generations = Vec::with_capacity(ancestors.len() + 1);
        generations.extend_from_slice(ancestors);
        generations.push(generation);
        Self(Some(generations.into()))
    }

    /// Reserves each ancestor while its registration or existing work keeps it live.
    pub(crate) fn retain_work(&self) -> OperationWork {
        OperationWork {
            _leases: self
                .0
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(|lineage| NamedWork {
                    _work: lineage.add_work(),
                    _notification: NamedRelease(Arc::clone(lineage)),
                })
                .collect(),
        }
    }

    /// Retains live ancestors, returning partial ownership if a later ancestor rejects.
    ///
    /// Callers must destroy their concrete payload before dropping either result's leases.
    pub(crate) fn try_retain_work(
        &self,
    ) -> Result<OperationWork, (TaskAccessReason, OperationWork)> {
        let mut leases = Vec::with_capacity(self.0.as_deref().unwrap_or_default().len());
        for lineage in self.0.as_deref().unwrap_or_default() {
            // Return partial ownership on rejection: the caller must destroy its
            // concrete payload before releasing even a partially acquired set.
            let Some(work) = lineage.try_add_work() else {
                return Err((
                    TaskAccessReason::Cancelled,
                    OperationWork { _leases: leases },
                ));
            };
            leases.push(NamedWork {
                _work: work,
                _notification: NamedRelease(Arc::clone(lineage)),
            });
        }

        Ok(OperationWork { _leases: leases })
    }

    /// Detects a captured incomplete ancestor using generation identity, not user keys.
    fn contains_incomplete(&self, captured: &[Arc<LineageState>]) -> bool {
        let Some(ancestors) = &self.0 else {
            return false;
        };

        captured.iter().any(|lineage| {
            !lineage.is_finished()
                && ancestors
                    .iter()
                    .any(|ancestor| Arc::ptr_eq(lineage, ancestor))
        })
    }
}

/// Owns named-ancestor leases released after the caller's concrete payload.
#[derive(Default)]
pub(crate) struct OperationWork {
    /// Empty for unnamed work; each entry releases its lease before notifying control.
    _leases: Vec<NamedWork>,
}

/// Orders causal release before actor notification, including during unwinding.
struct NamedWork {
    /// First field: drops causal ownership before its terminal notifier.
    _work: LineageWork,
    /// Runs after the lease even if a completion-listener waker panics during release.
    _notification: NamedRelease,
}

/// Notifies actor control when the preceding release made the lineage terminal.
struct NamedRelease(Arc<LineageState>);

impl Drop for NamedRelease {
    fn drop(&mut self) {
        self.0.notify_control_if_finished();
    }
}

/// Releases the initial named-generation hold before notifying actor control.
pub(crate) struct OperationRegistration {
    /// Registration hold ended by this guard's destructor, before field drop glue.
    lineage: Arc<LineageState>,
    /// Runs even when completion wakes a panicking listener during guard destruction.
    _notification: NamedRelease,
}

impl OperationRegistration {
    /// Takes responsibility for ending the lineage's initial registration hold once.
    pub(crate) fn new(lineage: Arc<LineageState>) -> Self {
        Self {
            _notification: NamedRelease(Arc::clone(&lineage)),
            lineage,
        }
    }
}

impl Drop for OperationRegistration {
    fn drop(&mut self) {
        self.lineage.finish_registration();
    }
}

// This runtime-domain-local scope contains metadata only. Never retain a borrow
// across user polling; even empty ancestry must mask a populated outer scope.
thread_local! {
    /// Actual current poll's metadata, borrowed only outside user future polling.
    static POLL_ANCESTRY: RefCell<OperationAncestry> = const {
        RefCell::new(OperationAncestry(None))
    };
}

/// Restores replaced ancestry; `None` avoids a redundant empty-to-empty TLS write.
struct PollScope(Option<OperationAncestry>);

impl PollScope {
    /// Installs this poll's ancestry, including empty metadata that masks an outer poll.
    fn enter(ancestry: OperationAncestry) -> Self {
        Self(POLL_ANCESTRY.with(|current| {
            let mut current = current.borrow_mut();
            if ancestry.0.is_none() && current.0.is_none() {
                None
            } else {
                Some(std::mem::replace(&mut *current, ancestry))
            }
        }))
    }
}

impl Drop for PollScope {
    fn drop(&mut self) {
        if let Some(previous) = self.0.take() {
            POLL_ANCESTRY.with(|current| current.replace(previous));
        }
    }
}

pin_project_lite::pin_project! {
    /// Installs managed ancestry without holding a TLS borrow across user future polling.
    ///
    /// Owns the pinned `future` and its named `ancestry`. The `()` ancestry
    /// specialisation represents unnamed work, which still masks an outer poll.
    pub(crate) struct ManagedPoll<F, A = OperationAncestry> {
        // Projection preserves the owned future's pinning.
        #[pin]
        future: F,
        // Named metadata, or `()` for unnamed work.
        ancestry: A,
    }
}

impl<F, A> ManagedPoll<F, A> {
    /// Wraps work without polling it or changing the current thread's ancestry.
    pub(crate) fn new(future: F, ancestry: A) -> Self {
        Self { future, ancestry }
    }
}

impl<F: Future> Future for ManagedPoll<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let _scope = PollScope::enter(this.ancestry.clone());
        this.future.poll(cx)
    }
}

impl<F: Future> Future for ManagedPoll<F, ()> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Unnamed work still masks an outer named poll, including after moving
        // between threads. Only its persistent metadata storage is omitted.
        let _scope = PollScope::enter(OperationAncestry::default());
        self.project().future.poll(cx)
    }
}

#[cfg(test)]
ordinary_test_items! {
mod tests {
    use std::future::poll_fn;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::pin::Pin;
    use std::sync::{Arc, Weak};
    use std::task::{Context, Poll, Wake, Waker};

    use super::{
        ManagedPoll, OperationAncestry, OperationDeclaration, OperationPolicy,
        OperationRegistration, OperationWait, SelfWait,
    };
    use crate::effect::CancellationIdentity;
    use crate::effect::lineage::{ControlWake, LineageState};

    fn context() -> Context<'static> {
        Context::from_waker(Waker::noop())
    }

    #[test]
    fn empty_poll_scope_drop_does_not_write_unchanged_tls() {
        super::POLL_ANCESTRY.with(|current| assert!(current.borrow().0.is_none()));
        let scope = super::PollScope::enter(OperationAncestry::default());
        super::POLL_ANCESTRY.with(|current| {
            let ancestry = current.borrow();
            assert!(ancestry.0.is_none());
            // Private no-write cost oracle: no user poll runs with this borrow.
            drop(scope);
            assert!(ancestry.0.is_none());
        });
    }

    #[test]
    fn empty_scope_restores_after_nested_named_ready_and_pending() {
        for ready in [false, true] {
            let lineage = LineageState::new(Weak::new());
            let nested_lineage = Arc::clone(&lineage);
            let mut outer = Box::pin(ManagedPoll::new(
                poll_fn(move |cx| {
                    let ancestry =
                        OperationAncestry::default().with_generation(Arc::clone(&nested_lineage));
                    let mut inner = Box::pin(ManagedPoll::new(
                        OperationWait::new(vec![Arc::clone(&nested_lineage)]),
                        ancestry,
                    ));
                    assert_eq!(inner.as_mut().poll(cx), Poll::Ready(Err(SelfWait)));
                    let mut restored = OperationWait::new(vec![Arc::clone(&nested_lineage)]);
                    assert!(Pin::new(&mut restored).poll(cx).is_pending());
                    if ready {
                        Poll::Ready(())
                    } else {
                        Poll::Pending
                    }
                }),
                (),
            ));
            let expected = if ready {
                Poll::Ready(())
            } else {
                Poll::Pending
            };
            assert_eq!(outer.as_mut().poll(&mut context()), expected);
            let mut external = OperationWait::new(vec![Arc::clone(&lineage)]);
            assert!(Pin::new(&mut external).poll(&mut context()).is_pending());
            drop(outer);
            lineage.finish_registration();
        }
    }

    #[test]
    fn empty_scope_restores_after_nested_named_unwind() {
        let lineage = LineageState::new(Weak::new());
        let ancestry = OperationAncestry::default().with_generation(Arc::clone(&lineage));
        let mut inner = Box::pin(ManagedPoll::new(
            poll_fn(|_| -> Poll<()> {
                panic!("actual nested user poll");
            }),
            ancestry,
        ));
        let mut outer = Box::pin(ManagedPoll::new(
            poll_fn(move |cx| inner.as_mut().poll(cx)),
            (),
        ));
        let panic = catch_unwind(AssertUnwindSafe(|| outer.as_mut().poll(&mut context())))
            .expect_err("nested user poll must panic");
        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"actual nested user poll")
        );
        let mut external = OperationWait::new(vec![Arc::clone(&lineage)]);
        assert!(Pin::new(&mut external).poll(&mut context()).is_pending());
        drop(outer);
        lineage.finish_registration();
    }

    #[test]
    fn empty_scope_rechecks_named_outer_after_thread_migration() {
        let lineage = LineageState::new(Weak::new());
        let migrated_lineage = Arc::clone(&lineage);
        let mut migrated = Box::pin(ManagedPoll::new(
            poll_fn(move |cx| {
                let mut masked = OperationWait::new(vec![Arc::clone(&migrated_lineage)]);
                assert!(Pin::new(&mut masked).poll(cx).is_pending());
                Poll::Pending::<()>
            }),
            (),
        ));
        assert!(migrated.as_mut().poll(&mut context()).is_pending());

        let ancestry = OperationAncestry::default().with_generation(Arc::clone(&lineage));
        let checked_lineage = Arc::clone(&lineage);
        let external_lineage = Arc::clone(&lineage);
        std::thread::spawn(move || {
            let mut outer = Box::pin(ManagedPoll::new(
                poll_fn(move |cx| {
                    assert!(migrated.as_mut().poll(cx).is_pending());
                    let mut restored = OperationWait::new(vec![Arc::clone(&checked_lineage)]);
                    assert_eq!(Pin::new(&mut restored).poll(cx), Poll::Ready(Err(SelfWait)));
                    Poll::Ready(())
                }),
                ancestry,
            ));
            assert_eq!(outer.as_mut().poll(&mut context()), Poll::Ready(()));
            let mut external = OperationWait::new(vec![external_lineage]);
            assert!(Pin::new(&mut external).poll(&mut context()).is_pending());
        })
        .join()
        .expect("migrated empty poll completes");
        let mut external = OperationWait::new(vec![Arc::clone(&lineage)]);
        assert!(Pin::new(&mut external).poll(&mut context()).is_pending());
        lineage.finish_registration();
    }

    #[test]
    fn named_identity_cannot_cancel_an_ordinary_equal_key() {
        let named = OperationDeclaration::new(7_u8, OperationPolicy::Coexist);
        assert_ne!(named.identity, CancellationIdentity::local(7_u8));
        assert_eq!(named.identity, OperationDeclaration::identity(7_u8));
        assert_ne!(named.identity, OperationDeclaration::identity(7_u16));
    }

    #[test]
    fn passive_wait_owns_no_work_and_does_not_cancel() {
        let lineage = LineageState::new(Weak::new());
        let mut wait = OperationWait::new(vec![Arc::clone(&lineage)]);
        assert!(Pin::new(&mut wait).poll(&mut context()).is_pending());
        drop(wait);
        assert!(!lineage.is_cancelled());
        lineage.finish_registration();
        assert!(lineage.is_finished());
    }

    #[test]
    fn unpolled_cancelling_wait_cancels_only_its_capture() {
        let captured = LineageState::new(Weak::new());
        let later = LineageState::new(Weak::new());
        drop(OperationWait::new(vec![Arc::clone(&captured)]).finish_cancelling_on_drop());
        assert!(captured.is_cancelled());
        assert!(!later.is_cancelled());
        captured.finish_registration();
        later.finish_registration();
    }

    #[test]
    fn cancelling_wait_requests_every_generation_after_one_control_wake_panics() {
        struct PanicWake;

        impl Wake for PanicWake {
            fn wake(self: Arc<Self>) {
                panic!("actor control wake");
            }
        }

        let control = ControlWake::new();
        let first = LineageState::new(Arc::downgrade(&control));
        let second = LineageState::new(Arc::downgrade(&control));
        let mut listener = control.event().listen();
        let waker = Waker::from(Arc::new(PanicWake));
        assert!(
            Pin::new(&mut listener)
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
        let wait = OperationWait::new(vec![Arc::clone(&first), Arc::clone(&second)])
            .finish_cancelling_on_drop();
        let later = LineageState::new(Arc::downgrade(&control));

        let panic = catch_unwind(AssertUnwindSafe(|| drop(wait)))
            .expect_err("actor control wake must propagate its panic");
        first.finish_registration();
        second.finish_registration();
        later.finish_registration();

        assert_eq!(panic.downcast_ref::<&str>(), Some(&"actor control wake"));
        assert!(first.is_cancelled());
        assert!(
            second.is_cancelled(),
            "a panicking first wake must not leave later captured work uncancelled"
        );
        assert!(
            !later.is_cancelled(),
            "uncaptured work must remain untouched"
        );
        assert_eq!(control.epoch(), 2);
    }

    #[test]
    fn next_actual_poll_rechecks_all_captured_generations_and_disarms() {
        let unrelated = LineageState::new(Weak::new());
        let current = LineageState::new(Weak::new());
        let ancestry = OperationAncestry::default().with_generation(Arc::clone(&current));
        let mut wait = OperationWait::new(vec![Arc::clone(&unrelated), Arc::clone(&current)])
            .finish_cancelling_on_drop();
        assert!(Pin::new(&mut wait).poll(&mut context()).is_pending());
        let mut managed = Box::pin(ManagedPoll::new(
            poll_fn(move |cx| Pin::new(&mut wait).poll(cx)),
            ancestry,
        ));
        assert_eq!(
            managed.as_mut().poll(&mut context()),
            Poll::Ready(Err(SelfWait))
        );
        drop(managed);
        assert!(!current.is_cancelled());
        assert!(!unrelated.is_cancelled());
        current.finish_registration();
        unrelated.finish_registration();
    }

    #[test]
    fn terminal_self_wait_conversion_does_not_rearm_cancellation() {
        let lineage = LineageState::new(Weak::new());
        let ancestry = OperationAncestry::default().with_generation(Arc::clone(&lineage));
        let mut wait = OperationWait::new(vec![Arc::clone(&lineage)]);
        {
            let mut managed = Box::pin(ManagedPoll::new(&mut wait, ancestry));
            assert_eq!(
                managed.as_mut().poll(&mut context()),
                Poll::Ready(Err(SelfWait))
            );
        };
        drop(wait.finish_cancelling_on_drop());
        assert!(!lineage.is_cancelled());
        lineage.finish_registration();
    }

    #[test]
    fn completed_captured_ancestor_does_not_self_wait() {
        let lineage = LineageState::new(Weak::new());
        let ancestry = OperationAncestry::default().with_generation(Arc::clone(&lineage));
        lineage.finish_registration();
        let mut managed = Box::pin(ManagedPoll::new(
            OperationWait::new(vec![lineage]),
            ancestry,
        ));
        assert_eq!(managed.as_mut().poll(&mut context()), Poll::Ready(Ok(())));
    }

    #[test]
    fn nested_empty_scope_masks_outer_and_restores_after_pending() {
        let lineage = LineageState::new(Weak::new());
        let ancestry = OperationAncestry::default().with_generation(Arc::clone(&lineage));
        let inner_lineage = Arc::clone(&lineage);
        let mut outer = Box::pin(ManagedPoll::new(
            poll_fn(move |cx| {
                let mut inner = Box::pin(ManagedPoll::new(
                    OperationWait::new(vec![Arc::clone(&inner_lineage)]),
                    (),
                ));
                assert!(inner.as_mut().poll(cx).is_pending());
                let mut restored = OperationWait::new(vec![Arc::clone(&inner_lineage)]);
                assert_eq!(Pin::new(&mut restored).poll(cx), Poll::Ready(Err(SelfWait)));
                Poll::Pending::<()>
            }),
            ancestry,
        ));
        assert!(outer.as_mut().poll(&mut context()).is_pending());
        let mut external = OperationWait::new(vec![Arc::clone(&lineage)]);
        assert!(Pin::new(&mut external).poll(&mut context()).is_pending());
        drop(outer);
        lineage.finish_registration();
    }

    #[test]
    fn nested_named_scope_replaces_outer_and_restores_after_ready() {
        let outer_lineage = LineageState::new(Weak::new());
        let inner_lineage = LineageState::new(Weak::new());
        let outer_ancestry =
            OperationAncestry::default().with_generation(Arc::clone(&outer_lineage));
        let inner_ancestry =
            OperationAncestry::default().with_generation(Arc::clone(&inner_lineage));
        let inner_outer_lineage = Arc::clone(&outer_lineage);
        let mut inner = Box::pin(ManagedPoll::new(
            poll_fn(move |cx| {
                let mut unrelated = OperationWait::new(vec![Arc::clone(&inner_outer_lineage)]);
                assert!(Pin::new(&mut unrelated).poll(cx).is_pending());
                Poll::Ready(())
            }),
            inner_ancestry,
        ));
        let checked_outer_lineage = Arc::clone(&outer_lineage);
        let mut outer = Box::pin(ManagedPoll::new(
            poll_fn(move |cx| {
                assert_eq!(inner.as_mut().poll(cx), Poll::Ready(()));
                let mut restored = OperationWait::new(vec![Arc::clone(&checked_outer_lineage)]);
                assert_eq!(Pin::new(&mut restored).poll(cx), Poll::Ready(Err(SelfWait)));
                Poll::Ready(())
            }),
            outer_ancestry,
        ));
        assert_eq!(outer.as_mut().poll(&mut context()), Poll::Ready(()));
        let mut external = OperationWait::new(vec![Arc::clone(&outer_lineage)]);
        assert!(Pin::new(&mut external).poll(&mut context()).is_pending());
        outer_lineage.finish_registration();
        inner_lineage.finish_registration();
    }

    #[test]
    fn successful_cancelling_wait_disarms_before_its_drop() {
        let lineage = LineageState::new(Weak::new());
        let mut wait = OperationWait::new(vec![Arc::clone(&lineage)]).finish_cancelling_on_drop();
        lineage.finish_registration();
        assert_eq!(
            Pin::new(&mut wait).poll(&mut context()),
            Poll::Ready(Ok(()))
        );
        drop(wait);
        assert!(!lineage.is_cancelled());
    }

    #[test]
    fn managed_scope_restores_on_unwind_and_thread_migration() {
        let lineage = LineageState::new(Weak::new());
        let ancestry = OperationAncestry::default().with_generation(Arc::clone(&lineage));
        let mut panicking = Box::pin(ManagedPoll::new(
            poll_fn(|_| -> Poll<()> {
                panic!("actual user poll");
            }),
            ancestry.clone(),
        ));
        catch_unwind(AssertUnwindSafe(|| panicking.as_mut().poll(&mut context())))
            .as_ref()
            .expect_err("user poll must panic");
        let mut external = OperationWait::new(vec![Arc::clone(&lineage)]);
        assert!(Pin::new(&mut external).poll(&mut context()).is_pending());

        let migrated_lineage = Arc::clone(&lineage);
        let external_lineage = Arc::clone(&lineage);
        let mut migrated = Box::pin(ManagedPoll::new(
            poll_fn(move |cx| {
                let mut self_wait = OperationWait::new(vec![Arc::clone(&migrated_lineage)]);
                assert_eq!(
                    Pin::new(&mut self_wait).poll(cx),
                    Poll::Ready(Err(SelfWait))
                );
                Poll::Pending::<()>
            }),
            ancestry,
        ));
        assert!(migrated.as_mut().poll(&mut context()).is_pending());
        std::thread::spawn(move || {
            assert!(migrated.as_mut().poll(&mut context()).is_pending());
            let mut external = OperationWait::new(vec![external_lineage]);
            assert!(Pin::new(&mut external).poll(&mut context()).is_pending());
        })
        .join()
        .expect("migrated poll completes");
        lineage.finish_registration();
    }

    #[test]
    fn final_named_work_notifies_idle_control() {
        let control = ControlWake::new();
        let lineage = LineageState::new(Arc::downgrade(&control));
        let registration = OperationRegistration::new(Arc::clone(&lineage));
        let ancestry = OperationAncestry::default().with_generation(Arc::clone(&lineage));
        let work = ancestry.retain_work();
        drop(registration);
        assert_eq!(control.epoch(), 0);
        drop(work);
        assert!(lineage.is_finished());
        assert_eq!(control.epoch(), 1);
    }

    #[test]
    fn completion_listener_panic_preserves_terminal_control_notification() {
        struct PanicWake;
        impl Wake for PanicWake {
            fn wake(self: Arc<Self>) {
                panic!("completion listener wake");
            }
        }
        let control = ControlWake::new();
        let lineage = LineageState::new(Arc::downgrade(&control));
        let registration = OperationRegistration::new(Arc::clone(&lineage));
        let ancestry = OperationAncestry::default().with_generation(Arc::clone(&lineage));
        let work = ancestry.retain_work();
        let mut wait = OperationWait::new(vec![Arc::clone(&lineage)]);
        let waker = Waker::from(Arc::new(PanicWake));
        assert!(
            Pin::new(&mut wait)
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
        drop(registration);
        assert!(catch_unwind(AssertUnwindSafe(|| drop(work))).is_err());
        assert!(lineage.is_finished());
        assert_eq!(control.epoch(), 1);
    }

    #[test]
    fn late_admission_rejects_a_completed_ancestor() {
        let lineage = LineageState::new(Weak::new());
        let ancestry = OperationAncestry::default().with_generation(Arc::clone(&lineage));
        lineage.finish_registration();
        assert!(ancestry.try_retain_work().is_err());
        assert!(lineage.is_finished());
    }

    #[test]
    fn rejected_partial_acquisition_keeps_ownership_until_payload_destruction() {
        use std::sync::atomic::{AtomicBool, Ordering};

        struct Payload {
            outer: Arc<LineageState>,
            destroyed: Arc<AtomicBool>,
        }
        impl Drop for Payload {
            fn drop(&mut self) {
                assert!(
                    !self.outer.is_finished(),
                    "completion preceded payload destruction"
                );
                self.destroyed.store(true, Ordering::Release);
            }
        }

        let control = ControlWake::new();
        let outer = LineageState::new(Arc::downgrade(&control));
        let outer_registration = OperationRegistration::new(Arc::clone(&outer));
        let outer_ancestry = OperationAncestry::default().with_generation(Arc::clone(&outer));
        let outer_body = outer_ancestry.retain_work();
        drop(outer_registration);
        let inner = LineageState::new(Weak::new());
        let nested = outer_ancestry.with_generation(Arc::clone(&inner));
        inner.finish_registration();
        let destroyed = Arc::new(AtomicBool::new(false));
        let payload = Payload {
            outer: Arc::clone(&outer),
            destroyed: Arc::clone(&destroyed),
        };

        let Err((reason, partial_work)) = nested.try_retain_work() else {
            panic!("terminal inner ancestor must reject admission");
        };
        assert_eq!(reason, crate::TaskAccessReason::Cancelled);
        drop(outer_body);
        assert!(!outer.is_finished());
        assert_eq!(control.epoch(), 0);
        drop(payload);
        assert!(destroyed.load(Ordering::Acquire));
        assert_eq!(control.epoch(), 0);
        drop(partial_work);
        assert!(outer.is_finished());
        assert_eq!(control.epoch(), 1);
    }
}
}
