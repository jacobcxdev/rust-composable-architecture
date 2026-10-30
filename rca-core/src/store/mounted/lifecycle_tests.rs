//! Direct lock witnesses through the borrowed production transaction kernel.
//! Public actor installation/retirement is covered by `lifecycle_diagnostics`;
//! this test exposes the actual node guard, not a replacement runtime adapter.
use super::tests::panic_message;
use super::*;
use crate::diagnostics::{LifecycleEvent, LifecycleKind};
use crate::store::scope::{ScopeIdentity, ScopeSlot};
use crate::store::transaction::{Mutation, commit, tests::Host};
use crate::{Reduce, TaskSendError};
#[cfg(feature = "local")]
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

/// Retains child selection and authority while a lifecycle callback runs.
struct State {
    /// Supplies the child projection without unrelated application work.
    child: (),
    /// The parent authority expected in the observed installation event.
    token: Option<ScopeToken>,
}
/// Mounts a nested scope so the real commit emits a lifecycle event.
struct Child {
    /// Retains the nested scope identity across reductions.
    slot: ScopeSlot,
}
impl Feature for Child {
    type State = ();
    type Action = ();
    fn update(&mut self, (): &mut (), (): &(), context: &mut FeatureContext<'_, (), ()>) {
        let _ = context
            .reducer_context()
            .mount(ReduceContext::node(&mut self.slot), ScopeIdentity::new(()));
    }
}
#[cfg(feature = "local")]
impl local::Feature for Child {
    type State = ();
    type Action = ();
    fn update(&mut self, (): &mut (), (): &(), context: &mut local::FeatureContext<'_, (), ()>) {
        let _ = context
            .reducer_context()
            .mount(ReduceContext::node(&mut self.slot), ScopeIdentity::new(()));
    }
}

/// Adapts setup to the Shared production transaction driver.
fn shared_driver(
    mut reduce: impl FnMut(&mut State, &(), &mut ReduceContext<'_>) + Send + 'static,
) -> Box<<Shared as Carriers<State, ()>>::Driver> {
    Box::new(Reduce::new(
        move |state: &mut State, action: &(), context: &mut ReduceContext<'_>| {
            reduce(state, action, context);
            crate::Effect::none()
        },
    ))
}
#[cfg(feature = "local")]
/// Adapts setup without adding Send requirements to Local.
fn local_driver(
    mut reduce: impl FnMut(&mut State, &(), &mut ReduceContext<'_>) + 'static,
) -> Box<<local::Local as Carriers<State, ()>>::Driver> {
    Box::new(local::Driver(local::Reduce::new(
        move |state: &mut State, action: &(), context: &mut ReduceContext<'_>| {
            reduce(state, action, context);
            local::Effect::none()
        },
    )))
}

/// Applies the same lifecycle and unwind assertions to both carrier families.
macro_rules! witness {
    ($name:ident, $carrier:ty, $driver:ident, $node:ident, $weak:expr, $available:expr, $context:expr) => {
        #[test]
        fn $name() {
            for should_panic in [false, true] {
                let mut slot = ScopeSlot::new();
                let mut host = Host::<State, (), $carrier>::new(
                    State {
                        child: (),
                        token: None,
                    },
                    $driver(move |state, _, context| {
                        state.token = Some(context.mount(
                            ReduceContext::node(&mut slot),
                            ScopeIdentity::new(()),
                        ));
                    }),
                );
                drop(host.action(()));
                let token = host.state.token.unwrap();
                let node = $node::new(
                    Child {
                        slot: ScopeSlot::new(),
                    },
                    token,
                    0,
                );
                let weak = ($weak)(&node.owned);
                let context = ($context)(&node, &host);
                host.mounted.push(Box::new(node));

                let events = Arc::new(Mutex::new(Vec::new()));
                let recorded = Arc::clone(&events);
                let mut sink: Box<<$carrier as Carriers<State, ()>>::LifecycleSink> =
                    Box::new(move |event: &LifecycleEvent| {
                        assert!(
                            ($available)(&weak),
                            "legacy lifecycle callback ran under the installed node guard"
                        );
                        recorded.lock().push(*event);
                        assert!(!should_panic, "unlocked lifecycle callback panic");
                    });
                let mut request = Box::pin(context.send(()));
                assert!(
                    request
                        .as_mut()
                        .poll(&mut Context::from_waker(Waker::noop()))
                        .is_pending()
                );
                let Command::Mounted {
                    call,
                    provenance,
                    authority,
                    operation_work,
                } = host.receiver.try_recv().unwrap() else {
                    panic!("actual child endpoint must admit the mounted command");
                };

                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let mut parts = host.parts();
                    parts.lifecycle_sink = Some(&mut *sink);
                    let turn = commit(parts, Mutation::Mounted(call, provenance, authority));
                    drop(operation_work);
                    turn
                }));
                if should_panic {
                    let error = result.err().expect("the lifecycle callback must panic");
                    assert_eq!(panic_message(&*error), "unlocked lifecycle callback panic");
                    assert!(matches!(
                        request.as_mut().poll(&mut Context::from_waker(Waker::noop())),
                        Poll::Ready(Err(TaskSendError::Failed { error }))
                            if error.progress() == TaskProgress::MayHaveRun
                    ));
                    assert_eq!(host.version, 1);
                } else {
                    drop(result.unwrap());
                    assert!(matches!(
                        request.as_mut().poll(&mut Context::from_waker(Waker::noop())),
                        Poll::Ready(Ok(_))
                    ));
                    assert_eq!(host.version, 2);
                }

                let events = events.lock();
                assert_eq!(events.len(), 1);
                assert_eq!(events[0].last_committed_version(), 1);
                assert!(matches!(
                    events[0].kind(),
                    LifecycleKind::Installed {
                        parent: Some(parent),
                        configuration: None,
                        ..
                    } if parent == token.diagnostic_id()
                ));
            }
        }
    };
}
witness!(
    shared_legacy_lifecycle_callback_and_panic_are_unlocked,
    Shared,
    shared_driver,
    SharedNode,
    Arc::downgrade,
    |weak: &Weak<SharedInstalled<Child>>| weak
        .upgrade()
        .is_some_and(|node| node.node.try_lock().is_some()),
    |node: &SharedNode<Child>, host: &Host<State, (), Shared>| node
        .downgrade()
        .context::<State, ()>(
            OptionalPath::new(
                |root: &State| Some(&root.child),
                |root: &mut State| Some(&mut root.child)
            ),
            std::sync::Weak::clone(&host.weak),
            &host.scopes
        )
);
#[cfg(feature = "local")]
witness!(
    local_legacy_lifecycle_callback_and_panic_are_unlocked,
    local::Local,
    local_driver,
    LocalNode,
    Rc::downgrade,
    |weak: &std::rc::Weak<LocalInstalled<Child>>| weak
        .upgrade()
        .is_some_and(|node| node.node.try_borrow_mut().is_ok()),
    |node: &LocalNode<Child>, host: &Host<State, (), local::Local>| node.context::<State, ()>(
        OptionalPath::new(
            |root: &State| Some(&root.child),
            |root: &mut State| Some(&mut root.child)
        ),
        std::sync::Weak::clone(&host.weak),
        Provenance::External { handle: None },
        host.scopes.authority(host.state.token.unwrap())
    )
);
