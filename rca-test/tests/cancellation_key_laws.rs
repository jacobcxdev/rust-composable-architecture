//! Checks owned cancellation keys through actual reducer scope consumers.

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use futures_executor::block_on;
use rca_core::{
    ActionPath, ActionTask, CaseStatePath, Clock, Effect, EffectSender, KeyedAction,
    KeyedActionPath, KeyedState, KeyedStatePath, OptionalStatePath, ReduceContext, Reducer,
    ReducerExt, ScopedStore, StackAction, StackId, StackState, StatePath, Store, stack,
};
use rca_test::DeterministicRuntime;

/// Uses owned value equality without `Clone`, `Debug` or `Display` requirements.
///
/// Independent strings compare by content. Mutable metadata is excluded from
/// equality, and the deliberately constant hash remains valid when metadata changes.
struct OwnedKey {
    identity: String,
    _metadata: Arc<AtomicUsize>,
}

impl PartialEq for OwnedKey {
    fn eq(&self, other: &Self) -> bool {
        self.identity == other.identity
    }
}

impl Eq for OwnedKey {}

impl Hash for OwnedKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // Deliberate valid collision: equality must still distinguish values.
        0_u8.hash(state);
    }
}

/// Gives an equal-looking key a different Rust type identity.
#[derive(Eq, Hash, PartialEq)]
struct OtherKey(OwnedKey);

/// Observes destruction of the effect's actual capture, not just cancellation status.
struct DropCount(Arc<AtomicUsize>);

impl Drop for DropCount {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// Starts labelled work with controlled key identity or emits a visible output.
enum ChildAction {
    Start {
        label: u8,
        identity: &'static str,
        metadata: Arc<AtomicUsize>,
        dropped: Arc<AtomicUsize>,
        other_type: bool,
        replace: bool,
    },
    Cancel,
    Output(u8),
}

/// Retains output senders while one-second work remains pending on the host clock.
struct Child {
    clock: Clock,
    captures: mpsc::Sender<(u8, EffectSender<ChildAction>)>,
}

impl Reducer for Child {
    type State = Vec<u8>;
    type Action = ChildAction;

