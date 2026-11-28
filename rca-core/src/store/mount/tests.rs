//! Checks owner-local mounting with non-Send values and disappearing selections.

use super::local::{LocalOptionalMount, attach_local};
use super::*;
use crate::store::local;
use crate::store::transaction::tests::Host;
use crate::store::transaction::{self, Mutation};
use std::cell::Cell;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

/// Retains child state without making its `Rc` storage thread-safe.
struct Root {
    child: Option<Rc<Cell<usize>>>,
}
/// Carries a uniquely owned, non-Send root input.
struct RootAction(Rc<()>);
/// Carries the increment through the child endpoint without a Send bound.
struct ChildAction(Rc<usize>);
/// Counts reducer calls separately from the child's state value.
struct Child(Rc<Cell<usize>>);
impl local::Feature for Child {
    type State = Rc<Cell<usize>>;
    type Action = ChildAction;
    fn update(
        &mut self,
        state: &mut Self::State,
        action: &ChildAction,
        _: &mut local::FeatureContext<'_, Self::State, ChildAction>,
    ) {
        self.0.set(self.0.get() + 1);
        state.set(state.get() + *action.0);
    }
}
/// Polls one explicitly driven step; this helper does not test wake delivery.
fn poll<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}

#[test]
fn local_declaration_acquisition_and_dispatch_keep_rc_values_local() {
    let root = local::Reduce::new(
        |_: &mut Root, action: &RootAction, _: &mut crate::ReduceContext<'_>| {
            assert_eq!(Rc::strong_count(&action.0), 1);
            local::Effect::none()
        },
    );
    let mut host = Host::<_, _, local::Local>::new(
        Root {
            child: Some(Rc::new(Cell::new(3))),
        },
        Box::new(local::Driver(root)),
    );
    let calls = Rc::new(Cell::new(0));
    let factory_calls = Rc::clone(&calls);
    let descriptor = LocalOptionalMount::new(
        |root: &Root| root.child.as_ref(),
        |root: &mut Root| root.child.as_mut(),
        |root: &Root| {
            root.child.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| Child(Rc::clone(&factory_calls)),
    );
    let (owner, site) = attach_local(descriptor);
    let lifecycle = std::sync::Arc::clone(&host.lifecycle);
    lifecycle.pending_installation();
    owner.bind(&host.ingress, &lifecycle);
    host.mounts.push(owner);
    let turn = transaction::commit(host.parts(), Mutation::Structural);
    assert!(turn.installation);
    lifecycle.mark_ready();
    let mut acquisition = Box::pin(site.acquire(host.scopes.root_identity()));
    assert!(poll(acquisition.as_mut()).is_pending());
    let Command::Acquire(call) = host.receiver.try_recv().unwrap() else {
        panic!("typed metadata read");
    };
    call.execute(&host.scopes, &lifecycle);
    let Poll::Ready(Ok(Some(child))) = poll(acquisition.as_mut()) else {
        panic!("local child context");
    };
    let mut send = Box::pin(child.send(ChildAction(Rc::new(4))));
    assert!(poll(send.as_mut()).is_pending());
    drop(host.next());
    assert!(matches!(poll(send.as_mut()), Poll::Ready(Ok(_))));
    let mut read = Box::pin(child.read(|state| Rc::new(state.get())));
    assert!(poll(read.as_mut()).is_pending());
    drop(host.next());
    assert!(matches!(poll(read.as_mut()), Poll::Ready(Ok(value)) if *value == 7));
    assert_eq!(calls.get(), 1);
    drop(host.action(RootAction(Rc::new(()))));
}

