//! Couples mailbox admission and closure with the actor's control wake.
//!
//! A command is visible in the queue before its control notification. Rejected
//! submissions do not advance the wake epoch. The final ingress owner closes
//! admission before waking the actor, so it cannot mistake owner loss for idleness.

use std::sync::Arc;

use super::command::Command;
use crate::effect::lineage::ControlWake;

/// Shared-family command ingress; Local uses the same transport with Local carriers.
pub(crate) type Ingress<State, Action> = CommandIngress<Command<State, Action>>;

/// Owns the sole sender and publishes actor wake-ups for every accepted command.
///
/// Shared owners clone their `Arc` to this object, not the transport endpoint.
/// The sender's drop follows this object's close-and-notify destructor.
pub(crate) struct CommandIngress<C> {
    /// Sole transport sender; dropping the final ingress closes producer admission.
    sender: super::mailbox::Sender<C>,
    /// Actor control signal, also shared with non-mailbox lifecycle events.
    pub(super) control: Arc<ControlWake>,
}

impl<C> CommandIngress<C> {
    /// Pairs the sole sender with the control signal already observed by its actor.
    pub(super) fn new(sender: super::mailbox::Sender<C>, control: Arc<ControlWake>) -> Self {
        Self { sender, control }
    }

    /// Waits for admission, then wakes the actor before retiring capacity interest.
    ///
    /// Cancellation before admission drops the input; afterwards it cannot retract
    /// the command. A control-waker panic likewise leaves admission intact.
    pub(super) fn send(
        &self,
        command: C,
    ) -> impl Future<Output = Result<(), async_channel::SendError<C>>> {
        self.sender.send(command, || self.control.notify())
    }

    /// Waits with typed ownership, converting only when a queue slot is available.
    ///
    /// `make` must obey [`super::mailbox::Sender::try_send_map`]'s constructor-only
    /// contract: it runs under the queue lock and must not invoke user code.
    pub(super) fn send_map<T>(
        &self,
        input: T,
        make: fn(T) -> C,
    ) -> impl Future<Output = Result<(), async_channel::SendError<T>>> {
        self.sender.send_map(input, make, || self.control.notify())
    }

    /// Tries immediate admission and notifies only when the queue accepted ownership.
    pub(super) fn try_send(&self, command: C) -> Result<(), async_channel::TrySendError<C>> {
        self.sender.try_send(command)?;
        self.control.notify();
        Ok(())
    }

    /// Returns rejected typed input unchanged, without conversion or actor wake-up.
    ///
    /// On success, `make` runs under the same constructor-only contract as
    /// [`Self::send_map`], and the actor is notified after the queue lock is released.
    pub(super) fn try_send_map<T>(
        &self,
        input: T,
        make: fn(T) -> C,
    ) -> Result<(), async_channel::TrySendError<T>> {
        self.sender.try_send_map(input, make)?;
        self.control.notify();
        Ok(())
    }

    /// Publishes closure once, then wakes the actor and all blocked producers.
    ///
    /// The mailbox's unwind guard still wakes producers if the actor waker panics.
    /// Returns whether this call changed admission state, without draining commands.
    pub(super) fn close(&self) -> bool {
        self.sender.close(|| self.control.notify())
    }
}

impl<C> Drop for CommandIngress<C> {
    fn drop(&mut self) {
        // Close before notifying, including automatic loss of the final sender.
        // The actor must observe closure when it consumes this control wake.
        self.close();
    }
}

