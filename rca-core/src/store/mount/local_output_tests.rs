//! Concrete owner-local ancestor handlers lower both child work kinds.

use super::local as mounts;
use crate::local::{
    Effect, Feature, FeatureContext, Output, Runtime, RuntimeDriver, RuntimeFuture, Store, Task,
};
use crate::{KeyedState, MonotonicInstant, MountIdentity, OutputFlow};
use std::cell::{Cell, RefCell};
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

/// Keeps non-Send actor tasks on the thread that drives each test.
///
/// Its clock stays at zero: elapsed sleeps complete, while future waits remain
/// pending. Yields request a wake and offer one pending poll before completion.
#[derive(Clone, Default)]
struct Driver(Rc<RefCell<Vec<RuntimeFuture>>>);
impl std::fmt::Debug for Driver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LocalAncestorOutputDriver")
    }
}
impl RuntimeDriver for Driver {
    fn spawn(&self, task: RuntimeFuture) {
        self.0.borrow_mut().push(task);
    }
    fn now(&self) -> MonotonicInstant {
        MonotonicInstant::ZERO
    }
    fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
        if deadline <= self.now() {
            Box::pin(std::future::ready(()))
        } else {
            Box::pin(std::future::pending())
        }
    }
    fn yield_now(&self) -> RuntimeFuture {
        let mut yielded = false;
        Box::pin(std::future::poll_fn(move |context| {
            if yielded {
                Poll::Ready(())
            } else {
                yielded = true;
                context.waker().wake_by_ref();
                Poll::Pending
            }
        }))
    }
}
impl Driver {
    /// Polls the current batch outside the queue borrow; new tasks wait.
    fn turn(&self) {
        let pending = std::mem::take(&mut *self.0.borrow_mut());
        for mut future in pending {
            if poll(future.as_mut()).is_pending() {
                self.0.borrow_mut().push(future);
            }
        }
    }
    /// Allows 64 explicit turns, not a wall-clock deadline or wake-delivery proof.
    fn finish<F: Future>(&self, future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        for _ in 0..64 {
            if let Poll::Ready(value) = poll(future.as_mut()) {
                return value;
            }
            self.turn();
        }
        panic!("finite local ancestor output did not finish");
    }
}

#[test]
fn driver_honours_frozen_clock() {
    let driver = Driver::default();
    let mut elapsed = driver.sleep_until(MonotonicInstant::ZERO);
    assert!(
        poll(elapsed.as_mut()).is_ready(),
        "elapsed deadline must be ready"
    );

    for deadline in [
        MonotonicInstant::from_elapsed(std::time::Duration::from_nanos(1)),
        MonotonicInstant::MAX,
    ] {
        let mut future = driver.sleep_until(deadline);
        assert!(poll(future.as_mut()).is_pending());
        assert_eq!(driver.now(), MonotonicInstant::ZERO);
    }
}

#[test]
fn driver_yields_and_wakes_before_completing() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Counts thread-safe wake notifications without owning any local payload.
    struct CountWake(AtomicUsize);
    impl std::task::Wake for CountWake {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    let driver = Driver::default();
    let wake = Arc::new(CountWake(AtomicUsize::new(0)));
    let waker = Waker::from(Arc::clone(&wake));
    let context = &mut Context::from_waker(&waker);
    let mut yielding = driver.yield_now();

    assert_eq!(wake.0.load(Ordering::SeqCst), 0);
    assert!(
        yielding.as_mut().poll(context).is_pending(),
        "yield must offer a scheduling turn"
    );
    assert_eq!(wake.0.load(Ordering::SeqCst), 1);
    assert!(yielding.as_mut().poll(context).is_ready());
    assert_eq!(wake.0.load(Ordering::SeqCst), 1);
}

/// Polls one owner-thread step without registering an executor wake.
fn poll<F: Future + ?Sized>(future: Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}
/// Separates handler, effect and task changes in one ancestor's state.
struct Row {
    source: Option<()>,
    handled: usize,
    effects: usize,
    tasks: usize,
}
/// Retains the exact output allocation after the root consumes it.
struct State {
    rows: KeyedState<String, Row>,
    received: Option<Rc<Cell<usize>>>,
}
/// Makes the root action non-Send and distinct from the child action.
struct RootAction(Rc<()>);
/// Carries a non-Send effect increment to the ancestor reducer.
struct RowAction(Rc<usize>);
/// Names the source endpoint's otherwise unused action type.
struct SourceAction;
/// Hosts the root output destination without handling child actions.
struct Root;
/// Applies effect increments in the ancestor's own action domain.
struct Parent;
/// Provides the descendant from which output travels to ancestor handlers.
struct Source;
impl Feature for Root {
    type State = State;
    type Action = RootAction;
    fn update(
        &mut self,
        _: &mut State,
        action: &RootAction,
        _: &mut FeatureContext<'_, State, RootAction>,
    ) {
        let _ = &action.0;
    }
}
impl Feature for Parent {
    type State = Row;
    type Action = RowAction;
    fn update(
        &mut self,
        state: &mut Row,
        action: &RowAction,
        _: &mut FeatureContext<'_, Row, RowAction>,
    ) {
        state.effects += *action.0;
    }
}
impl Feature for Source {
    type State = ();
    type Action = SourceAction;
    fn update(
        &mut self,
        (): &mut (),
        _: &SourceAction,
        _: &mut FeatureContext<'_, (), SourceAction>,
    ) {
    }
}

