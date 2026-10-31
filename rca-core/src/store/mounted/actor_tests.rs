//! Deterministic witnesses through the real actor dispatcher and timer batch.
use super::*;
use crate::store::mounted::tests::panic_message;
use crate::store::mounted::{OptionalPath, SharedNode};
use crate::store::scope::{ScopeIdentity, ScopeSlot};
use crate::{Feature, FeatureContext, MonotonicInstant, Reduce, RuntimeDriver, Task, TaskContext};
use std::sync::{
    Mutex,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

/// Captures spawned work for explicit polling without owning an actor task.
///
/// Tests advance `seconds` and poll waits or the actor's timer batch manually.
/// Sleeps observe that same clock; yields request a wake before their second poll.
#[derive(Clone, Default)]
struct Driver {
    /// Work that each scenario takes, polls or drops outside this mutex guard.
    tasks: Arc<Mutex<Vec<RuntimeFuture>>>,
    /// The manual clock sample read by actual timer registration and dispatch.
    seconds: Arc<AtomicU64>,
}
impl std::fmt::Debug for Driver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MountedDriver").finish_non_exhaustive()
    }
}
impl RuntimeDriver for Driver {
    fn spawn(&self, task: RuntimeFuture) {
        self.tasks.lock().unwrap().push(task);
    }
    fn now(&self) -> MonotonicInstant {
        MonotonicInstant::from_elapsed(Duration::from_secs(self.seconds.load(Ordering::SeqCst)))
    }
    fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
        let seconds = Arc::clone(&self.seconds);
        Box::pin(std::future::poll_fn(move |_| {
            let now =
                MonotonicInstant::from_elapsed(Duration::from_secs(seconds.load(Ordering::SeqCst)));
            if deadline <= now {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }))
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
/// Separates root authority from the independently reduced child action domain.
struct State {
    /// Records the exact delivery order across effects, tasks and timers.
    child: Option<Vec<u8>>,
    /// The scope installed before constructing the child endpoint.
    token: Option<ScopeToken>,
}
/// Creates child authority through the root reducer.
enum RootAction {
    Install,
}
/// Selects work, retirement and failure schedules in the mounted reducer.
enum ChildAction {
    Start,
    Push(u8),
    Retire,
    RetireNested,
    Panic(GuardProbe),
    Notify,
    #[cfg(feature = "test-util")]
    Work,
    #[cfg(feature = "test-util")]
    DropProbe(ProbeDrop),
    Interval,
}
/// Declares real mounted work with independent poll and destruction observations.
struct Child {
    /// Counts task entry or interval factory calls, depending on the scenario.
    polls: Arc<AtomicUsize>,
    /// Counts captures released before their task or effect receives a poll.
    drops: Arc<AtomicUsize>,
    /// Checks guard availability when a panicking reduction destroys the Feature.
    feature_probe: Option<GuardProbe>,
}
impl Drop for Child {
    fn drop(&mut self) {
        if let Some(probe) = &self.feature_probe {
            probe.observe();
        }
    }
}
/// Makes unpolled work destruction observable without assertions in Drop.
struct Capture(Arc<AtomicUsize>);
impl Drop for Capture {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
impl Feature for Child {
    type State = Vec<u8>;
    type Action = ChildAction;
    fn update(
        &mut self,
        state: &mut Vec<u8>,
        action: &ChildAction,
        context: &mut FeatureContext<'_, Vec<u8>, ChildAction>,
    ) {
        match action {
            ChildAction::Push(value) => state.push(*value),
            ChildAction::Notify => {
                context
                    .reducer_context()
                    .report_issue(crate::Issue::new("mounted", "reentrant admission"))
                    .unwrap();
            }
            #[cfg(feature = "test-util")]
            ChildAction::Work => context.task(Task::run(|_| async {})),
            #[cfg(feature = "test-util")]
            ChildAction::DropProbe(probe) => {
                let _ = probe;
                panic!("retired probe must not enter the reducer");
            }
            ChildAction::Interval => {
                let calls = Arc::clone(&self.polls);
                context.effect(Effect::interval(Duration::from_secs(1), move || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    ChildAction::Push(7)
                }));
            }
            ChildAction::Start => {
                context.effect(Effect::merge([
                    Effect::send(ChildAction::Push(1)),
                    Effect::send(ChildAction::Push(2)).delay(Duration::from_secs(1)),
                    Effect::send_after(Duration::from_secs(1), ChildAction::Push(4)),
                ]));
                let polls = Arc::clone(&self.polls);
                context.task(Task::<Vec<u8>, ChildAction>::run(
                    move |context| async move {
                        polls.fetch_add(1, Ordering::SeqCst);
                        let _ = context.update(|state| state.push(3)).await.unwrap();
                    },
                ));
            }
            ChildAction::Retire => {
                let polls = Arc::clone(&self.polls);
                let capture = Capture(Arc::clone(&self.drops));
                context.task(Task::<Vec<u8>, ChildAction>::run(move |_| async move {
                    polls.fetch_add(1, Ordering::SeqCst);
                    drop(capture);
                }));
                context.effect(Effect::send(ChildAction::Push(99)).delay(Duration::from_secs(1)));
                let scope = context.reducer_context().scope_token();
                context.reducer_context().leave(scope);
            }
            ChildAction::RetireNested => {
                let inner = context.reducer_context().mount(
                    ReduceContext::node(&mut ScopeSlot::new()),
                    ScopeIdentity::new(()),
                );
                let polls = Arc::clone(&self.polls);
                let capture = Capture(Arc::clone(&self.drops));
                context.effect(
                    Effect::run(move |_| async move {
                        polls.fetch_add(1, Ordering::SeqCst);
                        drop(capture);
                    })
                    .in_scope(inner),
                );
                context.effect(Effect::send(ChildAction::Push(5)));
                context.task(Task::<Vec<u8>, ChildAction>::run(|context| async move {
                    context.update(|state| state.push(6)).await.unwrap();
                }));
                context.reducer_context().leave(inner);
            }
            ChildAction::Panic(probe) => {
                self.feature_probe = Some(probe.clone());
                probe.observe();
                let capture = ProbeDrop(probe.clone());
                context.task(Task::<Vec<u8>, ChildAction>::run(move |_| async move {
                    drop(capture);
                }));
                panic!("mounted reducer panic");
            }
        }
    }
}
#[expect(
    clippy::type_complexity,
    reason = "keep the actor fixture ownership and observation handles explicit"
)]
/// Installs a real child with a one-slot mailbox and explicit actor ownership.
fn fixture() -> (
    Actor<State, RootAction>,
    Arc<Ingress<State, RootAction>>,
    TaskContext<Vec<u8>, ChildAction>,
    Driver,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
    Arc<dyn Fn() -> bool + Send + Sync>,
) {
    let driver = Driver::default();
    let polls = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let (sender, receiver) =
        crate::store::mailbox::bounded(std::num::NonZeroUsize::new(1).unwrap());
    let ingress = Arc::new(Ingress::new(sender, ControlWake::new()));
    let mut slot = ScopeSlot::new();
    let feature = Reduce::new(
        move |state: &mut State, _: &RootAction, context: &mut ReduceContext<'_>| {
            state.token =
                Some(context.mount(ReduceContext::node(&mut slot), ScopeIdentity::new(())));
            Effect::none()
        },
    );
    let mut actor = Actor::<State, RootAction>::with_driver(
        State {
            child: Some(Vec::new()),
            token: None,
        },
        Box::new(feature),
        DependencyValues::new(),
        crate::IssueReporter::default(),
        receiver,
        &ingress,
        Arc::new(Lifecycle::new()),
        Runtime::new(driver.clone()),
        #[cfg(feature = "test-util")]
        None,
    );
    let turn = actor.commit_action(ActionCommand::external(
        RootAction::Install,
        crate::store::command::CommitSender::disconnected(),
    ));
    assert!(actor.launch(turn));
    let token = actor.state.as_ref().unwrap().token.unwrap();
    let node = SharedNode::new(
        Child {
            polls: Arc::clone(&polls),
            drops: Arc::clone(&drops),
            feature_probe: None,
        },
        token,
        0,
    );
    let context = node.downgrade().context::<State, RootAction>(
        OptionalPath::<State, _>::new(|state| state.child.as_ref(), |state| state.child.as_mut()),
        Arc::downgrade(&ingress),
        &actor.scopes,
    );
    let probe = crate::store::mounted::tests::shared_guard_probe(&node);
    actor.mounted.push(Box::new(node));
    (actor, ingress, context, driver, polls, drops, probe)
}
/// Processes exactly one admitted command and registers its actual returned work.
fn dispatch(actor: &mut Actor<State, RootAction>) {
    let command = actor.receiver.try_recv().unwrap();
    let turn = futures_executor::block_on(actor.process_command(command));
    assert!(actor.launch(turn));
}

