//! Child-domain access, inspection and retirement through the production transaction kernel.

use super::*;
use crate::store::carriers::Shared;
#[cfg(feature = "local")]
use crate::store::command::Provenance;
#[cfg(feature = "local")]
use crate::store::local;
use crate::store::scope::{ScopeIdentity, ScopeSlot, ScopeToken};
use crate::store::transaction::tests::Host;
use crate::{Feature, FeatureContext, Reduce, ReduceContext, ReducerFeature};
use std::cell::Cell;
#[cfg(feature = "local")]
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

/// Observes guard availability without retaining the installed Feature.
pub(in crate::store) fn shared_guard_probe<F: Feature + Send + 'static>(
    node: &SharedNode<F>,
) -> Arc<dyn Fn() -> bool + Send + Sync>
where
    F::Action: Send,
{
    let weak = Arc::downgrade(&node.owned);
    Arc::new(move || {
        let Some(node) = weak.upgrade() else {
            return true;
        };
        if node.node.try_lock().is_none() {
            return false;
        }
        #[cfg(feature = "test-util")]
        {
            let Some(pending) = node.pending.try_lock() else {
                return false;
            };
            let slot = pending.as_ref().map(|(_, slot)| slot.clone());
            drop(pending);
            let unlocked = match slot {
                None => true,
                Some(PendingChildSlot::Typed(slot)) => slot
                    .upgrade()
                    .is_none_or(|slot| slot.0.try_lock().is_some()),
                Some(PendingChildSlot::Effect(slot)) => slot
                    .upgrade()
                    .is_none_or(|slot| slot.0.try_lock().is_some()),
            };
            if !unlocked {
                return false;
            }
        }
        true
    })
}

/// Reads either standard string panic payload without formatting or allocating.
pub(in crate::store) fn panic_message(payload: &(dyn std::any::Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .expect("the fixture must panic with its expected string diagnostic")
}

#[cfg(feature = "local")]
/// Keeps non-Send state in the same actor as an independent child action domain.
struct LocalRoot {
    /// The selected state shared only within the owner thread.
    child: Option<Rc<Cell<usize>>>,
    /// Authority installed by the root reducer before child access.
    token: Option<ScopeToken>,
}
#[cfg(feature = "local")]
/// Carries a non-Send input through root admission.
enum LocalRootAction {
    Install(Rc<()>),
}
#[cfg(feature = "local")]
/// Distinguishes child work declaration from its later delivered action.
enum LocalChildAction {
    Start(Rc<usize>),
    Add(Rc<usize>),
}
#[cfg(feature = "local")]
/// Retains mutable feature state across child reductions.
struct LocalChildFeature {
    /// Counts reductions independently of the selected state.
    cache: Rc<Cell<usize>>,
    /// Records the committed task-update result before its later send.
    result: Rc<Cell<usize>>,
}
#[cfg(feature = "local")]
impl local::Feature for LocalChildFeature {
    type State = Rc<Cell<usize>>;
    type Action = LocalChildAction;
    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        context: &mut local::FeatureContext<'_, Self::State, Self::Action>,
    ) {
        self.cache.set(self.cache.get() + 1);
        match action {
            LocalChildAction::Start(amount) => {
                let amount = Rc::clone(amount);
                context.effect(
                    local::Effect::run(move |sender| async move {
                        sender.send(amount).await.unwrap();
                    })
                    .map(LocalChildAction::Add),
                );
                let result = Rc::clone(&self.result);
                context.task(local::Task::<Rc<Cell<usize>>, LocalChildAction>::run(
                    move |context| async move {
                        let amount = Rc::new(5);
                        let committed = context
                            .update(move |state| {
                                state.set(state.get() + *amount);
                                Rc::new(state.get())
                            })
                            .await
                            .unwrap();
                        result.set(**committed.value());
                        let _ = context
                            .send(LocalChildAction::Add(Rc::new(2)))
                            .await
                            .unwrap();
                    },
                ));
            }
            LocalChildAction::Add(amount) => state.set(state.get() + **amount),
        }
    }
}