    fn reduce(
        &mut self,
        state: &mut Vec<u8>,
        action: &ChildAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<ChildAction> {
        match action {
            ChildAction::Start {
                label,
                identity,
                metadata,
                dropped,
                other_type,
                replace,
            } => {
                let key = OwnedKey {
                    identity: (*identity).to_owned(),
                    _metadata: Arc::clone(metadata),
                };
                let sleep = self.clock.sleep(Duration::from_secs(1));
                let captures = self.captures.clone();
                let label = *label;
                let dropped = DropCount(Arc::clone(dropped));
                let effect = Effect::run(move |sender| async move {
                    let _dropped = dropped;
                    captures.send((label, sender.clone())).unwrap();
                    sleep.await;
                    sender.send(ChildAction::Output(label)).await.unwrap();
                });
                if *other_type {
                    effect.cancellable(OtherKey(key))
                } else if *replace {
                    effect.cancellable_in_flight(key)
                } else {
                    effect.cancellable(key)
                }
            }
            ChildAction::Cancel => Effect::cancel(OwnedKey {
                identity: "selected".to_owned(),
                _metadata: Arc::new(AtomicUsize::new(99)),
            }),
            ChildAction::Output(value) => {
                state.push(*value);
                Effect::none()
            }
        }
    }
}

/// Supplies an initially absent case child with a separate structural lifetime.
enum Screen {
    Inactive,
    Active(Vec<u8>),
}

/// Keeps root, optional, case, keyed and stack consumers in one host.
struct State {
    root: Vec<u8>,
    optional: Option<Vec<u8>>,
    screen: Screen,
    rows: KeyedState<u8, Vec<u8>>,
    path: StackState<Vec<u8>>,
}

/// Routes each consumer independently and activates the case child explicitly.
enum Action {
    Root(ChildAction),
    Optional(ChildAction),
    Case(ChildAction),
    Row(KeyedAction<u8, ChildAction>),
    Path(StackAction<(), ChildAction>),
    ActivateCase,
}

/// Reduces root work directly while composed reducers own the child families.
struct Parent(Child);

impl Reducer for Parent {
    type State = State;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut State,
        action: &Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        match action {
            Action::Root(action) => self
                .0
                .reduce(&mut state.root, action, context)
                .map(Action::Root),
            Action::ActivateCase => {
                state.screen = Screen::Active(Vec::new());
                Effect::none()
            }
            _ => Effect::none(),
        }
    }
}

// These local expression macros reuse concrete closure paths in reduction and
// scoped access. They introduce no items, hidden work or changed async behaviour.
/// Borrows only the optional child.
macro_rules! optional_state {
    () => {
        OptionalStatePath::new(
            |s: &State| s.optional.as_ref(),
            |s: &mut State| s.optional.as_mut(),
        )
    };
}

/// Extracts and embeds only optional-child actions.
macro_rules! optional_action {
    () => {
        ActionPath::new(
            |a: &Action| match a {
                Action::Optional(a) => Some(a),
                _ => None,
            },
            Action::Optional,
        )
    };
}

/// Borrows the case child only while its case is active.
macro_rules! case_state {
    () => {
        CaseStatePath::new(
            |s: &State| match &s.screen {
                Screen::Active(s) => Some(s),
                Screen::Inactive => None,
            },
            |s: &mut State| match &mut s.screen {
                Screen::Active(s) => Some(s),
                Screen::Inactive => None,
            },
        )
    };
}

/// Extracts and embeds only active-case actions.
macro_rules! case_action {
    () => {
        ActionPath::new(
            |a: &Action| match a {
                Action::Case(a) => Some(a),
                _ => None,
            },
            Action::Case,
        )
    };
}

/// Borrows the collection whose entries retain independent cancellation scopes.
macro_rules! keyed_state {
    () => {
        KeyedStatePath::new(|s: &State| &s.rows, |s: &mut State| &mut s.rows)
    };
}

/// Preserves the row key when routing an owned child action.
macro_rules! keyed_action {
    () => {
        KeyedActionPath::new(
            |a: &Action| match a {
                Action::Row(a) => Some(a),
                _ => None,
            },
            Action::Row,
        )
    };
}

/// Borrows the stack containing two independently owned entry lifetimes.
macro_rules! stack_state {
    () => {
        StatePath::new(|s: &State| &s.path, |s: &mut State| &mut s.path)
    };
}

/// Preserves stack-operation and entry routing through the parent action.
macro_rules! stack_action {
    () => {
        ActionPath::new(
            |a: &Action| match a {
                Action::Path(a) => Some(a),
                _ => None,
            },
            Action::Path,
        )
    };
}

/// Selects the consumer boundary for an otherwise identical cancellation-key trace.
#[derive(Clone, Copy)]
enum Family {
    Root,
    Optional,
    Case,
    Keyed,
    Stack,
}

impl Family {
    /// Chooses another lifetime that must survive the selected consumer's cancellation.
    fn sibling(self) -> Self {
        match self {
            Self::Root | Self::Keyed => Self::Keyed,
            Self::Optional => Self::Case,
            Self::Case => Self::Optional,
            Self::Stack => Self::Stack,
        }
    }
}

/// Captures one child scope, or uses `None` to select the root consumer.
async fn scope(
    store: &Store<State, Action>,
    family: Family,
    row: u8,
    entry: StackId,
) -> Option<ScopedStore<Vec<u8>, ChildAction>> {
    Some(match family {
        Family::Root => return None,
        Family::Optional => store
            .scope_some(optional_state!(), optional_action!())
            .await
            .unwrap(),
        Family::Case => store
            .scope_case(case_state!(), case_action!())
            .await
            .unwrap(),
        Family::Keyed => store
            .scope_keyed(keyed_state!(), keyed_action!(), row)
            .await
            .unwrap(),
        Family::Stack => store
            .scope_stack(stack_state!(), stack_action!(), entry)
            .await
            .unwrap(),
    })
}

/// Sends through the selected public handle without changing output authority.
async fn send(
    store: &Store<State, Action>,
    child: Option<&ScopedStore<Vec<u8>, ChildAction>>,
    action: ChildAction,
) -> ActionTask {
    match child {
        Some(child) => child.send(action).await.unwrap(),
        None => store.send(Action::Root(action)).await.unwrap(),
    }
}

/// Reads owned output values without asserting unrelated equal-deadline ordering.
async fn values(
    store: &Store<State, Action>,
    child: Option<&ScopedStore<Vec<u8>, ChildAction>>,
) -> Vec<u8> {
    let mut values = match child {
        Some(child) => child.read(Clone::clone).await.unwrap(),
        None => store.read(|s| s.root.clone()).await.unwrap(),
    };
    // Equal-deadline output ordering is not part of the cancellation-key law.
    values.sort_unstable();
    values
}

/// Builds every consumer family and returns two distinct stack entry identities.
fn store(
    host: &DeterministicRuntime,
    captures: &mpsc::Sender<(u8, EffectSender<ChildAction>)>,
) -> (Store<State, Action>, StackId, StackId) {
    let child = || Child {
        clock: host.runtime().clock(),
        captures: captures.clone(),
    };
    let reducer = stack(
        Parent(child())
            .if_some(optional_state!(), optional_action!(), child())
            .if_case(case_state!(), case_action!(), child())
            .for_each(keyed_state!(), keyed_action!(), child()),
        stack_state!(),
        stack_action!(),
        |(): &(), _: &mut ReduceContext<'_>| Vec::new(),
        child(),
    );
    let mut path = StackState::new();
    let first = path.push(Vec::new());
    let second = path.push(Vec::new());
    let store = Store::new(
        State {
            root: Vec::new(),
            optional: Some(Vec::new()),
            screen: Screen::Inactive,
            rows: [(1, Vec::new()), (2, Vec::new())].into_iter().collect(),
            path,
        },
        reducer,
        host.runtime(),
    );
    (store, first, second)
}

/// Checks value, hash, type and scope distinctions with real retained senders.
///
/// Literal output sets and destruction counts expose pointer-based equality,
/// hash-only equality, scope confusion and retention of obsolete work.
fn check(family: Family, replace: bool) {
    let host = DeterministicRuntime::new();
    let (captures, received) = mpsc::channel();
    let (store, first, second) = store(&host, &captures);
    let metadata: [_; 7] = std::array::from_fn(|_| Arc::new(AtomicUsize::new(0)));
    let drops: [_; 7] = std::array::from_fn(|_| Arc::new(AtomicUsize::new(0)));
    let driver = host.clone();
    block_on(driver.run(async move {
        let start = |label: u8, identity, other_type, replace| ChildAction::Start {
            label,
            identity,
            metadata: Arc::clone(&metadata[usize::from(label)]),
            dropped: Arc::clone(&drops[usize::from(label)]),
            other_type,
            replace,
        };
        store
            .send(Action::ActivateCase)
            .await
            .unwrap()
            .finish()
            .await;
        let selected = scope(&store, family, 1, first).await;
        let sibling = scope(&store, family.sibling(), 2, second).await;
        let old_first = send(
            &store,
            selected.as_ref(),
            start(1, "selected", false, false),
        )
        .await;
        let old_second = send(
            &store,
            selected.as_ref(),
            start(2, "selected", false, false),
        )
        .await;
        let collision = send(&store, selected.as_ref(), start(3, "other", false, false)).await;
        let typed = send(&store, selected.as_ref(), start(4, "selected", true, false)).await;
        let survivor = send(&store, sibling.as_ref(), start(5, "selected", false, false)).await;
        host.run_until_stalled().unwrap();
        let senders: BTreeMap<_, _> = received.try_iter().collect();
        assert_eq!(senders.keys().copied().collect::<Vec<_>>(), [1, 2, 3, 4, 5]);
        for counter in &metadata {
            counter.store(42, Ordering::SeqCst);
        }

        let replacement = if replace {
            Some(send(&store, selected.as_ref(), start(6, "selected", false, true)).await)
        } else {
            send(&store, selected.as_ref(), ChildAction::Cancel)
                .await
                .finish()
                .await;
            None
        };
        host.run_until_stalled().unwrap();
        for label in [1_u8, 2] {
            assert!(
                senders[&label]
                    .send(ChildAction::Output(99))
                    .await
                    .unwrap_err()
                    .is_cancelled()
            );
            assert_eq!(drops[usize::from(label)].load(Ordering::SeqCst), 1);
        }
        assert!(!collision.is_finished());
        assert!(!typed.is_finished());
        assert!(!survivor.is_finished());
        assert_eq!(values(&store, selected.as_ref()).await, []);
        assert_eq!(values(&store, sibling.as_ref()).await, []);
        for label in [3, 4, 5] {
            assert_eq!(drops[label].load(Ordering::SeqCst), 0);
        }
        // A surviving causal sender and its actual scoped handle both retain
        // authority; cancelling a key must not retire the whole reducer scope.
        senders[&5].send(ChildAction::Output(70)).await.unwrap();
        send(&store, sibling.as_ref(), ChildAction::Output(71))
            .await
            .finish()
            .await;
        host.advance(Duration::from_secs(1)).unwrap();
        assert_eq!(values(&store, sibling.as_ref()).await, [5, 70, 71]);
        assert_eq!(
            values(&store, selected.as_ref()).await,
            if replace { vec![3, 4, 6] } else { vec![3, 4] },
        );
        for task in [old_first, old_second, collision, typed, survivor] {
            assert!(task.is_finished());
            task.finish().await;
        }
        if let Some(replacement) = replacement {
            assert!(replacement.is_finished());
            replacement.finish().await;
            assert_eq!(drops[6].load(Ordering::SeqCst), 1);
        }
        for label in [1, 2, 3, 4, 5] {
            assert_eq!(drops[label].load(Ordering::SeqCst), 1);
        }
        assert_eq!(host.pending_sleeps(), [] as [rca_test::PendingSleep; 0]);
        store.shutdown().await.unwrap();
    }))
    .unwrap();
}

#[test]
fn root_owned_keys_cancel_only_equal_typed_work() {
    for replace in [false, true] {
        check(Family::Root, replace);
    }
}

#[test]
fn optional_owned_keys_preserve_other_child_authority() {
    for replace in [false, true] {
        check(Family::Optional, replace);
    }
}

#[test]
fn case_owned_keys_preserve_other_child_authority() {
    for replace in [false, true] {
        check(Family::Case, replace);
    }
}

#[test]
fn keyed_owned_keys_preserve_sibling_entry_authority() {
    for replace in [false, true] {
        check(Family::Keyed, replace);
    }
}

#[test]
fn stack_owned_keys_preserve_sibling_entry_authority() {
    for replace in [false, true] {
        check(Family::Stack, replace);
    }
}