#[test]
fn local_descendant_output_lowers_distinct_ancestor_effect_and_task() {
    let driver = Driver::default();
    let output_cell = Rc::new(RefCell::new(None::<Output<Rc<Cell<usize>>>>));
    let parent_cell = Rc::new(RefCell::new(
        None::<mounts::LocalMountSite<State, Row, RowAction, RootAction>>,
    ));
    let source_cell = Rc::new(RefCell::new(None));
    let recipe_output = std::rc::Rc::clone(&output_cell);
    let recipe_parent = std::rc::Rc::clone(&parent_cell);
    let recipe_source = std::rc::Rc::clone(&source_cell);
    let (owner, parent) = mounts::attach_selected_key::<State, RootAction, Parent, _, _>(
        |state| &state.rows,
        |state| &mut state.rows,
        "one".to_owned(),
        |_| Parent,
        move |_, children| {
            let source = children.optional(mounts::LocalOptionalMount::new(
                |row: &Row| row.source.as_ref(),
                |row: &mut Row| row.source.as_mut(),
                |row: &Row| {
                    row.source.as_ref().map(|()| MountIdentity {
                        key: (),
                        replacement: 0,
                        configuration: 0,
                    })
                },
                |()| Source,
            ));
            let output = children.output::<Rc<Cell<usize>>>(&source, |handlers| {
                handlers.at(
                    recipe_parent.borrow().as_ref().unwrap(),
                    |row, value, context| {
                        row.handled += value.get();
                        context.effect(Effect::send(RowAction(Rc::new(2))));
                        context.task(Task::run(|context| async move {
                            context
                                .update(|row: &mut Row| row.tasks += 3)
                                .await
                                .unwrap();
                        }));
                        OutputFlow::Continue(value)
                    },
                );
                handlers.at_root(|root, value, _| {
                    root.received = Some(value);
                    OutputFlow::Consumed
                });
            });
            *recipe_output.borrow_mut() = Some(output);
            *recipe_source.borrow_mut() = Some(source);
        },
    );
    *parent_cell.borrow_mut() = Some(parent);
    let store = Store::builder(
        State {
            rows: [(
                "one".to_owned(),
                Row {
                    source: Some(()),
                    handled: 0,
                    effects: 0,
                    tasks: 0,
                },
            )]
            .into_iter()
            .collect(),
            received: None,
        },
        Root,
        Runtime::new(driver.clone()),
    )
    .with_mount(owner)
    .build()
    .unwrap();
    driver.finish(store.ready()).unwrap();
    let source = source_cell.borrow().as_ref().unwrap().clone();
    let context = driver.finish(store.acquire(&source)).unwrap().unwrap();
    let output = output_cell.borrow().as_ref().unwrap().clone();
    let value = Rc::new(Cell::new(7));
    let identity = Rc::as_ptr(&value);
    let delivery = driver.finish(context.output(&output, value)).unwrap();
    let crate::OutputDelivery::Consumed { task } = delivery else {
        panic!("root consumes after ancestor");
    };
    driver.finish(task.finish());
    assert_eq!(
        driver
            .finish(store.read(move |state| {
                let row = state.rows.get(&"one".to_owned()).unwrap();
                (
                    row.handled,
                    row.effects,
                    row.tasks,
                    Rc::as_ptr(state.received.as_ref().unwrap()) == identity,
                )
            }))
            .unwrap(),
        (7, 2, 3, true)
    );
    drop(store);
    driver.turn();
}

/// Drives retained senders across capacity, cancellation and completion boundaries.
mod mounted_effect_sender {
    use super::*;
    use crate::ActionTask;
    use crate::local::EffectSender;

    /// Counts destruction and exposes allocation identity without implementing Clone.
    struct Payload(Rc<Cell<usize>>);
    impl Drop for Payload {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    /// Starts the producer or delivers its separately owned payload.
    enum Action {
        Start,
        Value(Payload),
    }

    /// Adds a distinguishable root mutation to the occupied mailbox control.
    struct RootFeature;
    impl Feature for RootFeature {
        type State = Option<Rc<Cell<usize>>>;
        type Action = ();

        fn update(
            &mut self,
            state: &mut Self::State,
            (): &(),
            _: &mut FeatureContext<'_, Self::State, ()>,
        ) {
            let value = state.as_ref().unwrap();
            value.set(value.get() + 100);
        }
    }

    /// Exports an effect sender while retaining its optional completion signal.
    struct ChildFeature {
        export: Rc<RefCell<Option<EffectSender<Action>>>>,
        completion: Option<Rc<Cell<bool>>>,
    }
    impl Feature for ChildFeature {
        type State = Rc<Cell<usize>>;
        type Action = Action;

