//! Tracks complete causal effect lifetimes.
//!
//! A lineage starts with one registration hold. Each tracked descendant owns a work lease,
//! released after its concrete payload. Shared references alone do not hold work.
//! Completion becomes permanent when the last hold ends. Cancellation requests
//! revoke authority separately and do not release those holds.
//!
//! Finish listeners share a permanent interest flag. Paired fences prevent the
//! last release from missing a concurrent listener. Actor control notifications
//! use a separate epoch so the actor can detect changes before it sleeps.

use std::fmt;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::Ordering;
use std::task::{Context, Poll};

#[cfg(all(test, loom))]
use loom::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, fence};
#[cfg(not(all(test, loom)))]
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, fence};

use event_listener::Event;
use event_listener::EventListener;

/// Wakes the actor when control work changes, independently of causal completion.
#[derive(Debug)]
pub(crate) struct ControlWake {
    /// Wrapping change sequence, published before the corresponding event notification.
    epoch: AtomicU64,
    /// Broadcast event for actors that registered before checking their processed epoch.
    changed: Event,
}

impl ControlWake {
    /// Creates the actor's shared change signal with no recorded notifications.
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            epoch: AtomicU64::new(0),
            changed: Event::new(),
        })
    }

    /// Publishes a new control epoch, then wakes every registered actor listener.
    ///
    /// This requests another control pass. It does not acknowledge task destruction.
    pub(crate) fn notify(&self) {
        self.epoch.fetch_add(1, Ordering::Release);
        self.changed.notify(usize::MAX);
    }

    /// Reads the change sequence for comparison with the actor's last processed epoch.
    ///
    /// Equality cannot distinguish a complete wrap between observations. The actor
    /// protocol assumes fewer than 2^64 notifications between checks.
    pub(crate) fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }

    /// Borrows the event for stack-based listener registration in the actor loop.
    ///
    /// Before waiting, register and compare epochs again so a notification cannot be lost.
    pub(crate) fn event(&self) -> &Event {
        &self.changed
    }

    #[cfg(feature = "test-util")]
    /// Registers an owned listener for tests that inspect actor wake behaviour.
    pub(crate) fn listen(&self) -> EventListener {
        self.changed.listen()
    }
}

#[cfg(all(test, loom))]
ordinary_test_items! {
mod control_wake_tests {
    use std::pin::Pin;
    use std::sync::Arc;
    use std::task::{Context, Wake, Waker};

    use loom::sync::atomic::{AtomicBool, Ordering};

    use super::ControlWake;

    struct WakeProbe(AtomicBool);

    impl Wake for WakeProbe {
        fn wake(self: Arc<Self>) {
            self.0.store(true, Ordering::Release);
        }
    }

    #[test]
    fn completion_notification_cannot_leave_registered_actor_asleep() {
        loom::model(|| {
            let control = ControlWake::new();
            let producer_control = Arc::clone(&control);
            let producer = loom::thread::spawn(move || {
                producer_control.notify();
                producer_control.notify();
            });

            let consumer_control = Arc::clone(&control);
            let consumer = loom::thread::spawn(move || {
                // Match the actor: register before comparing with its last processed epoch.
                event_listener::listener!(consumer_control.event() => listener);
                if consumer_control.epoch() != 0 {
                    producer.join().expect("notifications complete");
                    return;
                }
                let probe = Arc::new(WakeProbe(AtomicBool::new(false)));
                let waker = Waker::from(Arc::clone(&probe));
                let pending = Pin::new(&mut listener)
                    .poll(&mut Context::from_waker(&waker))
                    .is_pending();
                producer.join().expect("notifications complete");
                if pending {
                    assert!(
                        probe.0.load(Ordering::Acquire),
                        "pending actor was not woken"
                    );
                    assert!(
                        Pin::new(&mut listener)
                            .poll(&mut Context::from_waker(&waker))
                            .is_ready(),
                        "woken actor lost its notification",
                    );
                }
            });

            consumer.join().expect("actor registers");
            assert_eq!(control.epoch(), 2);
        });
    }
}
}

/// Separates causal completion ownership from cooperative cancellation authority.
///
/// An `Arc` keeps this metadata alive, but only registration and work leases keep
/// completion pending. Callers release each lease after its concrete payload.
pub(crate) struct LineageState {
    /// One initial registration hold plus one count for each live work lease.
    pending: AtomicUsize,
    /// Records the first cancellation request accepted before observed completion.
    cancelled: AtomicBool,
    /// Publishes the final release before any completion listener can wake.
    terminal: AtomicBool,
    /// Remains true after any finish listener registers, even if that waiter leaves.
    finish_interest: AtomicBool,
    /// Notifies finish waiters only after terminal completion is published.
    changed: Event,
    /// Requests actor control without keeping the actor's signal alive.
    control: Weak<ControlWake>,
}

