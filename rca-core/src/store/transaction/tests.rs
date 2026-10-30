//! Drives borrowed production parts; no test reduction or commit implementation.

use super::*;
#[cfg(feature = "local")]
use crate::store::local;
use crate::store::mounted::path::BorrowPath;
use crate::store::mounted::{OptionalPath, SharedNode};
use crate::store::scope::SelectedChild;
use std::cell::Cell;
#[cfg(feature = "local")]
use std::cell::RefCell;
#[cfg(feature = "local")]
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};

pub(in crate::store) use super::test_support::Host;
use super::test_support::counter_host;

/// Counts actual receipt wake calls, independently of manual future polling.
#[derive(Default)]
struct Wakes(AtomicUsize);

impl Wake for Wakes {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// Keeps local state non-Send while exposing committed values outside the host.
#[cfg(feature = "local")]
struct LocalState(Rc<Cell<usize>>);
/// Starts concurrent local work or carries its owned update.
#[cfg(feature = "local")]
enum LocalAction {
    Start,
    Add(Rc<usize>),
}
/// Records lazy factory entry and typed read/update results through the real kernel.
#[cfg(feature = "local")]
struct LocalFeature {
    factories: Rc<Cell<usize>>,
    results: Rc<RefCell<Vec<usize>>>,
}
#[cfg(feature = "local")]
impl local::Feature for LocalFeature {
    type State = LocalState;
    type Action = LocalAction;
    fn update(
        &mut self,
        state: &mut LocalState,
        action: &LocalAction,
        context: &mut local::FeatureContext<'_, LocalState, LocalAction>,
    ) {
        match action {
            LocalAction::Start => {
                let factories = Rc::clone(&self.factories);
                let amount = Rc::new(3);
                context.effect(
                    local::Effect::run(move |sender| async move {
                        factories.set(factories.get() + 1);
                        sender.send(*amount).await.unwrap();
                    })
                    .map(|amount| LocalAction::Add(Rc::new(amount))),
                );
                let results = Rc::clone(&self.results);
                context.task(local::Task::<LocalState, LocalAction>::run(
                    move |context| async move {
                        let _ = context.send(LocalAction::Add(Rc::new(2))).await.unwrap();
                        let read = context.read(|state| Rc::new(state.0.get())).await.unwrap();
                        let capture = Rc::new(5);
                        let result = context
                            .update(move |state| {
                                state.0.set(state.0.get() + *capture);
                                Rc::new(state.0.get())
                            })
                            .await
                            .unwrap();
                        results.borrow_mut().extend([
                            *read,
                            **result.value(),
                            usize::try_from(result.revision())
                                .expect("fixture revision fits usize"),
                        ]);
                    },
                ));
            }
            LocalAction::Add(amount) => state.0.set(state.0.get() + **amount),
        }
    }
}

/// Allows the ancestor lifetime to end independently of the nested child lifetime.
#[derive(Default)]
struct ParentState {
    child: Option<ChildState>,
}
/// Allows a grandchild to be removed and reinserted without replacing its parent.
struct ChildState {
    grandchild: Option<usize>,
}
/// Routes nested work or replaces the ancestor's entire subtree.
enum ParentAction {
    Child(ChildAction),
    Remove,
    Reinsert,
}
/// Routes grandchild work or replaces only that child's state.
enum ChildAction {
    Grandchild(()),
    Remove,
    Reinsert,
}

/// Counts projection entry so rejection can be proved to happen before user selection.
#[derive(Clone)]
struct CountedPath<P> {
    path: P,
    selections: Arc<AtomicUsize>,
}
impl<R, P: BorrowPath<R>> BorrowPath<R> for CountedPath<P> {
    type State = P::State;
    fn read<'a>(&self, root: &'a R) -> Option<&'a Self::State> {
        self.selections.fetch_add(1, Ordering::SeqCst);
        self.path.read(root)
    }
    fn write<'a>(&self, root: &'a mut R) -> Option<&'a mut Self::State> {
        self.selections.fetch_add(1, Ordering::SeqCst);
        self.path.write(root)
    }
}

/// Supports mounted reads and updates; action reduction would invalidate the scenario.
struct ProjectionFeature;
impl crate::Feature for ProjectionFeature {
    type State = usize;
    type Action = ();
    fn update(&mut self, _: &mut usize, (): &(), _: &mut crate::FeatureContext<'_, usize, ()>) {
        panic!("projection witnesses never send child actions");
    }
}

/// Keeps a mounted node alive while returning its weak, typed access context.
fn mounted_access<R: Send + 'static, A: Send + 'static>(
    host: &mut Host<R, A, Shared>,
    token: ScopeToken,
    path: impl BorrowPath<R, State = usize> + Send + Sync + 'static,
) -> crate::TaskContext<usize, ()> {
    let node = SharedNode::new(ProjectionFeature, token, 0);
    let context = node
        .downgrade()
        .context(path, std::sync::Weak::clone(&host.weak), &host.scopes);
    host.mounted.push(Box::new(node));
    context
}

/// Projects through both optional lifetimes and counts each actual selection.
fn scoped_access(
    host: &mut Host<ParentState, ParentAction, Shared>,
    token: ScopeToken,
    selections: Arc<AtomicUsize>,
) -> crate::TaskContext<usize, ()> {
    mounted_access(
        host,
        token,
        CountedPath {
            path: OptionalPath::new(
                |state: &ParentState| state.child.as_ref()?.grandchild.as_ref(),
                |state: &mut ParentState| state.child.as_mut()?.grandchild.as_mut(),
            ),
            selections,
        },
    )
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "keep stale incarnation, stale ancestor and fresh access checks in one mount lifetime"
)]
fn installed_incarnation_and_ancestor_reject_before_selection_then_fresh_access_succeeds() {
    use crate::{ActionPath, IfSome, OptionalStatePath, Reduce};
    let token = Arc::new(std::sync::Mutex::new(None));
    let captured = Arc::clone(&token);
    let grandchild = Reduce::new(
        move |_: &mut usize, (): &(), context: &mut ReduceContext<'_>| {
            *captured.lock().unwrap() = Some(context.scope_token());
            crate::Effect::none()
        },
    );
    let child = IfSome::new(
        Reduce::new(
            |state: &mut ChildState, action: &ChildAction, _: &mut ReduceContext<'_>| {
                match action {
                    ChildAction::Remove => state.grandchild = None,
                    ChildAction::Reinsert => state.grandchild = Some(0),
                    ChildAction::Grandchild(()) => {}
                }
                crate::Effect::none()
            },
        ),
        OptionalStatePath::new(
            |state: &ChildState| state.grandchild.as_ref(),
            |state: &mut ChildState| state.grandchild.as_mut(),
        ),
        ActionPath::new(
            |action: &ChildAction| {
                if let ChildAction::Grandchild(action) = action {
                    Some(action)
                } else {
                    None
                }
            },
            ChildAction::Grandchild,
        ),
        grandchild,
    );
    let reducer = IfSome::new(
        Reduce::new(
            |state: &mut ParentState, action: &ParentAction, _: &mut ReduceContext<'_>| {
                match action {
                    ParentAction::Remove => state.child = None,
                    ParentAction::Reinsert => {
                        state.child = Some(ChildState {
                            grandchild: Some(0),
                        });
                    }
                    ParentAction::Child(_) => {}
                }
                crate::Effect::none()
            },
        ),
        OptionalStatePath::new(
            |state: &ParentState| state.child.as_ref(),
            |state: &mut ParentState| state.child.as_mut(),
        ),
        ActionPath::new(
            |action: &ParentAction| {
                if let ParentAction::Child(action) = action {
                    Some(action)
                } else {
                    None
                }
            },
            ParentAction::Child,
        ),
        child,
    );
    let mut host = Host::<_, _, Shared>::new(
        ParentState {
            child: Some(ChildState {
                grandchild: Some(0),
            }),
        },
        Box::new(reducer),
    );
    drop(host.action(ParentAction::Child(ChildAction::Grandchild(()))));
    let initial = token.lock().unwrap().unwrap();
    let selects = Arc::new(AtomicUsize::new(0));
    let old = scoped_access(&mut host, initial, Arc::clone(&selects));
    drop(host.action(ParentAction::Child(ChildAction::Remove)));
    drop(host.action(ParentAction::Child(ChildAction::Reinsert)));
    let updates = Arc::new(AtomicUsize::new(0));
    let called = Arc::clone(&updates);
    let mut rejected = Box::pin(old.update(move |value| {
        called.fetch_add(1, Ordering::SeqCst);
        *value += 1;
    }));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(rejected.as_mut().poll(&mut cx).is_pending());
    let revision = host.version;
    let turn = host.next();
    let Poll::Ready(Err(error)) = rejected.as_mut().poll(&mut cx) else {
        panic!("old child rejected")
    };
    #[cfg(feature = "test-util")]
    assert!(matches!(turn.receipt.as_ref(), Some(Err(recorded)) if *recorded == error));
    drop(turn);
    assert_eq!(
        (error.reason(), error.progress()),
        (TaskAccessReason::ScopeEnded, TaskProgress::NotStarted)
    );
    assert_eq!(selects.load(Ordering::SeqCst), 0);
    assert_eq!(updates.load(Ordering::SeqCst), 0);
    assert_eq!(host.version, revision);
    drop(host.action(ParentAction::Child(ChildAction::Grandchild(()))));
    let installed = token.lock().unwrap().unwrap();
    assert_ne!(initial, installed);
    let descendant = scoped_access(&mut host, installed, Arc::clone(&selects));
    drop(host.action(ParentAction::Remove));
    drop(host.action(ParentAction::Reinsert));
    let mut read = Box::pin(descendant.read(|value| *value));
    assert!(read.as_mut().poll(&mut cx).is_pending());
    drop(host.next());
    let Poll::Ready(Err(error)) = read.as_mut().poll(&mut cx) else {
        panic!("ancestor retirement rejected")
    };
    assert_eq!(error.progress(), TaskProgress::NotStarted);
    assert_eq!(selects.load(Ordering::SeqCst), 0);
    drop(host.action(ParentAction::Child(ChildAction::Grandchild(()))));
    let fresh = scoped_access(
        &mut host,
        token.lock().unwrap().unwrap(),
        Arc::clone(&selects),
    );
    let mut update = Box::pin(fresh.update(|state| {
        *state = 7;
        Cell::new(*state)
    }));
    assert!(update.as_mut().poll(&mut cx).is_pending());
    drop(host.next());
    let Poll::Ready(Ok(result)) = update.as_mut().poll(&mut cx) else {
        panic!("fresh typed access commits")
    };
    assert_eq!(result.value().get(), 7);
    assert_eq!(selects.load(Ordering::SeqCst), 1);
    let (_foreign_lifecycle, foreign) = ScopeRegistry::for_test();
    let foreign_node = SharedNode::new(ProjectionFeature, foreign.root(), 0);
    let foreign_access = foreign_node.downgrade().context(
        CountedPath {
            path: OptionalPath::new(
                |state: &ParentState| state.child.as_ref()?.grandchild.as_ref(),
                |state: &mut ParentState| state.child.as_mut()?.grandchild.as_mut(),
            ),
            selections: Arc::clone(&selects),
        },
        std::sync::Weak::clone(&host.weak),
        &foreign,
    );
    host.mounted.push(Box::new(foreign_node));
    let mut wrong_root = Box::pin(foreign_access.update(|_| panic!("foreign root never enters")));
    assert!(wrong_root.as_mut().poll(&mut cx).is_pending());
    drop(host.next());
    assert!(
        matches!(wrong_root.as_mut().poll(&mut cx), Poll::Ready(Err(error)) if error.progress() == TaskProgress::NotStarted)
    );
    assert_eq!(
        selects.load(Ordering::SeqCst),
        1,
        "foreign root rejected before selector entry"
    );
}