        fn update(
            &mut self,
            state: &mut Self::State,
            action: &Action,
            context: &mut FeatureContext<'_, Self::State, Action>,
        ) {
            match action {
                Action::Start => {
                    let export = Rc::clone(&self.export);
                    let completion = self.completion.clone();
                    let named = completion.is_some();
                    let effect = Effect::run(move |sender| async move {
                        *export.borrow_mut() = Some(sender);
                        std::future::poll_fn(move |_| {
                            if completion.as_ref().is_some_and(|ready| ready.get()) {
                                Poll::Ready(())
                            } else {
                                Poll::Pending
                            }
                        })
                        .await;
                    });
                    context.effect(if named {
                        effect.named("sender", crate::OperationPolicy::Coexist)
                    } else {
                        effect
                    });
                }
                Action::Value(payload) => {
                    let _ = &payload.0;
                    state.set(state.get() + 7);
                }
            }
        }
    }

    /// Owns actor and producer futures separately to control admission order.
    struct Fixture {
        store: Store<Option<Rc<Cell<usize>>>, ()>,
        actor: RuntimeFuture,
        run: RuntimeFuture,
        task: ActionTask,
        sender: EffectSender<Action>,
        value: Rc<Cell<usize>>,
        drops: Rc<Cell<usize>>,
    }

    impl Fixture {
        /// Starts an unnamed producer that remains pending until disposal.
        fn new() -> Self {
            Self::with_completion(None).0
        }

        /// Starts the real actor and exports a sender from its registered producer.
        fn with_completion(
            completion: Option<Rc<Cell<bool>>>,
        ) -> (Self, Option<crate::OperationWait>) {
            let driver = Driver::default();
            let value = Rc::new(Cell::new(0));
            let export = Rc::new(RefCell::new(None));
            let capture = Rc::clone(&export);
            let named = completion.is_some();
            let descriptor = mounts::LocalOptionalMount::new(
                |state: &Option<Rc<Cell<usize>>>| state.as_ref(),
                |state: &mut Option<Rc<Cell<usize>>>| state.as_mut(),
                |state: &Option<Rc<Cell<usize>>>| {
                    state.as_ref().map(|_| MountIdentity {
                        key: (),
                        replacement: 0,
                        configuration: 0,
                    })
                },
                move |_| ChildFeature {
                    export: Rc::clone(&capture),
                    completion: completion.clone(),
                },
            );
            let (builder, site) = Store::builder(
                Some(Rc::clone(&value)),
                RootFeature,
                Runtime::new(driver.clone()),
            )
            .mailbox_capacity(1)
            .mount_with_site(descriptor);
            let store = builder.build().unwrap();
            driver.finish(store.ready()).unwrap();
            let child = driver.finish(store.acquire(&site)).unwrap().unwrap();
            assert_eq!(driver.0.borrow().len(), 1);
            let mut actor = driver.0.borrow_mut().pop().unwrap();
            let mut start = Box::pin(child.send(Action::Start));
            assert!(poll(start.as_mut()).is_pending());
            assert!(poll(actor.as_mut()).is_pending());
            let Poll::Ready(Ok(task)) = poll(start.as_mut()) else {
                panic!("mounted effect must be registered before exporting its sender");
            };
            drop(start);
            let wait = named.then(|| {
                let mut capture = Box::pin(child.operation("sender").capture());
                assert!(poll(capture.as_mut()).is_pending());
                assert!(poll(actor.as_mut()).is_pending());
                let Poll::Ready(Ok(wait)) = poll(capture.as_mut()) else {
                    panic!("named mounted operation must be captured while its body is live");
                };
                wait
            });
            drop(child);
            drop(site);
            assert_eq!(driver.0.borrow().len(), 1);
            let mut run = driver.0.borrow_mut().pop().unwrap();
            assert!(poll(run.as_mut()).is_pending());
            let sender = export.borrow_mut().take().unwrap();
            (
                Self {
                    store,
                    actor,
                    run,
                    task,
                    sender,
                    value,
                    drops: Rc::new(Cell::new(0)),
                },
                wait,
            )
        }

        /// Creates a fresh payload whose disposal belongs to this fixture.
        fn action(&self) -> Action {
            Action::Value(Payload(Rc::clone(&self.drops)))
        }
    }

    #[test]
    fn cancellation_before_send_rejects_without_committing() {
        let fixture = Fixture::new();
        fixture.task.cancel();
        let mut send = Box::pin(fixture.sender.send(fixture.action()));
        let Poll::Ready(Err(error)) = poll(send.as_mut()) else {
            panic!("cancelled mounted authority must reject before admission");
        };
        assert!(error.is_cancelled());
        assert_eq!(fixture.value.get(), 0);
        assert_eq!(fixture.drops.get(), 1);
        drop(send);
        assert_eq!(fixture.drops.get(), 1);
    }

