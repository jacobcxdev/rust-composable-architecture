use super::{Driver, poll};
use rca_core::{
    Effect, Feature, FeatureContext, MountIdentity, OptionalMount, Runtime, Store,
    TaskAccessReason, TaskProgress, TaskSendError,
};
use std::pin::Pin;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::task::Poll;

struct Root;
impl Feature for Root {
    type State = Option<usize>;
    type Action = ();
    fn reduce(
        &mut self,
        _: &mut Self::State,
        (): &(),
        _: &mut FeatureContext<'_, Self::State, ()>,
    ) {
    }
}
/// Counts each disposal of a captured callback, Feature or owned action.
struct Dropped(Arc<AtomicUsize>);
impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
struct Child {
    _dropped: Dropped,
}
impl Feature for Child {
    type State = usize;
    type Action = u8;
    fn reduce(&mut self, _: &mut usize, _: &u8, _: &mut FeatureContext<'_, usize, u8>) {}
}
/// Declares a child whose callback capture and Feature have separate drop counts.
fn descriptor(
    callback_drops: Arc<AtomicUsize>,
    feature_drops: Arc<AtomicUsize>,
) -> OptionalMount<Option<usize>, Child, ()> {
    let capture = Dropped(callback_drops);
    OptionalMount::new(
        Option::as_ref,
        Option::as_mut,
        |state: &Option<usize>| {
            state.map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| {
            let _ = &capture;
            Child {
                _dropped: Dropped(std::sync::Arc::clone(&feature_drops)),
            }
        },
    )
}
/// Drives one public operation for at most eight manual turns.
fn finish<F: Future>(driver: &Driver, future: F) -> F::Output {
    let mut future = Box::pin(future);
    for _ in 0..8 {
        if let Poll::Ready(result) = poll(future.as_mut()) {
            return result;
        }
        driver.turn();
    }
    panic!("bounded operation did not finish");
}

#[test]
fn missing_mounted_projection_drops_input_before_terminal_wake() {
    struct RejectingChild;
    impl Feature for RejectingChild {
        type State = usize;
        type Action = Dropped;

        fn reduce(
            &mut self,
            _: &mut usize,
            _: &Dropped,
            _: &mut FeatureContext<'_, usize, Dropped>,
        ) {
            panic!("an absent projection cannot reduce its input");
        }
    }

    struct TerminalWake {
        drops: Arc<AtomicUsize>,
        observed: AtomicUsize,
        wakes: AtomicUsize,
    }
    impl std::task::Wake for TerminalWake {
        fn wake(self: Arc<Self>) {
            self.observed
                .store(self.drops.load(Ordering::SeqCst), Ordering::SeqCst);
            self.wakes.fetch_add(1, Ordering::SeqCst);
        }
    }

    let driver = Driver::default();
    let (builder, site) = Store::feature_builder(Some(1), Root, Runtime::new(driver.clone()))
        .mount_with_site(OptionalMount::new(
            Option::as_ref,
            |state: &mut Option<usize>| {
                *state = None;
                None
            },
            |state: &Option<usize>| {
                state.map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            |_| RejectingChild,
        ));
    let store = builder.build().unwrap();
    finish(&driver, store.ready()).unwrap();
    let child = finish(&driver, store.acquire(&site)).unwrap().unwrap();
    let drops = Arc::new(AtomicUsize::new(0));
    let terminal = Arc::new(TerminalWake {
        drops: Arc::clone(&drops),
        observed: AtomicUsize::new(usize::MAX),
        wakes: AtomicUsize::new(0),
    });
    let waker = std::task::Waker::from(Arc::clone(&terminal));
    let mut send = Box::pin(child.send(Dropped(Arc::clone(&drops))));
    assert!(
        send.as_mut()
            .poll(&mut std::task::Context::from_waker(&waker))
            .is_pending()
    );
    driver.turn();
    assert_eq!(terminal.wakes.load(Ordering::SeqCst), 1);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(
        terminal.observed.load(Ordering::SeqCst),
        1,
        "terminal failure must follow destruction of its unrecoverable input"
    );
    assert!(matches!(
        poll(send.as_mut()),
        Poll::Ready(Err(TaskSendError::Failed { error }))
            if error.reason() == TaskAccessReason::ScopeEnded
                && error.progress() == TaskProgress::MayHaveRun
    ));
    finish(&driver, store.shutdown()).unwrap();
}

enum EffectAction {
    Start,
    Value(Box<Dropped>),
}
/// Launches its one owned effect only after the mounted Start action commits.
struct SendingChild {
    effect: Option<Effect<EffectAction>>,
    observed: Arc<AtomicUsize>,
}
impl Feature for SendingChild {
    type State = usize;
    type Action = EffectAction;
    fn reduce(
        &mut self,
        state: &mut usize,
        action: &EffectAction,
        context: &mut FeatureContext<'_, usize, EffectAction>,
    ) {
        match action {
            EffectAction::Start => context.effect(self.effect.take().unwrap()),
            EffectAction::Value(value) => {
                self.observed
                    .store(std::ptr::from_ref(&**value).addr(), Ordering::SeqCst);
                *state += 1;
            }
        }
    }
}
/// Keeps the effect in the constructor until the child Feature owns it.
fn sending_child(
    effect: Effect<EffectAction>,
    observed: Arc<AtomicUsize>,
) -> OptionalMount<Option<usize>, SendingChild, ()> {
    let mut effect = Some(effect);
    OptionalMount::new(
        Option::as_ref,
        Option::as_mut,
        |state: &Option<usize>| {
            state.map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| SendingChild {
            effect: effect.take(),
            observed: Arc::clone(&observed),
        },
    )
}

// Catches sites or lazy futures retaining a consumed builder's callback payload.
#[test]
fn dropped_builder_site_and_lazy_future_do_not_retain_callbacks() {
    let driver = Driver::default();
    let drops = Arc::new(AtomicUsize::new(0));
    let feature_drops = Arc::new(AtomicUsize::new(0));
    let (builder, site) = Store::feature_builder(Some(1), Root, Runtime::new(driver.clone()))
        .mount_with_site(descriptor(
            std::sync::Arc::clone(&drops),
            std::sync::Arc::clone(&feature_drops),
        ));
    let other = Store::feature_builder(Some(1), Root, Runtime::new(driver.clone()))
        .build()
        .unwrap();
    let lazy = other.acquire(&site);
    drop(builder);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(feature_drops.load(Ordering::SeqCst), 0);
    assert_eq!(
        finish(&driver, lazy).unwrap_err().reason(),
        TaskAccessReason::StoreClosed
    );
    finish(&driver, other.shutdown()).unwrap();
}

// Catches eager readiness capture, wrong-root payload access, and weak-owner leaks.
#[test]
fn readiness_is_checked_on_first_poll_and_retained_capabilities_do_not_own_store() {
    let driver = Driver::default();
    let callbacks = Arc::new(AtomicUsize::new(0));
    let features = Arc::new(AtomicUsize::new(0));
    let (builder, site) = Store::feature_builder(Some(1), Root, Runtime::new(driver.clone()))
        .mount_with_site(descriptor(
            std::sync::Arc::clone(&callbacks),
            std::sync::Arc::clone(&features),
        ));
    let store = builder.build().unwrap();
    let before_ready = store.acquire(&site);
    assert_eq!(
        finish(&driver, store.acquire(&site)).unwrap_err().reason(),
        TaskAccessReason::NotReady
    );
    finish(&driver, store.ready()).unwrap();
    let child = finish(&driver, before_ready).unwrap().unwrap();
    let other = Store::feature_builder(Some(1), Root, Runtime::new(driver.clone()))
        .build()
        .unwrap();
    assert_eq!(
        finish(&driver, other.acquire(&site)).unwrap_err().reason(),
        TaskAccessReason::ScopeEnded
    );
    let cloned_site = site.clone();
    let lazy = store.acquire(&cloned_site);
    drop(store);
    driver.turn();
    assert_eq!(callbacks.load(Ordering::SeqCst), 1);
    assert_eq!(features.load(Ordering::SeqCst), 1);
    assert_eq!(
        finish(&driver, lazy).unwrap_err().reason(),
        TaskAccessReason::StoreClosed
    );
    assert_eq!(
        finish(&driver, child.read(|_| panic!("closed selector")))
            .unwrap_err()
            .reason(),
        TaskAccessReason::StoreClosed
    );
    finish(&driver, other.shutdown()).unwrap();
}

// Catches a blocked admission holding a Store owner or strong declaration node.
#[test]
fn capacity_blocked_acquisition_closes_when_last_store_owner_drops() {
    let driver = Driver::default();
    let callbacks = Arc::new(AtomicUsize::new(0));
    let features = Arc::new(AtomicUsize::new(0));
    let (builder, site) = Store::feature_builder(Some(1), Root, Runtime::new(driver.clone()))
        .mailbox_capacity(1)
        .mount_with_site(descriptor(
            std::sync::Arc::clone(&callbacks),
            std::sync::Arc::clone(&features),
        ));
    let store = builder.build().unwrap();
    finish(&driver, store.ready()).unwrap();
    let mut admitted = Box::pin(store.acquire(&site));
    let mut blocked = Box::pin(store.acquire(&site));
    assert!(poll(admitted.as_mut()).is_pending());
    assert!(poll(blocked.as_mut()).is_pending());
    drop(store);
    driver.turn();
    assert_eq!(callbacks.load(Ordering::SeqCst), 1);
    assert_eq!(features.load(Ordering::SeqCst), 1);
    let admitted = poll(admitted.as_mut());
    assert!(
        matches!(admitted, Poll::Ready(Err(error)) if error.reason() == TaskAccessReason::StoreClosed),
        "{admitted:?}"
    );
    let blocked = poll(blocked.as_mut());
    assert!(
        matches!(blocked, Poll::Ready(Err(error)) if error.reason() == TaskAccessReason::StoreClosed),
        "{blocked:?}"
    );
}

// Blocked task requests must reject without entering or retaining their callbacks.
#[test]
fn capacity_blocked_child_read_and_update_release_captures_when_store_drops() {
    let driver = Driver::default();
    let callbacks = Arc::new(AtomicUsize::new(0));
    let features = Arc::new(AtomicUsize::new(0));
    let (builder, site) = Store::feature_builder(Some(1), Root, Runtime::new(driver.clone()))
        .mailbox_capacity(1)
        .mount_with_site(descriptor(Arc::clone(&callbacks), Arc::clone(&features)));
    let store = builder.build().unwrap();
    finish(&driver, store.ready()).unwrap();
    let child = finish(&driver, store.acquire(&site)).unwrap().unwrap();
    let admitted = store.try_send(()).unwrap();
    assert!(store.try_send(()).unwrap_err().is_full());

    let calls = Arc::new(AtomicUsize::new(0));
    let captures = Arc::new(AtomicUsize::new(0));
    let read_capture = Dropped(Arc::clone(&captures));
    let read_calls = Arc::clone(&calls);
    let mut read = Box::pin(child.read(move |state| {
        let _ = &read_capture;
        read_calls.fetch_add(1, Ordering::SeqCst);
        *state
    }));
    let update_capture = Dropped(Arc::clone(&captures));
    let update_calls = Arc::clone(&calls);
    let mut update = Box::pin(child.update(move |state| {
        let _ = &update_capture;
        update_calls.fetch_add(1, Ordering::SeqCst);
        *state += 1;
    }));
    assert!(poll(read.as_mut()).is_pending());
    assert!(poll(update.as_mut()).is_pending());
    assert_eq!(captures.load(Ordering::SeqCst), 0);
    drop(store);

    // No actor turn: these requests must still be awaiting mailbox capacity.
    let Poll::Ready(Err(read_error)) = poll(read.as_mut()) else {
        panic!("blocked child read rejects as ingress closes");
    };
    let Poll::Ready(Err(update_error)) = poll(update.as_mut()) else {
        panic!("blocked child update rejects as ingress closes");
    };
    for error in [read_error, update_error] {
        assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
        assert_eq!(error.progress(), TaskProgress::NotStarted);
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(captures.load(Ordering::SeqCst), 2);
    assert_eq!(Arc::strong_count(&calls), 1);
    driver.turn();
    finish(&driver, admitted.finish()).unwrap_err();
    assert_eq!(callbacks.load(Ordering::SeqCst), 1);
    assert_eq!(features.load(Ordering::SeqCst), 1);
    assert!(driver.0.lock().unwrap().is_empty());
}

// A mounted request blocked before publication must return its sole owned action.
#[test]
fn capacity_blocked_child_send_returns_original_action_when_store_drops() {
    struct OwnedChild {
        _dropped: Dropped,
        calls: Arc<AtomicUsize>,
    }
    impl Feature for OwnedChild {
        type State = usize;
        type Action = Box<Dropped>;
        fn reduce(
            &mut self,
            state: &mut usize,
            _: &Self::Action,
            _: &mut FeatureContext<'_, usize, Self::Action>,
        ) {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *state += 1;
        }
    }
    let driver = Driver::default();
    let callbacks = Arc::new(AtomicUsize::new(0));
    let features = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let capture = Dropped(Arc::clone(&callbacks));
    let feature_drops = Arc::clone(&features);
    let child_calls = Arc::clone(&calls);
    let mount = OptionalMount::new(
        Option::as_ref,
        Option::as_mut,
        |state: &Option<usize>| {
            state.map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| {
            let _ = &capture;
            OwnedChild {
                _dropped: Dropped(Arc::clone(&feature_drops)),
                calls: Arc::clone(&child_calls),
            }
        },
    );
    let (builder, site) = Store::feature_builder(Some(1), Root, Runtime::new(driver.clone()))
        .mailbox_capacity(1)
        .mount_with_site(mount);
    let store = builder.build().unwrap();
    finish(&driver, store.ready()).unwrap();
    let child = finish(&driver, store.acquire(&site)).unwrap().unwrap();
    let admitted = store.try_send(()).unwrap();
    assert!(store.try_send(()).unwrap_err().is_full());
    let drops = Arc::new(AtomicUsize::new(0));
    let action = Box::new(Dropped(Arc::clone(&drops)));
    let identity = std::ptr::from_ref(&*action);
    let mut send = Box::pin(child.send(action));
    assert!(poll(send.as_mut()).is_pending());
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(store);

    // Rejection occurs in the blocked sender, before the actor drains its queue.
    let Poll::Ready(Err(TaskSendError::Rejected { action, error })) = poll(send.as_mut()) else {
        panic!("blocked mounted send returns its original action");
    };
    assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), TaskProgress::NotStarted);
    assert_eq!(std::ptr::from_ref(&*action), identity);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(action);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    driver.turn();
    finish(&driver, admitted.finish()).unwrap_err();
    assert_eq!(callbacks.load(Ordering::SeqCst), 1);
    assert_eq!(features.load(Ordering::SeqCst), 1);
    assert_eq!(Arc::strong_count(&calls), 1);
    assert!(driver.0.lock().unwrap().is_empty());
}

// A retained child must reject before actor cleanup when no ingress owner remains.
#[test]
fn child_requests_reject_missing_ingress_without_entering_or_losing_payload() {
    let driver = Driver::default();
    let observed = Arc::new(AtomicUsize::new(0));
    let (builder, site) = Store::feature_builder(Some(1), Root, Runtime::new(driver.clone()))
        .mount_with_site(sending_child(Effect::none(), Arc::clone(&observed)));
    let store = builder.build().unwrap();
    finish(&driver, store.ready()).unwrap();
    let child = finish(&driver, store.acquire(&site)).unwrap().unwrap();
    assert_eq!(driver.0.lock().unwrap().len(), 1, "only the actor remains");
    drop(store);

    // No request was polled before closure, and the actor has not run since it.
    let calls = Arc::new(AtomicUsize::new(0));
    let captures = Arc::new(AtomicUsize::new(0));
    let read_capture = Dropped(Arc::clone(&captures));
    let read_calls = Arc::clone(&calls);
    let mut read = Box::pin(child.read(move |state| {
        let _ = &read_capture;
        read_calls.fetch_add(1, Ordering::SeqCst);
        *state
    }));
    let update_capture = Dropped(Arc::clone(&captures));
    let update_calls = Arc::clone(&calls);
    let mut update = Box::pin(child.update(move |state| {
        let _ = &update_capture;
        update_calls.fetch_add(1, Ordering::SeqCst);
        *state += 1;
    }));
    let Poll::Ready(Err(read_error)) = poll(read.as_mut()) else {
        panic!("child read immediately rejects missing ingress");
    };
    let Poll::Ready(Err(update_error)) = poll(update.as_mut()) else {
        panic!("child update immediately rejects missing ingress");
    };
    for error in [read_error, update_error] {
        assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
        assert_eq!(error.progress(), TaskProgress::NotStarted);
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(captures.load(Ordering::SeqCst), 2);
    assert_eq!(Arc::strong_count(&calls), 1);

    let drops = Arc::new(AtomicUsize::new(0));
    let action = Box::new(Dropped(Arc::clone(&drops)));
    let identity = std::ptr::from_ref(&*action);
    let mut send = Box::pin(child.send(EffectAction::Value(action)));
    let Poll::Ready(Err(TaskSendError::Rejected {
        action: EffectAction::Value(action),
        error,
    })) = poll(send.as_mut())
    else {
        panic!("child send immediately returns its original value");
    };
    assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), TaskProgress::NotStarted);
    assert_eq!(std::ptr::from_ref(&*action), identity);
    assert_eq!(observed.load(Ordering::SeqCst), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(action);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    driver.turn();
    assert_eq!(observed.load(Ordering::SeqCst), 0);
    assert_eq!(Arc::strong_count(&observed), 1);
    assert!(driver.0.lock().unwrap().is_empty());
}

// Capacity admission alone must not acknowledge the effect's owned child action.
#[test]
fn mounted_effect_send_waits_for_capacity_then_reduction_commit() {
    let driver = Driver::default();
    let drops = Arc::new(AtomicUsize::new(0));
    let captures = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new(AtomicUsize::new(0));
    let outcome = Arc::new(Mutex::new(None));
    let result = Arc::clone(&outcome);
    let action = Box::new(Dropped(Arc::clone(&drops)));
    let identity = std::ptr::from_ref(&*action).addr();
    let capture = Dropped(Arc::clone(&captures));
    let effect = Effect::run(move |sender| async move {
        let sent = sender.send(EffectAction::Value(action)).await;
        *result.lock().unwrap() = Some(sent);
        drop(capture);
    });
    let (builder, site) = Store::feature_builder(Some(1), Root, Runtime::new(driver.clone()))
        .mailbox_capacity(1)
        .mount_with_site(sending_child(effect, Arc::clone(&observed)));
    let store = builder.build().unwrap();
    finish(&driver, store.ready()).unwrap();
    let child = finish(&driver, store.acquire(&site)).unwrap().unwrap();
    let mut actor = {
        let mut tasks = driver.0.lock().unwrap();
        assert_eq!(tasks.len(), 1, "only the Store actor is scheduled");
        tasks.pop().unwrap()
    };
    let mut start = Box::pin(child.send(EffectAction::Start));
    assert!(poll(start.as_mut()).is_pending());
    assert!(poll(Pin::new(&mut actor)).is_pending());
    let Poll::Ready(Ok(task)) = poll(start.as_mut()) else {
        panic!("mounted effect launch committed");
    };
    let mut effect = {
        let mut tasks = driver.0.lock().unwrap();
        assert_eq!(tasks.len(), 1, "the held actor spawned exactly one effect");
        tasks.pop().unwrap()
    };

    let filler = store.try_send(()).unwrap();
    assert!(store.try_send(()).unwrap_err().is_full());
    assert!(poll(Pin::new(&mut effect)).is_pending());
    assert!(outcome.lock().unwrap().is_none());
    assert_eq!(observed.load(Ordering::SeqCst), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    assert_eq!(captures.load(Ordering::SeqCst), 0);
    assert!(!task.is_finished());

    // Only the actor runs: it frees capacity, but cannot publish the held send.
    assert!(poll(Pin::new(&mut actor)).is_pending());
    assert!(matches!(
        poll(Box::pin(filler.finish()).as_mut()),
        Poll::Ready(Ok(_))
    ));
    assert_eq!(observed.load(Ordering::SeqCst), 0);
    assert!(poll(Pin::new(&mut effect)).is_pending());
    assert!(store.try_send(()).unwrap_err().is_full());
    assert!(outcome.lock().unwrap().is_none());
    assert_eq!(observed.load(Ordering::SeqCst), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    assert!(!task.is_finished());

    // Admission filled the mailbox; only this actor poll may reduce the value.
    assert!(poll(Pin::new(&mut actor)).is_pending());
    assert_eq!(observed.load(Ordering::SeqCst), identity);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert!(outcome.lock().unwrap().is_none());
    assert_eq!(captures.load(Ordering::SeqCst), 0);
    assert!(poll(Pin::new(&mut effect)).is_ready());
    outcome.lock().unwrap().take().unwrap().unwrap();
    assert_eq!(captures.load(Ordering::SeqCst), 1);
    assert!(poll(Pin::new(&mut actor)).is_pending());
    assert!(task.is_finished());
    assert!(!task.is_cancelled());
    assert!(poll(Box::pin(task.finish()).as_mut()).is_ready());

    let mut shutdown = Box::pin(store.shutdown_into_state());
    assert!(poll(shutdown.as_mut()).is_pending());
    assert!(poll(Pin::new(&mut actor)).is_ready());
    let Poll::Ready(Ok(state)) = poll(shutdown.as_mut()) else {
        panic!("shutdown returns the committed child state");
    };
    assert_eq!(state, Some(2));
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert!(driver.0.lock().unwrap().is_empty());
}

// A stable mounted identity can outlive a user-defined mutable projection.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one effect schedule keeps projection failure, owned action disposal and causal completion together"
)]
fn mounted_effect_send_reports_cancelled_after_projection_disappears() {
    struct ProjectedChild {
        effect: Option<Effect<EffectAction>>,
        observed: Arc<AtomicUsize>,
    }
    impl Feature for ProjectedChild {
        type State = usize;
        type Action = EffectAction;
        fn reduce(
            &mut self,
            state: &mut usize,
            action: &EffectAction,
            context: &mut FeatureContext<'_, usize, EffectAction>,
        ) {
            match action {
                EffectAction::Start => {
                    *state = 2;
                    context.effect(self.effect.take().expect("Start launches once"));
                }
                EffectAction::Value(value) => {
                    self.observed
                        .store(std::ptr::from_ref(&**value).addr(), Ordering::SeqCst);
                    *state += 1;
                }
            }
        }
    }

    let driver = Driver::default();
    let drops = Arc::new(AtomicUsize::new(0));
    let captures = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new(AtomicUsize::new(0));
    let outcome = Arc::new(Mutex::new(None));
    let result = Arc::clone(&outcome);
    let action = Box::new(Dropped(Arc::clone(&drops)));
    let capture = Dropped(Arc::clone(&captures));
    let effect = Effect::run(move |sender| async move {
        *result.lock().unwrap() = Some(sender.send(EffectAction::Value(action)).await);
        drop(capture);
    });
    let mut effect = Some(effect);
    let child_observed = Arc::clone(&observed);
    let mount = OptionalMount::new(
        Option::as_ref,
        |state: &mut Option<usize>| state.as_mut().filter(|value| **value == 1),
        |state: &Option<usize>| {
            state.map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| ProjectedChild {
            effect: effect.take(),
            observed: Arc::clone(&child_observed),
        },
    );
    let (builder, site) =
        Store::feature_builder(Some(1), Root, Runtime::new(driver.clone())).mount_with_site(mount);
    let store = builder.build().unwrap();
    finish(&driver, store.ready()).unwrap();
    let child = finish(&driver, store.acquire(&site)).unwrap().unwrap();
    let mut actor = {
        let mut tasks = driver.0.lock().unwrap();
        assert_eq!(tasks.len(), 1, "only the Store actor is scheduled");
        tasks.pop().unwrap()
    };
    let mut start = Box::pin(child.send(EffectAction::Start));
    assert!(poll(start.as_mut()).is_pending());
    assert!(poll(Pin::new(&mut actor)).is_pending());
    let Poll::Ready(Ok(task)) = poll(start.as_mut()) else {
        panic!("Start commits while the mutable projection exists");
    };
    let mut effect = {
        let mut tasks = driver.0.lock().unwrap();
        assert_eq!(tasks.len(), 1, "Start spawned exactly one effect");
        tasks.pop().unwrap()
    };
    assert!(!task.is_finished());
    assert!(poll(Pin::new(&mut effect)).is_pending());
    assert!(outcome.lock().unwrap().is_none());
    assert_eq!(drops.load(Ordering::SeqCst), 0);

    // The immutable selector and identity still find the child, but its write
    // projection now returns None after Start changed the value from 1 to 2.
    assert!(poll(Pin::new(&mut actor)).is_pending());
    assert_eq!(observed.load(Ordering::SeqCst), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert!(outcome.lock().unwrap().is_none());
    assert!(poll(Pin::new(&mut effect)).is_ready());
    assert!(
        outcome
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .unwrap_err()
            .is_cancelled()
    );
    assert_eq!(captures.load(Ordering::SeqCst), 1);
    assert!(poll(Pin::new(&mut actor)).is_pending());
    assert!(task.is_finished());
    assert!(poll(Box::pin(task.finish()).as_mut()).is_ready());

    let mut shutdown = Box::pin(store.shutdown_into_state());
    assert!(poll(shutdown.as_mut()).is_pending());
    assert!(poll(Pin::new(&mut actor)).is_ready());
    let Poll::Ready(Ok(state)) = poll(shutdown.as_mut()) else {
        panic!("shutdown returns the unreduced child state");
    };
    assert_eq!(state, Some(2));
    assert_eq!(observed.load(Ordering::SeqCst), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(captures.load(Ordering::SeqCst), 1);
    assert!(driver.0.lock().unwrap().is_empty());
}

// Actor shutdown rejects an admitted effect before its child reducer can run.
#[test]
fn admitted_mounted_effect_is_rejected_when_store_closes_before_actor_entry() {
    let driver = Driver::default();
    let drops = Arc::new(AtomicUsize::new(0));
    let captures = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new(AtomicUsize::new(0));
    let outcome = Arc::new(Mutex::new(None));
    let result = Arc::clone(&outcome);
    let action = Box::new(Dropped(Arc::clone(&drops)));
    let capture = Dropped(Arc::clone(&captures));
    let effect = Effect::run(move |sender| async move {
        *result.lock().unwrap() = Some(sender.send(EffectAction::Value(action)).await);
        drop(capture);
    });
    let (builder, site) = Store::feature_builder(Some(1), Root, Runtime::new(driver.clone()))
        .mailbox_capacity(1)
        .mount_with_site(sending_child(effect, Arc::clone(&observed)));
    let store = builder.build().unwrap();
    finish(&driver, store.ready()).unwrap();
    let child = finish(&driver, store.acquire(&site)).unwrap().unwrap();
    let mut actor = {
        let mut tasks = driver.0.lock().unwrap();
        assert_eq!(tasks.len(), 1);
        tasks.pop().unwrap()
    };
    let mut start = Box::pin(child.send(EffectAction::Start));
    assert!(poll(start.as_mut()).is_pending());
    assert!(poll(Pin::new(&mut actor)).is_pending());
    let Poll::Ready(Ok(task)) = poll(start.as_mut()) else {
        panic!("mounted effect launch committed");
    };
    let mut effect = {
        let mut tasks = driver.0.lock().unwrap();
        assert_eq!(tasks.len(), 1);
        tasks.pop().unwrap()
    };

    // The effect action is in the mailbox, but the actor has not entered it.
    assert!(poll(Pin::new(&mut effect)).is_pending());
    assert!(store.try_send(()).unwrap_err().is_full());
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    assert_eq!(captures.load(Ordering::SeqCst), 0);
    assert_eq!(observed.load(Ordering::SeqCst), 0);
    drop(store);

    assert!(poll(Pin::new(&mut actor)).is_ready());
    assert!(poll(Pin::new(&mut effect)).is_ready());
    // Actor teardown rejects the queued command and aborts its effect body
    // before the sender continuation can publish an outcome.
    assert!(outcome.lock().unwrap().is_none());
    assert_eq!(observed.load(Ordering::SeqCst), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(captures.load(Ordering::SeqCst), 1);
    assert!(task.is_finished());
    assert!(task.is_cancelled());
    assert!(poll(Box::pin(task.finish()).as_mut()).is_ready());
    assert!(driver.0.lock().unwrap().is_empty());
}

// Waiting admission, closed ingress, and missing ingress each dispose one value.
#[test]
fn mounted_effect_send_reports_store_closed_without_reducing_or_retaining_action() {
    for (close_before_first_send, retain_ingress) in [(false, false), (true, true), (true, false)] {
        let driver = Driver::default();
        let drops = Arc::new(AtomicUsize::new(0));
        let captures = Arc::new(AtomicUsize::new(0));
        let observed = Arc::new(AtomicUsize::new(0));
        let outcome = Arc::new(Mutex::new(None));
        let result = Arc::clone(&outcome);
        let action = Box::new(Dropped(Arc::clone(&drops)));
        let capture = Dropped(Arc::clone(&captures));
        let effect = Effect::run(move |sender| async move {
            let sent = sender.send(EffectAction::Value(action)).await;
            *result.lock().unwrap() = Some(sent);
            drop(capture);
        });
        let (builder, site) = Store::feature_builder(Some(1), Root, Runtime::new(driver.clone()))
            .mailbox_capacity(1)
            .mount_with_site(sending_child(effect, Arc::clone(&observed)));
        let store = builder.build().unwrap();
        finish(&driver, store.ready()).unwrap();
        let child = finish(&driver, store.acquire(&site)).unwrap().unwrap();
        let mut actor = {
            let mut tasks = driver.0.lock().unwrap();
            assert_eq!(tasks.len(), 1, "only the Store actor is scheduled");
            tasks.pop().unwrap()
        };
        let mut start = Box::pin(child.send(EffectAction::Start));
        assert!(poll(start.as_mut()).is_pending());
        assert!(poll(Pin::new(&mut actor)).is_pending());
        let Poll::Ready(Ok(task)) = poll(start.as_mut()) else {
            panic!("mounted effect launch committed");
        };
        let mut effect = {
            let mut tasks = driver.0.lock().unwrap();
            assert_eq!(tasks.len(), 1, "the held actor spawned exactly one effect");
            tasks.pop().unwrap()
        };
        let filler = store.try_send(()).unwrap();
        assert!(store.try_send(()).unwrap_err().is_full());
        let mut read = Box::pin(child.read(|_| -> usize { panic!("closed read must not enter") }));
        if retain_ingress {
            // This public pending request retains ingress, not a Store owner.
            assert!(poll(read.as_mut()).is_pending());
        } else if !close_before_first_send {
            assert!(poll(Pin::new(&mut effect)).is_pending());
        }
        assert!(!task.is_finished());
        assert!(outcome.lock().unwrap().is_none());
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        assert_eq!(captures.load(Ordering::SeqCst), 0);
        drop(store);

        // No actor poll: closure, rather than actor-driven cancellation, wins.
        assert!(poll(Pin::new(&mut effect)).is_ready());
        assert!(
            outcome
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .unwrap_err()
                .is_store_stopped()
        );
        assert_eq!(observed.load(Ordering::SeqCst), 0);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(captures.load(Ordering::SeqCst), 1);
        if retain_ingress {
            let Poll::Ready(Err(error)) = poll(read.as_mut()) else {
                panic!("pending read releases closed ingress");
            };
            assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
            assert_eq!(error.progress(), TaskProgress::NotStarted);
        }
        drop(read);
        assert!(poll(Pin::new(&mut actor)).is_ready());
        assert!(matches!(
            poll(Box::pin(filler.finish()).as_mut()),
            Poll::Ready(Err(_))
        ));
        assert!(task.is_finished());
        assert!(poll(Box::pin(task.finish()).as_mut()).is_ready());
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(driver.0.lock().unwrap().is_empty());
    }
}

// Catches acquisition gaining authority after an earlier ordered shutdown.
#[test]
fn ready_admission_revalidates_closure_at_ordered_handling() {
    let driver = Driver::default();
    let (builder, site) =
        Store::feature_builder(Some(1), Root, Runtime::new(driver.clone())).mount_with_site(
            descriptor(Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0))),
        );
    let store = builder.build().unwrap();
    finish(&driver, store.ready()).unwrap();
    let mut shutdown = Box::pin(store.shutdown());
    assert!(poll(shutdown.as_mut()).is_pending());
    let mut acquisition = Box::pin(store.acquire(&site));
    assert!(poll(acquisition.as_mut()).is_pending());
    driver.turn();
    let result = poll(acquisition.as_mut());
    assert!(
        matches!(result, Poll::Ready(Err(error)) if error.reason() == TaskAccessReason::StoreClosed),
        "{result:?}"
    );
    assert!(poll(shutdown.as_mut()).is_ready());
}

// Catches normal-closure handling accidentally masking a real actor unwind.
#[test]
fn actor_panic_rejects_queued_acquisition_as_failed_and_drops_owned_payload_once() {
    struct PanickingRoot;
    impl Feature for PanickingRoot {
        type State = Option<usize>;
        type Action = ();
        fn reduce(
            &mut self,
            _: &mut Self::State,
            (): &(),
            _: &mut FeatureContext<'_, Self::State, ()>,
        ) {
            panic!("root reducer panic");
        }
    }
    let driver = Driver::default();
    let callbacks = Arc::new(AtomicUsize::new(0));
    let features = Arc::new(AtomicUsize::new(0));
    let (builder, site) =
        Store::feature_builder(Some(1), PanickingRoot, Runtime::new(driver.clone()))
            .mount_with_site(descriptor(
                std::sync::Arc::clone(&callbacks),
                std::sync::Arc::clone(&features),
            ));
    let store = builder.build().unwrap();
    finish(&driver, store.ready()).unwrap();
    let mut action = Box::pin(store.send(()));
    assert!(poll(action.as_mut()).is_pending());
    let mut acquisition = Box::pin(store.acquire(&site));
    assert!(poll(acquisition.as_mut()).is_pending());
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| driver.turn())).is_err());
    assert!(
        matches!(poll(acquisition.as_mut()), Poll::Ready(Err(error)) if error.reason() == TaskAccessReason::ActorFailed)
    );
    assert_eq!(
        finish(&driver, store.acquire(&site)).unwrap_err().reason(),
        TaskAccessReason::ActorFailed
    );
    assert_eq!(
        finish(&driver, store.ready()).unwrap_err().reason(),
        TaskAccessReason::ActorFailed
    );
    assert_eq!(callbacks.load(Ordering::SeqCst), 1);
    assert_eq!(features.load(Ordering::SeqCst), 1);
}

// Startup failure must wake an already-polled readiness waiter and destroy
// declaration captures/installed cache, even while its public site is retained.
#[test]
fn installation_callback_panic_fails_readiness_and_drops_payload_once() {
    struct ReadyWake(AtomicUsize);
    impl std::task::Wake for ReadyWake {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    struct PanicDependency;
    impl rca_core::DependencyKey for PanicDependency {
        type Value = ();
        fn live_value() {
            panic!("initial dependency panic")
        }
    }
    for phase in 0..5 {
        let driver = Driver::default();
        let callbacks = Arc::new(AtomicUsize::new(0));
        let features = Arc::new(AtomicUsize::new(0));
        let identity = Dropped(std::sync::Arc::clone(&callbacks));
        let constructor = Dropped(std::sync::Arc::clone(&callbacks));
        let selected = Dropped(std::sync::Arc::clone(&callbacks));
        let factory = Dropped(std::sync::Arc::clone(&callbacks));
        let feature_drops = std::sync::Arc::clone(&features);
        let descriptor = OptionalMount::new(
            Option::as_ref,
            Option::as_mut,
            move |state: &Option<usize>| {
                let _ = &identity;
                assert_ne!(phase, 0, "identity panic");
                state.map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            move |_| {
                let _ = &constructor;
                assert_ne!(phase, 1, "constructor panic");
                Child {
                    _dropped: Dropped(std::sync::Arc::clone(&feature_drops)),
                }
            },
        )
        .on_initial(
            move |state| {
                let _ = &selected;
                assert_ne!(phase, 2, "selected value panic");
                *state
            },
            move |_, mut context| {
                let _ = &factory;
                assert_ne!(phase, 3, "factory panic");
                context.dependency::<PanicDependency>();
                rca_core::Task::run(|_| async { panic!("failed startup cannot poll task body") })
            },
        );
        let (builder, site) = Store::feature_builder(Some(1), Root, Runtime::new(driver.clone()))
            .mount_with_site(descriptor);
        let store = builder.build().unwrap();
        let mut ready = Box::pin(store.ready());
        let notified = Arc::new(ReadyWake(AtomicUsize::new(0)));
        let waker = std::task::Waker::from(std::sync::Arc::clone(&notified));
        assert!(
            ready
                .as_mut()
                .poll(&mut std::task::Context::from_waker(&waker))
                .is_pending()
        );
        assert_eq!(notified.0.load(Ordering::SeqCst), 0);
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| driver.turn())).is_err());
        assert!(
            notified.0.load(Ordering::SeqCst) > 0,
            "failed installation wakes readiness"
        );
        assert!(
            matches!(poll(ready.as_mut()), Poll::Ready(Err(error)) if error.reason() == TaskAccessReason::ActorFailed)
        );
        assert_eq!(
            finish(&driver, store.acquire(&site)).unwrap_err().reason(),
            TaskAccessReason::ActorFailed
        );
        assert_eq!(callbacks.load(Ordering::SeqCst), 4, "phase {phase}");
        assert_eq!(
            features.load(Ordering::SeqCst),
            usize::from(phase >= 2),
            "phase {phase}"
        );
    }
}
