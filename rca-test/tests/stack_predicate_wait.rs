//! Exercises owned predicate waits through public stack composition and scopes.

use std::pin::Pin;
use std::sync::mpsc;
use std::task::{Context, Poll, Waker};

use futures_executor::block_on;
use rca_core::{
    ActionPath, Effect, EffectSender, PredicateWait, PredicateWaitOutcome as Outcome,
    ReduceContext, Reducer, ScopedStore, StackAction, StackId, StackState, StatePath, Store, stack,
};
use rca_test::DeterministicRuntime;

/// Gives each stack entry its own predicate value.
struct Route {
    busy: bool,
}

/// Changes the predicate or starts unfinished work owned by the selected send.
enum ChildAction {
    Set(bool),
    Start(mpsc::Sender<EffectSender<ChildAction>>),
}

/// Separates stack routing, same-ID replacement and independent root work.
enum Action {
    Path(StackAction<bool, ChildAction>),
    Recreate(StackId, bool),
    RootStart(mpsc::Sender<EffectSender<Action>>),
}

/// Replaces an entry without reusing its lifetime and can start root-owned work.
struct Parent;
impl Reducer for Parent {
    type State = StackState<Route>;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        match action {
            Action::Recreate(id, busy) => {
                state.try_pop_from(id).unwrap();
                state
                    .try_push(id.clone(), Route { busy: *busy })
                    .unwrap_or_else(|_| panic!("removed ID can be reused"));
                Effect::none()
            }
            Action::RootStart(captured) => {
                let captured = captured.clone();
                Effect::run(move |sender| async move {
                    captured.send(sender).unwrap();
                    std::future::pending::<()>().await;
                })
            }
            Action::Path(_) => Effect::none(),
        }
    }
}

/// Publishes a busy value before exposing an unfinished effect's output handle.
struct Child;
impl Reducer for Child {
    type State = Route;
    type Action = ChildAction;