    #[test]
    fn full_send_recovers_capacity_and_acknowledges_commit() {
        let mut fixture = Fixture::new();
        let blocker = fixture.store.try_send(()).ok().unwrap();
        let mut send = Box::pin(fixture.sender.send(fixture.action()));
        assert!(poll(send.as_mut()).is_pending());
        assert_eq!(fixture.drops.get(), 0);
        assert!(poll(fixture.actor.as_mut()).is_pending());
        let mut blocker = Box::pin(blocker.finish());
        assert!(matches!(poll(blocker.as_mut()), Poll::Ready(Ok(_))));
        assert_eq!(fixture.value.get(), 100);
        assert!(poll(send.as_mut()).is_pending());
        assert_eq!(fixture.drops.get(), 0);
        assert!(poll(fixture.actor.as_mut()).is_pending());
        assert!(matches!(poll(send.as_mut()), Poll::Ready(Ok(()))));
        assert_eq!(fixture.value.get(), 107);
        assert_eq!(fixture.drops.get(), 1);
    }

    #[test]
    fn final_owner_close_rejects_full_and_later_sends_without_committing() {
        let mut fixture = Fixture::new();
        let blocker = fixture.store.try_send(()).ok().unwrap();
        let waiting_action = fixture.action();
        let rejected_action = fixture.action();
        let expired_action = fixture.action();
        let mut waiting = Box::pin(fixture.sender.send(waiting_action));
        assert!(poll(waiting.as_mut()).is_pending());
        assert_eq!(fixture.drops.get(), 0);

        // A waiting send retains transport, but not Store lifecycle ownership.
        drop(fixture.store);
        let mut rejected = Box::pin(fixture.sender.send(rejected_action));
        let Poll::Ready(Err(error)) = poll(rejected.as_mut()) else {
            panic!("closed mounted ingress must reject a still-live sender");
        };
        assert!(error.is_store_stopped());
        assert_eq!(fixture.drops.get(), 1);
        drop(rejected);
        let Poll::Ready(Err(error)) = poll(waiting.as_mut()) else {
            panic!("closure must release the full mounted send");
        };
        assert!(error.is_store_stopped());
        drop(waiting);
        assert_eq!(fixture.drops.get(), 2);

        let mut expired = Box::pin(fixture.sender.send(expired_action));
        let Poll::Ready(Err(error)) = poll(expired.as_mut()) else {
            panic!("sender must reject after the last ingress owner is released");
        };
        assert!(error.is_store_stopped());
        assert_eq!(fixture.drops.get(), 3);
        assert_eq!(fixture.value.get(), 0);
        assert!(poll(fixture.actor.as_mut()).is_ready());
        let mut blocker = Box::pin(blocker.finish());
        assert!(matches!(poll(blocker.as_mut()), Poll::Ready(Err(_))));
        assert_eq!(fixture.value.get(), 0);
        assert_eq!(fixture.drops.get(), 3);
    }

    #[test]
    fn operation_completion_observer_rejects_send_before_task_terminal_publication() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::task::Wake;
        use std::thread::{self, ThreadId};

        thread_local! {
            /// Lets a Send + Sync waker enter one non-Send owner-thread callback.
            static OBSERVER: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
        }
        /// Records notification and rejects calls from any other thread.
        struct ObserverWake {
            owner: ThreadId,
            calls: AtomicUsize,
        }
        impl Wake for ObserverWake {
            fn wake(self: Arc<Self>) {
                assert_eq!(thread::current().id(), self.owner);
                self.calls.fetch_add(1, Ordering::SeqCst);
                let callback = OBSERVER.with(|slot| slot.borrow_mut().take());
                if let Some(callback) = callback {
                    callback();
                }
            }
        }

        let completion = Rc::new(Cell::new(false));
        let (mut fixture, wait) = Fixture::with_completion(Some(Rc::clone(&completion)));
        let mut wait = Box::pin(wait.unwrap());
        let observed = Rc::new(Cell::new(false));
        let callback_observed = Rc::clone(&observed);
        let sender = fixture.sender.clone();
        let action = fixture.action();
        let value = Rc::clone(&fixture.value);
        let drops = Rc::clone(&fixture.drops);
        OBSERVER.with(|slot| {
            assert!(slot.borrow().is_none());
            *slot.borrow_mut() = Some(Box::new(move || {
                // This is an external passive observer, not a driver task/waker.
                // It neither polls the actor nor re-enters the completing effect.
                let mut send = Box::pin(sender.send(action));
                let Poll::Ready(Err(error)) = poll(send.as_mut()) else {
                    panic!("terminal operation ownership must not be resurrected by its observer");
                };
                assert!(error.is_cancelled());
                assert_eq!(value.get(), 0);
                assert_eq!(drops.get(), 1);
                callback_observed.set(true);
            }));
        });
        let wake = Arc::new(ObserverWake {
            owner: thread::current().id(),
            calls: AtomicUsize::new(0),
        });
        let waker = Waker::from(Arc::clone(&wake));
        assert!(
            wait.as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
        assert!(!observed.get());
        completion.set(true);
        assert!(poll(fixture.run.as_mut()).is_ready());
        assert_eq!(wake.calls.load(Ordering::SeqCst), 1);
        assert!(observed.get());
        assert!(matches!(poll(wait.as_mut()), Poll::Ready(Ok(()))));
        assert_eq!(fixture.drops.get(), 1);
        assert_eq!(fixture.value.get(), 0);
        assert!(poll(fixture.actor.as_mut()).is_pending());
        assert_eq!(fixture.value.get(), 0);
        assert!(fixture.task.is_finished());
        OBSERVER.with(|slot| assert!(slot.borrow().is_none()));
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "one named task, operation observer, and owner-thread wake form a single lifecycle schedule"
    )]
    fn named_task_completion_observer_rejects_local_endpoint_send() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::task::Wake;
        use std::thread::{self, ThreadId};

