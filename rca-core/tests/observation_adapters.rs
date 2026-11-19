use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use rca_core::{ExternalObservation, ObservationProducer};

// Starting at construction or emitting only the latest value loses this initial selection.
#[test]
fn external_activation_is_lazy_and_initial_precedes_coalesced_updates() {
    let starts = Rc::new(Cell::new(0));
    let producer = Rc::new(RefCell::new(None));
    let mut values = ExternalObservation::<_, ()>::new({
        let starts = Rc::clone(&starts);
        let producer = Rc::clone(&producer);
        move |source| {
            starts.set(starts.get() + 1);
            *producer.borrow_mut() = Some(source);
            (Rc::new(10), || {})
        }
    });
    assert_eq!(starts.get(), 0);
    let unpolled = values.next();
    drop(unpolled);
    assert_eq!(starts.get(), 0);
    futures_executor::block_on(async {
        assert_eq!(*values.next().await.unwrap().unwrap(), 10);
        assert_eq!(starts.get(), 1);
        let source = producer.borrow().as_ref().unwrap().clone();
        source.publish(Rc::new(20)).unwrap();
        source.publish(Rc::new(30)).unwrap();
        assert_eq!(*values.next().await.unwrap().unwrap(), 30);
        assert!(source.finish());
        assert_eq!(values.next().await.unwrap(), None);
        assert_eq!(values.next().await.unwrap(), None);
    });
}

// Producer failure must follow the buffered value once, not become cancellation or finish.
#[test]
fn external_failure_drains_latest_once_then_terminates() {
    let mut values = ExternalObservation::new(|source| {
        source.publish(2).unwrap();
        assert!(source.fail(Rc::new("offline")));
        assert_eq!(source.publish(3), Err(3));
        assert!(!source.finish());
        (1, || {})
    });
    futures_executor::block_on(async {
        assert_eq!(values.next().await.unwrap(), Some(1));
        assert_eq!(values.next().await.unwrap(), Some(2));
        let error = values.next().await.unwrap_err();
        assert!(!error.is_cancelled());
        assert_eq!(*error.into_failure().unwrap(), "offline");
        assert_eq!(values.next().await.unwrap(), None);
    });
}

// Cancellation must discard buffered output and deactivate before reentrant shutdown.
#[test]
fn cancellation_rejects_reentrant_shutdown_and_late_values() {
    let shutdowns = Rc::new(Cell::new(0));
    let producer = Rc::new(RefCell::new(None));
    let mut values = ExternalObservation::<_, ()>::new({
        let shutdowns = Rc::clone(&shutdowns);
        let producer = Rc::clone(&producer);
        move |source| {
            *producer.borrow_mut() = Some(source.clone());
            (1, move || {
                shutdowns.set(shutdowns.get() + 1);
                assert_eq!(source.publish(99), Err(99));
                assert!(!source.finish());
            })
        }
    });
    futures_executor::block_on(async {
        assert_eq!(values.next().await.unwrap(), Some(1));
        producer.borrow().as_ref().unwrap().publish(2).unwrap();
        values.cancel();
        assert_eq!(shutdowns.get(), 1);
        assert!(values.next().await.unwrap_err().is_cancelled());
        assert_eq!(values.next().await.unwrap(), None);
        values.cancel();
    });
    drop(values);
    assert_eq!(shutdowns.get(), 1);
}