#[test]
#[cfg(feature = "local")]
fn local_child_rc_task_and_mapped_run_use_the_existing_kernel() {
    let mut slot = ScopeSlot::new();
    let root = local::ReducerFeature(local::Reduce::new(
        move |state: &mut LocalRoot, action: &LocalRootAction, context: &mut ReduceContext<'_>| {
            let LocalRootAction::Install(capture) = action;
            assert_eq!(Rc::strong_count(capture), 1);
            state.token =
                Some(context.mount(ReduceContext::node(&mut slot), ScopeIdentity::new(())));
            local::Effect::none()
        },
    ));
    let mut host = Host::<_, _, local::Local>::new(
        LocalRoot {
            child: Some(Rc::new(Cell::new(1))),
            token: None,
        },
        Box::new(local::Driver(root)),
    );
    drop(host.action(LocalRootAction::Install(Rc::new(()))));
    let cache = Rc::new(Cell::new(0));
    let result = Rc::new(Cell::new(0));
    let token = host.state.token.unwrap();
    let node = LocalNode::new(
        LocalChildFeature {
            cache: Rc::clone(&cache),
            result: Rc::clone(&result),
        },
        token,
        0,
    );
    let child = node.context::<LocalRoot, LocalRootAction>(
        OptionalPath::<LocalRoot, _>::new(
            |state| state.child.as_ref(),
            |state| state.child.as_mut(),
        ),
        std::sync::Weak::clone(&host.weak),
        Provenance::External { handle: None },
        host.scopes.authority(token),
    );
    host.mounted.push(Box::new(node));
    let mut cx = Context::from_waker(Waker::noop());
    let mut send = Box::pin(child.send(LocalChildAction::Start(Rc::new(3))));
    assert!(send.as_mut().poll(&mut cx).is_pending());
    let turn = host.next();
    assert!(matches!(send.as_mut().poll(&mut cx), Poll::Ready(Ok(_))));
    let mut launches = turn.launches.into_iter();
    let mut run = launches.next().unwrap();
    let mut task = launches.next().unwrap();
    assert!(launches.next().is_none());
    assert!(run.as_mut().poll(&mut cx).is_pending());
    drop(host.next());
    assert!(run.as_mut().poll(&mut cx).is_ready());
    assert!(task.as_mut().poll(&mut cx).is_pending());
    drop(host.next());
    assert!(task.as_mut().poll(&mut cx).is_pending());
    drop(host.next());
    assert!(task.as_mut().poll(&mut cx).is_ready());
    assert_eq!(result.get(), 9);
    assert_eq!(cache.get(), 3);
    assert_eq!(host.state.child.as_ref().unwrap().get(), 11);
    let mut read = Box::pin(child.read(|state| Rc::new(state.get())));
    assert!(read.as_mut().poll(&mut cx).is_pending());
    drop(host.next());
    assert!(matches!(read.as_mut().poll(&mut cx), Poll::Ready(Ok(value)) if *value == 11));
}

/// Supplies Send-but-not-Sync state to Shared child operations.
struct Root {
    /// The selected Cell must not acquire an unnecessary Sync requirement.
    child: Option<Cell<usize>>,
    /// The scope created before the independent child is installed.
    token: Option<ScopeToken>,
}
/// Installs child authority without sharing the child action type.
enum RootAction {
    Install,
}
/// Adds a value within the separate child action domain.
struct ChildAction(usize);
/// Adds a changing feature-local counter so restoration affects the result.
struct ChildFeature(Cell<usize>);
impl Feature for ChildFeature {
    type State = Cell<usize>;
    type Action = ChildAction;
    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &ChildAction,
        _: &mut FeatureContext<'_, Self::State, ChildAction>,
    ) {
        self.0.set(self.0.get() + 1);
        state.set(state.get() + action.0 + self.0.get());
    }
}

#[test]
fn independent_child_send_reads_and_updates_send_not_sync_state() {
    let mut slot = ScopeSlot::new();
    let root = ReducerFeature::new(Reduce::new(
        move |state: &mut Root, _: &RootAction, context: &mut ReduceContext<'_>| {
            state.token =
                Some(context.mount(ReduceContext::node(&mut slot), ScopeIdentity::new(())));
            crate::Effect::none()
        },
    ));
    let mut host = Host::<_, _, Shared>::new(
        Root {
            child: Some(Cell::new(3)),
            token: None,
        },
        Box::new(root),
    );
    drop(host.action(RootAction::Install));
    let token = host.state.token.unwrap();
    let node = SharedNode::new(ChildFeature(Cell::new(0)), token, 0);
    let child = node.downgrade().context::<Root, RootAction>(
        OptionalPath::<Root, _>::new(|state| state.child.as_ref(), |state| state.child.as_mut()),
        std::sync::Weak::clone(&host.weak),
        &host.scopes,
    );
    host.mounted.push(Box::new(node));
    let mut cx = Context::from_waker(Waker::noop());
    let mut send = Box::pin(child.send(ChildAction(4)));
    assert!(send.as_mut().poll(&mut cx).is_pending());
    drop(host.next());
    assert!(matches!(send.as_mut().poll(&mut cx), Poll::Ready(Ok(_))));
    assert_eq!(host.state.child.as_ref().unwrap().get(), 8);
    let mut read = Box::pin(child.read(|state| Box::new(state.get())));
    assert!(read.as_mut().poll(&mut cx).is_pending());
    drop(host.next());
    assert!(matches!(read.as_mut().poll(&mut cx), Poll::Ready(Ok(value)) if *value == 8));
    let mut update = Box::pin(child.update(|state| {
        state.set(12);
        Box::new(19)
    }));
    assert!(update.as_mut().poll(&mut cx).is_pending());
    drop(host.next());
    assert!(
        matches!(update.as_mut().poll(&mut cx), Poll::Ready(Ok(value)) if **value.value() == 19 && value.revision() == 3)
    );
    assert_eq!(host.state.child.as_ref().unwrap().get(), 12);
}

