//! Checks action ownership before admission, after admission and during effect cancellation.

#[expect(dead_code, reason = "effect-send tests manually poll runtime tasks")]
mod support;

use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::task::{Context, Poll, Waker};

use rca_core::{ActionTask, Effect, EffectSender, ReduceContext, Reducer, RuntimeFuture, Store};

/// A move-only, non-`Sync` payload counts its one destruction independently of commits.
struct Child {
    value: Cell<usize>,
    drops: Arc<AtomicUsize>,
}

impl Drop for Child {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

/// Separates effect launch, mailbox occupation and a counted payload commit.
enum Action {
    Start,
    Blocker,
    Value(Child),
}

/// Exports one sender and records blocker and payload commits with distinct totals.
struct Feature {
    export: Option<mpsc::Sender<EffectSender<Action>>>,
    commits: Arc<AtomicUsize>,
}

impl Reducer for Feature {
    type State = ();
    type Action = Action;

    fn reduce(
        &mut self,
        (): &mut (),
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        match action {
            Action::Start => {
                let export = self.export.take().unwrap();
                Effect::run(move |sender| async move {
                    export.send(sender).unwrap();
                    std::future::pending::<()>().await;
                })
            }
            Action::Blocker => {
                self.commits.fetch_add(100, Ordering::SeqCst);
                Effect::none()
            }
            Action::Value(child) => {
                self.commits.fetch_add(child.value.get(), Ordering::SeqCst);
                Effect::none()
            }
        }
    }
}

/// Holds an exported sender and every manually polled task for one bounded mailbox.
///
/// No executor thread runs in the background. Tests choose the admission, commit,
/// cancellation and drop order; dropping the remaining fields releases pending work.
struct Fixture {
    _runtime: support::ManualRuntime,
    store: Store<(), Action>,
    actor: RuntimeFuture,
    run: RuntimeFuture,
    task: ActionTask,
    sender: EffectSender<Action>,
    commits: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
}

impl Fixture {
    /// Commits launch and polls the effect once so its sender is available.
    fn new() -> Self {
        let runtime = support::ManualRuntime::new();
        let (export, receive) = mpsc::channel();
        let commits = Arc::new(AtomicUsize::new(0));
        let store = Store::builder(
            (),
            Feature {
                export: Some(export),
                commits: Arc::clone(&commits),
            },
            runtime.runtime(),
        )
        .mailbox_capacity(1)
        .build()
        .unwrap();
        let mut actor = runtime.take_only_queued_task();
        let receipt = store.try_send(Action::Start).ok().unwrap();
        let mut cx = Context::from_waker(Waker::noop());
        assert!(actor.as_mut().poll(&mut cx).is_pending());
        let task = futures_executor::block_on(receipt.finish()).unwrap();
        let mut run = runtime.take_only_queued_task();
        assert!(run.as_mut().poll(&mut cx).is_pending());
        let sender = receive.try_recv().unwrap();
        Self {
            _runtime: runtime,
            store,
            actor,
            run,
            task,
            sender,
            commits,
            drops: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Creates a payload adding seven to the total and sharing the drop witness.
    fn action(&self) -> Action {
        Action::Value(Child {
            value: Cell::new(7),
            drops: Arc::clone(&self.drops),
        })
    }
}

#[test]
fn full_mailbox_effect_send_recovers_capacity_and_acknowledges_commit() {
    let mut fixture = Fixture::new();
    let blocker = fixture.store.try_send(Action::Blocker).ok().unwrap();
    let mut send = Box::pin(fixture.sender.send(fixture.action()));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(send.as_mut().poll(&mut cx).is_pending());
    assert_eq!(format!("{:?}", fixture.sender), "EffectSender { .. }");
    assert_eq!(fixture.commits.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.drops.load(Ordering::SeqCst), 0);
    assert!(fixture.actor.as_mut().poll(&mut cx).is_pending());
    drop(futures_executor::block_on(blocker.finish()).unwrap());
    assert_eq!(fixture.commits.load(Ordering::SeqCst), 100);
    assert!(send.as_mut().poll(&mut cx).is_pending());
    assert!(fixture.actor.as_mut().poll(&mut cx).is_pending());
    assert!(matches!(send.as_mut().poll(&mut cx), Poll::Ready(Ok(()))));
    assert_eq!(fixture.commits.load(Ordering::SeqCst), 107);
    assert_eq!(fixture.drops.load(Ordering::SeqCst), 1);
}

#[test]
fn final_owner_close_wakes_actor_when_producer_waker_panics() {
    check_final_owner_close(false);
}

#[test]
fn final_owner_close_wakes_producers_when_actor_waker_panics() {
    check_final_owner_close(true);
}

/// Checks both close notifications even when either registered waker panics.
fn check_final_owner_close(panicking_actor: bool) {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::task::Wake;

    /// Counts notification before optionally raising the expected panic payload.
    struct CloseWake {
        calls: AtomicUsize,
        panic_message: Option<&'static str>,
    }
    impl Wake for CloseWake {
        fn wake(self: Arc<Self>) {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(message) = self.panic_message {
                std::panic::panic_any(message);
            }
        }
    }

    let mut fixture = Fixture::new();
    let blocker = fixture.store.try_send(Action::Blocker).ok().unwrap();
    let mut first = Box::pin(fixture.sender.send(fixture.action()));
    let mut second = Box::pin(fixture.sender.send(fixture.action()));
    let mut cx = Context::from_waker(Waker::noop());
    let producer = Arc::new(CloseWake {
        calls: AtomicUsize::new(0),
        panic_message: (!panicking_actor).then_some("producer close panic"),
    });
    let producer_waker = Waker::from(Arc::clone(&producer));
    assert!(first.as_mut().poll(&mut cx).is_pending());
    assert!(
        second
            .as_mut()
            .poll(&mut Context::from_waker(&producer_waker))
            .is_pending()
    );
    let actor = Arc::new(CloseWake {
        calls: AtomicUsize::new(0),
        panic_message: panicking_actor.then_some("actor close panic"),
    });
    let actor_waker = Waker::from(Arc::clone(&actor));
    assert!(
        fixture
            .actor
            .as_mut()
            .poll(&mut Context::from_waker(&actor_waker))
            .is_pending()
    );
    assert_eq!(fixture.commits.load(Ordering::SeqCst), 100);
    assert_eq!(producer.calls.load(Ordering::SeqCst), 0);
    actor.calls.store(0, Ordering::SeqCst);

    let panic = catch_unwind(AssertUnwindSafe(|| drop(fixture.store))).unwrap_err();
    let expected = if panicking_actor {
        "actor close panic"
    } else {
        "producer close panic"
    };
    assert_eq!(panic.downcast_ref::<&str>(), Some(&expected));
    let actor_calls = actor.calls.load(Ordering::SeqCst);
    let producer_calls = producer.calls.load(Ordering::SeqCst);
    for send in [&mut first, &mut second] {
        let Poll::Ready(Err(error)) = send.as_mut().poll(&mut cx) else {
            panic!("final owner closure must reject pending effect sends");
        };
        assert!(error.is_store_stopped());
    }
    drop(first);
    drop(second);
    let finished = fixture.actor.as_mut().poll(&mut cx).is_ready();
    if !finished {
        // A separate real control event keeps the failing baseline teardown bounded.
        fixture.task.cancel();
        assert!(fixture.actor.as_mut().poll(&mut cx).is_ready());
    }
    assert!(fixture.run.as_mut().poll(&mut cx).is_ready());
    assert!(fixture.task.is_finished());
    drop(blocker);
    assert_eq!(fixture.drops.load(Ordering::SeqCst), 2);
    assert_eq!(fixture.commits.load(Ordering::SeqCst), 100);
    assert_eq!(
        actor_calls, 1,
        "closure must publish the actor control wake"
    );
    assert_eq!(
        producer_calls, 1,
        "closure must also notify blocked producers"
    );
    assert!(finished, "actor must finish without another control event");
}

// Lifecycle notification must not skip transport closure during final-owner unwind.
#[test]
fn final_owner_close_survives_panicking_closed_waiter_with_retained_ingress() {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::task::Wake;

    /// Separates lifecycle panic notification from actor and producer notifications.
    struct CloseWake {
        calls: AtomicUsize,
        panics: bool,
    }
    impl Wake for CloseWake {
        fn wake(self: Arc<Self>) {
            self.calls.fetch_add(1, Ordering::SeqCst);
            assert!(!self.panics, "closed waiter panic");
        }
    }

    let mut fixture = Fixture::new();
    let blocker = fixture.store.try_send(Action::Blocker).ok().unwrap();
    let mut first = Box::pin(fixture.sender.send(fixture.action()));
    let mut second = Box::pin(fixture.sender.send(fixture.action()));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(first.as_mut().poll(&mut cx).is_pending());
    let producer = Arc::new(CloseWake {
        calls: AtomicUsize::new(0),
        panics: false,
    });
    let producer_waker = Waker::from(Arc::clone(&producer));
    assert!(
        second
            .as_mut()
            .poll(&mut Context::from_waker(&producer_waker))
            .is_pending()
    );
    let actor = Arc::new(CloseWake {
        calls: AtomicUsize::new(0),
        panics: false,
    });
    let actor_waker = Waker::from(Arc::clone(&actor));
    assert!(
        fixture
            .actor
            .as_mut()
            .poll(&mut Context::from_waker(&actor_waker))
            .is_pending()
    );
    assert_eq!(fixture.commits.load(Ordering::SeqCst), 100);
    assert_eq!(producer.calls.load(Ordering::SeqCst), 0);
    actor.calls.store(0, Ordering::SeqCst);

    let lifecycle = Arc::new(CloseWake {
        calls: AtomicUsize::new(0),
        panics: true,
    });
    let lifecycle_waker = Waker::from(Arc::clone(&lifecycle));
    let mut closed = Box::pin(fixture.store.closed());
    assert!(
        closed
            .as_mut()
            .poll(&mut Context::from_waker(&lifecycle_waker))
            .is_pending()
    );
    let panic = catch_unwind(AssertUnwindSafe(|| drop(fixture.store))).unwrap_err();
    assert_eq!(panic.downcast_ref::<&str>(), Some(&"closed waiter panic"));

    // Snapshot before polling or dropping any send can publish a rescue control wake.
    let actor_calls = actor.calls.load(Ordering::SeqCst);
    let producer_calls = producer.calls.load(Ordering::SeqCst);
    let rejected_immediately = match second.as_mut().poll(&mut cx) {
        Poll::Ready(Err(error)) => error.is_store_stopped(),
        _ => false,
    };
    let drops_before_cleanup = fixture.drops.load(Ordering::SeqCst);

    // Release retained transport only after collecting the failing-baseline evidence.
    drop(first);
    drop(second);
    assert!(fixture.actor.as_mut().poll(&mut cx).is_ready());
    assert!(fixture.run.as_mut().poll(&mut cx).is_ready());
    assert!(fixture.task.is_finished());
    assert!(closed.as_mut().poll(&mut cx).is_ready());
    drop(blocker);
    assert_eq!(fixture.drops.load(Ordering::SeqCst), 2);
    assert_eq!(fixture.commits.load(Ordering::SeqCst), 100);
    assert_eq!(lifecycle.calls.load(Ordering::SeqCst), 1);
    assert_eq!(actor_calls, 1, "lifecycle unwind must still wake the actor");
    assert_eq!(
        producer_calls, 1,
        "lifecycle unwind must still notify blocked producers"
    );
    assert!(
        rejected_immediately,
        "retained ingress must already be closed"
    );
    assert_eq!(drops_before_cleanup, 1);
}

#[test]
fn dropping_full_send_after_admission_preserves_accepted_action() {
    let mut fixture = Fixture::new();
    let blocker = fixture.store.try_send(Action::Blocker).ok().unwrap();
    let mut send = Box::pin(fixture.sender.send(fixture.action()));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(send.as_mut().poll(&mut cx).is_pending());
    assert!(fixture.actor.as_mut().poll(&mut cx).is_pending());
    let mut blocker = std::pin::pin!(blocker.finish());
    assert!(matches!(blocker.as_mut().poll(&mut cx), Poll::Ready(Ok(_))));
    assert_eq!(fixture.commits.load(Ordering::SeqCst), 100);
    assert!(send.as_mut().poll(&mut cx).is_pending());
    drop(send);
    assert_eq!(fixture.drops.load(Ordering::SeqCst), 0);
    assert!(fixture.actor.as_mut().poll(&mut cx).is_pending());
    assert_eq!(fixture.commits.load(Ordering::SeqCst), 107);
    assert_eq!(fixture.drops.load(Ordering::SeqCst), 1);
}

#[test]
fn final_owner_drop_rejects_full_send_after_admission_once() {
    let mut fixture = Fixture::new();
    let blocker = fixture.store.try_send(Action::Blocker).ok().unwrap();
    let mut send = Box::pin(fixture.sender.send(fixture.action()));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(send.as_mut().poll(&mut cx).is_pending());
    assert!(fixture.actor.as_mut().poll(&mut cx).is_pending());
    let mut blocker = std::pin::pin!(blocker.finish());
    assert!(matches!(blocker.as_mut().poll(&mut cx), Poll::Ready(Ok(_))));
    assert!(send.as_mut().poll(&mut cx).is_pending());
    assert_eq!(fixture.drops.load(Ordering::SeqCst), 0);
    drop(fixture.store);
    assert!(fixture.actor.as_mut().poll(&mut cx).is_ready());
    let Poll::Ready(Err(error)) = send.as_mut().poll(&mut cx) else {
        panic!("closure must reject the action admitted from a full mailbox");
    };
    assert!(error.is_store_stopped());
    assert_eq!(fixture.commits.load(Ordering::SeqCst), 100);
    assert_eq!(fixture.drops.load(Ordering::SeqCst), 1);
    drop(send);
    assert_eq!(fixture.drops.load(Ordering::SeqCst), 1);
}

#[test]
fn abandoning_full_mailbox_send_drops_unadmitted_action_once() {
    let mut fixture = Fixture::new();
    let blocker = fixture.store.try_send(Action::Blocker).ok().unwrap();
    let mut send = Box::pin(fixture.sender.send(fixture.action()));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(send.as_mut().poll(&mut cx).is_pending());
    drop(send);
    assert_eq!(fixture.drops.load(Ordering::SeqCst), 1);
    assert!(fixture.actor.as_mut().poll(&mut cx).is_pending());
    drop(futures_executor::block_on(blocker.finish()).unwrap());
    assert_eq!(fixture.commits.load(Ordering::SeqCst), 100);
    assert_eq!(fixture.drops.load(Ordering::SeqCst), 1);
}

#[test]
fn lineage_cancelled_while_full_rejects_action_after_capacity_returns() {
    let mut fixture = Fixture::new();
    let blocker = fixture.store.try_send(Action::Blocker).ok().unwrap();
    let mut send = Box::pin(fixture.sender.send(fixture.action()));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(send.as_mut().poll(&mut cx).is_pending());
    fixture.task.cancel();
    assert!(fixture.actor.as_mut().poll(&mut cx).is_pending());
    drop(futures_executor::block_on(blocker.finish()).unwrap());
    assert!(send.as_mut().poll(&mut cx).is_pending());
    assert!(fixture.actor.as_mut().poll(&mut cx).is_pending());
    let Poll::Ready(Err(error)) = send.as_mut().poll(&mut cx) else {
        panic!("actor must reject cancelled lineage");
    };
    assert!(error.is_cancelled());
    assert_eq!(fixture.commits.load(Ordering::SeqCst), 100);
    assert_eq!(fixture.drops.load(Ordering::SeqCst), 1);
}

#[test]
fn final_owner_drop_closes_admitted_effect_send_without_committing() {
    let mut fixture = Fixture::new();
    let mut send = Box::pin(fixture.sender.send(fixture.action()));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(send.as_mut().poll(&mut cx).is_pending());
    drop(fixture.store);
    assert!(fixture.actor.as_mut().poll(&mut cx).is_ready());
    let Poll::Ready(Err(error)) = send.as_mut().poll(&mut cx) else {
        panic!("closed actor must release admitted reply");
    };
    assert!(error.is_store_stopped());
    assert_eq!(fixture.commits.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.drops.load(Ordering::SeqCst), 1);
}

#[test]
fn final_owner_drop_closes_full_mailbox_send_and_drops_payload_once() {
    let mut fixture = Fixture::new();
    let blocker = fixture.store.try_send(Action::Blocker).ok().unwrap();
    let mut send = Box::pin(fixture.sender.send(fixture.action()));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(send.as_mut().poll(&mut cx).is_pending());
    assert_eq!(fixture.drops.load(Ordering::SeqCst), 0);
    drop(fixture.store);
    assert!(fixture.actor.as_mut().poll(&mut cx).is_ready());
    futures_executor::block_on(blocker.finish()).unwrap_err();
    let Poll::Ready(Err(error)) = send.as_mut().poll(&mut cx) else {
        panic!("mailbox closure must release the waiting send");
    };
    assert!(error.is_store_stopped());
    assert_eq!(fixture.commits.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.drops.load(Ordering::SeqCst), 1);
    drop(send);
    assert_eq!(fixture.drops.load(Ordering::SeqCst), 1);
}

#[test]
fn cancellation_precedes_store_stopped_during_send_preflight() {
    for cancelled in [false, true] {
        let fixture = Fixture::new();
        let action = fixture.action();
        if cancelled {
            fixture.task.cancel();
        }
        drop(fixture.store);
        let error = futures_executor::block_on(fixture.sender.send(action)).unwrap_err();
        assert_eq!(error.is_cancelled(), cancelled);
        assert_eq!(error.is_store_stopped(), !cancelled);
        assert_eq!(fixture.commits.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.drops.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn closed_mailbox_rejects_live_sender_while_another_send_retains_ingress() {
    let mut fixture = Fixture::new();
    let blocker = fixture.store.try_send(Action::Blocker).ok().unwrap();
    let waiting_action = fixture.action();
    let rejected_action = fixture.action();
    let mut waiting = Box::pin(fixture.sender.send(waiting_action));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(waiting.as_mut().poll(&mut cx).is_pending());
    assert_eq!(fixture.drops.load(Ordering::SeqCst), 0);

    // The waiting send retains transport, but is not a Store lifecycle owner.
    drop(fixture.store);
    let mut rejected = std::pin::pin!(fixture.sender.send(rejected_action));
    let Poll::Ready(Err(error)) = rejected.as_mut().poll(&mut cx) else {
        panic!("closed mailbox must reject the still-live sender immediately");
    };
    assert!(error.is_store_stopped());
    assert_eq!(fixture.commits.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.drops.load(Ordering::SeqCst), 1);

    let Poll::Ready(Err(error)) = waiting.as_mut().poll(&mut cx) else {
        panic!("closure must also release the waiting send");
    };
    assert!(error.is_store_stopped());
    drop(waiting);
    assert_eq!(fixture.drops.load(Ordering::SeqCst), 2);
    assert!(fixture.actor.as_mut().poll(&mut cx).is_ready());
    let mut blocker = std::pin::pin!(blocker.finish());
    assert!(matches!(
        blocker.as_mut().poll(&mut cx),
        Poll::Ready(Err(_))
    ));
    let Fixture { run, .. } = fixture;
    drop(run);
    assert!(fixture.task.is_cancelled());
    assert!(fixture.task.is_finished());
    let mut finished = std::pin::pin!(fixture.task.finish());
    assert!(finished.as_mut().poll(&mut cx).is_ready());
    assert_eq!(fixture.commits.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.drops.load(Ordering::SeqCst), 2);
}

#[test]
fn named_cancellation_rejects_exported_sender_before_effect_exit() {
    /// Invalidates the named generation separately from dropping its runtime future.
    enum Request {
        Start,
        Cancel,
        Value(Child),
    }

    let runtime = support::ManualRuntime::new();
    let (export, receive) = mpsc::channel();
    let mut export = Some(export);
    let commits = Arc::new(AtomicUsize::new(0));
    let committed_values = Arc::clone(&commits);
    let reducer = rca_core::Reduce::new(move |(): &mut (), action: &Request, _| match action {
        Request::Start => {
            let export = export.take().unwrap();
            Effect::run(move |sender| async move {
                export.send(sender).unwrap();
                std::future::pending::<()>().await;
            })
            .cancellable("request")
        }
        Request::Cancel => Effect::cancel("request"),
        Request::Value(child) => {
            committed_values.fetch_add(child.value.get(), Ordering::SeqCst);
            Effect::none()
        }
    });
    let store = Store::new((), reducer, runtime.runtime());
    let mut actor = runtime.take_only_queued_task();
    let mut cx = Context::from_waker(Waker::noop());
    let receipt = store.try_send(Request::Start).ok().unwrap();
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    let mut receipt = std::pin::pin!(receipt.finish());
    let Poll::Ready(Ok(task)) = receipt.as_mut().poll(&mut cx) else {
        panic!("start must commit before exporting its sender");
    };
    let mut run = runtime.take_only_queued_task();
    assert!(run.as_mut().poll(&mut cx).is_pending());
    let sender = receive.try_recv().unwrap();
    let drops = Arc::new(AtomicUsize::new(0));
    let mut live = Box::pin(sender.send(Request::Value(Child {
        value: Cell::new(7),
        drops: Arc::clone(&drops),
    })));
    assert!(live.as_mut().poll(&mut cx).is_pending());
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    assert!(matches!(live.as_mut().poll(&mut cx), Poll::Ready(Ok(()))));
    drop(live);
    assert_eq!(commits.load(Ordering::SeqCst), 7);
    assert_eq!(drops.load(Ordering::SeqCst), 1);

    let cancellation = store.try_send(Request::Cancel).ok().unwrap();
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    let mut cancellation = std::pin::pin!(cancellation.finish());
    assert!(matches!(
        cancellation.as_mut().poll(&mut cx),
        Poll::Ready(Ok(_))
    ));
    // The generation is invalid, but its aborted runtime future has not exited.
    assert!(!task.is_cancelled());
    assert!(!task.is_finished());
    let mut rejected = Box::pin(sender.send(Request::Value(Child {
        value: Cell::new(11),
        drops: Arc::clone(&drops),
    })));
    let Poll::Ready(Err(error)) = rejected.as_mut().poll(&mut cx) else {
        panic!("named cancellation must reject before the effect is polled again");
    };
    assert!(error.is_cancelled());
    assert!(!error.is_store_stopped());
    drop(rejected);
    assert_eq!(commits.load(Ordering::SeqCst), 7);
    assert_eq!(drops.load(Ordering::SeqCst), 2);

    assert!(run.as_mut().poll(&mut cx).is_ready());
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    assert!(task.is_finished());
    let mut finished = std::pin::pin!(task.finish());
    assert!(finished.as_mut().poll(&mut cx).is_ready());
    drop(store);
    assert!(actor.as_mut().poll(&mut cx).is_ready());
    assert_eq!(commits.load(Ordering::SeqCst), 7);
    assert_eq!(drops.load(Ordering::SeqCst), 2);
}