// Dropping an inactive sequence must not subscribe; dropping an active one must unsubscribe.
#[test]
fn drop_owns_shutdown_without_activating_an_unused_sequence() {
    #[derive(Debug)]
    struct Failure(Rc<Cell<usize>>);

    impl Drop for Failure {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    let unused = ExternalObservation::<usize, ()>::new(|_| {
        panic!("unused sequence must not start");
        #[expect(
            unreachable_code,
            reason = "type-checks the deliberately unreachable start result"
        )]
        (0, || {})
    });
    drop(unused);
    let shutdowns = Rc::new(Cell::new(0));
    let producer: Rc<RefCell<Option<ObservationProducer<usize, Failure>>>> = Rc::default();
    let mut values = ExternalObservation::new({
        let shutdowns = Rc::clone(&shutdowns);
        let producer = Rc::clone(&producer);
        move |source| {
            *producer.borrow_mut() = Some(source);
            (1, move || shutdowns.set(shutdowns.get() + 1))
        }
    });
    assert_eq!(futures_executor::block_on(values.next()).unwrap(), Some(1));
    drop(values);
    assert_eq!(shutdowns.get(), 1);
    assert_eq!(producer.borrow().as_ref().unwrap().publish(9), Err(9));
    let source = producer.borrow_mut().take().unwrap();
    assert!(!source.finish());
    let failure_drops = Rc::new(Cell::new(0));
    assert!(!source.fail(Failure(Rc::clone(&failure_drops))));
    assert_eq!(failure_drops.get(), 1);
    assert_eq!(shutdowns.get(), 1);
    drop(source);
    assert_eq!(failure_drops.get(), 1);
    assert_eq!(shutdowns.get(), 1);
}

// Dropping a pending next must leave activation and the producer owned by the sequence.
#[test]
fn dropping_next_preserves_the_live_subscription_and_wakes_on_publish() {
    let producer = Rc::new(RefCell::new(None));
    let mut values = ExternalObservation::<_, ()>::new({
        let producer = Rc::clone(&producer);
        move |source| {
            *producer.borrow_mut() = Some(source);
            (1, || {})
        }
    });
    assert_eq!(futures_executor::block_on(values.next()).unwrap(), Some(1));
    let waker = futures_util::task::noop_waker();
    let mut context = Context::from_waker(&waker);
    let mut next = Box::pin(values.next());
    assert!(matches!(next.as_mut().poll(&mut context), Poll::Pending));
    drop(next);
    let task = Arc::new(WakeCount(AtomicUsize::new(0)));
    let waker = Waker::from(Arc::clone(&task));
    let mut next = Box::pin(values.next());
    assert!(matches!(
        next.as_mut().poll(&mut Context::from_waker(&waker)),
        Poll::Pending
    ));
    producer.borrow().as_ref().unwrap().publish(2).unwrap();
    assert_eq!(task.0.load(Ordering::SeqCst), 1);
    assert_eq!(futures_executor::block_on(next).unwrap(), Some(2));
}