#[test]
fn heterogeneous_child_timer_concat_and_task_use_actor_registration() {
    let (mut actor, _ingress, child, driver, _, _, _) = fixture();
    let mut cx = Context::from_waker(Waker::noop());
    let mut start = Box::pin(child.send(ChildAction::Start));
    assert!(start.as_mut().poll(&mut cx).is_pending());
    dispatch(&mut actor);
    let Poll::Ready(Ok(receipt)) = start.as_mut().poll(&mut cx) else {
        panic!("child committed")
    };
    assert!(!receipt.is_finished());
    let mut launched = std::mem::take(&mut *driver.tasks.lock().unwrap()).into_iter();
    let mut effect = launched.next().unwrap();
    let mut task = launched.next().unwrap();
    assert!(launched.next().is_none());
    assert!(effect.as_mut().poll(&mut cx).is_pending());
    dispatch(&mut actor);
    assert!(effect.as_mut().poll(&mut cx).is_ready());
    assert!(task.as_mut().poll(&mut cx).is_pending());
    dispatch(&mut actor);
    assert!(task.as_mut().poll(&mut cx).is_ready());
    drop(effect);
    drop(task);
    let turn = actor.process_control();
    assert!(actor.launch(turn));
    assert_eq!(
        actor.state.as_ref().unwrap().child.as_deref(),
        Some([1, 3].as_slice())
    );
    driver.seconds.store(1, Ordering::SeqCst);
    assert!(futures_executor::block_on(actor.process_timer_batch()));
    assert_eq!(
        actor.state.as_ref().unwrap().child.as_deref(),
        Some([1, 3, 4].as_slice())
    );
    let turn = actor.process_control();
    assert!(actor.launch(turn));
    let mut continuation = driver.tasks.lock().unwrap().pop().unwrap();
    assert!(continuation.as_mut().poll(&mut cx).is_pending());
    dispatch(&mut actor);
    assert!(continuation.as_mut().poll(&mut cx).is_ready());
    drop(continuation);
    let turn = actor.process_control();
    assert!(actor.launch(turn));
    assert_eq!(
        actor.state.as_ref().unwrap().child.as_deref(),
        Some([1, 3, 4, 2].as_slice())
    );
    assert!(receipt.is_finished());
    assert!(actor.sequences.is_empty());
    #[cfg(feature = "test-util")]
    assert_eq!(actor.registry.live_task_ids(), [] as [u64; 0]);
}