#[test]
#[cfg(feature = "local")]
fn local_rc_mapped_run_and_typed_task_use_one_capacity_one_production_transaction() {
    let factories = Rc::new(Cell::new(0));
    let results = Rc::new(RefCell::new(Vec::new()));
    let mut host = Host::<_, _, local::Local>::new(
        LocalState(Rc::new(Cell::new(1))),
        Box::new(local::Driver(LocalFeature {
            factories: Rc::clone(&factories),
            results: Rc::clone(&results),
        })),
    );
    let (turn, receive) = host.action(LocalAction::Start);
    assert_eq!(host.version, 1);
    assert_eq!(
        factories.get(),
        0,
        "registration does not invoke the user factory"
    );
    let task = futures_executor::block_on(receive).unwrap().unwrap();
    assert!(!task.is_finished());
    let mut launches = turn.launches.into_iter();
    let mut run = launches.next().unwrap();
    let mut typed = launches.next().unwrap();
    assert!(launches.next().is_none());
    let (reply, filler) = super::super::command::commit_channel();
    host.ingress
        .try_send(Command::Action(ActionCommand::external(
            LocalAction::Add(Rc::new(1)),
            reply,
        )))
        .as_ref()
        .expect("filler command admission succeeds");
    let wakes = Arc::new(Wakes::default());
    let task_waker = Waker::from(Arc::clone(&wakes));
    let mut cx = Context::from_waker(&task_waker);
    assert!(run.as_mut().poll(&mut cx).is_pending());
    assert_eq!(factories.get(), 1);
    assert_eq!(
        wakes.0.load(Ordering::SeqCst),
        0,
        "full admission does not self-wake"
    );
    drop(host.next());
    assert!(
        wakes.0.load(Ordering::SeqCst) > 0,
        "capacity release wakes the exact pending producer"
    );
    futures_executor::block_on(filler)
        .unwrap()
        .as_ref()
        .expect("filler command commits");
    assert!(run.as_mut().poll(&mut cx).is_pending());
    drop(host.next());
    assert!(run.as_mut().poll(&mut cx).is_ready());
    drop(run);
    for _ in 0..3 {
        assert!(typed.as_mut().poll(&mut cx).is_pending());
        drop(host.next());
    }
    assert!(typed.as_mut().poll(&mut cx).is_ready());
    drop(typed);
    assert!(!task.is_finished(), "registry destruction owns completion");
    drop(host.collect());
    assert!(task.is_finished());
    assert_eq!(*results.borrow(), [7, 12, 5]);
    assert_eq!(host.state.0.get(), 12);
    assert_eq!(host.version, 5, "read consumes no revision");
}

#[test]
#[cfg(feature = "local")]
fn local_native_reducer_maps_future_fallible_and_interval_leaves_through_one_program() {
    let capture = Rc::new(1usize);
    let reducer = local::Reduce::new(
        move |state: &mut LocalState, action: &LocalAction, _: &mut ReduceContext<'_>| {
            let LocalAction::Start = action else {
                if let LocalAction::Add(amount) = action {
                    state.0.set(state.0.get() + **amount);
                }
                return local::Effect::none();
            };
            let future_capture = Rc::clone(&capture);
            let error = Rc::clone(&capture);
            let interval = Rc::clone(&capture);
            local::Effect::from_parts(
                Program::<usize, local::LocalEffects>::Merge(vec![
                    local::Effect::none(),
                    local::Effect::send(1),
                    local::Effect::future(async move { Some(*future_capture) }),
                    local::Effect::try_run(
                        move |_| async move { Err(crate::EffectError::Failed(error)) },
                        |error, sender| async move {
                            sender.send(*error).await.unwrap();
                        },
                    ),
                    local::Effect::interval(std::time::Duration::from_secs(1), move || *interval),
                ]),
                None,
            )
            .map(|amount| LocalAction::Add(Rc::new(amount)))
        },
    );
    let mut host = Host::<_, _, local::Local>::new(
        LocalState(Rc::new(Cell::new(0))),
        Box::new(local::Driver(reducer)),
    );
    let (turn, receipt) = host.action(LocalAction::Start);
    let task = futures_executor::block_on(receipt).unwrap().unwrap();
    let mut cx = Context::from_waker(Waker::noop());
    for mut launch in turn.launches {
        assert!(launch.as_mut().poll(&mut cx).is_pending());
        drop(host.next());
        assert!(launch.as_mut().poll(&mut cx).is_ready());
    }
    assert_eq!(host.state.0.get(), 3);
    let mut due = host.timers.pop_due(MonotonicInstant::MAX, 1);
    let timer = due
        .pop()
        .expect("local interval retained in the real timer queue");
    let DeferredTimer::Root(TimerKind::Interval {
        mut make_action, ..
    }) = timer.payload.kind
    else {
        panic!("interval leaf")
    };
    let p = timer.payload.provenance;
    let action = ActionCommand::effect(
        make_action(),
        super::super::command::CommitSender::disconnected(),
        p.scope,
        p.lineage,
        p.task,
        p.generations,
        p.operations,
        timer.payload.operation_work,
    );
    drop(commit(host.parts(), Mutation::Action(action, None)));
    host.registry
        .complete_actor(timer.payload.task, TaskStatus::Completed);
    drop(host.collect());
    assert_eq!(host.state.0.get(), 4);
    assert!(task.is_finished());
}

/// Records exact capture destruction while the test retains the counter owner.
struct CountDrop(Arc<AtomicUsize>);

impl Drop for CountDrop {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn typed_admission_is_lazy_wake_driven_and_receiver_drop_does_not_cancel_update() {
    let mut host = counter_host();
    let context = host.context();
    let drops = Arc::new(AtomicUsize::new(0));
    let capture = CountDrop(Arc::clone(&drops));
    drop(context.update(move |_| {
        drop(capture);
    }));
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert!(matches!(
        host.receiver.try_recv(),
        Err(async_channel::TryRecvError::Empty)
    ));
    let (reply, receive) = super::super::command::commit_channel();
    host.ingress
        .try_send(Command::Action(ActionCommand::external(1, reply)))
        .as_ref()
        .expect("command admission succeeds");
    let result = CountDrop(Arc::clone(&drops));
    let mut update = Box::pin(context.update(move |state| {
        *state += 2;
        result
    }));
    let wakes = Arc::new(Wakes::default());
    let task_waker = Waker::from(Arc::clone(&wakes));
    let mut cx = Context::from_waker(&task_waker);
    assert!(update.as_mut().poll(&mut cx).is_pending());
    assert_eq!(wakes.0.load(Ordering::SeqCst), 0);
    drop(host.next());
    assert!(wakes.0.load(Ordering::SeqCst) > 0);
    let _ = futures_executor::block_on(receive).unwrap().unwrap();
    assert!(update.as_mut().poll(&mut cx).is_pending());
    drop(update);
    drop(host.next());
    assert_eq!(host.state, 3);
    assert_eq!(host.version, 2);
    assert_eq!(
        drops.load(Ordering::SeqCst),
        2,
        "owned result drops once even after caller disappears"
    );
}

#[test]
#[expect(
    clippy::borrowed_box,
    reason = "the reducer Action is Box<usize> so the test can verify original allocation identity"
)]
fn full_close_returns_original_action_and_actor_discard_is_not_a_false_scope_rejection() {
    let host = Host::<usize, Box<usize>, Shared>::new(
        0,
        Box::new(crate::Reduce::new(
            |state: &mut usize, action: &Box<usize>, _: &mut ReduceContext<'_>| {
                *state += **action;
                crate::Effect::none()
            },
        )),
    );
    let context = host.context();
    let (reply, _receive) = super::super::command::commit_channel();
    host.ingress
        .try_send(Command::Action(ActionCommand::external(Box::new(1), reply)))
        .as_ref()
        .expect("boxed action admission succeeds");
    let action = Box::new(7);
    let address = &raw const *action;
    let mut send = Box::pin(context.send(action));
    let wakes = Arc::new(Wakes::default());
    let task_waker = Waker::from(Arc::clone(&wakes));
    let mut cx = Context::from_waker(&task_waker);
    assert!(send.as_mut().poll(&mut cx).is_pending());
    assert_eq!(wakes.0.load(Ordering::SeqCst), 0);
    host.ingress.close();
    assert!(wakes.0.load(Ordering::SeqCst) > 0);
    let Poll::Ready(Err(crate::TaskSendError::Rejected { action, error })) =
        send.as_mut().poll(&mut cx)
    else {
        panic!("closed pending admission returns payload")
    };
    assert_eq!(&raw const *action, address);
    assert_eq!(
        (error.reason(), error.progress()),
        (TaskAccessReason::StoreClosed, TaskProgress::NotStarted)
    );
    let other = counter_host();
    let mut discarded = Box::pin(other.context().send(9));
    assert!(discarded.as_mut().poll(&mut cx).is_pending());
    drop(other.receiver.try_recv().unwrap());
    let Poll::Ready(Err(crate::TaskSendError::Rejected { action, error })) =
        discarded.as_mut().poll(&mut cx)
    else {
        panic!("discard retains original action")
    };
    assert_eq!(action, 9);
    assert_eq!(
        (error.reason(), error.progress()),
        (TaskAccessReason::ActorFailed, TaskProgress::NotStarted)
    );
    assert_eq!(other.version, 0);
}