#[test]
fn local_initial_factory_and_task_keep_owned_rc_capture_and_use_same_commit() {
    let root = local::Reduce::new(
        |_: &mut Root, _: &RootAction, _: &mut crate::ReduceContext<'_>| local::Effect::none(),
    );
    let mut host = Host::<_, _, local::Local>::new(
        Root {
            child: Some(Rc::new(Cell::new(3))),
        },
        Box::new(local::Driver(root)),
    );
    let trace = Rc::new(std::cell::RefCell::new(Vec::new()));
    let hook_trace = std::rc::Rc::clone(&trace);
    let descriptor = LocalOptionalMount::new(
        |root: &Root| root.child.as_ref(),
        |root: &mut Root| root.child.as_mut(),
        |root: &Root| {
            root.child.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        |_| Child(Rc::new(Cell::new(0))),
    )
    .on_initial(
        |child| Rc::new(child.get()),
        move |change, _| {
            let ReactionChange::Initial { current } = change else {
                panic!("initial hook");
            };
            hook_trace.borrow_mut().push("factory");
            let trace = std::rc::Rc::clone(&hook_trace);
            local::Task::run(move |context| async move {
                trace.borrow_mut().push("body");
                let _ = context.send(ChildAction(current)).await.unwrap();
            })
        },
    );
    let (owner, _site) = attach_local(descriptor);
    let lifecycle = std::sync::Arc::clone(&host.lifecycle);
    lifecycle.pending_installation();
    owner.bind(&host.ingress, &lifecycle);
    host.mounts.push(owner);
    let turn = transaction::commit(host.parts(), Mutation::Structural);
    assert_eq!(*trace.borrow(), ["factory"]);
    lifecycle.mark_ready();
    let mut launches = turn.launches.into_iter();
    let mut launch = launches.next().expect("one initial task");
    assert!(
        launch
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    assert_eq!(*trace.borrow(), ["factory", "body"]);
    drop(host.next());
    assert!(
        launch
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_ready()
    );
    assert_eq!(host.state.child.as_ref().unwrap().get(), 6);
    assert!(launches.next().is_none());
    drop(host.action(RootAction(Rc::new(()))));
    assert_eq!(*trace.borrow(), ["factory", "body"]);
}

/// Keeps selection visibility separate from the retained child value.
type SelectableState = (Rc<Cell<bool>>, Rc<Cell<usize>>);

/// Keeps identity stable while callbacks change whether the path selects state.
fn selectable_mount(
    make: impl FnMut(&Rc<Cell<usize>>) -> Child + 'static,
) -> LocalOptionalMount<SelectableState, Child, ()> {
    LocalOptionalMount::new(
        |state: &SelectableState| state.0.get().then_some(&state.1),
        |state| state.0.get().then_some(&mut state.1),
        |_| {
            Some(MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        make,
    )
}

#[test]
fn local_child_hook_path_disappearance_after_constructor_skips_initial_callbacks() {
    for singleton in [true, false] {
        let visible = Rc::new(Cell::new(true));
        let constructor_visible = Rc::clone(&visible);
        let selected = Rc::new(Cell::new(0));
        let selector_count = Rc::clone(&selected);
        let descriptor = selectable_mount(move |_| {
            constructor_visible.set(false);
            Child(Rc::new(Cell::new(0)))
        });
        let select = move |state: &Rc<Cell<usize>>| {
            selector_count.set(selector_count.get() + 1);
            state.get()
        };
        let factory = |_: ReactionChange<usize>,
                       _: ReactionContext<'_, Rc<Cell<usize>>, ChildAction>|
         -> local::Task<Rc<Cell<usize>>, ChildAction> {
            panic!("hidden child must not enter its initial factory")
        };
        let descriptor = if singleton {
            descriptor.on_initial(select, factory)
        } else {
            descriptor.on_change(select, InitialPolicy::Run, factory)
        };
        let (owner, _) = attach_local(descriptor);
        let root = local::Reduce::<SelectableState, (), _>::new(
            |_: &mut SelectableState, (): &(), _: &mut ReduceContext<'_>| local::Effect::none(),
        );
        let mut host = Host::<SelectableState, (), local::Local>::new(
            (visible, Rc::new(Cell::new(7))),
            Box::new(local::Driver(root)),
        );
        owner.bind(&host.ingress, &host.lifecycle);
        host.mounts.push(owner);

        drop(transaction::commit(host.parts(), Mutation::Structural));

        assert_eq!(selected.get(), 0);
        assert_eq!(host.mounted.len(), 1);
        assert_eq!(host.version, 1);
        host.lifecycle.access().unwrap();
    }
}

#[test]
fn local_child_hook_path_disappearance_after_capture_releases_previous_without_factory() {
    /// Counts disposal of a before-value when the after-value disappears.
    struct Selected {
        value: usize,
        drops: Rc<Cell<usize>>,
    }

    impl PartialEq for Selected {
        fn eq(&self, other: &Self) -> bool {
            self.value == other.value
        }
    }

    impl Drop for Selected {
        fn drop(&mut self) {
            self.drops.set(self.drops.get() + 1);
        }
    }

    let selected = Rc::new(Cell::new(0));
    let selector_count = Rc::clone(&selected);
    let drops = Rc::new(Cell::new(0));
    let selected_drops = Rc::clone(&drops);
    let descriptor = selectable_mount(|_| Child(Rc::new(Cell::new(0)))).on_change(
        move |state| {
            selector_count.set(selector_count.get() + 1);
            Selected {
                value: state.get(),
                drops: Rc::clone(&selected_drops),
            }
        },
        InitialPolicy::Skip,
        |_, _| panic!("hidden child has no current value to compare"),
    );
    let (owner, _) = attach_local(descriptor);
    let root = local::Reduce::<SelectableState, bool, _>::new(
        |state: &mut SelectableState, visible: &bool, _: &mut ReduceContext<'_>| {
            state.0.set(*visible);
            local::Effect::none()
        },
    );
    let mut host = Host::<SelectableState, bool, local::Local>::new(
        (Rc::new(Cell::new(true)), Rc::new(Cell::new(7))),
        Box::new(local::Driver(root)),
    );
    owner.bind(&host.ingress, &host.lifecycle);
    host.mounts.push(owner);
    drop(transaction::commit(host.parts(), Mutation::Structural));
    assert_eq!(selected.get(), 0);

    let (turn, mut reply) = host.action(false);
    drop(turn);

    assert!(matches!(reply.try_recv(), Ok(Some(Ok(_)))));
    assert_eq!(selected.get(), 1);
    assert_eq!(drops.get(), 1);
    assert_eq!(host.version, 2);
    host.lifecycle.access().unwrap();
}

#[test]
fn local_mount_selection_disappearance_before_construction_leaves_no_partial_installation() {
    let hide = Rc::new(Cell::new(true));
    let identity_hide = Rc::clone(&hide);
    let constructions = Rc::new(Cell::new(0));
    let constructor_count = Rc::clone(&constructions);
    let descriptor = LocalOptionalMount::new(
        |state: &SelectableState| state.0.get().then_some(&state.1),
        |state| state.0.get().then_some(&mut state.1),
        move |state: &SelectableState| {
            if identity_hide.get() {
                state.0.set(false);
            }
            Some(MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| {
            constructor_count.set(constructor_count.get() + 1);
            Child(Rc::new(Cell::new(0)))
        },
    );
    let (owner, site) = attach_local(descriptor);
    let root = local::Reduce::<SelectableState, (), _>::new(
        |_: &mut SelectableState, (): &(), _: &mut ReduceContext<'_>| local::Effect::none(),
    );
    let mut host = Host::<SelectableState, (), local::Local>::new(
        (Rc::new(Cell::new(true)), Rc::new(Cell::new(7))),
        Box::new(local::Driver(root)),
    );
    owner.bind(&host.ingress, &host.lifecycle);
    host.mounts.push(owner);

    drop(transaction::commit(host.parts(), Mutation::Structural));

    assert_eq!(constructions.get(), 0);
    assert!(host.mounted.is_empty());
    assert!(host.mounts[0].installed_scope().is_none());
    host.lifecycle.access().unwrap();
    let mut absent = Box::pin(site.acquire(host.scopes.root_identity()));
    assert!(poll(absent.as_mut()).is_pending());
    let Command::Acquire(call) = host.receiver.try_recv().unwrap() else {
        panic!("ordered acquisition of an absent declaration");
    };
    call.execute(&host.scopes, &host.lifecycle);
    assert!(matches!(poll(absent.as_mut()), Poll::Ready(Ok(None))));

    hide.set(false);
    host.state.0.set(true);
    drop(transaction::commit(host.parts(), Mutation::Structural));

    assert_eq!(constructions.get(), 1);
    assert_eq!(host.mounted.len(), 1);
    let token = host.mounts[0].installed_scope().unwrap();
    assert!(host.scopes.is_current(token));
    let mut present = Box::pin(site.acquire(host.scopes.root_identity()));
    assert!(poll(present.as_mut()).is_pending());
    let Command::Acquire(call) = host.receiver.try_recv().unwrap() else {
        panic!("ordered acquisition after deferred construction");
    };
    call.execute(&host.scopes, &host.lifecycle);
    assert!(matches!(poll(present.as_mut()), Poll::Ready(Ok(Some(_)))));
}