        thread_local! {
            /// Holds the one-shot owner-thread observer, not production global state.
            static OBSERVER: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
        }
        /// Enters the callback before causal task completion is published.
        struct ObserverWake {
            owner: ThreadId,
            calls: AtomicUsize,
        }
        impl Wake for ObserverWake {
            fn wake(self: Arc<Self>) {
                assert_eq!(thread::current().id(), self.owner);
                self.calls.fetch_add(1, Ordering::SeqCst);
                if let Some(callback) = OBSERVER.with(|slot| slot.borrow_mut().take()) {
                    callback();
                }
            }
        }

        /// Exports named or unnamed task authority, or carries an owned input.
        enum EndpointAction {
            Start,
            ExportUnnamed,
            Value(Payload),
        }
        /// Names the non-Send child endpoint retained past producer completion.
        type ChildContext = crate::local::TaskContext<Rc<Cell<usize>>, EndpointAction>;
        /// Exports real task authority and waits for an explicitly polled signal.
        struct EndpointFeature {
            export: Rc<RefCell<Option<ChildContext>>>,
            completion: Rc<Cell<bool>>,
        }
        impl Feature for EndpointFeature {
            type State = Rc<Cell<usize>>;
            type Action = EndpointAction;

            fn update(
                &mut self,
                state: &mut Self::State,
                action: &EndpointAction,
                context: &mut FeatureContext<'_, Self::State, EndpointAction>,
            ) {
                match action {
                    EndpointAction::Start => {
                        let export = Rc::clone(&self.export);
                        let completion = Rc::clone(&self.completion);
                        context.task(Task::run_named(
                            "endpoint",
                            crate::OperationPolicy::Coexist,
                            move |task_context| async move {
                                *export.borrow_mut() = Some(task_context.clone());
                                std::future::poll_fn(move |_| {
                                    if completion.get() {
                                        Poll::Ready(())
                                    } else {
                                        Poll::Pending
                                    }
                                })
                                .await;
                            },
                        ));
                    }
                    EndpointAction::ExportUnnamed => {
                        let export = Rc::clone(&self.export);
                        context.task(Task::run(move |task_context| async move {
                            *export.borrow_mut() = Some(task_context);
                        }));
                    }
                    EndpointAction::Value(payload) => {
                        let _ = &payload.0;
                        state.set(state.get() + 7);
                    }
                }
            }
        }