#[test]
fn child_owned_work_is_pruned_after_its_handler_retires_scope() {
    let (mut actor, _ingress, child, driver, polls, drops, _) = fixture();
    let mut cx = Context::from_waker(Waker::noop());
    let mut retire = Box::pin(child.send(ChildAction::Retire));
    assert!(retire.as_mut().poll(&mut cx).is_pending());
    dispatch(&mut actor);
    let Poll::Ready(Ok(receipt)) = retire.as_mut().poll(&mut cx) else {
        panic!("retirement committed")
    };
    assert!(receipt.is_finished());
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(polls.load(Ordering::SeqCst), 0);
    assert!(actor.mounted.is_empty());
    assert!(driver.tasks.lock().unwrap().is_empty());
    assert!(actor.timers.is_empty());
    assert_eq!(
        actor.state.as_ref().unwrap().child.as_deref(),
        Some([].as_slice())
    );
}

#[test]
fn nested_retirement_prunes_new_effect_but_keeps_mounted_sibling_work() {
    let (mut actor, _ingress, child, driver, polls, drops, _) = fixture();
    let scope = actor.state.as_ref().unwrap().token.unwrap();
    let mut cx = Context::from_waker(Waker::noop());
    let mut send = Box::pin(child.send(ChildAction::RetireNested));
    assert!(send.as_mut().poll(&mut cx).is_pending());
    dispatch(&mut actor);
    let Poll::Ready(Ok(receipt)) = send.as_mut().poll(&mut cx) else {
        panic!("nested retirement committed");
    };
    assert!(!receipt.is_finished());
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(polls.load(Ordering::SeqCst), 0);
    assert!(actor.scopes.is_current(scope));
    assert_eq!(actor.mounted.len(), 1);
    let mut launched = std::mem::take(&mut *driver.tasks.lock().unwrap()).into_iter();
    let mut effect = launched.next().unwrap();
    let mut task = launched.next().unwrap();
    assert!(launched.next().is_none());
    assert!(effect.as_mut().poll(&mut cx).is_pending());
    dispatch(&mut actor);
    assert!(effect.as_mut().poll(&mut cx).is_ready());
    assert!(task.as_mut().poll(&mut cx).is_pending());
    dispatch(&mut actor);
    assert!(task.as_mut().poll(&mut cx).is_ready());
    drop(effect);
    drop(task);
    let turn = actor.process_control();
    assert!(actor.launch(turn));
    assert!(receipt.is_finished());
    assert!(!receipt.is_cancelled());
    assert_eq!(
        actor.state.as_ref().unwrap().child.as_deref(),
        Some([5, 6].as_slice())
    );
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(polls.load(Ordering::SeqCst), 0);
    assert!(actor.scopes.is_current(scope));
    assert!(actor.sequences.is_empty());
    assert!(actor.timers.is_empty());
    #[cfg(feature = "test-util")]
    assert_eq!(actor.registry.live_task_ids(), [] as [u64; 0]);
}