#[test]
fn query_update_and_publication_panics_report_the_last_actual_phase() {
    let mut host = counter_host();
    let context = host.context();
    let mut cx = Context::from_waker(Waker::noop());
    let mut query = Box::pin(context.read(|_| -> usize { panic!("query panic") }));
    assert!(query.as_mut().poll(&mut cx).is_pending());
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| host.next())).is_err());
    assert!(
        matches!(query.as_mut().poll(&mut cx), Poll::Ready(Err(error)) if error.progress() == TaskProgress::MayHaveRun)
    );
    let mut update = Box::pin(context.update(|state| {
        *state = 3;
        panic!("update panic");
    }));
    assert!(update.as_mut().poll(&mut cx).is_pending());
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| host.next())).is_err());
    assert!(
        matches!(update.as_mut().poll(&mut cx), Poll::Ready(Err(error)) if error.progress() == TaskProgress::MayHaveRun)
    );
    assert_eq!(host.state, 3);
    assert_eq!(host.version, 0);
    let signal = crate::observation::ObservationSignal::new(Arc::downgrade(&host.control));
    let (registration, _ack) = super::super::observation::new_registration(
        |state: &usize| {
            assert_ne!(*state, 4, "publication panic");
            *state
        },
        signal,
    );
    registration.execute(
        &host.state,
        &mut host.observations,
        host.version,
        host.scopes.root(),
        None,
        &SelectedChild::root(&host.state),
    );
    let mut committed = Box::pin(context.update(|state| *state = 4));
    assert!(committed.as_mut().poll(&mut cx).is_pending());
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| host.next())).is_err());
    assert!(
        matches!(committed.as_mut().poll(&mut cx), Poll::Ready(Err(error)) if error.progress() == TaskProgress::Committed { revision: 1 })
    );
    assert_eq!(host.state, 4);
    assert_eq!(host.version, 1);
}

/// Makes original allocation identity and exactly-once destruction observable.
struct OwnedAction {
    drops: CountDrop,
    panic: bool,
    identity: Box<usize>,
}
/// Mutates state before the requested panic to distinguish entry from commit.
fn owned_action_host() -> Host<usize, OwnedAction, Shared> {
    Host::<usize, OwnedAction, Shared>::new(
        0,
        Box::new(crate::Reduce::new(
            |state: &mut usize, action: &OwnedAction, _: &mut ReduceContext<'_>| {
                let _ = &action.drops;
                *state += *action.identity;
                assert!(!action.panic, "reducer entry panic");
                crate::Effect::none()
            },
        )),
    )
}
/// Creates a move-only payload with an independently retained destruction counter.
fn owned_action(drops: &Arc<AtomicUsize>, panic: bool) -> OwnedAction {
    OwnedAction {
        drops: CountDrop(Arc::clone(drops)),
        panic,
        identity: Box::new(1),
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "keep all payload ownership outcomes and exact drop counts in one scenario"
)]
fn non_clone_send_payload_drops_once_across_abandon_reject_enter_commit_and_waiter_loss() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut host = owned_action_host();
    let context = host.context();
    let mut cx = Context::from_waker(Waker::noop());
    drop(context.send(owned_action(&drops, false)));
    assert_eq!(
        drops.load(Ordering::SeqCst),
        1,
        "unpolled call owns its action"
    );

    let action = owned_action(&drops, false);
    let identity = &raw const *action.identity;
    let mut abandoned = Box::pin(context.send(action));
    assert!(abandoned.as_mut().poll(&mut cx).is_pending());
    drop(host.receiver.try_recv().unwrap());
    let Poll::Ready(Err(crate::TaskSendError::Rejected { action, error })) =
        abandoned.as_mut().poll(&mut cx)
    else {
        panic!("abandoned command returns action")
    };
    assert_eq!(&raw const *action.identity, identity);
    assert_eq!(error.progress(), TaskProgress::NotStarted);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    drop(action);
    assert_eq!(drops.load(Ordering::SeqCst), 2);

    let mut entered = Box::pin(context.send(owned_action(&drops, true)));
    assert!(entered.as_mut().poll(&mut cx).is_pending());
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| host.next())).is_err());
    assert!(
        matches!(entered.as_mut().poll(&mut cx), Poll::Ready(Err(crate::TaskSendError::Failed { error })) if error.progress() == TaskProgress::MayHaveRun)
    );
    assert_eq!(
        (host.state, host.version, drops.load(Ordering::SeqCst)),
        (1, 0, 3)
    );

    let signal = crate::observation::ObservationSignal::new(Arc::downgrade(&host.control));
    let (registration, _ack) = super::super::observation::new_registration(
        |state: &usize| {
            assert_ne!(*state, 2, "publication panic");
            *state
        },
        signal,
    );
    registration.execute(
        &host.state,
        &mut host.observations,
        host.version,
        host.scopes.root(),
        None,
        &SelectedChild::root(&host.state),
    );
    let mut committed = Box::pin(context.send(owned_action(&drops, false)));
    assert!(committed.as_mut().poll(&mut cx).is_pending());
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| host.next())).is_err());
    assert!(
        matches!(committed.as_mut().poll(&mut cx), Poll::Ready(Err(crate::TaskSendError::Failed { error })) if error.progress() == TaskProgress::Committed { revision: 1 })
    );
    assert_eq!(
        (host.state, host.version, drops.load(Ordering::SeqCst)),
        (2, 1, 4)
    );

    let mut completed = Box::pin(context.send(owned_action(&drops, false)));
    assert!(completed.as_mut().poll(&mut cx).is_pending());
    drop(host.next());
    assert!(
        matches!(completed.as_mut().poll(&mut cx), Poll::Ready(Ok(task)) if task.is_finished())
    );
    assert_eq!(
        (host.state, host.version, drops.load(Ordering::SeqCst)),
        (3, 2, 5)
    );

    let mut lost_waiter = Box::pin(context.send(owned_action(&drops, false)));
    assert!(lost_waiter.as_mut().poll(&mut cx).is_pending());
    drop(lost_waiter);
    drop(host.next());
    assert_eq!(
        (host.state, host.version, drops.load(Ordering::SeqCst)),
        (4, 3, 6)
    );

    host.ingress.close();
    let action = owned_action(&drops, false);
    let identity = &raw const *action.identity;
    let mut rejected = Box::pin(context.send(action));
    let Poll::Ready(Err(crate::TaskSendError::Rejected { action, error })) =
        rejected.as_mut().poll(&mut cx)
    else {
        panic!("closed ingress returns action")
    };
    assert_eq!(&raw const *action.identity, identity);
    assert_eq!(
        (error.reason(), error.progress()),
        (TaskAccessReason::StoreClosed, TaskProgress::NotStarted)
    );
    drop(action);
    assert_eq!(drops.load(Ordering::SeqCst), 7);

    let closed_host = owned_action_host();
    let closed_context = closed_host.context();
    let weak = Weak::clone(&closed_host.weak);
    drop(closed_host);
    assert!(
        weak.upgrade().is_none(),
        "retained context is not an actor lifetime owner"
    );
    let mut gone = Box::pin(closed_context.send(owned_action(&drops, false)));
    assert!(
        matches!(gone.as_mut().poll(&mut cx), Poll::Ready(Err(crate::TaskSendError::Rejected { error, .. })) if error.reason() == TaskAccessReason::StoreClosed)
    );
    assert_eq!(drops.load(Ordering::SeqCst), 8);
}

