//! Finite polling for already-created deterministic host futures, including Local.

use std::future::Future;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::task::{Context, Poll, Wake, Waker};

/// Polls self-contained work at most 65,536 times, accepting only fresh wakes.
///
/// This does not wait for external signals or advance timers. Each poll and
/// destructor must return. An error drops this future, not tasks already admitted
/// to its host; the caller remains responsible for their cancellation and teardown.
pub(crate) fn finish<F: Future>(future: F) -> Result<F::Output, &'static str> {
    struct Signal(AtomicBool);
    impl Wake for Signal {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.store(true, Ordering::Release);
        }
    }
    let signal = Arc::new(Signal(AtomicBool::new(false)));
    let waker = Waker::from(Arc::clone(&signal));
    let mut context = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    for _ in 0..65_536 {
        signal.0.store(false, Ordering::Release);
        if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return Ok(output);
        }
        if !signal.0.load(Ordering::Acquire) {
            return Err("deterministic fixture became quiescent");
        }
    }
    Err("deterministic fixture exceeded outer poll budget")
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::future::{poll_fn, ready};
    use std::rc::Rc;
    use std::task::Poll;

    use super::finish;

    #[test]
    fn returns_ready_value_without_send_bounds() {
        let value = Rc::new(Cell::new(7));
        let actual = finish(ready(value.clone())).unwrap();
        assert!(Rc::ptr_eq(&actual, &value));
        assert_eq!(actual.get(), 7);
    }

    #[test]
    fn honours_a_real_wake_and_rejects_quiescence() {
        let mut polls = 0;
        let result = finish(poll_fn(|context| {
            polls += 1;
            if polls == 1 {
                context.waker().wake_by_ref();
                Poll::Pending
            } else {
                Poll::Ready(11)
            }
        }));
        assert_eq!(result, Ok(11));
        assert_eq!(polls, 2);
        assert_eq!(
            finish(poll_fn(|_| Poll::<()>::Pending)),
            Err("deterministic fixture became quiescent")
        );
        let mut polls = 0;
        let result = finish(poll_fn(|context| {
            polls += 1;
            if polls == 1 {
                context.waker().wake_by_ref();
            }
            Poll::<()>::Pending
        }));
        assert_eq!(result, Err("deterministic fixture became quiescent"));
        assert_eq!(
            polls, 2,
            "a previous poll's wake cannot authorise a later poll"
        );
    }

    #[test]
    fn bounded_wake_loop_drops_its_owned_future() {
        struct Owner(Rc<Cell<usize>>);
        impl Drop for Owner {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }
        let dropped = Rc::new(Cell::new(0));
        let owner = Owner(dropped.clone());
        let polls = Rc::new(Cell::new(0));
        let observed_polls = Rc::clone(&polls);
        let result = finish(poll_fn(move |context| {
            let _ = &owner;
            observed_polls.set(observed_polls.get() + 1);
            context.waker().wake_by_ref();
            Poll::<()>::Pending
        }));
        assert_eq!(
            result,
            Err("deterministic fixture exceeded outer poll budget")
        );
        assert_eq!(polls.get(), 65_536);
        assert_eq!(dropped.get(), 1);
    }
}