struct WakeCount(AtomicUsize);
impl Wake for WakeCount {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

// Retaining the cancelled waiter's waker can keep its completed executor task alive.
#[test]
fn dropping_pending_next_releases_its_task_waker() {
    let mut values = ExternalObservation::<_, ()>::new(|_| (1, || {}));
    assert_eq!(futures_executor::block_on(values.next()).unwrap(), Some(1));
    let task = Arc::new(WakeCount(AtomicUsize::new(0)));
    let weak_task = Arc::downgrade(&task);
    let waker = Waker::from(task);
    let mut next = Box::pin(values.next());
    assert!(matches!(
        next.as_mut().poll(&mut Context::from_waker(&waker)),
        Poll::Pending
    ));
    drop(next);
    drop(waker);
    assert!(
        weak_task.upgrade().is_none(),
        "cancelled wait must release its executor task"
    );
}

// A terminal request must wake a waiting consumer and release shutdown once drained.
#[test]
fn finish_wakes_the_consumer_and_shuts_down_once_without_becoming_cancellation() {
    let source = Rc::new(RefCell::new(None));
    let shutdowns = Rc::new(Cell::new(0));
    let mut values = ExternalObservation::<_, ()>::new({
        let source = Rc::clone(&source);
        let shutdowns = Rc::clone(&shutdowns);
        move |producer| {
            *source.borrow_mut() = Some(producer);
            (1, move || shutdowns.set(shutdowns.get() + 1))
        }
    });
    assert_eq!(futures_executor::block_on(values.next()).unwrap(), Some(1));
    let task = Arc::new(WakeCount(AtomicUsize::new(0)));
    let waker = Waker::from(Arc::clone(&task));
    let mut next = Box::pin(values.next());
    assert!(matches!(
        next.as_mut().poll(&mut Context::from_waker(&waker)),
        Poll::Pending
    ));
    assert!(source.borrow().as_ref().unwrap().finish());
    assert_eq!(task.0.load(Ordering::SeqCst), 1);
    assert_eq!(futures_executor::block_on(next).unwrap(), None);
    assert_eq!(shutdowns.get(), 1);
    values.cancel();
    assert_eq!(futures_executor::block_on(values.next()).unwrap(), None);
    drop(values);
    assert_eq!(shutdowns.get(), 1);
}

// Adding domain Clone/Debug/Send bounds would reject these owned local payloads.
#[test]
fn move_only_local_values_and_failures_remain_owned() {
    struct Local(Rc<usize>);
    let mut values = ExternalObservation::new(|source| {
        assert!(matches!(source.publish(Local(Rc::new(2))), Ok(())));
        assert!(source.fail(Local(Rc::new(3))));
        (Local(Rc::new(1)), || {})
    });
    futures_executor::block_on(async {
        assert_eq!(*values.next().await.unwrap().unwrap().0, 1);
        assert_eq!(*values.next().await.unwrap().unwrap().0, 2);
        let Err(error) = values.next().await else {
            panic!("typed local failure")
        };
        assert_eq!(*error.into_failure().unwrap().0, 3);
    });
}

// Cancelling before activation must not subscribe just to report cancellation.
#[test]
fn cancellation_before_first_poll_never_activates() {
    let starts = Rc::new(Cell::new(0));
    let mut values = ExternalObservation::<_, ()>::new({
        let starts = Rc::clone(&starts);
        move |_| {
            starts.set(starts.get() + 1);
            (1, || {})
        }
    });
    values.cancel();
    assert!(
        futures_executor::block_on(values.next())
            .unwrap_err()
            .is_cancelled()
    );
    assert_eq!(futures_executor::block_on(values.next()).unwrap(), None);
    assert_eq!(starts.get(), 0);
}

fn ready<T>(future: impl Future<Output = T>) -> T {
    let mut future = std::pin::pin!(future);
    let Poll::Ready(value) = future
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    else {
        panic!("synchronously completed observation remained pending");
    };
    value
}

// Diagnostics must not activate the producer, expose payloads, or lose its typed failure.
#[test]
fn external_diagnostics_preserve_private_values_and_failure_source() {
    struct Value(&'static str);

    #[derive(Debug)]
    struct Failure(Rc<str>);

    impl std::fmt::Display for Failure {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.0.as_ref())
        }
    }

    impl std::error::Error for Failure {}

    let starts = Rc::new(Cell::new(0));
    let source = Rc::new(RefCell::new(None));
    let failure: Rc<str> = Rc::from("private-producer-failure");
    let mut values = ExternalObservation::new({
        let starts = Rc::clone(&starts);
        let source = Rc::clone(&source);
        let failure = Rc::clone(&failure);
        move |producer| {
            starts.set(starts.get() + 1);
            assert!(producer.publish(Value("private-latest-value")).is_ok());
            assert!(producer.fail(Failure(failure)));
            *source.borrow_mut() = Some(producer);
            (Value("private-initial-value"), || {})
        }
    });
    let mut diagnostics = vec![format!("{values:?}")];
    assert_eq!(starts.get(), 0);
    assert_eq!(
        ready(values.next()).unwrap().unwrap().0,
        "private-initial-value"
    );
    diagnostics.push(format!("{values:?}"));
    diagnostics.push(format!("{:?}", source.borrow().as_ref().unwrap()));
    assert_eq!(starts.get(), 1);
    assert_eq!(
        ready(values.next()).unwrap().unwrap().0,
        "private-latest-value"
    );
    let Err(error) = ready(values.next()) else {
        panic!("producer failure must follow its buffered value");
    };
    diagnostics.push(format!("{error:?}"));
    diagnostics.push(error.to_string());
    assert!(!error.is_cancelled());
    assert!(error.to_string().contains("failed"));
    let cause = std::error::Error::source(&error)
        .unwrap()
        .downcast_ref::<Failure>()
        .unwrap();
    assert!(Rc::ptr_eq(&cause.0, &failure));
    assert_eq!(cause.to_string(), "private-producer-failure");
    assert!(format!("{cause:?}").contains("private-producer-failure"));
    assert!(Rc::ptr_eq(&error.into_failure().unwrap().0, &failure));
    assert!(
        diagnostics
            .iter()
            .all(|rendered| !rendered.contains("private-")),
        "wrapper diagnostics must redact domain payloads"
    );
    assert!(ready(values.next()).unwrap().is_none());
}