#[test]
fn admitted_projection_absence_and_panic_stamp_entry_without_running_callbacks() {
    let mut host = counter_host();
    let selections = Arc::new(AtomicUsize::new(0));
    let root = host.scopes.root();
    let missing = mounted_access(
        &mut host,
        root,
        CountedPath {
            path: OptionalPath::<usize, usize>::new(|_| None, |_| None),
            selections: Arc::clone(&selections),
        },
    );
    let mut update = Box::pin(missing.update(|_| panic!("absent projection never updates")));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(update.as_mut().poll(&mut cx).is_pending());
    let turn = host.next();
    let Poll::Ready(Err(error)) = update.as_mut().poll(&mut cx) else {
        panic!("absent projection rejects the update")
    };
    assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
    assert_eq!(error.progress(), TaskProgress::MayHaveRun);
    #[cfg(feature = "test-util")]
    assert!(matches!(turn.receipt.as_ref(), Some(Err(recorded)) if *recorded == error));
    drop(turn);
    assert_eq!((host.version, selections.load(Ordering::SeqCst)), (0, 1));
    let queries = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&queries);
    let mut read = Box::pin(missing.read(move |_| counted.fetch_add(1, Ordering::SeqCst)));
    assert!(read.as_mut().poll(&mut cx).is_pending());
    drop(host.next());
    let Poll::Ready(Err(error)) = read.as_mut().poll(&mut cx) else {
        panic!("absent projection rejects the read")
    };
    assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
    assert_eq!(error.progress(), TaskProgress::MayHaveRun);
    assert_eq!(queries.load(Ordering::SeqCst), 0);
    assert_eq!(Arc::strong_count(&queries), 1);
    assert_eq!((host.version, selections.load(Ordering::SeqCst)), (0, 2));
    let panicking = mounted_access(
        &mut host,
        root,
        OptionalPath::<usize, usize>::new(
            |_| panic!("read selector panic"),
            |_| panic!("update selector panic"),
        ),
    );
    let mut read = Box::pin(panicking.read(|_| panic!("query never entered")));
    assert!(read.as_mut().poll(&mut cx).is_pending());
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| host.next())).is_err());
    assert!(
        matches!(read.as_mut().poll(&mut cx), Poll::Ready(Err(error)) if error.progress() == TaskProgress::MayHaveRun)
    );
    let mut update = Box::pin(panicking.update(|_| panic!("update never entered")));
    assert!(update.as_mut().poll(&mut cx).is_pending());
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| host.next())).is_err());
    assert!(
        matches!(update.as_mut().poll(&mut cx), Poll::Ready(Err(error)) if error.progress() == TaskProgress::MayHaveRun)
    );
    assert_eq!(host.version, 0);
}

/// Proves a local capture is released once and on its original thread.
#[cfg(feature = "local")]
struct OwnerDrop {
    drops: Rc<Cell<usize>>,
    owner: std::thread::ThreadId,
}
#[cfg(feature = "local")]
impl Drop for OwnerDrop {
    fn drop(&mut self) {
        assert_eq!(std::thread::current().id(), self.owner);
        self.drops.set(self.drops.get() + 1);
    }
}
/// Moves one local capture into a lazily constructed pending task.
#[cfg(feature = "local")]
struct CancelLocal {
    capture: Option<OwnerDrop>,
    factories: Rc<Cell<usize>>,
}
#[cfg(feature = "local")]
impl local::Feature for CancelLocal {
    type State = Rc<Cell<usize>>;
    type Action = ();
    fn update(
        &mut self,
        _: &mut Self::State,
        (): &(),
        context: &mut local::FeatureContext<'_, Self::State, ()>,
    ) {
        let capture = self.capture.take().unwrap();
        let factories = Rc::clone(&self.factories);
        context.task(local::Task::run(move |_| {
            factories.set(factories.get() + 1);
            async move {
                std::hint::black_box(&capture);
                std::future::pending::<()>().await;
            }
        }));
    }
}

#[test]
#[cfg(feature = "local")]
fn local_metadata_commands_borrow_native_rc_ownership_without_a_frontend_adapter() {
    use crate::effect::operation::OperationDeclaration;
    use crate::store::operation::{OperationMetadata, OperationRequest, OperationTarget};
    use crate::store::task_context::ReplyGuard;

    /// Requires the common operation path to borrow native Rc-owned metadata.
    struct Metadata {
        identity: crate::effect::CancellationIdentity,
        target: OperationTarget,
        provenance: Provenance,
        identity_reads: Rc<Cell<usize>>,
    }
    impl OperationMetadata for Metadata {
        fn identity(&self) -> &crate::effect::CancellationIdentity {
            self.identity_reads.set(self.identity_reads.get() + 1);
            &self.identity
        }
        fn target(&self) -> &OperationTarget {
            &self.target
        }
        fn provenance(&self) -> &Provenance {
            &self.provenance
        }
    }

    let reducer = local::Reduce::new(
        |_: &mut Rc<Cell<usize>>, _: &Rc<usize>, _: &mut ReduceContext<'_>| local::Effect::none(),
    );
    let mut host =
        Host::<_, _, local::Local>::new(Rc::new(Cell::new(0)), Box::new(local::Driver(reducer)));
    let identity_reads = Rc::new(Cell::new(0));
    let metadata: Rc<dyn OperationMetadata> = Rc::new(Metadata {
        identity: OperationDeclaration::identity("native-metadata"),
        target: OperationTarget::Root {
            lifecycle: Arc::downgrade(&host.lifecycle),
        },
        provenance: Provenance::External { handle: None },
        identity_reads: Rc::clone(&identity_reads),
    });
    let (reply, status) = ReplyGuard::<crate::OperationStatus>::channel();
    host.ingress
        .try_send(Command::Operation {
            request: OperationRequest::Status(reply),
            metadata: Rc::clone(&metadata),
        })
        .as_ref()
        .expect("status command admission succeeds");
    drop(host.next());
    assert_eq!(
        futures_executor::block_on(status)
            .unwrap()
            .unwrap()
            .active_count(),
        0
    );
    let (reply, capture) = ReplyGuard::<crate::OperationWait>::channel();
    host.ingress
        .try_send(Command::Operation {
            request: OperationRequest::Capture(reply),
            metadata,
        })
        .as_ref()
        .expect("capture command admission succeeds");
    drop(host.next());
    let mut captured = Box::pin(futures_executor::block_on(capture).unwrap().unwrap());
    assert_eq!(
        captured
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop())),
        Poll::Ready(Ok(()))
    );
    assert_eq!(
        identity_reads.get(),
        2,
        "the common actor borrows local metadata"
    );
    assert_eq!(
        host.version, 0,
        "metadata requests do not mutate local state"
    );
}

#[test]
#[cfg(feature = "local")]
fn local_named_task_keeps_rc_captures_and_checks_self_wait_during_actual_poll() {
    /// Attempts a captured self-wait from inside the named operation's actual poll.
    struct NamedLocal {
        capture: Option<OwnerDrop>,
        wait: Rc<RefCell<Option<std::pin::Pin<Box<crate::OperationWait>>>>>,
        entered: Rc<Cell<usize>>,
        outcome: Rc<Cell<Option<Result<(), crate::SelfWait>>>>,
        release: Rc<Cell<bool>>,
    }
    impl local::Feature for NamedLocal {
        type State = Rc<Cell<usize>>;
        type Action = ();

        fn update(
            &mut self,
            _: &mut Self::State,
            (): &(),
            context: &mut local::FeatureContext<'_, Self::State, ()>,
        ) {
            let capture = self.capture.take().unwrap();
            let wait = Rc::clone(&self.wait);
            let entered = Rc::clone(&self.entered);
            let outcome = Rc::clone(&self.outcome);
            let release = Rc::clone(&self.release);
            context.task(local::Task::run_named(
                "native-local",
                crate::OperationPolicy::Coexist,
                move |_| async move {
                    let _capture = capture;
                    entered.set(entered.get() + 1);
                    let mut captured = wait.borrow_mut().take().unwrap();
                    outcome.set(Some(
                        std::future::poll_fn(|cx| captured.as_mut().poll(cx)).await,
                    ));
                    drop(captured);
                    std::future::poll_fn(|_| {
                        if release.get() {
                            Poll::Ready(())
                        } else {
                            Poll::Pending
                        }
                    })
                    .await;
                },
            ));
        }
    }

    let drops = Rc::new(Cell::new(0));
    let entered = Rc::new(Cell::new(0));
    let outcome = Rc::new(Cell::new(None));
    let release = Rc::new(Cell::new(false));
    let wait = Rc::new(RefCell::new(None));
    let mut host = Host::<_, _, local::Local>::new(
        Rc::new(Cell::new(0)),
        Box::new(local::Driver(NamedLocal {
            capture: Some(OwnerDrop {
                drops: Rc::clone(&drops),
                owner: std::thread::current().id(),
            }),
            wait: Rc::clone(&wait),
            entered: Rc::clone(&entered),
            outcome: Rc::clone(&outcome),
            release: Rc::clone(&release),
        })),
    );
    let (turn, receipt) = host.action(());
    let task = futures_executor::block_on(receipt).unwrap().unwrap();
    let identity = crate::effect::operation::OperationDeclaration::identity("native-local")
        .in_scope(host.scopes.root());
    assert_eq!(host.registry.operation_status(&identity).active_count(), 1);
    assert_eq!(entered.get(), 0, "compilation must not poll a local body");
    let mut captured = Box::pin(host.registry.capture_operation(&identity));
    let mut completion = Box::pin(host.registry.capture_operation(&identity));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(captured.as_mut().poll(&mut cx).is_pending());
    assert!(completion.as_mut().poll(&mut cx).is_pending());
    *wait.borrow_mut() = Some(captured);
    let mut launch = turn.launches.into_iter().next().unwrap();
    assert!(launch.as_mut().poll(&mut cx).is_pending());
    assert_eq!(entered.get(), 1);
    assert_eq!(outcome.get(), Some(Err(crate::SelfWait)));
    assert_eq!(drops.get(), 0);
    assert_eq!(host.registry.operation_status(&identity).active_count(), 1);
    assert!(completion.as_mut().poll(&mut cx).is_pending());

    release.set(true);
    assert!(launch.as_mut().poll(&mut cx).is_ready());
    assert_eq!(
        drops.get(),
        1,
        "the native Rc payload dies on its owner thread"
    );
    assert_eq!(completion.as_mut().poll(&mut cx), Poll::Ready(Ok(())));
    assert_eq!(host.registry.operation_status(&identity).active_count(), 0);
    assert!(
        !task.is_finished(),
        "the Ready runtime shell is still retained"
    );
    drop(host.collect());
    assert!(task.is_finished());
    drop(launch);
}

