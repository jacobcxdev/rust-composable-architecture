//! Shares readiness and terminal state without retaining a Store owner.
//!
//! Readiness is an installation milestone, not a promise of continued authority.
//! Closure is first-writer-wins: later teardown cannot replace its recorded cause.
//! Waiters retain this metadata while polled, but do not keep admission open.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};

use event_listener::Event;

/// The first terminal transition wins; these values are internal atomic tags.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CloseState {
    /// Admission may proceed once structural installation is ready.
    Open = 0,
    /// The actor reached normal shutdown after ordered teardown.
    Graceful = 1,
    /// Ownership loss or an explicit abort ended admission.
    Abortive = 2,
    /// Actor unwind ended admission before another terminal cause was recorded.
    Failed = 3,
}

/// Shared lifecycle metadata, distinct from the count of public Store owners.
#[derive(Debug)]
pub(crate) struct Lifecycle {
    /// Stable root identity can outlive this metadata without retaining it.
    root: Arc<()>,
    /// Release-publishes completed structural installation to admission checks.
    ready: AtomicBool,
    /// Monotonic first terminal cause; only `Open` may transition.
    state: AtomicU8,
    /// Public Store handles only; actors, waiters and weak capabilities do not count.
    owners: AtomicUsize,
    /// Wakes readiness and closure waiters after publishing either state change.
    changed: Event,
}

impl Lifecycle {
    /// Starts one Store owner, ready unless its builder attaches structural mounts.
    pub(crate) fn new() -> Self {
        Self {
            root: Arc::new(()),
            ready: AtomicBool::new(true),
            state: AtomicU8::new(CloseState::Open as u8),
            owners: AtomicUsize::new(1),
            changed: Event::new(),
        }
    }

    /// Shares only identity storage, not this lifecycle or a public Store owner.
    pub(super) fn root_identity(&self) -> Arc<()> {
        Arc::clone(&self.root)
    }

    /// Marks builder-attached mounts pending before the Store is published.
    ///
    /// This is not a runtime reset; readiness waiters rely on pending installation
    /// eventually becoming ready or closed, not returning to pending later.
    pub(super) fn pending_installation(&self) {
        self.ready.store(false, Ordering::Release);
    }

    /// Publishes installation before waking waiters; it does not reopen closure.
    ///
    /// A waker may re-enter lifecycle methods or panic after readiness is visible.
    pub(super) fn mark_ready(&self) {
        self.ready.store(true, Ordering::Release);
        self.changed.notify(usize::MAX);
    }

    /// Samples closure before readiness, without reserving future actor authority.
    ///
    /// Concurrent closure may follow a successful sample. Ordered actor commands
    /// must validate their captured authority again before entering user code.
    pub(super) fn access(&self) -> Result<(), super::task_context::TaskAccessError> {
        use super::task_context::{TaskAccessError, TaskAccessReason, TaskProgress};
        let reason = match self.state.load(Ordering::Acquire) {
            0 if self.ready.load(Ordering::Acquire) => return Ok(()),
            0 => TaskAccessReason::NotReady,
            3 => TaskAccessReason::ActorFailed,
            _ => TaskAccessReason::StoreClosed,
        };
        Err(TaskAccessError::new(reason, TaskProgress::NotStarted))
    }

    /// Waits for readiness or observed closure, sampling only when polled.
    ///
    /// Polling temporarily retains metadata, not Store ownership. Registering
    /// before the second access check closes the notification race while pending.
    pub(super) async fn ready_weak(
        lifecycle: Weak<Self>,
    ) -> Result<(), super::task_context::TaskAccessError> {
        use super::task_context::{TaskAccessError, TaskAccessReason, TaskProgress};
        let Some(lifecycle) = lifecycle.upgrade() else {
            return Err(TaskAccessError::new(
                TaskAccessReason::StoreClosed,
                TaskProgress::NotStarted,
            ));
        };
        loop {
            match lifecycle.access() {
                Err(error) if error.reason() == TaskAccessReason::NotReady => {}
                result => return result,
            }
            let changed = lifecycle.changed.listen();
            match lifecycle.access() {
                Err(error) if error.reason() == TaskAccessReason::NotReady => changed.await,
                result => return result,
            }
        }
    }

    /// Counts a clone made from an existing Store owner, even after closure.
    pub(crate) fn add_owner(&self) {
        self.owners.fetch_add(1, Ordering::Relaxed);
    }

    /// Releases one Store owner and identifies the final owner responsible for abort.
    ///
    /// Callers must close admission separately; metadata references do not count.
    pub(crate) fn drop_owner(&self) -> bool {
        self.owners.fetch_sub(1, Ordering::AcqRel) == 1
    }

    /// Publishes the first non-open cause, then notifies lifecycle waiters.
    ///
    /// Returns whether this call won the transition. Notification may panic after
    /// publication, so callers must protect any required later cleanup on unwind.
    pub(crate) fn close(&self, next: CloseState) -> bool {
        debug_assert_ne!(next, CloseState::Open);
        let changed = self
            .state
            .compare_exchange(
                CloseState::Open as u8,
                next as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok();
        if changed {
            self.changed.notify(usize::MAX);
        }
        changed
    }

    /// Ends ownership authority, then closes transport even if a lifecycle waker panics.
    ///
    /// The borrowed guard preserves normal notification order without relying on
    /// final ingress destruction: pending sends may retain ingress after owner loss.
    pub(super) fn abort<C>(&self, ingress: &super::ingress::CommandIngress<C>) {
        /// Closes transport on normal return and lifecycle-notification unwind.
        struct CloseIngress<'a, C>(
            /// Borrowed admission endpoint; this guard does not add an owner.
            &'a super::ingress::CommandIngress<C>,
        );

        impl<C> Drop for CloseIngress<'_, C> {
            fn drop(&mut self) {
                self.0.close();
            }
        }