// Consumer cancellation must not masquerade as a producer failure in diagnostics or chains.
#[test]
fn cancellation_diagnostics_have_no_producer_failure_source() {
    let mut values = ExternalObservation::<usize, std::io::Error>::new(|_| (1, || {}));
    values.cancel();
    let error = ready(values.next()).unwrap_err();
    assert!(error.is_cancelled());
    assert!(error.to_string().contains("cancelled"));
    assert!(format!("{error:?}").contains("cancelled: true"));
    assert!(std::error::Error::source(&error).is_none());
    assert!(error.into_failure().is_none());
    assert_eq!(ready(values.next()).unwrap(), None);
}

// A buffered payload panic must not skip or reorder the owned external unsubscription.
#[test]
fn retirement_runs_shutdown_even_if_buffered_payload_destructor_panics() {
    struct Payload(bool, Rc<RefCell<Vec<&'static str>>>);
    impl Drop for Payload {
        fn drop(&mut self) {
            if self.0 {
                self.1.borrow_mut().push("payload");
                panic!("buffered payload destructor panic");
            }
        }
    }
    for failure in [false, true] {
        for cancel in [false, true] {
            let events = Rc::new(RefCell::new(Vec::new()));
            let mut values = Some(ExternalObservation::new({
                let events = Rc::clone(&events);
                move |producer| {
                    let payload = Payload(true, Rc::clone(&events));
                    if failure {
                        assert!(producer.fail(payload));
                    } else {
                        assert!(producer.publish(payload).is_ok());
                    }
                    (Payload(false, Rc::clone(&events)), move || {
                        events.borrow_mut().push("shutdown");
                        assert!(
                            producer
                                .publish(Payload(false, Rc::clone(&events)))
                                .is_err()
                        );
                    })
                }
            }));
            drop(ready(values.as_mut().unwrap().next()).unwrap());
            let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if cancel {
                    values.as_mut().unwrap().cancel();
                } else {
                    drop(values.take());
                }
            }));
            panic.expect_err("buffered payload destructor must panic");
            assert_eq!(&*events.borrow(), &["payload", "shutdown"]);
            if let Some(values) = &mut values {
                let Err(error) = ready(values.next()) else {
                    panic!("explicit cancellation must remain observable");
                };
                assert!(error.is_cancelled());
                assert!(ready(values.next()).unwrap().is_none());
                values.cancel();
            }
            drop(values);
            assert_eq!(&*events.borrow(), &["payload", "shutdown"]);
        }
    }
}

#[test]
fn shutdown_panic_cannot_repeat_unsubscription_or_reopen_admission() {
    let calls = Rc::new(Cell::new(0));
    let source = Rc::new(RefCell::new(None));
    let mut values = ExternalObservation::<_, ()>::new({
        let calls = Rc::clone(&calls);
        let source = Rc::clone(&source);
        move |producer| {
            *source.borrow_mut() = Some(producer);
            (1, move || {
                calls.set(calls.get() + 1);
                panic!("shutdown panic");
            })
        }
    });
    assert_eq!(ready(values.next()).unwrap(), Some(1));
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| values.cancel()));
    panic.expect_err("shutdown must panic");
    assert_eq!(calls.get(), 1);
    assert_eq!(source.borrow().as_ref().unwrap().publish(2), Err(2));
    assert!(ready(values.next()).unwrap_err().is_cancelled());
    values.cancel();
    drop(values);
    assert_eq!(calls.get(), 1);
}

// An initial selection returned before suspension needs no inline slot in its owner.
#[test]
fn external_sequence_owner_size_does_not_scale_with_value_payload() {
    let small = std::mem::size_of::<ExternalObservation<u8, ()>>();
    let large = std::mem::size_of::<ExternalObservation<[u8; 4096], ()>>();
    println!("external owner bytes: small={small}, large={large}");
    assert_eq!(
        large, small,
        "values belong to the producer slot or consumer"
    );
}