#[cfg(feature = "test-util")]
#[test]
fn mounted_frontier_has_no_root_action_and_requires_explicit_release() {
    let (mut actor, _ingress, child, _, _, _, _) = fixture();
    let gate = crate::testing::TransactionGate::new();
    actor.set_transaction_gate(Some(gate.attach().unwrap()));
    let mut cx = Context::from_waker(Waker::noop());
    let mut send = Box::pin(child.send(ChildAction::Push(8)));
    assert!(send.as_mut().poll(&mut cx).is_pending());
    let command = actor.receiver.try_recv().unwrap();
    let mut processing = Box::pin(actor.process_command(command));
    assert!(processing.as_mut().poll(&mut cx).is_pending());
    assert!(gate.has_pending());
    assert_eq!(
        gate.inspect_pending(|_| panic!("root matcher cannot inspect ChildAction")),
        None::<()>
    );
    assert!(gate.has_pending());
    assert!(send.as_mut().poll(&mut cx).is_pending());
    assert!(gate.release_pending());
    assert!(processing.as_mut().poll(&mut cx).is_pending());
    assert!(matches!(send.as_mut().poll(&mut cx), Poll::Ready(Ok(_))));
    assert!(gate.acknowledge(gate.boundary().unwrap()));
    let Poll::Ready(turn) = processing.as_mut().poll(&mut cx) else {
        panic!("acknowledged frontier released")
    };
    drop(processing);
    assert!(actor.launch(turn));
    assert_eq!(
        actor.state.as_ref().unwrap().child.as_deref(),
        Some([8].as_slice())
    );
}

#[cfg(feature = "test-util")]
#[test]
fn mounted_frontier_rejects_root_context_without_entering_matcher() {
    let (mut actor, ingress, child, _, _, _, _) = fixture();
    let root_scope = actor.scopes.root();
    let root = TaskContext::new(
        Arc::downgrade(&ingress),
        root_scope,
        crate::store::command::Provenance::External { handle: None },
        actor.scopes.authority(root_scope),
    );
    let gate = crate::testing::TransactionGate::new();
    actor.set_transaction_gate(Some(gate.attach().unwrap()));
    let mut cx = Context::from_waker(Waker::noop());
    let mut send = Box::pin(child.send(ChildAction::Push(8)));
    assert!(send.as_mut().poll(&mut cx).is_pending());
    let command = actor.receiver.try_recv().unwrap();
    let mut processing = Box::pin(actor.process_command(command));
    assert!(processing.as_mut().poll(&mut cx).is_pending());
    let boundary = gate.boundary();
    assert_eq!(
        gate.inspect_mounted(&root, |_| panic!("root context cannot inspect child input")),
        Ok(None::<()>)
    );
    assert_eq!(gate.boundary(), boundary);
    assert_eq!(
        gate.inspect_mounted(&child, |action| matches!(action, ChildAction::Push(8))),
        Ok(Some(true))
    );
    assert!(send.as_mut().poll(&mut cx).is_pending());
    assert!(gate.release_pending());
    assert!(processing.as_mut().poll(&mut cx).is_pending());
    assert!(matches!(send.as_mut().poll(&mut cx), Poll::Ready(Ok(_))));
    assert!(gate.acknowledge(boundary.unwrap()));
    let Poll::Ready(turn) = processing.as_mut().poll(&mut cx) else {
        panic!("acknowledged child transaction");
    };
    drop(processing);
    assert!(actor.launch(turn));
}