#[cfg(test)]
ordinary_test_items! {
mod tests {
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll, Wake, Waker};

    use super::Ingress;
    use crate::effect::lineage::ControlWake;
    use crate::store::command::{ActionCommand, Command, commit_channel};

    struct Mailbox {
        ingress: Ingress<(), ()>,
        receiver: crate::store::mailbox::Receiver<Command<(), ()>>,
        control: Arc<ControlWake>,
    }

    impl Mailbox {
        fn new() -> Self {
            let (sender, receiver) = crate::store::mailbox::bounded(
                std::num::NonZeroUsize::new(1).expect("test capacity is nonzero"),
            );
            let control = ControlWake::new();
            Self {
                ingress: Ingress::new(sender, Arc::clone(&control)),
                receiver,
                control,
            }
        }
    }

    fn command() -> Command<(), ()> {
        let (reply, _receive) = commit_channel();
        Command::Action(ActionCommand::external((), reply))
    }

    #[derive(Default)]
    struct Notifications(AtomicUsize);

    impl Wake for Notifications {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn admission_before_registration_remains_visible_after_epoch_is_absorbed() {
        let mailbox = Mailbox::new();
        assert!(matches!(mailbox.ingress.try_send(command()), Ok(())));
        let processed_epoch = mailbox.control.epoch();
        event_listener::listener!(mailbox.control.event() => listener);
        assert_eq!(mailbox.control.epoch(), processed_epoch);
        assert!(matches!(
            mailbox.receiver.try_recv(),
            Ok(Command::Action(_))
        ));
        assert!(
            Pin::new(&mut listener)
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
    }

    #[test]
    fn admission_between_empty_check_and_first_listener_poll_is_ready() {
        let mailbox = Mailbox::new();
        let processed_epoch = mailbox.control.epoch();
        event_listener::listener!(mailbox.control.event() => listener);
        assert_eq!(mailbox.control.epoch(), processed_epoch);
        assert!(matches!(
            mailbox.receiver.try_recv(),
            Err(async_channel::TryRecvError::Empty)
        ));
        assert!(matches!(mailbox.ingress.try_send(command()), Ok(())));
        assert!(
            Pin::new(&mut listener)
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_ready()
        );
        assert!(matches!(
            mailbox.receiver.try_recv(),
            Ok(Command::Action(_))
        ));
    }

    #[test]
    fn async_admission_wakes_already_pending_control_listener() {
        let mailbox = Mailbox::new();
        let notifications = Arc::new(Notifications::default());
        let waker = Waker::from(Arc::clone(&notifications));
        let mut cx = Context::from_waker(&waker);
        event_listener::listener!(mailbox.control.event() => listener);
        assert!(matches!(
            mailbox.receiver.try_recv(),
            Err(async_channel::TryRecvError::Empty)
        ));
        assert!(Pin::new(&mut listener).poll(&mut cx).is_pending());
        let mut send = std::pin::pin!(mailbox.ingress.send(command()));
        assert!(matches!(
            send.as_mut().poll(&mut Context::from_waker(Waker::noop())),
            Poll::Ready(Ok(()))
        ));
        assert!(notifications.0.load(Ordering::SeqCst) > 0);
        assert!(Pin::new(&mut listener).poll(&mut cx).is_ready());
        assert!(matches!(
            mailbox.receiver.try_recv(),
            Ok(Command::Action(_))
        ));
    }

    #[test]
    fn final_ingress_drop_wakes_listener_after_empty_check_and_closes_queue() {
        let mailbox = Mailbox::new();
        let notifications = Arc::new(Notifications::default());
        let waker = Waker::from(Arc::clone(&notifications));
        let mut cx = Context::from_waker(&waker);
        event_listener::listener!(mailbox.control.event() => listener);
        assert!(matches!(
            mailbox.receiver.try_recv(),
            Err(async_channel::TryRecvError::Empty)
        ));
        assert!(Pin::new(&mut listener).poll(&mut cx).is_pending());
        drop(mailbox.ingress);
        assert!(notifications.0.load(Ordering::SeqCst) > 0);
        assert!(Pin::new(&mut listener).poll(&mut cx).is_ready());
        assert!(matches!(
            mailbox.receiver.try_recv(),
            Err(async_channel::TryRecvError::Closed)
        ));
    }

    #[test]
    fn full_and_closed_admissions_do_not_advance_control_epoch() {
        let mailbox = Mailbox::new();
        assert!(matches!(mailbox.ingress.try_send(command()), Ok(())));
        let admitted_epoch = mailbox.control.epoch();
        assert!(matches!(
            mailbox.ingress.try_send(command()),
            Err(async_channel::TrySendError::Full(_))
        ));
        assert_eq!(mailbox.control.epoch(), admitted_epoch);
        assert!(mailbox.ingress.close());
        let closed_epoch = mailbox.control.epoch();
        assert!(matches!(
            mailbox.ingress.try_send(command()),
            Err(async_channel::TrySendError::Closed(_))
        ));
        assert_eq!(mailbox.control.epoch(), closed_epoch);
    }
}
}