#[test]
#[cfg(feature = "test-util")]
fn ancestor_refresh_rejects_stale_inspection_without_taking_current_input() {
    let mut slot = ScopeSlot::new();
    let root = ReducerFeature::new(Reduce::new(
        move |state: &mut Root, _: &RootAction, context: &mut ReduceContext<'_>| {
            state.token =
                Some(context.mount(ReduceContext::node(&mut slot), ScopeIdentity::new(())));
            crate::Effect::none()
        },
    ));
    let mut host = Host::<_, _, Shared>::new(
        Root {
            child: Some(Cell::new(3)),
            token: None,
        },
        Box::new(root),
    );
    drop(host.action(RootAction::Install));
    let token = host.state.token.unwrap();
    let node = SharedNode::new(ChildFeature(Cell::new(0)), token, 0);
    let path =
        || OptionalPath::<Root, _>::new(|state| state.child.as_ref(), |state| state.child.as_mut());
    let stale =
        node.downgrade()
            .context::<Root, RootAction>(path(), Weak::clone(&host.weak), &host.scopes);
    // Refreshing an ancestor preserves this child's node and own configuration.
    host.scopes.refresh_configuration(host.scopes.root());
    let current =
        node.downgrade()
            .context::<Root, RootAction>(path(), Weak::clone(&host.weak), &host.scopes);
    host.mounted.push(Box::new(node));
    let mut cx = Context::from_waker(Waker::noop());
    let mut send = Box::pin(current.send(ChildAction(4)));
    assert!(send.as_mut().poll(&mut cx).is_pending());
    let command = host.receiver.try_recv().unwrap();
    let Command::Mounted {
        call, authority, ..
    } = &command
    else {
        panic!("child send owns a mounted command");
    };
    call.publish_input();
    let identity = call.input_identity().unwrap();
    let Err(error) = stale.inspect_input(identity, authority) else {
        panic!("stale ancestor authority cannot inspect current input");
    };
    assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
    assert_eq!(error.progress(), TaskProgress::NotStarted);
    assert_eq!(host.state.child.as_ref().unwrap().get(), 3);
    let request = current.inspect_input(identity, authority).unwrap().unwrap();
    assert_eq!(request.action().0, 4);
    drop(request);
    assert!(send.as_mut().poll(&mut cx).is_pending());
    host.ingress.try_send(command).unwrap();
    drop(host.next());
    assert!(matches!(send.as_mut().poll(&mut cx), Poll::Ready(Ok(_))));
    assert_eq!(host.state.child.as_ref().unwrap().get(), 8);
}