#[test]
#[cfg(feature = "local")]
fn local_task_cancellation_and_unlaunched_drop_destroy_rc_captures_on_owner_thread() {
    // Ambiguity is a compiler error if a local retained carrier becomes Send.
    trait AmbiguousIfSend<T> {
        fn check() {}
    }
    impl<T: ?Sized> AmbiguousIfSend<()> for T {}
    impl<T: ?Sized + Send> AmbiguousIfSend<u8> for T {}
    let _ = <local::Task<Rc<Cell<usize>>, Rc<usize>> as AmbiguousIfSend<_>>::check;
    let _ = <local::TaskContext<Rc<Cell<usize>>, Rc<usize>> as AmbiguousIfSend<_>>::check;
    let _ = <local::LocalFuture as AmbiguousIfSend<_>>::check;
    for poll_before_cancel in [false, true] {
        let drops = Rc::new(Cell::new(0));
        let factories = Rc::new(Cell::new(0));
        let mut host = Host::<_, _, local::Local>::new(
            Rc::new(Cell::new(0)),
            Box::new(local::Driver(CancelLocal {
                capture: Some(OwnerDrop {
                    drops: Rc::clone(&drops),
                    owner: std::thread::current().id(),
                }),
                factories: Rc::clone(&factories),
            })),
        );
        let (turn, receipt) = host.action(());
        let task = futures_executor::block_on(receipt).unwrap().unwrap();
        assert_eq!(factories.get(), 0);
        let mut launch = turn.launches.into_iter().next().unwrap();
        let mut cx = Context::from_waker(Waker::noop());
        if poll_before_cancel {
            assert!(launch.as_mut().poll(&mut cx).is_pending());
        }
        task.cancel();
        drop(host.collect());
        assert!(launch.as_mut().poll(&mut cx).is_ready());
        drop(launch);
        assert_eq!(factories.get(), usize::from(poll_before_cancel));
        assert_eq!(drops.get(), 1);
        drop(host.collect());
        assert!(task.is_finished());
    }
    let drops = Rc::new(Cell::new(0));
    let factories = Rc::new(Cell::new(0));
    let mut host = Host::<_, _, local::Local>::new(
        Rc::new(Cell::new(0)),
        Box::new(local::Driver(CancelLocal {
            capture: Some(OwnerDrop {
                drops: Rc::clone(&drops),
                owner: std::thread::current().id(),
            }),
            factories: Rc::clone(&factories),
        })),
    );
    let (turn, receipt) = host.action(());
    drop(turn);
    let task = futures_executor::block_on(receipt).unwrap().unwrap();
    drop(host.collect());
    assert!(task.is_finished());
    assert_eq!(factories.get(), 0);
    assert_eq!(drops.get(), 1);
}

/// Injects a terminal-delivery panic after the reply becomes readable.
struct PanicWake;
impl Wake for PanicWake {
    fn wake(self: Arc<Self>) {
        panic!("reply waiter wake panic");
    }
}
/// Detects any retry of a lost result's deliberately panicking destructor.
struct PanicDrop(Arc<AtomicUsize>);
impl Drop for PanicDrop {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("owned result destructor panic");
    }
}

#[test]
fn reply_waker_and_lost_result_panics_do_not_retry_terminal_delivery() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut host = owned_action_host();
    let mut send = Box::pin(host.context().send(owned_action(&drops, false)));
    let waker = Waker::from(Arc::new(PanicWake));
    let mut panicking = Context::from_waker(&waker);
    assert!(send.as_mut().poll(&mut panicking).is_pending());
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| host.next())).is_err());
    let mut cx = Context::from_waker(Waker::noop());
    assert!(matches!(send.as_mut().poll(&mut cx), Poll::Ready(Ok(task)) if task.is_finished()));
    assert_eq!((host.version, drops.load(Ordering::SeqCst)), (1, 1));
    let result_drops = Arc::new(AtomicUsize::new(0));
    let result = PanicDrop(Arc::clone(&result_drops));
    let mut update = Box::pin(host.context().update(move |state| {
        *state += 1;
        result
    }));
    assert!(update.as_mut().poll(&mut cx).is_pending());
    drop(update);
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| host.next())).is_err());
    assert_eq!(
        (
            host.state,
            host.version,
            result_drops.load(Ordering::SeqCst)
        ),
        (2, 2, 1)
    );
}

/// Shares an ordered phase log across reduction, publication, receipt and task callbacks.
type Trace = Arc<std::sync::Mutex<Vec<&'static str>>>;
/// Separates registration from both factory entry and polling the task body.
struct TraceFeature(Trace);
impl crate::Feature for TraceFeature {
    type State = usize;
    type Action = ();
    fn update(
        &mut self,
        state: &mut usize,
        (): &(),
        context: &mut crate::FeatureContext<'_, usize, ()>,
    ) {
        self.0.lock().unwrap().push("reduce");
        *state += 1;
        let trace = Arc::clone(&self.0);
        context.task(crate::Task::run(move |_| {
            trace.lock().unwrap().push("factory");
            async move {
                trace.lock().unwrap().push("body");
            }
        }));
    }
}
/// Checks observation publication synchronously inside the receipt's wake callback.
struct ReceiptTrace(Trace);
impl Wake for ReceiptTrace {
    fn wake(self: Arc<Self>) {
        let mut trace = self.0.lock().unwrap();
        assert_eq!(*trace, ["reduce", "observation"]);
        trace.push("receipt-ready");
    }
}

#[cfg(feature = "test-util")]
#[test]
fn entered_send_failure_records_terminal_before_waking_receiver() {
    use crate::store::task_context::TerminalReceipt;

    /// Reads the recorded terminal result before allowing the waiter to resume.
    struct TerminalWake {
        terminal: TerminalReceipt,
        error: TaskAccessError,
        calls: Arc<AtomicUsize>,
    }
    impl Wake for TerminalWake {
        fn wake(self: Arc<Self>) {
            assert_eq!(self.terminal.result(), Some(Err(self.error)));
            self.calls.fetch_add(1, Ordering::SeqCst);
        }
    }

    let terminal = TerminalReceipt::default();
    let error = TaskAccessError::new(TaskAccessReason::StoreClosed, TaskProgress::MayHaveRun);
    let calls = Arc::new(AtomicUsize::new(0));
    let waker = Waker::from(Arc::new(TerminalWake {
        terminal: terminal.clone(),
        error,
        calls: Arc::clone(&calls),
    }));
    let mut cx = Context::from_waker(&waker);
    let (mut request, receive) = TypedSend::channel(Box::new(7));
    request.attach_terminal(terminal.clone());
    let mut receive = std::pin::pin!(receive);
    assert!(receive.as_mut().poll(&mut cx).is_pending());
    let (action, reply) = request.enter();
    assert_eq!(*action, 7);
    assert_eq!(terminal.result(), None);
    reply.fail(error);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let Poll::Ready(Ok(Err(crate::TaskSendError::Failed { error: actual }))) =
        receive.as_mut().poll(&mut cx)
    else {
        panic!("entered failure must publish its original error");
    };
    assert_eq!(actual, error);
    assert_eq!(terminal.result(), Some(Err(error)));
    terminal.record(Ok(()));
    assert_eq!(terminal.result(), Some(Err(error)));
}