impl LineageState {
    /// Creates a lineage with one registration hold and no cancellation request.
    ///
    /// The registering owner must call [`Self::finish_registration`] exactly once.
    pub(crate) fn new(control: Weak<ControlWake>) -> Arc<Self> {
        Arc::new(Self {
            pending: AtomicUsize::new(1),
            cancelled: AtomicBool::new(false),
            terminal: AtomicBool::new(false),
            finish_interest: AtomicBool::new(false),
            changed: Event::new(),
            control,
        })
    }

    /// Acquires a work lease while the caller already owns unfinished work.
    ///
    /// Keep that ownership through this call. Metadata references alone do not
    /// prevent completion. Late admissions must use [`Self::try_add_work`] instead.
    pub(crate) fn add_work(self: &Arc<Self>) -> LineageWork {
        let previous = self.pending.fetch_add(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "terminal lineage cannot accept work");
        LineageWork {
            lineage: Arc::clone(self),
        }
    }

    /// Acquires a lease only if a concurrent final release has not won.
    ///
    /// A cancelled lineage can still own work. This method tracks that ownership;
    /// callers check cancellation authority separately. A zero count never revives.
    pub(crate) fn try_add_work(self: &Arc<Self>) -> Option<LineageWork> {
        let mut pending = self.pending.load(Ordering::Acquire);
        while pending != 0 {
            match self.pending.compare_exchange_weak(
                pending,
                pending + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(LineageWork {
                        lineage: Arc::clone(self),
                    });
                }
                Err(current) => pending = current,
            }
        }
        None
    }

    /// Releases the initial registration hold after descendant ownership is installed.
    ///
    /// The registering owner calls this exactly once before completion can occur.
    pub(crate) fn finish_registration(&self) {
        self.complete_work();
    }

    /// Releases one hold and publishes completion when no owner remains.
    ///
    /// Callers own the released count. Completion is published before notification,
    /// so a panicking waker cannot leave the lineage nonterminal.
    fn complete_work(&self) {
        if self.pending.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.terminal.store(true, Ordering::Release);
            // Pair with finish's fence: either completion sees interest or the
            // registered waiter sees terminal, so skipping notify cannot strand it.
            fence(Ordering::SeqCst);
            if self.finish_interest.load(Ordering::Acquire) {
                self.changed.notify(usize::MAX);
            }
        }
    }

    /// Records cancellation once and requests actor control if completion was not observed.
    ///
    /// This does not release work. A request concurrent with completion can win;
    /// a request that observes terminal completion has no effect.
    pub(crate) fn cancel(&self) {
        if !self.terminal.load(Ordering::Acquire)
            && !self.cancelled.swap(true, Ordering::AcqRel)
            && let Some(control) = self.control.upgrade()
        {
            control.notify();
        }
    }

    /// Reads whether a cancellation request was accepted, independently of work completion.
    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// Reads permanent completion after the final registration or work hold ended.
    pub(crate) fn is_finished(&self) -> bool {
        self.terminal.load(Ordering::Acquire)
    }

    /// Requests named-generation retirement after its final work release completes.
    ///
    /// Named-release guards call this after the lease, including during unwinding.
    pub(crate) fn notify_control_if_finished(&self) {
        if self.is_finished()
            && let Some(control) = self.control.upgrade()
        {
            control.notify();
        }
    }

    /// Polls completion with caller-owned listener storage for this lineage alone.
    ///
    /// Do not reuse another lineage's listener here. The caller retains the listener
    /// on `Ready` and can disarm cancellation before listener destruction invokes
    /// another waker. Notifications never represent partial progress: only the
    /// final release notifies this event.
    pub(crate) fn poll_finish(
        &self,
        listener: &mut Option<EventListener>,
        cx: &mut Context<'_>,
    ) -> Poll<()> {
        if self.is_finished() {
            return Poll::Ready(());
        }
        if listener.is_none() {
            // Interest is permanent: dropping one waiter must not hide another.
            self.finish_interest.store(true, Ordering::Release);
            *listener = Some(self.changed.listen());
            // Register before the paired fence and terminal recheck. Do not rely
            // on the event implementation to supply this cross-atomic ordering.
            fence(Ordering::SeqCst);
            if self.is_finished() {
                return Poll::Ready(());
            }
        }
        // Only terminal completion notifies this event. The notification publishes
        // that monotone state, so Ready needs neither another check nor a retry.
        // The caller owns removal, allowing cancelling observers to disarm before
        // listener drop can invoke another waker.
        Pin::new(
            listener
                .as_mut()
                .expect("completion listener is registered"),
        )
        .poll(cx)
    }

    /// Waits for complete causal destruction without requesting cancellation on drop.
    pub(crate) async fn finish(&self) {
        let mut listener = None;
        std::future::poll_fn(|cx| self.poll_finish(&mut listener, cx)).await;
    }
}

