//! Erases ordered state query types inside the actor.
//!
//! The mailbox orders queries. The reply channel only transfers their results.
//! After admission, waiter cancellation does not retract a query.

use futures_channel::oneshot;

use super::scope::SelectedChild;

/// Runs one admitted query without exposing its result type to the actor.
pub(crate) trait ReadCommand<State> {
    /// Runs the query once against state at its ordered mailbox position.
    ///
    /// Query and reply-waker panics propagate to the actor. A missing receiver
    /// discards the result, not the query.
    fn execute(self: Box<Self>, state: &State);
}

/// Runs a scoped query against the exact child already validated by the actor.
pub(crate) trait CheckedReadCommand<State> {
    /// Runs only after the actor gate accepts the selected child's lifetime.
    fn execute(self: Box<Self>, state: &State, validation: &SelectedChild<'_>);
}

impl<State, F, Output> CheckedReadCommand<State> for TypedRead<F, Output>
where
    F: FnOnce(&State, &SelectedChild<'_>) -> Output + 'static,
    Output: 'static,
{
    fn execute(self: Box<Self>, state: &State, validation: &SelectedChild<'_>) {
        let output = (self.query)(state, validation);
        let _ = self.reply.send(output);
    }
}

/// Prepares a scoped query whose selection and validation share one borrowed child.
pub(crate) fn new_checked_read<State, F, Output>(
    query: F,
) -> (
    Box<dyn CheckedReadCommand<State> + Send>,
    oneshot::Receiver<Output>,
)
where
    F: FnOnce(&State, &SelectedChild<'_>) -> Output + Send + 'static,
    Output: Send + 'static,
{
    let (command, receive) = prepare_read(query);
    (Box::new(command), receive)
}

/// Owns a query until actor execution or command disposal.
struct TypedRead<F, Output> {
    /// Consumed only by execution. Command disposal drops its captures uncalled.
    query: F,
    /// Transfers one result, or closes without a result on command disposal.
    reply: oneshot::Sender<Output>,
}

impl<State, F, Output> ReadCommand<State> for TypedRead<F, Output>
where
    F: FnOnce(&State) -> Output + 'static,
    Output: 'static,
{
    fn execute(self: Box<Self>, state: &State) {
        let output = (self.query)(state);
        let _ = self.reply.send(output);
    }
}

/// Prepares a cross-thread command and reply without admitting the command.
pub(crate) fn new_read<State, F, Output>(
    query: F,
) -> (
    Box<dyn ReadCommand<State> + Send>,
    oneshot::Receiver<Output>,
)
where
    F: FnOnce(&State) -> Output + Send + 'static,
    Output: Send + 'static,
{
    let (command, receive) = prepare_read(query);
    (Box::new(command), receive)
}

#[cfg(feature = "local")]
/// Prepares an owner-local command without imposing `Send` on captures or results.
pub(crate) fn new_local_read<State, F, Output>(
    query: F,
) -> (Box<dyn ReadCommand<State>>, oneshot::Receiver<Output>)
where
    F: FnOnce(&State) -> Output + 'static,
    Output: 'static,
{
    let (command, receive) = prepare_read(query);
    (Box::new(command), receive)
}

/// Pairs query ownership with one result slot before carrier-specific erasure.
fn prepare_read<F, Output>(query: F) -> (TypedRead<F, Output>, oneshot::Receiver<Output>) {
    let (reply, receive) = oneshot::channel();
    (TypedRead { query, reply }, receive)
}

#[cfg(test)]
ordinary_test_items! {
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll, Wake, Waker};

    use super::new_read;

    #[test]
    fn discarded_command_releases_query_without_entering_it() {
        let capture = Arc::new(());
        let weak = Arc::downgrade(&capture);
        let (command, receive) = new_read(move |(): &()| {
            std::hint::black_box(capture);
            panic!("discarded query must not run");
        });

        drop(command);

        assert!(weak.upgrade().is_none());
        assert!(futures_executor::block_on(receive).is_err());
    }

    #[test]
    fn abandoned_reply_does_not_cancel_query_and_drops_output() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&calls);
        let output = Arc::new(());
        let weak = Arc::downgrade(&output);
        let (command, receive) = new_read(move |state: &usize| {
            assert_eq!(*state, 7);
            counted.fetch_add(1, Ordering::Relaxed);
            output
        });
        drop(receive);

        command.execute(&7);

        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn query_panic_closes_reply_and_releases_capture() {
        let capture = Arc::new(());
        let weak = Arc::downgrade(&capture);
        let (command, receive) = new_read(move |(): &()| {
            std::hint::black_box(capture);
            panic!("query panic");
        });

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            command.execute(&());
        }));

        assert!(result.is_err());
        assert!(weak.upgrade().is_none());
        assert!(futures_executor::block_on(receive).is_err());
    }

    #[test]
    fn receiver_wake_panic_does_not_retract_written_reply() {
        struct PanicWake;

        impl Wake for PanicWake {
            fn wake(self: Arc<Self>) {
                panic!("reply wake panic");
            }
        }

        let (command, receive) = new_read(|state: &usize| *state);
        let mut receive = std::pin::pin!(receive);
        let waker = Waker::from(Arc::new(PanicWake));
        assert!(
            receive.as_mut().poll(&mut Context::from_waker(&waker)).is_pending()
        );

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            command.execute(&7);
        }));

        assert!(result.is_err());
        assert!(matches!(
            receive.as_mut().poll(&mut Context::from_waker(Waker::noop())),
            Poll::Ready(Ok(7))
        ));
    }

    #[cfg(feature = "local")]
    #[test]
    fn checked_local_query_keeps_non_send_captures_and_results() {
        use std::cell::Cell;
        use std::rc::Rc;
        use std::sync::Weak;

        use crate::store::scope::{
            HandleRegistrationCommandCore, HandleRegistryCore, HandleSelector, HandleSignal,
            ScopeIdentity, TypedHandleSelector,
        };

        let state = Rc::new(7_usize);
        let mut handles: HandleRegistryCore<dyn HandleSelector<Rc<usize>>> =
            HandleRegistryCore::new();
        let (reply, receive) = async_channel::bounded(1);
        handles.register(
            &state,
            HandleRegistrationCommandCore {
                identity: Box::new(TypedHandleSelector::new(|state: &Rc<usize>| {
                    Some((ScopeIdentity::unit(), state))
                })),
                signal: HandleSignal::new(Weak::new()),
                reply,
            },
        );
        let handle = receive.try_recv().unwrap().unwrap();
        let calls = Rc::new(Cell::new(0));
        let counted = Rc::clone(&calls);
        let owner = std::thread::current().id();
        let (command, receive) = super::prepare_read(
            move |root: &Rc<usize>, selected: &super::SelectedChild<'_>| {
                assert_eq!(std::thread::current().id(), owner);
                let child = selected.downcast::<Rc<usize>>();
                assert!(Rc::ptr_eq(root, child));
                counted.set(counted.get() + 1);
                Rc::clone(child)
            },
        );
        let command: Box<dyn super::CheckedReadCommand<Rc<usize>>> = Box::new(command);

        assert_eq!(
            handles.with_validation(&state, &handle, |selected| {
                command.execute(&state, selected);
            }),
            Some(())
        );

        let output = futures_executor::block_on(receive).unwrap();
        assert!(Rc::ptr_eq(&state, &output));
        assert_eq!(calls.get(), 1);
        assert_eq!(Rc::strong_count(&calls), 1);
        drop(output);
        assert_eq!(Rc::strong_count(&state), 1);
    }

    #[cfg(feature = "local")]
    #[test]
    fn local_query_can_capture_and_return_owner_local_values() {
        let capture = std::rc::Rc::new(std::cell::Cell::new(0));
        let counted = std::rc::Rc::clone(&capture);
        let owner = std::thread::current().id();
        let (command, receive) = super::new_local_read(move |state: &usize| {
            assert_eq!(std::thread::current().id(), owner);
            counted.set(*state);
            counted
        });

        command.execute(&7);
        let output = futures_executor::block_on(receive).unwrap();

        assert!(std::rc::Rc::ptr_eq(&capture, &output));
        assert_eq!(output.get(), 7);
        drop(output);
        assert_eq!(std::rc::Rc::strong_count(&capture), 1);
    }
}
}