#[test]
fn receipt_readiness_precedes_return_and_launch_but_follows_observation() {
    let trace = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut host = Host::<_, _, Shared>::new(0, Box::new(TraceFeature(Arc::clone(&trace))));
    let observe = Arc::clone(&trace);
    let signal = crate::observation::ObservationSignal::new(Arc::downgrade(&host.control));
    let (registration, _ack) = super::super::observation::new_registration(
        move |state: &usize| {
            if *state != 0 {
                observe.lock().unwrap().push("observation");
            }
            *state
        },
        signal,
    );
    registration.execute(
        &host.state,
        &mut host.observations,
        host.version,
        host.scopes.root(),
        None,
        &SelectedChild::root(&host.state),
    );
    let (reply, mut receive) = super::super::command::commit_channel();
    let waker = Waker::from(Arc::new(ReceiptTrace(Arc::clone(&trace))));
    assert!(
        std::pin::Pin::new(&mut receive)
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    let turn = commit(
        host.parts(),
        Mutation::Action(ActionCommand::external((), reply), None),
    );
    trace.lock().unwrap().push("return");
    assert_eq!(host.version, 1);
    #[cfg(feature = "test-util")]
    assert_eq!(host.registry.live_task_ids().len(), 1);
    let task = futures_executor::block_on(receive).unwrap().unwrap();
    assert!(!task.is_finished());
    trace.lock().unwrap().push("launch");
    for launch in turn.launches {
        futures_executor::block_on(launch);
    }
    assert_eq!(
        *trace.lock().unwrap(),
        [
            "reduce",
            "observation",
            "receipt-ready",
            "return",
            "launch",
            "factory",
            "body"
        ]
    );
    drop(host.collect());
    assert!(task.is_finished());
    eprintln!(
        "RC05 layout bytes: TypedSend<Box<usize>>={}, payload tuple={}, EnteredSend<Box<usize>>={}, SharedReply={}",
        std::mem::size_of::<TypedSend<Box<usize>>>(),
        std::mem::size_of::<(
            Box<usize>,
            futures_channel::oneshot::Sender<Result<ActionTask, crate::TaskSendError<Box<usize>>>>
        )>(),
        std::mem::size_of::<EnteredSend<Box<usize>>>(),
        std::mem::size_of::<super::super::task_context::SharedReply>()
    );
}

/// Supplies an inert mounted child so reactions and output ownership stay isolated.
struct TransactionChild;
impl crate::Feature for TransactionChild {
    type State = usize;
    type Action = ();
    fn update(&mut self, _: &mut usize, (): &(), _: &mut crate::FeatureContext<'_, usize, ()>) {}
}

/// Retains independent selection and factory counters around a real mounted reaction.
struct ReactionHost {
    host: Host<Option<usize>, usize, Shared>,
    selections: Arc<AtomicUsize>,
    factories: Arc<AtomicUsize>,
}

/// Installs a reaction that can close lifecycle during its pre-mutation selection.
fn reaction_host(close_during_capture: bool) -> ReactionHost {
    let mut host = Host::<Option<usize>, usize, Shared>::new(
        Some(0),
        Box::new(crate::Reduce::new(
            |state: &mut Option<usize>, action: &usize, _: &mut ReduceContext<'_>| {
                *state.as_mut().unwrap() += action;
                crate::Effect::none()
            },
        )),
    );
    let selections = Arc::new(AtomicUsize::new(0));
    let factories = Arc::new(AtomicUsize::new(0));
    let selected = Arc::clone(&selections);
    let produced = Arc::clone(&factories);
    let lifecycle = Arc::clone(&host.lifecycle);
    let descriptor = crate::OptionalMount::new(
        |state: &Option<usize>| state.as_ref(),
        |state: &mut Option<usize>| state.as_mut(),
        |state: &Option<usize>| {
            state.map(|_| crate::MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        |_| TransactionChild,
    )
    .on_change(
        move |_| {
            selected.fetch_add(1, Ordering::SeqCst);
            if close_during_capture {
                lifecycle.close(super::super::lifecycle::CloseState::Abortive);
            }
        },
        crate::InitialPolicy::Skip,
        move |_, _| {
            produced.fetch_add(1, Ordering::SeqCst);
            crate::Task::run(|_| async {})
        },
    );
    let (owner, _) = super::super::mount::attach(descriptor);
    owner.bind(&host.ingress, &host.lifecycle);
    host.mounts.push(owner);
    drop(commit(host.parts(), Mutation::Structural));
    assert_eq!(host.version, 1);
    assert_eq!(selections.load(Ordering::SeqCst), 0);
    ReactionHost {
        host,
        selections,
        factories,
    }
}

/// Requires rejection to stop the turn and launch no work, checking its receipt when enabled.
fn assert_rejected_turn(turn: Turn, reason: TaskAccessReason, progress: TaskProgress) {
    assert!(!turn.flow);
    #[cfg(feature = "test-util")]
    assert!(matches!(
        turn.receipt,
        Some(Err(error)) if (error.reason(), error.progress()) == (reason, progress)
    ));
    #[cfg(not(feature = "test-util"))]
    let _ = (reason, progress);
    assert!(turn.launches.into_iter().next().is_none());
}

#[test]
fn legacy_root_action_stops_when_reaction_capture_closes_the_store() {
    for close in [false, true] {
        let ReactionHost {
            mut host,
            selections,
            factories,
        } = reaction_host(close);
        let (turn, mut receive) = host.action(3);
        if close {
            assert_rejected_turn(
                turn,
                TaskAccessReason::StoreClosed,
                TaskProgress::MayHaveRun,
            );
            receive
                .try_recv()
                .expect_err("closed capture drops the commit reply");
            assert_eq!((host.state, host.version), (Some(0), 1));
            assert_eq!(selections.load(Ordering::SeqCst), 1);
        } else {
            assert!(turn.flow);
            assert!(turn.launches.into_iter().next().is_none());
            assert!(matches!(receive.try_recv(), Ok(Some(Ok(task))) if task.is_finished()));
            assert_eq!((host.state, host.version), (Some(3), 2));
            assert_eq!(selections.load(Ordering::SeqCst), 2);
        }
        assert_eq!(factories.load(Ordering::SeqCst), 0);
    }
}

#[cfg(feature = "test-util")]
#[test]
fn failed_root_capture_notifies_the_assertion_waiter() {
    let ReactionHost {
        mut host,
        selections,
        factories,
    } = reaction_host(true);
    let (reply, mut receive) = super::super::command::commit_channel();
    let (assertion_reply, assertion) = async_channel::bounded(1);
    let mut command = ActionCommand::external(3, reply);
    command.assertion_reply = Some(assertion_reply);
    let turn = commit(host.parts(), Mutation::Action(command, None));
    assert_rejected_turn(
        turn,
        TaskAccessReason::StoreClosed,
        TaskProgress::MayHaveRun,
    );
    receive
        .try_recv()
        .expect_err("closed capture drops the commit reply");
    assert!(matches!(assertion.try_recv(), Ok(Err(_))));
    assert_eq!((host.state, host.version), (Some(0), 1));
    assert_eq!(selections.load(Ordering::SeqCst), 1);
    assert_eq!(factories.load(Ordering::SeqCst), 0);
}

#[test]
fn typed_root_send_reports_entered_failure_when_reaction_capture_closes_the_store() {
    for close in [false, true] {
        let ReactionHost {
            mut host,
            selections,
            factories,
        } = reaction_host(close);
        let mut send = Box::pin(host.context().send(3));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(send.as_mut().poll(&mut cx).is_pending());
        let turn = host.next();
        if close {
            assert_rejected_turn(
                turn,
                TaskAccessReason::StoreClosed,
                TaskProgress::MayHaveRun,
            );
            let Poll::Ready(Err(crate::TaskSendError::Failed { error })) =
                send.as_mut().poll(&mut cx)
            else {
                panic!("capture entered before closure; the action is not returned as unentered");
            };
            assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
            assert_eq!(error.progress(), TaskProgress::MayHaveRun);
            assert_eq!((host.state, host.version), (Some(0), 1));
            assert_eq!(selections.load(Ordering::SeqCst), 1);
        } else {
            assert!(turn.flow);
            assert!(turn.launches.into_iter().next().is_none());
            assert!(
                matches!(send.as_mut().poll(&mut cx), Poll::Ready(Ok(task)) if task.is_finished())
            );
            assert_eq!((host.state, host.version), (Some(3), 2));
            assert_eq!(selections.load(Ordering::SeqCst), 2);
        }
        assert_eq!(factories.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn root_update_does_not_run_after_reaction_capture_closes_the_store() {
    for close in [false, true] {
        let ReactionHost {
            mut host,
            selections,
            factories,
        } = reaction_host(close);
        let updates = Arc::new(AtomicUsize::new(0));
        let called = Arc::clone(&updates);
        let mut update = Box::pin(host.context().update(move |state| {
            called.fetch_add(1, Ordering::SeqCst);
            *state.as_mut().unwrap() += 3;
            7
        }));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(update.as_mut().poll(&mut cx).is_pending());
        let turn = host.next();
        if close {
            assert_rejected_turn(
                turn,
                TaskAccessReason::StoreClosed,
                TaskProgress::MayHaveRun,
            );
            let Poll::Ready(Err(error)) = update.as_mut().poll(&mut cx) else {
                panic!("closed capture rejects the admitted update");
            };
            assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
            assert_eq!(error.progress(), TaskProgress::MayHaveRun);
            assert_eq!((host.state, host.version), (Some(0), 1));
            assert_eq!(updates.load(Ordering::SeqCst), 0);
            assert_eq!(selections.load(Ordering::SeqCst), 1);
        } else {
            assert!(turn.flow);
            assert!(turn.launches.into_iter().next().is_none());
            let Poll::Ready(Ok(receipt)) = update.as_mut().poll(&mut cx) else {
                panic!("live capture permits the update to commit");
            };
            assert_eq!(*receipt.value(), 7);
            assert_eq!(receipt.revision(), 2);
            assert_eq!((host.state, host.version), (Some(3), 2));
            assert_eq!(updates.load(Ordering::SeqCst), 1);
            assert_eq!(selections.load(Ordering::SeqCst), 2);
        }
        assert_eq!(factories.load(Ordering::SeqCst), 0);
    }
}

#[cfg(feature = "test-util")]
#[test]
fn cancelled_effect_action_rejects_before_reduction_and_notifies_assertion_waiter() {
    let mut host = counter_host();
    let root = host.scopes.root();
    let lineage = LineageState::new(Arc::downgrade(&host.control));
    let prepared = host.registry.prepare(root, Arc::clone(&lineage), &[]);
    let task = prepared.sender_token();
    let generations = prepared.generation_tokens();
    let runtime_task = host.registry.register(prepared, std::future::pending());
    lineage.finish_registration();
    let (reply, mut receive) = super::super::command::commit_channel();
    let mut command = ActionCommand::effect(
        3,
        reply,
        root,
        Arc::clone(&lineage),
        task,
        generations,
        OperationAncestry::default(),
        crate::effect::operation::OperationWork::default(),
    );
    let (assertion_reply, assertion) = async_channel::bounded(1);
    command.assertion_reply = Some(assertion_reply);
    lineage.cancel();
    assert!(
        host.scopes.is_current(root),
        "only causal provenance was cancelled"
    );
    let turn = commit(host.parts(), Mutation::Action(command, None));
    assert_rejected_turn(turn, TaskAccessReason::Cancelled, TaskProgress::NotStarted);
    assert!(matches!(
        receive.try_recv(),
        Ok(Some(Err(CommitError::Cancelled)))
    ));
    assert!(matches!(assertion.try_recv(), Ok(Err(_))));
    assert_eq!((host.state, host.version), (0, 0));
    drop(runtime_task);
    drop(host.collect());
}

#[test]
fn stale_handle_action_is_rejected_before_embedding_or_reduction() {
    use super::super::command::ScopedActionEmbed;
    use super::super::scope::{
        HandleRegistrationCommandCore, HandleSignal, ScopeIdentity, TypedHandleSelector,
    };

    let mut host = counter_host();
    let (reply, registered) = async_channel::bounded(1);
    let selections = Arc::new(AtomicUsize::new(0));
    let selected = Arc::clone(&selections);
    host.handles.register(
        &host.state,
        HandleRegistrationCommandCore {
            identity: Box::new(TypedHandleSelector::new(move |state: &usize| {
                selected.fetch_add(1, Ordering::SeqCst);
                Some((ScopeIdentity::new(*state), state))
            })),
            signal: HandleSignal::new(Arc::downgrade(&host.control)),
            reply,
        },
    );
    let handle = registered.try_recv().unwrap().unwrap();
    let embeddings = Arc::new(AtomicUsize::new(0));
    let called = Arc::clone(&embeddings);
    let (reply, mut receive) = super::super::command::commit_channel();
    let command = ActionCommand::scoped(
        3,
        reply,
        Arc::new(ScopedActionEmbed::new(move |action| {
            called.fetch_add(1, Ordering::SeqCst);
            action
        })),
        Some(handle),
    );
    drop(host.action(1));
    assert_eq!(selections.load(Ordering::SeqCst), 2);
    let turn = commit(host.parts(), Mutation::Action(command, None));
    assert_rejected_turn(turn, TaskAccessReason::ScopeEnded, TaskProgress::NotStarted);
    assert!(matches!(
        receive.try_recv(),
        Ok(Some(Err(CommitError::ScopeEnded)))
    ));
    assert_eq!(selections.load(Ordering::SeqCst), 2);
    assert_eq!(embeddings.load(Ordering::SeqCst), 0);
    assert_eq!((host.state, host.version), (1, 1));
}

#[cfg(feature = "test-util")]
#[test]
fn handle_selector_rejection_preserves_entered_progress_without_reduction() {
    use super::super::command::ScopedActionEmbed;
    use super::super::scope::{
        HandleRegistrationCommandCore, HandleSignal, ScopeIdentity, TypedHandleSelector,
    };
    use crate::store::task_context::TerminalReceipt;

    let mut host = counter_host();
    let present = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let selections = Arc::new(AtomicUsize::new(0));
    let selected = Arc::clone(&selections);
    let visible = Arc::clone(&present);
    let (reply, registered) = async_channel::bounded(1);
    host.handles.register(
        &host.state,
        HandleRegistrationCommandCore {
            identity: Box::new(TypedHandleSelector::new(move |state: &usize| {
                selected.fetch_add(1, Ordering::SeqCst);
                visible
                    .load(Ordering::SeqCst)
                    .then(|| (ScopeIdentity::unit(), state))
            })),
            signal: HandleSignal::new(Arc::downgrade(&host.control)),
            reply,
        },
    );
    let handle = registered.try_recv().unwrap().unwrap();
    assert_eq!(selections.load(Ordering::SeqCst), 1);
    let (mut reply, mut receive) = super::super::command::commit_channel();
    let terminal = TerminalReceipt::default();
    reply.attach_terminal(terminal.clone());
    let command = ActionCommand::scoped(
        3,
        reply,
        Arc::new(ScopedActionEmbed::new(|_: usize| {
            panic!("rejected scope must not embed its action");
        })),
        Some(handle),
    );
    // A selector may read shared/interior state without an intervening mutation.
    present.store(false, Ordering::SeqCst);
    let turn = commit(host.parts(), Mutation::Action(command, None));
    assert_eq!(selections.load(Ordering::SeqCst), 2);
    assert_eq!((host.state, host.version), (0, 0));
    assert!(matches!(
        receive.try_recv(),
        Ok(Some(Err(CommitError::ScopeEnded)))
    ));
    let error = terminal.result().unwrap().unwrap_err();
    assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
    assert_eq!(error.progress(), TaskProgress::MayHaveRun);
    assert_rejected_turn(turn, TaskAccessReason::ScopeEnded, TaskProgress::MayHaveRun);
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "Keep real scoped-effect admission, FIFO retirement and causal cleanup in one transaction schedule"
)]
fn mounted_effect_with_retired_inner_scope_rejects_while_mount_authority_stays_live() {
    use crate::{
        ActionPath, Effect, IfSome, MountIdentity, OptionalMount, OptionalStatePath, Reduce,
    };

    /// Retires an inner scope while keeping its containing mount alive.
    enum InnerAction {
        Scoped(u8),
        Remove,
    }

    let reductions = Arc::new(AtomicUsize::new(0));
    let scoped_reductions = Arc::new(AtomicUsize::new(0));
    let parent_calls = Arc::clone(&reductions);
    let child_calls = Arc::clone(&scoped_reductions);
    let descriptor = OptionalMount::new(
        Option::as_ref,
        Option::as_mut,
        |state: &Option<Option<usize>>| {
            state.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| {
            let parent_calls = Arc::clone(&parent_calls);
            let child_calls = Arc::clone(&child_calls);
            IfSome::new(
                Reduce::new(
                    move |state: &mut Option<usize>,
                          action: &InnerAction,
                          _: &mut ReduceContext<'_>| {
                        parent_calls.fetch_add(1, Ordering::SeqCst);
                        if matches!(action, InnerAction::Remove) {
                            *state = None;
                        }
                        Effect::none()
                    },
                ),
                OptionalStatePath::new(Option::as_ref, Option::as_mut),
                ActionPath::new(
                    |action: &InnerAction| match action {
                        InnerAction::Scoped(value) => Some(value),
                        InnerAction::Remove => None,
                    },
                    InnerAction::Scoped,
                ),
                Reduce::new(
                    move |state: &mut usize, action: &u8, _: &mut ReduceContext<'_>| {
                        child_calls.fetch_add(1, Ordering::SeqCst);
                        *state += usize::from(*action);
                        if *action == 0 {
                            Effect::send(1)
                        } else {
                            Effect::none()
                        }
                    },
                ),
            )
        },
    );
    let (owner, site) = super::super::mount::attach(descriptor);
    let mut host = Host::<_, (), Shared>::new(
        Some(Some(0)),
        Box::new(Reduce::new(
            |_: &mut Option<Option<usize>>, (): &(), _: &mut ReduceContext<'_>| Effect::none(),
        )),
    );
    owner.bind(&host.ingress, &host.lifecycle);
    host.mounts.push(owner);
    drop(commit(host.parts(), Mutation::Structural));

    let mut cx = Context::from_waker(Waker::noop());
    let mut acquisition = Box::pin(site.acquire(host.scopes.root_identity()));
    assert!(acquisition.as_mut().poll(&mut cx).is_pending());
    let Command::Acquire(call) = host.receiver.try_recv().unwrap() else {
        panic!("mounted child acquisition was admitted");
    };
    call.execute(&host.scopes, &host.lifecycle);
    let Poll::Ready(Ok(Some(child))) = acquisition.as_mut().poll(&mut cx) else {
        panic!("containing mount is installed");
    };
    drop(acquisition);

    let mut start = Box::pin(child.send(InnerAction::Scoped(0)));
    assert!(start.as_mut().poll(&mut cx).is_pending());
    let turn = host.next();
    let Poll::Ready(Ok(task)) = start.as_mut().poll(&mut cx) else {
        panic!("scoped effect registration committed");
    };
    let mut launches = turn.launches.into_iter();
    let mut producer = launches.next().unwrap();
    assert!(launches.next().is_none());
    assert!(!task.is_finished());

    // Dequeue the earlier removal before the effect producer gains mailbox
    // capacity. Retirement still commits before its queued successor.
    let mut remove = Box::pin(child.send(InnerAction::Remove));
    assert!(remove.as_mut().poll(&mut cx).is_pending());
    let Command::Mounted {
        call: remove_call,
        provenance: remove_provenance,
        authority: remove_authority,
        operation_work: remove_work,
    } = host.receiver.try_recv().unwrap()
    else {
        panic!("removal owns the earlier actor head");
    };
    assert!(producer.as_mut().poll(&mut cx).is_pending());
    let removed = commit(
        host.parts(),
        Mutation::Mounted(remove_call, remove_provenance, remove_authority),
    );
    drop(remove_work);
    assert!(removed.flow);
    assert!(removed.launches.into_iter().next().is_none());
    let Poll::Ready(Ok(removal_task)) = remove.as_mut().poll(&mut cx) else {
        panic!("inner scope retirement committed");
    };
    assert!(removal_task.is_finished());
    assert_eq!(host.state, Some(None));
    let revision = host.version;

    let Command::Mounted {
        call,
        provenance,
        authority,
        operation_work,
    } = host.receiver.try_recv().unwrap()
    else {
        panic!("the real scoped effect was admitted behind removal");
    };
    assert!(
        host.scopes.accepts(&authority),
        "the containing mount remains live"
    );
    assert!(!host.scopes.is_current(provenance.scope().unwrap()));
    let error = host.parts().rejection(&provenance, &authority).unwrap();
    assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
    assert_eq!(error.progress(), TaskProgress::NotStarted);
    let rejected = commit(host.parts(), Mutation::Mounted(call, provenance, authority));
    drop(operation_work);
    assert_rejected_turn(
        rejected,
        TaskAccessReason::ScopeEnded,
        TaskProgress::NotStarted,
    );
    assert_eq!((host.state, host.version), (Some(None), revision));
    assert_eq!(reductions.load(Ordering::SeqCst), 2);
    assert_eq!(scoped_reductions.load(Ordering::SeqCst), 1);
    assert!(host.receiver.try_recv().is_err());

    assert!(producer.as_mut().poll(&mut cx).is_ready());
    drop(producer);
    assert!(host.collect().into_iter().next().is_none());
    assert!(task.is_finished());
    assert!(host.task_groups.is_empty());
    assert!(host.sequence_waits.is_empty());
    assert!(host.sequences.is_empty());
    #[cfg(feature = "test-util")]
    assert!(host.registry.live_task_ids().is_empty());
}

#[test]
fn admitted_root_send_returns_its_action_when_only_provenance_is_cancelled() {
    let mut host = counter_host();
    let root = host.scopes.root();
    let lineage = LineageState::new(Arc::downgrade(&host.control));
    let prepared = host.registry.prepare(root, Arc::clone(&lineage), &[]);
    let provenance = Provenance::Effect {
        scope: root,
        lineage: Arc::clone(&lineage),
        task: prepared.sender_token(),
        generations: prepared.generation_tokens(),
        operations: OperationAncestry::default(),
    };
    let runtime_task = host.registry.register(prepared, std::future::pending());
    lineage.finish_registration();
    let context = crate::TaskContext::new(
        Weak::clone(&host.weak),
        root,
        provenance,
        host.scopes.authority(root),
    );
    let mut send = Box::pin(context.send(3));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(send.as_mut().poll(&mut cx).is_pending());
    lineage.cancel();
    assert!(
        host.scopes.is_current(root),
        "cancellation does not retire the root"
    );
    let turn = host.next();
    assert_rejected_turn(turn, TaskAccessReason::Cancelled, TaskProgress::NotStarted);
    let Poll::Ready(Err(crate::TaskSendError::Rejected { action, error })) =
        send.as_mut().poll(&mut cx)
    else {
        panic!("cancelled provenance returns the unentered action");
    };
    assert_eq!(action, 3);
    assert_eq!(error.reason(), TaskAccessReason::Cancelled);
    assert_eq!(error.progress(), TaskProgress::NotStarted);
    assert_eq!((host.state, host.version), (0, 0));
    drop(runtime_task);
    drop(host.collect());
}

#[cfg(feature = "diagnostics")]
#[test]
fn closed_structural_entry_does_not_publish_installation_or_call_mount_factories() {
    let mut host = counter_host();
    let constructors = Arc::new(AtomicUsize::new(0));
    let initials = Arc::new(AtomicUsize::new(0));
    let made = Arc::clone(&constructors);
    let started = Arc::clone(&initials);
    let descriptor = crate::OptionalMount::new(
        |state: &usize| Some(state),
        |state: &mut usize| Some(state),
        |_| {
            Some(crate::MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| {
            made.fetch_add(1, Ordering::SeqCst);
            TransactionChild
        },
    )
    .on_initial(
        |_| (),
        move |_, _| {
            started.fetch_add(1, Ordering::SeqCst);
            crate::Task::run(|_| async {})
        },
    );
    let (owner, _) = super::super::mount::attach(descriptor);
    owner.bind(&host.ingress, &host.lifecycle);
    host.mounts.push(owner);
    // Store's last-owner drop closes lifecycle before ingress. This is the
    // entry boundary after the actor's ingress check, without a scheduling race.
    host.lifecycle
        .close(super::super::lifecycle::CloseState::Abortive);
    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = Arc::clone(&events);
    let mut sink = move |event: &crate::diagnostics::LifecycleEvent| {
        recorded.lock().unwrap().push(*event);
    };
    let mut parts = host.parts();
    parts.lifecycle_sink = Some(&mut sink);
    let turn = commit(parts, Mutation::Structural);
    assert!(!events.lock().unwrap().iter().any(|event| matches!(
        event.kind(),
        crate::diagnostics::LifecycleKind::Installed { .. }
    )));
    assert_eq!(constructors.load(Ordering::SeqCst), 0);
    assert_eq!(initials.load(Ordering::SeqCst), 0);
    assert!(turn.launches.into_iter().next().is_none());
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "keeps declaration retirement, live source authority and owned output rejection in one schedule"
)]
fn retired_output_owner_returns_queued_value_from_live_sibling_source() {
    use super::super::mount::{self, children};
    use crate::{Feature, FeatureContext, MountIdentity, OptionalMount, Output, OutputFlow};
    use std::sync::Mutex;

    /// Separates output-source lifetime from the sibling that owns its declaration.
    struct State {
        source: Option<usize>,
        owner: Option<usize>,
    }
    /// Retires only the declaration owner, leaving the output source available.
    struct Root;
    impl Feature for Root {
        type State = State;
        type Action = ();
        fn update(&mut self, state: &mut State, (): &(), _: &mut FeatureContext<'_, State, ()>) {
            state.owner = None;
        }
    }
    let (source_owner, source_site) = mount::attach::<State, (), _, _>(OptionalMount::new(
        |state: &State| state.source.as_ref(),
        |state: &mut State| state.source.as_mut(),
        |state: &State| {
            state.source.map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        |_| TransactionChild,
    ));
    let output_slot = Arc::new(Mutex::new(None::<Output<Box<Cell<usize>>>>));
    let recipe_output = Arc::clone(&output_slot);
    let recipe_source = source_site.clone();
    let handlers = Arc::new(AtomicUsize::new(0));
    let recipe_handlers = Arc::clone(&handlers);
    let (declaration_owner, _) = children::attach::<State, (), _, _>(
        OptionalMount::new(
            |state: &State| state.owner.as_ref(),
            |state: &mut State| state.owner.as_mut(),
            |state: &State| {
                state.owner.map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            |_| TransactionChild,
        ),
        move |_, children| {
            let calls = Arc::clone(&recipe_handlers);
            let output = children.output(&recipe_source, move |handlers| {
                handlers.at_root(move |_, value, _| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    OutputFlow::Continue(value)
                });
            });
            *recipe_output.lock().unwrap() = Some(output);
        },
    );
    let mut host = Host::<_, _, Shared>::new(
        State {
            source: Some(3),
            owner: Some(5),
        },
        Box::new(Root),
    );
    source_owner.bind(&host.ingress, &host.lifecycle);
    declaration_owner.bind(&host.ingress, &host.lifecycle);
    host.mounts.extend([source_owner, declaration_owner]);
    drop(commit(host.parts(), Mutation::Structural));

    let mut cx = Context::from_waker(Waker::noop());
    let mut acquisition = Box::pin(source_site.acquire(host.scopes.root_identity()));
    assert!(acquisition.as_mut().poll(&mut cx).is_pending());
    let Command::Acquire(call) = host.receiver.try_recv().unwrap() else {
        panic!("source acquisition was admitted");
    };
    call.execute(&host.scopes, &host.lifecycle);
    let Poll::Ready(Ok(Some(source))) = acquisition.as_mut().poll(&mut cx) else {
        panic!("sibling source is installed");
    };
    drop(acquisition);

    // Retain the earlier actor head, freeing bounded admission for its successor.
    let (reply, mut retired) = super::super::command::commit_channel();
    host.ingress
        .try_send(Command::Action(ActionCommand::external((), reply)))
        .unwrap_or_else(|_| panic!("empty mailbox admits retirement"));
    let Command::Action(retire) = host.receiver.try_recv().unwrap() else {
        panic!("retirement is the earlier command");
    };
    let output = output_slot.lock().unwrap().take().unwrap();
    let value = Box::new(Cell::new(7));
    let identity = std::ptr::from_ref(value.as_ref());
    let mut delivery = Box::pin(source.output(&output, value));
    assert!(delivery.as_mut().poll(&mut cx).is_pending());

    drop(commit(host.parts(), Mutation::Action(retire, None)));
    assert!(matches!(retired.try_recv(), Ok(Some(Ok(_)))));
    assert_eq!(host.state.source, Some(3));
    assert_eq!(host.state.owner, None);
    let revision = host.version;

    let Command::Output {
        call,
        provenance,
        authority,
        operation_work,
    } = host.receiver.try_recv().unwrap()
    else {
        panic!("output was admitted before retirement committed");
    };
    assert!(
        host.parts().rejection(&provenance, &authority).is_none(),
        "the sibling source remains live; rejection concerns its output declaration"
    );
    let turn = commit(host.parts(), Mutation::Output(call, provenance, authority));
    drop(operation_work);
    assert_rejected_turn(turn, TaskAccessReason::ScopeEnded, TaskProgress::NotStarted);
    assert_eq!(host.version, revision);

    let Poll::Ready(Err(crate::OutputError::Returned { value, error })) =
        delivery.as_mut().poll(&mut cx)
    else {
        panic!("retired output declaration returns its unentered owned value");
    };
    assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
    assert_eq!(error.progress(), TaskProgress::NotStarted);
    assert_eq!(std::ptr::from_ref(value.as_ref()), identity);
    assert_eq!(value.get(), 7);
    assert_eq!(handlers.load(Ordering::SeqCst), 0);
}