impl fmt::Debug for LineageState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LineageState")
            .field("pending", &self.pending)
            .field("cancelled", &self.cancelled)
            .field("terminal", &self.terminal)
            .finish_non_exhaustive()
    }
}

/// Owns one unfinished-work count, released once when the lease is dropped.
///
/// The containing carrier must destroy its concrete payload before this lease.
pub(crate) struct LineageWork {
    /// Keeps metadata alive while this lease accounts for unfinished causal work.
    lineage: Arc<LineageState>,
}

impl Drop for LineageWork {
    fn drop(&mut self) {
        self.lineage.complete_work();
    }
}

#[cfg(all(test, loom))]
ordinary_test_items! {
mod finish_loom_tests {
    use std::sync::{Arc, Weak};
    use std::task::{Context, Wake, Waker};

    use loom::sync::atomic::{AtomicBool, Ordering};

    use super::LineageState;

    struct WakeProbe(AtomicBool);

    impl Wake for WakeProbe {
        fn wake(self: Arc<Self>) {
            self.0.store(true, Ordering::Release);
        }
    }

    #[test]
    fn completion_cannot_skip_notification_and_strand_finish() {
        loom::model(|| {
            let lineage = LineageState::new(Weak::new());
            let producer_lineage = Arc::clone(&lineage);
            let producer = loom::thread::spawn(move || {
                producer_lineage.finish_registration();
            });
            let probe = Arc::new(WakeProbe(AtomicBool::new(false)));
            let waker = Waker::from(Arc::clone(&probe));
            let mut finish = std::pin::pin!(lineage.finish());
            let pending = finish
                .as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending();
            if !pending {
                assert!(lineage.is_finished(), "Ready must publish completion");
            }
            producer.join().expect("lineage completes");
            if pending {
                assert!(
                    probe.0.load(Ordering::Acquire),
                    "pending finish was not woken"
                );
                assert!(
                    finish
                        .as_mut()
                        .poll(&mut Context::from_waker(&waker))
                        .is_ready(),
                    "woken finish did not observe completion",
                );
            }
            assert!(lineage.is_finished());
        });
    }

    #[test]
    fn try_add_work_racing_last_release_never_resurrects_completion() {
        loom::model(|| {
            let lineage = LineageState::new(Weak::new());
            let owner = Arc::clone(&lineage);
            let release = loom::thread::spawn(move || owner.finish_registration());
            let work = lineage.try_add_work();
            release.join().expect("registration released");
            let mut finish = std::pin::pin!(lineage.finish());
            let mut context = Context::from_waker(Waker::noop());
            if let Some(work) = work {
                assert!(!lineage.is_finished());
                assert!(finish.as_mut().poll(&mut context).is_pending());
                drop(work);
            }
            assert!(lineage.is_finished());
            assert!(finish.as_mut().poll(&mut context).is_ready());
            assert!(lineage.try_add_work().is_none());
        });
    }
}
}