#[cfg(feature = "test-util")]
#[test]
fn mounted_frontier_retirement_destructor_reenters_gate_outside_all_guards() {
    for unwind in [false, true] {
        let (mut actor, _ingress, child, _, _, _, available) = fixture();
        let gate = crate::testing::TransactionGate::new();
        actor.set_transaction_gate(Some(gate.attach().unwrap()));
        let core = actor.transaction_gate.as_ref().unwrap().gate().clone();
        let reentrant = core.clone();
        let probe = GuardProbe {
            available: Arc::new(move || {
                // No other thread accesses this gate. Reject a held guard before
                // real re-entry, so a lock-order regression cannot strand a worker.
                if !reentrant.test_guard_available() {
                    return false;
                }
                let _ = reentrant.has_pending(); // Acquires the real gate lock.
                available() // Tries the real node, publication and input-slot guards.
            }),
            calls: Arc::new(AtomicUsize::new(0)),
            locked: Arc::new(AtomicUsize::new(0)),
        };
        let mut cx = Context::from_waker(Waker::noop());
        let mut send = Box::pin(child.send(ChildAction::DropProbe(ProbeDrop(probe.clone()))));
        assert!(send.as_mut().poll(&mut cx).is_pending());
        let command = actor.receiver.try_recv().unwrap();
        let mut processing = Box::pin(actor.process_command(command));
        assert!(processing.as_mut().poll(&mut cx).is_pending());

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            gate.inspect_mounted(&child, |_| {
                drop(send); // The terminal rejection must destroy the returned input.
                core.clear(); // Retirement waits for the exact typed inspection guard.
                assert_eq!(probe.calls.load(Ordering::SeqCst), 0);
                assert!(!unwind, "matcher unwind before restoration");
            })
            .unwrap();
        }));
        if unwind {
            let error = result.expect_err("the matcher must panic before restoration");
            assert_eq!(panic_message(&*error), "matcher unwind before restoration");
        } else {
            result.unwrap();
        }
        assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
        assert_eq!(probe.locked.load(Ordering::SeqCst), 0);
        assert!(!gate.has_pending());
        drop(processing);
    }
}

#[derive(Clone)]
/// Records callback lock state without panicking during destructor or wake cleanup.
struct GuardProbe {
    /// Borrows actual guard state without retaining the installed Feature.
    available: Arc<dyn Fn() -> bool + Send + Sync>,
    /// Counts all callback, destruction and wake observations.
    calls: Arc<AtomicUsize>,
    /// Counts observations that found any checked guard held.
    locked: Arc<AtomicUsize>,
}
impl GuardProbe {
    /// Records the boundary so ordinary assertions can run after cleanup.
    fn observe(&self) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if !(self.available)() {
            self.locked.fetch_add(1, Ordering::SeqCst);
        }
    }
}
/// Runs the same boundary observation when an owned input or capture is destroyed.
struct ProbeDrop(GuardProbe);
impl Drop for ProbeDrop {
    fn drop(&mut self) {
        self.0.observe();
    }
}
impl std::task::Wake for GuardProbe {
    fn wake(self: Arc<Self>) {
        self.observe();
    }
}

#[test]
fn reducer_unwind_releases_node_before_context_cleanup_and_reply_wake() {
    let (mut actor, _ingress, child, driver, _, _, available) = fixture();
    let probe = GuardProbe {
        available,
        calls: Arc::new(AtomicUsize::new(0)),
        locked: Arc::new(AtomicUsize::new(0)),
    };
    let waker = Waker::from(Arc::new(probe.clone()));
    let mut cx = Context::from_waker(&waker);
    let mut send = Box::pin(child.send(ChildAction::Panic(probe.clone())));
    assert!(send.as_mut().poll(&mut cx).is_pending());
    let command = actor.receiver.try_recv().unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // The test-util dispatcher is async; the ordinary dispatcher
        // computes its Ready<Turn> synchronously. Catch both entry points.
        let mut processing = Box::pin(actor.process_command(command));
        processing.as_mut().poll(&mut cx)
    }));
    let error = result.err().expect("the mounted reducer must panic");
    assert_eq!(panic_message(&*error), "mounted reducer panic");
    assert_eq!(
        probe.calls.load(Ordering::SeqCst),
        4,
        "reducer callback, Feature destructor, unpolled task capture destructor and reply wake"
    );
    assert_eq!(probe.locked.load(Ordering::SeqCst), 0);
    assert!(driver.tasks.lock().unwrap().is_empty());
    let Poll::Ready(Err(crate::TaskSendError::Failed { error })) = send.as_mut().poll(&mut cx)
    else {
        panic!("entered reducer failed")
    };
    assert_eq!(error.progress(), crate::TaskProgress::MayHaveRun);
}