        let driver = Driver::default();
        let value = Rc::new(Cell::new(0));
        let drops = Rc::new(Cell::new(0));
        let export = Rc::new(RefCell::new(None));
        let completion = Rc::new(Cell::new(false));
        let output_calls = Rc::new(Cell::new(0));
        let output_drops = Rc::new(Cell::new(0));
        let descriptor = mounts::LocalOptionalMount::new(
            |state: &Option<Rc<Cell<usize>>>| state.as_ref(),
            |state: &mut Option<Rc<Cell<usize>>>| state.as_mut(),
            |state: &Option<Rc<Cell<usize>>>| {
                state.as_ref().map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            {
                let export = Rc::clone(&export);
                let completion = Rc::clone(&completion);
                move |_| EndpointFeature {
                    export: Rc::clone(&export),
                    completion: Rc::clone(&completion),
                }
            },
        );
        let (builder, site) = Store::builder(
            Some(Rc::clone(&value)),
            RootFeature,
            Runtime::new(driver.clone()),
        )
        .mailbox_capacity(1)
        .mount_with_site(descriptor);
        let handler_calls = Rc::clone(&output_calls);
        let (builder, output) = builder.output::<Payload>(&site, |handlers| {
            handlers.at_root(move |_, value, _| {
                handler_calls.set(handler_calls.get() + 1);
                OutputFlow::Continue(value)
            });
        });
        let store = builder.build().unwrap();
        driver.finish(store.ready()).unwrap();
        let child = driver.finish(store.acquire(&site)).unwrap().unwrap();
        let mut actor = driver.0.borrow_mut().pop().unwrap();
        let mut start = Box::pin(child.send(EndpointAction::Start));
        assert!(poll(start.as_mut()).is_pending());
        assert!(poll(actor.as_mut()).is_pending());
        let Poll::Ready(Ok(task)) = poll(start.as_mut()) else {
            panic!("named mounted task must be registered");
        };
        drop(start);
        let mut capture = Box::pin(child.operation("endpoint").capture());
        assert!(poll(capture.as_mut()).is_pending());
        assert!(poll(actor.as_mut()).is_pending());
        let Poll::Ready(Ok(wait)) = poll(capture.as_mut()) else {
            panic!("named mounted task must be captured while live");
        };
        drop(capture);
        drop(site);
        let mut run = driver.0.borrow_mut().pop().unwrap();
        assert!(poll(run.as_mut()).is_pending());
        let context = export.borrow_mut().take().unwrap();
        let late_context = context.clone();

        let mut wait = Box::pin(wait);
        let observed = Rc::new(Cell::new(false));
        let observed_in_wake = Rc::clone(&observed);
        let task_in_wake = task.clone();
        let value_in_wake = Rc::clone(&value);
        let drops_in_wake = Rc::clone(&drops);
        let output_in_wake = output.clone();
        let output_calls_in_wake = Rc::clone(&output_calls);
        let output_drops_in_wake = Rc::clone(&output_drops);
        OBSERVER.with(|slot| {
            assert!(slot.borrow().is_none());
            *slot.borrow_mut() = Some(Box::new(move || {
                assert!(
                    !task_in_wake.is_finished(),
                    "observer runs before task terminal publication"
                );
                let mut send = Box::pin(
                    context.send(EndpointAction::Value(Payload(Rc::clone(&drops_in_wake)))),
                );
                let Poll::Ready(Err(crate::TaskSendError::Rejected { action, error })) =
                    poll(send.as_mut())
                else {
                    panic!("terminal named ancestry must reject the escaped context");
                };
                assert_eq!(error.reason(), crate::TaskAccessReason::Cancelled);
                assert_eq!(error.progress(), crate::TaskProgress::NotStarted);
                assert_eq!(value_in_wake.get(), 0);
                assert_eq!(drops_in_wake.get(), 0);
                let EndpointAction::Value(payload) = action else {
                    panic!("rejection must return the original owned action");
                };
                assert!(Rc::ptr_eq(&payload.0, &drops_in_wake));
                drop(payload);
                assert_eq!(drops_in_wake.get(), 1);
                let mut delivery = Box::pin(
                    context.output(&output_in_wake, Payload(Rc::clone(&output_drops_in_wake))),
                );
                let Poll::Ready(Err(crate::OutputError::Returned { value, error })) =
                    poll(delivery.as_mut())
                else {
                    panic!("terminal named ancestry must return the unentered output");
                };
                assert_eq!(error.reason(), crate::TaskAccessReason::Cancelled);
                assert_eq!(error.progress(), crate::TaskProgress::NotStarted);
                assert!(Rc::ptr_eq(&value.0, &output_drops_in_wake));
                assert_eq!(output_calls_in_wake.get(), 0);
                assert_eq!(output_drops_in_wake.get(), 0);
                drop(value);
                drop(delivery);
                assert_eq!(output_drops_in_wake.get(), 1);
                observed_in_wake.set(true);
            }));
        });
        let wake = Arc::new(ObserverWake {
            owner: thread::current().id(),
            calls: AtomicUsize::new(0),
        });
        let waker = Waker::from(Arc::clone(&wake));
        assert!(
            wait.as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
        completion.set(true);
        assert!(poll(run.as_mut()).is_ready());
        assert_eq!(wake.calls.load(Ordering::SeqCst), 1);
        assert!(observed.get());
        assert!(matches!(poll(wait.as_mut()), Poll::Ready(Ok(()))));
        assert!(
            !task.is_finished(),
            "causal completion awaits actor harvest"
        );
        assert_eq!(value.get(), 0);
        assert_eq!(drops.get(), 1);
        assert!(poll(actor.as_mut()).is_pending());
        assert!(task.is_finished());

        let callbacks = Rc::new(Cell::new(0));
        let read_calls = Rc::clone(&callbacks);
        let mut read = Box::pin(late_context.read(move |_| {
            read_calls.set(read_calls.get() + 1);
            7
        }));
        let Poll::Ready(Err(error)) = poll(read.as_mut()) else {
            panic!("completed task context must reject a later read");
        };
        assert_eq!(error.reason(), crate::TaskAccessReason::Cancelled);
        assert_eq!(error.progress(), crate::TaskProgress::NotStarted);
        drop(read);

        let update_calls = Rc::clone(&callbacks);
        let mut update = Box::pin(late_context.update(move |state| {
            update_calls.set(update_calls.get() + 1);
            state.set(99);
        }));
        let Poll::Ready(Err(error)) = poll(update.as_mut()) else {
            panic!("completed task context must reject a later update");
        };
        assert_eq!(error.reason(), crate::TaskAccessReason::Cancelled);
        assert_eq!(error.progress(), crate::TaskProgress::NotStarted);
        drop(update);

        let mut send =
            Box::pin(late_context.send(EndpointAction::Value(Payload(Rc::clone(&drops)))));
        let Poll::Ready(Err(crate::TaskSendError::Rejected { action, error })) =
            poll(send.as_mut())
        else {
            panic!("completed task context must return a later owned action");
        };
        assert_eq!(error.reason(), crate::TaskAccessReason::Cancelled);
        assert_eq!(error.progress(), crate::TaskProgress::NotStarted);
        let EndpointAction::Value(payload) = action else {
            panic!("rejection must return the original owned action");
        };
        assert!(Rc::ptr_eq(&payload.0, &drops));
        assert_eq!(callbacks.get(), 0);
        assert_eq!(Rc::strong_count(&callbacks), 1);
        assert_eq!(value.get(), 0);
        assert_eq!(drops.get(), 1);
        drop(payload);
        drop(send);
        assert_eq!(drops.get(), 2);
        drop(late_context);

        let mut start_unnamed = Box::pin(child.send(EndpointAction::ExportUnnamed));
        assert!(poll(start_unnamed.as_mut()).is_pending());
        assert!(poll(actor.as_mut()).is_pending());
        let Poll::Ready(Ok(unnamed_task)) = poll(start_unnamed.as_mut()) else {
            panic!("the unnamed mounted task must be registered");
        };
        drop(start_unnamed);
        let mut unnamed_run = driver.0.borrow_mut().pop().unwrap();
        assert!(poll(unnamed_run.as_mut()).is_ready());
        assert!(!unnamed_task.is_finished());
        assert!(poll(actor.as_mut()).is_pending());
        assert!(unnamed_task.is_finished());
        let unnamed_context = export.borrow_mut().take().unwrap();
        let mut delivery =
            Box::pin(unnamed_context.output(&output, Payload(Rc::clone(&output_drops))));
        let Poll::Ready(Err(crate::OutputError::Returned {
            value: returned,
            error,
        })) = poll(delivery.as_mut())
        else {
            panic!("a completed unnamed task must return the unentered output");
        };
        assert_eq!(error.reason(), crate::TaskAccessReason::Cancelled);
        assert_eq!(error.progress(), crate::TaskProgress::NotStarted);
        assert!(Rc::ptr_eq(&returned.0, &output_drops));
        assert_eq!(output_drops.get(), 1);
        assert_eq!(output_calls.get(), 0);
        assert_eq!(value.get(), 0);
        drop(returned);
        drop(delivery);
        drop(unnamed_context);
        drop(unnamed_run);
        drop(unnamed_task);
        drop(child);
        drop(output);
        assert_eq!(output_drops.get(), 2);
        drop(store);
        assert!(poll(actor.as_mut()).is_ready());
        drop(actor);
        drop(run);
        drop(wait);
        drop(task);
        assert!(driver.0.borrow().is_empty());
        assert_eq!(Rc::strong_count(&export), 1);
        assert_eq!(Rc::strong_count(&completion), 1);
        assert_eq!(Rc::strong_count(&value), 1);
        assert_eq!(Rc::strong_count(&drops), 1);
        assert_eq!(Rc::strong_count(&output_calls), 1);
        assert_eq!(Rc::strong_count(&output_drops), 1);
        OBSERVER.with(|slot| assert!(slot.borrow().is_none()));
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "mounted admission, worker cancellation, and causal cleanup form one lifecycle schedule"
    )]
    fn cancellation_after_local_endpoint_retention_returns_original_action() {
        use crate::store::mounted::mounted_admission_test_hook;

        /// Names the local endpoint whose operation is cancelled after retention.
        type ChildContext = crate::local::TaskContext<Rc<Cell<usize>>, Action>;
        /// Exports a named task context while its producer remains pending.
        struct EndpointFeature(Rc<RefCell<Option<ChildContext>>>);
        impl Feature for EndpointFeature {
            type State = Rc<Cell<usize>>;
            type Action = Action;

            fn update(
                &mut self,
                state: &mut Self::State,
                action: &Action,
                context: &mut FeatureContext<'_, Self::State, Action>,
            ) {
                match action {
                    Action::Start => {
                        let export = Rc::clone(&self.0);
                        context.task(Task::run_named(
                            "endpoint-cancel",
                            crate::OperationPolicy::Coexist,
                            move |task_context| async move {
                                *export.borrow_mut() = Some(task_context.clone());
                                std::future::pending::<()>().await;
                            },
                        ));
                    }
                    Action::Value(payload) => {
                        let _ = &payload.0;
                        state.set(state.get() + 7);
                    }
                }
            }
        }

        let driver = Driver::default();
        let value = Rc::new(Cell::new(0));
        let drops = Rc::new(Cell::new(0));
        let export = Rc::new(RefCell::new(None));
        let descriptor = mounts::LocalOptionalMount::new(
            |state: &Option<Rc<Cell<usize>>>| state.as_ref(),
            |state: &mut Option<Rc<Cell<usize>>>| state.as_mut(),
            |state: &Option<Rc<Cell<usize>>>| {
                state.as_ref().map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            {
                let export = Rc::clone(&export);
                move |_| EndpointFeature(Rc::clone(&export))
            },
        );
        let (builder, site) = Store::builder(
            Some(Rc::clone(&value)),
            RootFeature,
            Runtime::new(driver.clone()),
        )
        .mailbox_capacity(1)
        .mount_with_site(descriptor);
        let store = builder.build().unwrap();
        driver.finish(store.ready()).unwrap();
        let child = driver.finish(store.acquire(&site)).unwrap().unwrap();
        let mut actor = driver.0.borrow_mut().pop().unwrap();
        let mut start = Box::pin(child.send(Action::Start));
        assert!(poll(start.as_mut()).is_pending());
        assert!(poll(actor.as_mut()).is_pending());
        let Poll::Ready(Ok(task)) = poll(start.as_mut()) else {
            panic!("named mounted task must be registered");
        };
        drop(start);
        let mut capture = Box::pin(child.operation("endpoint-cancel").capture());
        assert!(poll(capture.as_mut()).is_pending());
        assert!(poll(actor.as_mut()).is_pending());
        let Poll::Ready(Ok(wait)) = poll(capture.as_mut()) else {
            panic!("named mounted task must be captured while live");
        };
        drop(capture);
        drop(child);
        drop(site);
        let mut run = driver.0.borrow_mut().pop().unwrap();
        assert!(poll(run.as_mut()).is_pending());
        let context = export.borrow_mut().take().unwrap();
        let mut wait = Box::pin(wait);
        assert!(poll(wait.as_mut()).is_pending());
        assert!(!task.is_cancelled());
        let worker_task = task.clone();
        let mut send = Box::pin(context.send(Action::Value(Payload(Rc::clone(&drops)))));
        let result = mounted_admission_test_hook::with_after_retain(
            move || {
                std::thread::scope(|scope| {
                    scope.spawn(move || worker_task.cancel()).join().unwrap();
                });
            },
            || poll(send.as_mut()),
        );
        let Poll::Ready(Err(crate::TaskSendError::Rejected { action, error })) = result else {
            panic!("cancellation after retention must reject the original input");
        };
        assert_eq!(error.reason(), crate::TaskAccessReason::Cancelled);
        assert_eq!(error.progress(), crate::TaskProgress::NotStarted);
        let Action::Value(payload) = action else {
            panic!("rejection must return the original owned action");
        };
        assert!(Rc::ptr_eq(&payload.0, &drops));
        assert!(task.is_cancelled());
        assert_eq!(value.get(), 0);
        assert_eq!(drops.get(), 0);
        drop(payload);
        drop(send);
        drop(context);
        assert_eq!(drops.get(), 1);
        assert!(poll(wait.as_mut()).is_pending());

        assert!(poll(actor.as_mut()).is_pending());
        assert!(poll(run.as_mut()).is_ready());
        assert!(matches!(poll(wait.as_mut()), Poll::Ready(Ok(()))));
        assert!(poll(actor.as_mut()).is_pending());
        assert!(task.is_finished());
        assert_eq!(value.get(), 0);
        assert_eq!(drops.get(), 1);
        drop(store);
        assert!(poll(actor.as_mut()).is_ready());
        drop(actor);
        drop(run);
        drop(wait);
        drop(task);
        assert!(driver.0.borrow().is_empty());
        assert_eq!(Rc::strong_count(&export), 1);
        assert_eq!(Rc::strong_count(&value), 1);
        assert_eq!(Rc::strong_count(&drops), 1);
    }

    #[test]
    fn cancellation_after_retaining_operation_work_rejects_without_committing() {
        use crate::store::mounted::mounted_admission_test_hook;

        let completion = Rc::new(Cell::new(false));
        let (mut fixture, wait) = Fixture::with_completion(Some(completion));
        let mut wait = Box::pin(wait.unwrap());
        assert!(poll(wait.as_mut()).is_pending());
        assert!(!fixture.task.is_cancelled());
        let task = fixture.task.clone();
        let calls = Rc::new(Cell::new(0));
        let callback_calls = Rc::clone(&calls);
        let mut send = Box::pin(fixture.sender.send(fixture.action()));

        let outcome = mounted_admission_test_hook::with_after_retain(
            move || {
                callback_calls.set(callback_calls.get() + 1);
                // Pause only the owner thread. The worker uses the real public
                // cancellation API and never polls local work or the actor.
                std::thread::scope(|scope| {
                    scope.spawn(move || task.cancel()).join().unwrap();
                });
            },
            || poll(send.as_mut()),
        );
        let Poll::Ready(Err(error)) = outcome else {
            panic!("cancellation between retention and admission must reject immediately");
        };
        assert!(error.is_cancelled());
        assert_eq!(calls.get(), 1);
        assert!(fixture.task.is_cancelled());
        assert_eq!(fixture.drops.get(), 1);
        assert_eq!(fixture.value.get(), 0);
        drop(send);
        assert_eq!(fixture.drops.get(), 1);
        assert!(poll(wait.as_mut()).is_pending());

        assert!(poll(fixture.actor.as_mut()).is_pending());
        assert!(poll(fixture.run.as_mut()).is_ready());
        assert!(matches!(poll(wait.as_mut()), Poll::Ready(Ok(()))));
        assert!(poll(fixture.actor.as_mut()).is_pending());
        assert!(fixture.task.is_finished());
        assert_eq!(fixture.value.get(), 0);
        assert_eq!(fixture.drops.get(), 1);
    }
}