        let _ingress = CloseIngress(ingress);
        self.close(CloseState::Abortive);
    }

    /// Waits for terminal state with register-before-recheck wake-up ordering.
    async fn closed(&self) {
        while !self.is_closed() {
            let listener = self.changed.listen();
            if self.is_closed() {
                break;
            }
            listener.await;
        }
    }

    /// Waits without owning the Store; vanished metadata already implies closure.
    pub(crate) async fn wait_weak(lifecycle: Weak<Self>) {
        if let Some(lifecycle) = lifecycle.upgrade() {
            lifecycle.closed().await;
        }
    }

    /// Samples whether any terminal cause has been published.
    pub(crate) fn is_closed(&self) -> bool {
        self.state.load(Ordering::Acquire) != CloseState::Open as u8
    }

    /// Reports whether graceful teardown won the first terminal transition.
    pub(crate) fn was_graceful(&self) -> bool {
        self.state.load(Ordering::Acquire) == CloseState::Graceful as u8
    }
}

#[cfg(test)]
ordinary_test_items! {
mod tests {
    use std::future::Future;
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake, Waker};

    use super::{CloseState, Lifecycle};
    use crate::{TaskAccessReason, TaskProgress};

    #[test]
    fn constructor_owns_stable_root_identity_without_retaining_lifecycle() {
        let lifecycle = Arc::new(Lifecycle::new());
        let other = Lifecycle::new();
        let root = lifecycle.root_identity();
        assert!(Arc::ptr_eq(&root, &lifecycle.root_identity()));
        assert!(!Arc::ptr_eq(&root, &other.root_identity()));
        let weak_lifecycle = Arc::downgrade(&lifecycle);
        let weak_root = Arc::downgrade(&root);
        drop(lifecycle);
        assert!(weak_lifecycle.upgrade().is_none());
        assert!(weak_root.upgrade().is_some());
        drop(root);
        assert!(weak_root.upgrade().is_none());
    }

    #[test]
    fn close_notification_has_no_registration_race() {
        let lifecycle = Arc::new(Lifecycle::new());
        let waiter = {
            let weak = Arc::downgrade(&lifecycle);
            async move { Lifecycle::wait_weak(weak).await }
        };

        assert!(lifecycle.close(CloseState::Abortive));
        futures_executor::block_on(waiter);
        assert!(lifecycle.is_closed());
        assert!(!lifecycle.close(CloseState::Graceful));
    }

    // Catches readiness cached at future construction or before a later close.
    #[test]
    fn ready_checks_first_poll_and_terminal_state_after_notification() {
        for (cause, expected) in [
            (CloseState::Graceful, TaskAccessReason::StoreClosed),
            (CloseState::Abortive, TaskAccessReason::StoreClosed),
            (CloseState::Failed, TaskAccessReason::ActorFailed),
        ] {
            let lifecycle = Arc::new(Lifecycle::new());
            let future = Lifecycle::ready_weak(Arc::downgrade(&lifecycle));
            lifecycle.pending_installation();
            let mut future = std::pin::pin!(future);
            let mut context = Context::from_waker(Waker::noop());
            assert!(future.as_mut().poll(&mut context).is_pending());
            lifecycle.mark_ready();
            assert!(lifecycle.close(cause));
            let Poll::Ready(Err(error)) = future.as_mut().poll(&mut context) else {
                panic!("closure before readiness observation must win");
            };
            assert_eq!(error.reason(), expected);
            assert_eq!(error.progress(), TaskProgress::NotStarted);
            assert_eq!(lifecycle.was_graceful(), cause == CloseState::Graceful);
            assert!(!lifecycle.close(CloseState::Failed));
            lifecycle.mark_ready();
            assert_eq!(lifecycle.access().unwrap_err().reason(), expected);
        }
    }

    struct CloseOnReady(Arc<Lifecycle>);

    impl Wake for CloseOnReady {
        fn wake(self: Arc<Self>) {
            self.0.close(CloseState::Abortive);
        }
    }

    // Catches notification holding a lifecycle lock or readiness winning over close.
    #[test]
    fn readiness_waker_can_close_before_the_waiter_resumes() {
        let lifecycle = Arc::new(Lifecycle::new());
        lifecycle.pending_installation();
        let mut future = std::pin::pin!(Lifecycle::ready_weak(Arc::downgrade(&lifecycle)));
        let waker = Waker::from(Arc::new(CloseOnReady(Arc::clone(&lifecycle))));
        let mut context = Context::from_waker(&waker);
        assert!(future.as_mut().poll(&mut context).is_pending());
        lifecycle.mark_ready();
        let Poll::Ready(Err(error)) = future.as_mut().poll(&mut context) else {
            panic!("re-entrant close must reject readiness");
        };
        assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
    }

    // Catches either weak waiter keeping lifecycle metadata alive before polling.
    #[test]
    fn unpolled_waiters_do_not_retain_metadata() {
        let lifecycle = Arc::new(Lifecycle::new());
        let weak = Arc::downgrade(&lifecycle);
        let ready = Lifecycle::ready_weak(std::sync::Weak::clone(&weak));
        let closed = Lifecycle::wait_weak(std::sync::Weak::clone(&weak));
        drop(lifecycle);
        assert!(weak.upgrade().is_none());
        let error = futures_executor::block_on(ready).unwrap_err();
        assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
        assert_eq!(error.progress(), TaskProgress::NotStarted);
        futures_executor::block_on(closed);
    }
}
}
