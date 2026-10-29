//! Concrete owned-key paths through the existing transaction and task leaves.

use super::path::{BorrowPath, ComposedPath, KeyedPath};
use super::*;
use crate::store::scope::{ScopeIdentity, ScopeRegistry, ScopeSlot};
use crate::store::transaction::tests::Host;
use crate::{KeyedState, Reduce, ReducerFeature};
use std::cell::Cell;
use std::pin::Pin;
#[cfg(feature = "local")]
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

/// Supplies the optional projection after the owned-key lookup.
struct Parent<S> {
    /// The independently reduced child state.
    child: Option<S>,
}
/// Owns the keyed parent collection selected by the composed route.
struct Root<S> {
    /// Includes one selected row or no rows for absence checks.
    rows: KeyedState<String, Parent<S>>,
}
/// Exercises a root turn without changing the child action domain.
enum RootAction {
    Unrelated,
}
/// Carries the increment independently of root actions.
struct ChildAction(usize);
/// Uses a Cell to reject unnecessary Sync bounds on Shared Features.
struct Child(Cell<usize>);
impl Feature for Child {
    type State = Cell<usize>;
    type Action = ChildAction;
    fn reduce(
        &mut self,
        state: &mut Cell<usize>,
        action: &ChildAction,
        _: &mut FeatureContext<'_, Cell<usize>, ChildAction>,
    ) {
        self.0.set(self.0.get() + 1);
        state.set(state.get() + action.0);
    }
}
#[cfg(feature = "local")]
/// Records Local reductions through genuinely non-Send ownership.
struct LocalChild(Rc<Cell<usize>>);
#[cfg(feature = "local")]
/// Prevents the Local action from accidentally satisfying Send.
struct LocalAction(Rc<usize>);
#[cfg(feature = "local")]
impl local::Feature for LocalChild {
    type State = Rc<Cell<usize>>;
    type Action = LocalAction;
    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &LocalAction,
        _: &mut local::FeatureContext<'_, Self::State, LocalAction>,
    ) {
        self.0.set(self.0.get() + 1);
        state.set(state.get() + *action.0);
    }
}
/// Polls once after the caller explicitly admits or processes a transaction.
fn poll<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}
/// Creates the selected row outside the transaction under test.
fn state<S>(child: S) -> Root<S> {
    Root {
        rows: [(String::from("selected"), Parent { child: Some(child) })]
            .into_iter()
            .collect(),
    }
}

#[test]
fn shared_owned_key_optional_path_reads_updates_and_sends_without_sync_feature() {
    let driver = ReducerFeature::new(Reduce::new(
        |_: &mut Root<Cell<usize>>, _: &RootAction, _: &mut ReduceContext<'_>| {
            crate::Effect::none()
        },
    ));
    let mut host = Host::<_, _, Shared>::new(state(Cell::new(1)), Box::new(driver));
    let parent = host.scopes.mount(
        host.scopes.root(),
        ScopeRegistry::node(&mut ScopeSlot::new()),
        ScopeIdentity::new("parent"),
    );
    let token = host.scopes.mount(
        parent,
        ScopeRegistry::node(&mut ScopeSlot::new()),
        ScopeIdentity::new("child"),
    );
    let key = Arc::new(String::from("selected"));
    let path = ComposedPath::new(
        KeyedPath::new(
            |root: &Root<Cell<usize>>| &root.rows,
            |root: &mut Root<Cell<usize>>| &mut root.rows,
            Arc::clone(&key),
        ),
        OptionalPath::new(
            |parent: &Parent<Cell<usize>>| parent.child.as_ref(),
            |parent: &mut Parent<Cell<usize>>| parent.child.as_mut(),
        ),
    );
    assert_eq!(path.read(&host.state).unwrap().get(), 1);
    let mut absent = Root {
        rows: KeyedState::new(),
    };
    assert!(path.read(&absent).is_none());
    assert!(path.read_structural(&absent, &|| true).is_none());
    assert!(path.write(&mut absent).is_none());
    let node = SharedNode::new(Child(Cell::new(0)), token, 0);
    let context = node.downgrade().context::<Root<Cell<usize>>, RootAction>(
        path,
        std::sync::Weak::clone(&host.weak),
        &host.scopes,
    );
    host.mounted.push(Box::new(node));
    let mut send = Box::pin(context.send(ChildAction(3)));
    assert!(poll(send.as_mut()).is_pending());
    drop(host.next());
    assert!(matches!(poll(send.as_mut()), Poll::Ready(Ok(_))));
    let mut update = Box::pin(context.update(|child| {
        child.set(child.get() + 5);
        child.get()
    }));
    assert!(poll(update.as_mut()).is_pending());
    drop(host.next());
    assert!(matches!(poll(update.as_mut()), Poll::Ready(Ok(value)) if *value.value() == 9));
    let mut read = Box::pin(context.read(Cell::get));
    assert!(poll(read.as_mut()).is_pending());
    drop(host.next());
    assert!(matches!(poll(read.as_mut()), Poll::Ready(Ok(9))));
    drop(host.action(RootAction::Unrelated));
    assert_eq!(
        host.state
            .rows
            .get(&*key)
            .unwrap()
            .child
            .as_ref()
            .unwrap()
            .get(),
        9
    );
}

#[test]
#[cfg(feature = "local")]
fn local_owned_key_optional_path_retains_rc_on_the_local_carrier() {
    let driver = local::ReducerFeature(local::Reduce::new(
        |_: &mut Root<Rc<Cell<usize>>>, _: &Rc<()>, _: &mut ReduceContext<'_>| {
            local::Effect::none()
        },
    ));
    let mut host = Host::<_, _, local::Local>::new(
        state(Rc::new(Cell::new(2))),
        Box::new(local::Driver(driver)),
    );
    let parent = host.scopes.mount(
        host.scopes.root(),
        ScopeRegistry::node(&mut ScopeSlot::new()),
        ScopeIdentity::new("parent"),
    );
    let token = host.scopes.mount(
        parent,
        ScopeRegistry::node(&mut ScopeSlot::new()),
        ScopeIdentity::new("child"),
    );
    let path = ComposedPath::new(
        KeyedPath::new(
            |root: &Root<Rc<Cell<usize>>>| &root.rows,
            |root: &mut Root<Rc<Cell<usize>>>| &mut root.rows,
            Rc::new(String::from("selected")),
        ),
        OptionalPath::new(
            |parent: &Parent<Rc<Cell<usize>>>| parent.child.as_ref(),
            |parent: &mut Parent<Rc<Cell<usize>>>| parent.child.as_mut(),
        ),
    );
    let calls = Rc::new(Cell::new(0));
    let node = LocalNode::new(LocalChild(Rc::clone(&calls)), token, 0);
    let context = node.context::<Root<Rc<Cell<usize>>>, Rc<()>>(
        path,
        std::sync::Weak::clone(&host.weak),
        Provenance::External { handle: None },
        host.scopes.authority(token),
    );
    host.mounted.push(Box::new(node));
    let mut send = Box::pin(context.send(LocalAction(Rc::new(4))));
    assert!(poll(send.as_mut()).is_pending());
    drop(host.next());
    assert!(matches!(poll(send.as_mut()), Poll::Ready(Ok(_))));
    let mut read = Box::pin(context.read(|child| Rc::new(child.get())));
    assert!(poll(read.as_mut()).is_pending());
    drop(host.next());
    assert!(matches!(poll(read.as_mut()), Poll::Ready(Ok(value)) if *value == 6));
    assert_eq!(calls.get(), 1);
}