#[cfg(all(test, not(loom)))]
ordinary_test_items! {
mod finish_tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Weak};
    use std::task::{Context, Wake, Waker};

    use super::{ControlWake, LineageState};

    #[derive(Default)]
    struct WakeCount(AtomicUsize);

    impl Wake for WakeCount {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn cancellation_notifies_once_and_ignores_observed_completion() {
        let control = ControlWake::new();
        let lineage = LineageState::new(Arc::downgrade(&control));
        lineage.cancel();
        assert!(lineage.is_cancelled());
        assert!(!lineage.is_finished());
        assert_eq!(control.epoch(), 1);
        lineage.cancel();
        assert_eq!(control.epoch(), 1);
        lineage.finish_registration();
        lineage.cancel();
        assert_eq!(control.epoch(), 1);

        let completed = LineageState::new(Arc::downgrade(&control));
        completed.finish_registration();
        completed.cancel();
        assert!(completed.is_finished());
        assert!(!completed.is_cancelled());
        assert_eq!(control.epoch(), 1);
    }

    #[test]
    fn completion_wakes_all_remaining_finish_waiters() {
        let lineage = LineageState::new(Weak::new());
        let work = lineage.add_work();
        lineage.finish_registration();
        let first_probe = Arc::new(WakeCount::default());
        let second_probe = Arc::new(WakeCount::default());
        let first_waker = Waker::from(Arc::clone(&first_probe));
        let second_waker = Waker::from(Arc::clone(&second_probe));
        let mut first = Box::pin(lineage.finish());
        let mut second = Box::pin(lineage.finish());
        let mut abandoned = Box::pin(lineage.finish());
        assert!(
            first
                .as_mut()
                .poll(&mut Context::from_waker(&first_waker))
                .is_pending()
        );
        assert!(
            second
                .as_mut()
                .poll(&mut Context::from_waker(&second_waker))
                .is_pending()
        );
        assert!(
            abandoned
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        drop(abandoned);
        assert!(!lineage.is_finished());

        drop(work);

        assert!(lineage.is_finished());
        assert!(first_probe.0.load(Ordering::Relaxed) > 0);
        assert!(second_probe.0.load(Ordering::Relaxed) > 0);
        assert!(
            first
                .as_mut()
                .poll(&mut Context::from_waker(&first_waker))
                .is_ready()
        );
        assert!(
            second
                .as_mut()
                .poll(&mut Context::from_waker(&second_waker))
                .is_ready()
        );
    }

    #[test]
    fn try_add_work_before_last_release_keeps_finish_pending() {
        let lineage = LineageState::new(Weak::new());
        let owner = Arc::clone(&lineage);
        let (release, released) = std::sync::mpsc::sync_channel(0);
        let thread = std::thread::spawn(move || {
            released.recv().expect("release registration");
            owner.finish_registration();
        });
        let work = lineage.try_add_work().expect("registration is still held");
        release.send(()).expect("release owner");
        thread.join().expect("registration released");
        let mut finish = std::pin::pin!(lineage.finish());
        let mut context = Context::from_waker(Waker::noop());
        assert!(finish.as_mut().poll(&mut context).is_pending());
        drop(work);
        assert!(finish.as_mut().poll(&mut context).is_ready());
        assert!(lineage.try_add_work().is_none());
    }

    #[test]
    fn try_add_work_after_last_release_cannot_resurrect_completion() {
        let lineage = LineageState::new(Weak::new());
        let owner = Arc::clone(&lineage);
        std::thread::spawn(move || owner.finish_registration())
            .join()
            .expect("registration released");
        assert!(lineage.try_add_work().is_none());
        assert!(lineage.is_finished());
        let mut finish = std::pin::pin!(lineage.finish());
        assert!(finish.as_mut().poll(&mut Context::from_waker(Waker::noop())).is_ready());
    }

    #[test]
    fn try_add_work_tracks_completion_not_cancellation_authority() {
        let lineage = LineageState::new(Weak::new());
        lineage.cancel();
        let work = lineage.try_add_work().expect("cancelled work is still owned");
        lineage.finish_registration();
        assert!(lineage.is_cancelled());
        assert!(!lineage.is_finished());
        drop(work);
        assert!(lineage.is_finished());
        assert!(lineage.try_add_work().is_none());
    }

    #[test]
    fn contended_work_admission_preserves_last_completion_owner() {
        let lineage = LineageState::new(Weak::new());
        let last_owner = lineage.add_work();
        lineage.finish_registration();
        let start = std::sync::Barrier::new(4);
        std::thread::scope(|threads| {
            for _ in 0..4 {
                threads.spawn(|| {
                    start.wait();
                    for _ in 0..4096 {
                        let work = lineage
                            .try_add_work()
                            .expect("last owner keeps admission open");
                        assert!(!lineage.is_finished());
                        drop(work);
                    }
                });
            }
        });
        assert!(!lineage.is_finished());
        drop(last_owner);
        assert!(lineage.is_finished());
        assert!(lineage.try_add_work().is_none());
    }

    #[test]
    fn diagnostics_lineage_distinguishes_cancellation_from_completion() {
        let lineage = LineageState::new(Weak::new());
        let work = lineage.add_work();
        lineage.finish_registration();
        let task = crate::ActionTask::live(Arc::clone(&lineage));
        assert_eq!(
            format!("{lineage:?}"),
            "LineageState { pending: 1, cancelled: false, terminal: false, .. }",
        );
        assert_eq!(format!("{task:?}"), "ActionTask::Live");
        assert!(!task.is_finished());
        assert!(!task.is_cancelled());

        task.cancel();
        assert_eq!(
            format!("{lineage:?}"),
            "LineageState { pending: 1, cancelled: true, terminal: false, .. }",
        );
        assert!(!task.is_finished());
        drop(work);
        assert_eq!(
            format!("{lineage:?}"),
            "LineageState { pending: 0, cancelled: true, terminal: true, .. }",
        );
        assert_eq!(format!("{task:?}"), "ActionTask::Live");
        assert!(task.is_finished());
        assert!(task.is_cancelled());
    }

    #[test]
    fn finish_after_completion_is_ready_on_first_poll() {
        let lineage = LineageState::new(Weak::new());
        lineage.finish_registration();
        let mut finish = std::pin::pin!(lineage.finish());
        assert!(
            finish
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_ready()
        );
    }
}
}