#[test]
fn cancelling_child_receipt_before_first_poll_cleans_tasks_timers_and_sequences() {
    let (mut actor, _ingress, child, driver, polls, _, _) = fixture();
    let mut cx = Context::from_waker(Waker::noop());
    let mut send = Box::pin(child.send(ChildAction::Start));
    assert!(send.as_mut().poll(&mut cx).is_pending());
    dispatch(&mut actor);
    let Poll::Ready(Ok(receipt)) = send.as_mut().poll(&mut cx) else {
        panic!("child committed")
    };
    receipt.cancel();
    // Cancellation wakes the actor; harvesting it cancels registered futures
    // before this runtime gives them their first poll.
    let turn = actor.process_control();
    assert!(actor.launch(turn));
    for mut task in std::mem::take(&mut *driver.tasks.lock().unwrap()) {
        assert!(task.as_mut().poll(&mut cx).is_ready());
    }
    let turn = actor.process_control();
    assert!(actor.launch(turn));
    assert_eq!(polls.load(Ordering::SeqCst), 0);
    assert_eq!(
        actor.state.as_ref().unwrap().child.as_deref(),
        Some([].as_slice())
    );
    assert!(actor.timers.is_empty());
    assert!(actor.sequences.is_empty());
    #[cfg(feature = "test-util")]
    assert_eq!(actor.registry.live_task_ids(), [] as [u64; 0]);
    assert!(receipt.is_finished());
    assert_eq!(
        actor.mounted.len(),
        1,
        "cancelling work does not retire its Feature"
    );
}