/// Records attempted selection after child retirement and replacement.
struct RetiringRoot {
    /// The root removes this state in the same turn as its authority.
    child: Option<Cell<usize>>,
    /// Distinguishes the removed incarnation from a later installation.
    token: Option<ScopeToken>,
    /// Detects stale operations that enter either selector.
    selections: Arc<std::sync::atomic::AtomicUsize>,
}
/// Separates installation from the deliberately interleaved removal command.
enum RetiringAction {
    Install,
    Remove,
}
/// Makes feature destruction and unexpected reduction independently observable.
struct RetiringFeature {
    /// Counts destruction even while an endpoint and queued request remain.
    drops: Arc<std::sync::atomic::AtomicUsize>,
    /// Detects execution of the queued action after retirement.
    reductions: Arc<std::sync::atomic::AtomicUsize>,
}
impl Drop for RetiringFeature {
    fn drop(&mut self) {
        self.drops.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}
impl Feature for RetiringFeature {
    type State = Cell<usize>;
    type Action = ChildAction;
    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &ChildAction,
        _: &mut FeatureContext<'_, Self::State, ChildAction>,
    ) {
        self.reductions
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        state.set(state.get() + action.0);
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one retirement schedule keeps the endpoint and queued-send lifetime assertions together"
)]
fn retirement_destroys_feature_while_endpoint_and_queued_send_remain() {
    use crate::TaskSendError;
    use crate::store::transaction::request;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let drops = Arc::new(AtomicUsize::new(0));
    let reductions = Arc::new(AtomicUsize::new(0));
    let selections = Arc::new(AtomicUsize::new(0));
    let mut slot = ScopeSlot::new();
    let root = ReducerFeature::new(Reduce::new(
        move |state: &mut RetiringRoot,
              action: &RetiringAction,
              context: &mut ReduceContext<'_>| {
            match action {
                RetiringAction::Install => {
                    state.child = Some(Cell::new(0));
                    state.token =
                        Some(context.mount(ReduceContext::node(&mut slot), ScopeIdentity::new(())));
                }
                RetiringAction::Remove => {
                    context.leave(state.token.take().unwrap());
                    state.child = None;
                }
            }
            crate::Effect::none()
        },
    ));
    let mut host = Host::<_, _, Shared>::new(
        RetiringRoot {
            child: None,
            token: None,
            selections: Arc::clone(&selections),
        },
        Box::new(root),
    );
    drop(host.action(RetiringAction::Install));
    let token = host.state.token.unwrap();
    let node = SharedNode::new(
        RetiringFeature {
            drops: Arc::clone(&drops),
            reductions: Arc::clone(&reductions),
        },
        token,
        0,
    );
    let child = node.downgrade().context::<RetiringRoot, RetiringAction>(
        OptionalPath::<RetiringRoot, _>::new(
            |state| {
                state.selections.fetch_add(1, Ordering::SeqCst);
                state.child.as_ref()
            },
            |state| {
                state.selections.fetch_add(1, Ordering::SeqCst);
                state.child.as_mut()
            },
        ),
        std::sync::Weak::clone(&host.weak),
        &host.scopes,
    );
    host.mounted.push(Box::new(node));
    let mut cx = Context::from_waker(Waker::noop());
    let mut remove = Box::pin(host.context().send(RetiringAction::Remove));
    assert!(remove.as_mut().poll(&mut cx).is_pending());
    let Command::Task {
        request: removal,
        provenance,
        authority,
        operation_work,
    } = host.receiver.try_recv().unwrap()
    else {
        panic!("root typed removal")
    };
    // The actor has taken removal; another producer fills the bounded inbox.
    let mut queued = Box::pin(child.send(ChildAction(7)));
    assert!(queued.as_mut().poll(&mut cx).is_pending());
    drop(request(host.parts(), removal, provenance, authority));
    drop(operation_work);
    assert_eq!(
        drops.load(Ordering::SeqCst),
        1,
        "queued weak route must not retain Feature"
    );
    assert!(host.mounted.is_empty());
    assert!(matches!(remove.as_mut().poll(&mut cx), Poll::Ready(Ok(_))));
    let turn = host.next();
    let Poll::Ready(Err(TaskSendError::Rejected {
        action: ChildAction(7),
        error,
    })) = queued.as_mut().poll(&mut cx)
    else {
        panic!("retired child rejects the original queued action")
    };
    assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
    assert_eq!(error.progress(), TaskProgress::NotStarted);
    #[cfg(feature = "test-util")]
    assert!(matches!(turn.receipt.as_ref(), Some(Err(recorded)) if *recorded == error));
    drop(turn);
    drop(host.action(RetiringAction::Install));
    assert_ne!(host.state.token.unwrap(), token);
    let mut read = Box::pin(child.read(|_| panic!("stale read must not enter")));
    assert!(read.as_mut().poll(&mut cx).is_pending());
    drop(host.next());
    assert!(matches!(
        read.as_mut().poll(&mut cx),
        Poll::Ready(Err(error))
            if error.reason() == TaskAccessReason::ScopeEnded
                && error.progress() == TaskProgress::NotStarted
    ));
    let mut update = Box::pin(child.update(|_| panic!("stale update must not enter")));
    assert!(update.as_mut().poll(&mut cx).is_pending());
    drop(host.next());
    assert!(matches!(
        update.as_mut().poll(&mut cx),
        Poll::Ready(Err(error))
            if error.reason() == TaskAccessReason::ScopeEnded
                && error.progress() == TaskProgress::NotStarted
    ));
    assert_eq!(selections.load(Ordering::SeqCst), 0);
    assert_eq!(reductions.load(Ordering::SeqCst), 0);
}