    fn reduce(
        &mut self,
        state: &mut Route,
        action: &ChildAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<ChildAction> {
        match action {
            ChildAction::Set(busy) => {
                state.busy = *busy;
                Effect::none()
            }
            ChildAction::Start(captured) => {
                state.busy = true;
                let captured = captured.clone();
                Effect::run(move |sender| async move {
                    captured.send(sender).unwrap();
                    std::future::pending::<()>().await;
                })
            }
        }
    }
}

type StackStore = Store<StackState<Route>, Action>;

fn extract(action: &Action) -> Option<&StackAction<bool, ChildAction>> {
    match action {
        Action::Path(action) => Some(action),
        Action::Recreate(..) | Action::RootStart(_) => None,
    }
}

/// Returns a live prefix and selected tail with equal initial predicate values.
fn store(host: &DeterministicRuntime, busy: bool) -> (StackStore, StackId, StackId) {
    let mut state = StackState::new();
    let prefix = state.push(Route { busy });
    let selected = state.push(Route { busy });
    let reducer = stack(
        Parent,
        StatePath::new(|value: &StackState<Route>| value, |value| value),
        ActionPath::new(extract, Action::Path),
        |busy: &bool, _: &mut ReduceContext<'_>| Route { busy: *busy },
        Child,
    );
    (Store::new(state, reducer, host.runtime()), prefix, selected)
}

/// Runs one caller operation without advancing virtual time.
fn drive<F: Future + Send>(host: &DeterministicRuntime, future: F) -> F::Output
where
    F::Output: Send,
{
    block_on(host.run(future)).unwrap()
}

/// Acquires the selected entry's current lifetime through the public API.
fn scope(
    host: &DeterministicRuntime,
    store: &StackStore,
    id: StackId,
) -> ScopedStore<Route, ChildAction> {
    drive(
        host,
        store.scope_stack(
            StatePath::new(|value: &StackState<Route>| value, |value| value),
            ActionPath::new(extract, Action::Path),
            id,
        ),
    )
    .unwrap()
}

/// Advances only the caller future, leaving actor work queued in the host.
fn poll(wait: &mut PredicateWait) -> Poll<Outcome> {
    Pin::new(wait).poll(&mut Context::from_waker(Waker::noop()))
}

/// Leaves the initial predicate reply unread; sends first consume their commit receipt.
fn buffer_registration(host: &DeterministicRuntime, wait: &mut PredicateWait, sending: bool) {
    assert_eq!(poll(wait), Poll::Pending);
    host.run_until_stalled().unwrap();
    if sending {
        assert_eq!(poll(wait), Poll::Pending);
        host.run_until_stalled().unwrap();
    }
}

// Pre-send false and sibling false must not satisfy the selected post-send predicate.
#[test]
fn selected_post_commit_wait_ignores_sibling_false_and_accepts_selected_false() {
    let host = DeterministicRuntime::new();
    let (store, prefix, selected) = store(&host, false);
    let sibling = scope(&host, &store, prefix);
    let selected = scope(&host, &store, selected);
    let mut wait = selected.send_and_wait_while(ChildAction::Set(true), |route| route.busy);
    buffer_registration(&host, &mut wait, true);
    assert_eq!(poll(&mut wait), Poll::Pending);
    assert!(drive(&host, selected.read(|route| route.busy)).unwrap());
    drop(drive(&host, sibling.send(ChildAction::Set(false))).unwrap());
    assert_eq!(poll(&mut wait), Poll::Pending);
    host.run_until_stalled().unwrap();
    assert_eq!(poll(&mut wait), Poll::Pending);
    drop(drive(&host, selected.send(ChildAction::Set(false))).unwrap());
    assert_eq!(drive(&host, wait), Outcome::Satisfied);
    assert_eq!(
        drive(&host, store.runtime_snapshot())
            .unwrap()
            .observation_count,
        0
    );
    drive(&host, store.shutdown()).unwrap();
}

// Popping a selected tail ends its wait but not the untouched prefix lifetime.
#[test]
fn selected_pop_ends_only_its_wait_and_surviving_wait_stays_live() {
    let host = DeterministicRuntime::new();
    let (store, prefix, selected_id) = store(&host, true);
    let sibling = scope(&host, &store, prefix);
    let selected = scope(&host, &store, selected_id.clone());
    let mut sibling_wait = sibling.wait_while(|route| route.busy);
    let mut selected_wait = selected.wait_while(|route| route.busy);
    buffer_registration(&host, &mut sibling_wait, false);
    buffer_registration(&host, &mut selected_wait, false);
    assert_eq!(poll(&mut sibling_wait), Poll::Pending);
    assert_eq!(poll(&mut selected_wait), Poll::Pending);
    drop(
        drive(
            &host,
            store.send(Action::Path(StackAction::PopFrom { id: selected_id })),
        )
        .unwrap(),
    );
    assert_eq!(drive(&host, selected_wait), Outcome::ScopeEnded);
    assert_eq!(poll(&mut sibling_wait), Poll::Pending);
    assert!(drive(&host, sibling.read(|route| route.busy)).unwrap());
    drop(drive(&host, sibling.send(ChildAction::Set(false))).unwrap());
    assert_eq!(drive(&host, sibling_wait), Outcome::Satisfied);
    assert_eq!(drive(&host, store.read(StackState::len)).unwrap(), 1);
    drive(&host, store.shutdown()).unwrap();
}

// Same logical ID and equal false payload cannot revive old waits or callbacks.
#[test]
fn same_id_reinsertion_ends_buffered_false_wait_and_rejects_old_callback() {
    for sending in [false, true] {
        for taking in [false, true] {
            let host = DeterministicRuntime::new();
            let (store, prefix, selected_id) = store(&host, true);
            let sibling = scope(&host, &store, prefix);
            let old = scope(&host, &store, selected_id.clone());
            let (capture, output) = mpsc::channel();
            drop(drive(&host, old.send(ChildAction::Start(capture))).unwrap());
            host.run_until_stalled().unwrap();
            let callback = output.try_recv().unwrap();
            drop(drive(&host, old.send(ChildAction::Set(taking))).unwrap());
            let mut wait = if sending {
                old.send_and_wait_while(ChildAction::Set(taking), |route| route.busy)
            } else {
                old.wait_while(|route| route.busy)
            };
            buffer_registration(&host, &mut wait, sending);
            if taking {
                assert_eq!(poll(&mut wait), Poll::Pending);
                drop(drive(&host, old.send(ChildAction::Set(false))).unwrap());
                assert_eq!(poll(&mut wait), Poll::Pending);
                host.run_until_stalled().unwrap();
            }
            drop(
                drive(
                    &host,
                    store.send(Action::Recreate(selected_id.clone(), false)),
                )
                .unwrap(),
            );
            assert_eq!(drive(&host, wait), Outcome::ScopeEnded);
            assert!(
                drive(&host, callback.send(ChildAction::Set(true)))
                    .unwrap_err()
                    .is_cancelled()
            );
            assert!(
                drive(&host, old.read(|route| route.busy))
                    .unwrap_err()
                    .is_scope_ended()
            );
            let fresh = scope(&host, &store, selected_id);
            assert_eq!(
                drive(&host, fresh.wait_while(|route| route.busy)),
                Outcome::Satisfied
            );
            assert!(!drive(&host, fresh.read(|route| route.busy)).unwrap());
            assert!(drive(&host, sibling.read(|route| route.busy)).unwrap());
            drop(drive(&host, sibling.send(ChildAction::Set(false))).unwrap());
            assert_eq!(
                drive(&host, sibling.wait_while(|route| route.busy)),
                Outcome::Satisfied
            );
            drive(&host, store.shutdown()).unwrap();
        }
    }
}

// Cancellation retires captured selected work; ordinary Drop must leave it usable.
#[test]
fn selected_cancellation_and_drop_preserve_sibling_and_root_work() {
    for cancel in [false, true] {
        let host = DeterministicRuntime::new();
        let (store, prefix_id, selected_id) = store(&host, false);
        let sibling = scope(&host, &store, prefix_id.clone());
        let selected = scope(&host, &store, selected_id);
        let (root_capture, root_output) = mpsc::channel();
        drop(drive(&host, store.send(Action::RootStart(root_capture))).unwrap());
        let (sibling_capture, sibling_output) = mpsc::channel();
        drop(drive(&host, sibling.send(ChildAction::Start(sibling_capture))).unwrap());
        host.run_until_stalled().unwrap();
        let root_callback = root_output.try_recv().unwrap();
        let sibling_callback = sibling_output.try_recv().unwrap();
        let (selected_capture, selected_output) = mpsc::channel();
        let mut wait =
            selected.send_and_wait_while(ChildAction::Start(selected_capture), |route| route.busy);
        assert_eq!(poll(&mut wait), Poll::Pending);
        host.run_until_stalled().unwrap(); // Commit/effect ran; receipt is unconsumed.
        let selected_callback = selected_output.try_recv().unwrap();
        assert!(drive(&host, selected.read(|route| route.busy)).unwrap());
        if cancel {
            wait.cancel();
            assert_eq!(poll(&mut wait), Poll::Ready(Outcome::Cancelled));
        }
        drop(wait);
        host.run_until_stalled().unwrap();
        let output = drive(&host, selected_callback.send(ChildAction::Set(false)));
        if cancel {
            assert!(output.unwrap_err().is_cancelled());
            assert!(drive(&host, selected.read(|route| route.busy)).unwrap());
        } else {
            output.unwrap();
            assert!(!drive(&host, selected.read(|route| route.busy)).unwrap());
        }
        drive(&host, sibling_callback.send(ChildAction::Set(false))).unwrap();
        assert!(!drive(&host, sibling.read(|route| route.busy)).unwrap());
        drive(
            &host,
            root_callback.send(Action::Path(StackAction::Element {
                id: prefix_id,
                action: ChildAction::Set(true),
            })),
        )
        .unwrap();
        assert!(drive(&host, sibling.read(|route| route.busy)).unwrap());
        let snapshot = drive(&host, store.runtime_snapshot()).unwrap();
        assert_eq!(snapshot.observation_count, 0);
        assert_eq!(snapshot.live_task_ids.len(), if cancel { 2 } else { 3 });
        drive(&host, store.shutdown()).unwrap();
    }
}