#[test]
fn issue_callback_admits_child_input_without_reentering_leased_feature() {
    type PendingSend = std::pin::Pin<
        Box<
            dyn Future<Output = Result<crate::ActionTask, crate::TaskSendError<ChildAction>>>
                + Send,
        >,
    >;
    let (mut actor, _ingress, child, _, _, _, available) = fixture();
    let queued: Arc<Mutex<Option<PendingSend>>> = Arc::new(Mutex::new(None));
    let callback_queue = Arc::clone(&queued);
    let endpoint = child.clone();
    actor.issues = crate::IssueReporter::new(move |_| {
        assert!(available(), "issue handler runs outside the node guard");
        let mut send = Box::pin(endpoint.send(ChildAction::Push(5)));
        assert!(
            send.as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        *callback_queue.lock().unwrap() = Some(send);
    });
    let mut cx = Context::from_waker(Waker::noop());
    let mut notify = Box::pin(child.send(ChildAction::Notify));
    assert!(notify.as_mut().poll(&mut cx).is_pending());
    dispatch(&mut actor);
    assert!(matches!(notify.as_mut().poll(&mut cx), Poll::Ready(Ok(_))));
    assert_eq!(
        actor.state.as_ref().unwrap().child.as_deref(),
        Some([].as_slice())
    );
    let mut queued = queued.lock().unwrap().take().unwrap();
    assert!(queued.as_mut().poll(&mut cx).is_pending());
    dispatch(&mut actor);
    assert!(matches!(queued.as_mut().poll(&mut cx), Poll::Ready(Ok(_))));
    assert_eq!(
        actor.state.as_ref().unwrap().child.as_deref(),
        Some([5].as_slice())
    );
}

#[cfg(feature = "test-util")]
#[test]
fn held_mounted_frontier_harvests_independent_work_and_skip_recovers_child_input() {
    let (mut actor, _ingress, child, driver, _, _, _) = fixture();
    let mut cx = Context::from_waker(Waker::noop());
    let mut work = Box::pin(child.send(ChildAction::Work));
    assert!(work.as_mut().poll(&mut cx).is_pending());
    dispatch(&mut actor);
    let Poll::Ready(Ok(receipt)) = work.as_mut().poll(&mut cx) else {
        panic!("work registered")
    };
    let mut runtime_work = driver.tasks.lock().unwrap().pop().unwrap();
    let gate = crate::testing::TransactionGate::new();
    actor.set_transaction_gate(Some(gate.attach().unwrap()));
    let mut send = Box::pin(child.send(ChildAction::Push(8)));
    assert!(send.as_mut().poll(&mut cx).is_pending());
    let command = actor.receiver.try_recv().unwrap();
    let mut processing = Box::pin(actor.process_command(command));
    assert!(processing.as_mut().poll(&mut cx).is_pending());
    assert!(gate.has_pending());
    assert!(!receipt.is_finished());
    assert!(runtime_work.as_mut().poll(&mut cx).is_ready());
    drop(runtime_work);
    assert!(processing.as_mut().poll(&mut cx).is_pending());
    assert!(receipt.is_finished());
    assert!(gate.has_pending());
    assert!(gate.skip_pending());
    let Poll::Ready(turn) = processing.as_mut().poll(&mut cx) else {
        panic!("skip released frontier")
    };
    drop(processing);
    assert!(actor.launch(turn));
    assert!(
        matches!(send.as_mut().poll(&mut cx),Poll::Ready(Err(crate::TaskSendError::Rejected { action:ChildAction::Push(8),error })) if error.progress()==crate::TaskProgress::NotStarted)
    );
    assert_eq!(
        actor.state.as_ref().unwrap().child.as_deref(),
        Some([].as_slice())
    );
}

#[test]
fn child_interval_stops_factory_after_scope_retirement() {
    let (mut actor, _ingress, child, driver, calls, _, _) = fixture();
    let mut cx = Context::from_waker(Waker::noop());
    let mut start = Box::pin(child.send(ChildAction::Interval));
    assert!(start.as_mut().poll(&mut cx).is_pending());
    dispatch(&mut actor);
    let Poll::Ready(Ok(receipt)) = start.as_mut().poll(&mut cx) else {
        panic!("interval registered")
    };
    driver.seconds.store(1, Ordering::SeqCst);
    assert!(futures_executor::block_on(actor.process_timer_batch()));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(!receipt.is_finished());
    assert_eq!(
        actor.state.as_ref().unwrap().child.as_deref(),
        Some([7].as_slice())
    );
    let mut retire = Box::pin(child.send(ChildAction::Retire));
    assert!(retire.as_mut().poll(&mut cx).is_pending());
    dispatch(&mut actor);
    assert!(matches!(retire.as_mut().poll(&mut cx), Poll::Ready(Ok(_))));
    let turn = actor.process_control();
    assert!(actor.launch(turn));
    driver.seconds.store(2, Ordering::SeqCst);
    assert!(futures_executor::block_on(actor.process_timer_batch()));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(receipt.is_finished());
    assert!(actor.timers.is_empty());
    #[cfg(feature = "test-util")]
    assert_eq!(actor.registry.live_task_ids(), [] as [u64; 0]);
}

#[test]
fn mounted_driver_wait_rechecks_its_manual_clock() {
    let driver = Driver::default();
    let context = &mut Context::from_waker(Waker::noop());
    let mut elapsed = driver.sleep_until(MonotonicInstant::ZERO);
    assert!(
        elapsed.as_mut().poll(context).is_ready(),
        "elapsed deadline must be ready"
    );

    let mut future = driver.sleep_until(MonotonicInstant::from_elapsed(Duration::from_secs(2)));
    let mut maximum = driver.sleep_until(MonotonicInstant::MAX);
    assert!(future.as_mut().poll(context).is_pending());
    assert!(maximum.as_mut().poll(context).is_pending());
    driver.seconds.store(1, Ordering::SeqCst);
    assert!(future.as_mut().poll(context).is_pending());
    driver.seconds.store(2, Ordering::SeqCst);
    assert!(
        future.as_mut().poll(context).is_ready(),
        "an existing wait must observe manual advance"
    );
    assert!(maximum.as_mut().poll(context).is_pending());
}

#[test]
fn mounted_driver_yields_and_wakes_before_completing() {
    /// Counts notifications without borrowing actor or local task state.
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
