//! Qualifies keyed disjoint borrowing and independent child lifetimes.

use std::cell::Cell;
use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::task::{Context, Waker};

use futures_executor::block_on;
use rca_core::{
    ActionPath, Effect, EffectSender, KeyedAction, KeyedActionPath, KeyedState, Reduce,
    ReduceContext, Reducer, SharedReducer, StatePath, Store, combine, for_each_shared, scope,
};
use rca_test::{ControlledFuture, DeterministicRuntime, TestStore, controlled_future};

/// Holds move-only, non-`Debug`, non-`Sync` state for one keyed child.
struct Private(Cell<u64>);

/// Supplies one parent-owned value to each child's synchronous reduction.
struct Settings(Cell<u64>);

/// Records what the parent observes after child reduction.
struct State {
    rows: KeyedState<u8, Private>,
    settings: Settings,
    observed: Option<(u64, u64)>,
}

/// Starts two equal-valued children with distinct keyed lifetimes.
fn state() -> State {
    State {
        rows: [(1, Private(Cell::new(0))), (2, Private(Cell::new(0)))]
            .into_iter()
            .collect(),
        settings: Settings(Cell::new(0)),
        observed: None,
    }
}

/// Captures row order, shared settings and the parent's observed values.
type Snapshot = (Vec<(u8, u64)>, u64, Option<(u64, u64)>);

/// Copies comparable values without cloning either reducer input.
fn snapshot(state: &State) -> Snapshot {
    (
        state
            .rows
            .iter()
            .map(|(id, row)| (*id, row.0.get()))
            .collect(),
        state.settings.0.get(),
        state.observed,
    )
}

/// Exercises mutation, controlled output, cancellation and same-action removal.
enum ChildAction {
    Mutate(u64),
    Start,
    Capture,
    Cancel,
    Delete,
}

/// Separates keyed child actions from replacement and shared-only changes.
enum Action {
    Child(KeyedAction<u8, ChildAction>),
    Replace(u8),
    Settings(u64),
    Clear,
}

fn action(id: u8, action: ChildAction) -> Action {
    Action::Child(KeyedAction::new(id, action))
}

/// Supplies only the controlled resources needed by each effect scenario.
#[derive(Default)]
struct Child {
    responses: VecDeque<ControlledFuture<u64>>,
    captured: Option<mpsc::Sender<EffectSender<ChildAction>>>,
    launched: Option<Arc<AtomicUsize>>,
}
impl SharedReducer for Child {
    type State = Private;
    type Shared = Settings;
    type Action = ChildAction;
    fn reduce(
        &mut self,
        state: &mut Private,
        shared: &mut Settings,
        action: &ChildAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<ChildAction> {
        match action {
            ChildAction::Mutate(digit) => {
                state.0.set(shared.0.get() + digit);
                shared.0.set(shared.0.get() * 10 + digit);
            }
            ChildAction::Start => {
                let response = self.responses.pop_front().unwrap();
                return Effect::future(async move { response.await.ok().map(ChildAction::Mutate) })
                    .cancellable("pending");
            }
            ChildAction::Capture => {
                let captured = self.captured.as_ref().unwrap().clone();
                return Effect::run(move |sender| async move {
                    captured.send(sender).unwrap();
                    std::future::pending::<()>().await;
                });
            }
            ChildAction::Cancel => return Effect::cancel("pending"),
            ChildAction::Delete => {
                state.0.set(7);
                shared.0.set(9);
                let launched = Arc::clone(self.launched.as_ref().unwrap());
                return Effect::run(move |_| async move {
                    launched.fetch_add(1, Ordering::SeqCst);
                });
            }
        }
        Effect::none()
    }
}
/// Observes both child inputs before removing or replacing selected entries.
struct Parent;
impl Reducer for Parent {
    type State = State;
    type Action = Action;
    fn reduce(
        &mut self,
        state: &mut State,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        match action {
            Action::Child(child) => {
                state.observed = Some((state.rows[child.id()].0.get(), state.settings.0.get()));
                if matches!(child.action(), ChildAction::Delete) {
                    state.rows.remove(child.id()).unwrap();
                }
            }
            Action::Replace(id) => {
                state.rows.remove(id).unwrap();
                state.rows.insert(*id, Private(Cell::new(0)));
                state.settings.0.set(5);
            }
            Action::Settings(value) => state.settings.0.set(*value),
            Action::Clear => state.rows.clear(),
        }
        Effect::none()
    }
}
/// Reconciles keyed lifetimes around child-first shared reduction.
fn reducer(child: Child) -> impl Reducer<State = State, Action = Action> {
    for_each_shared(
        Parent,
        |s: &State| &s.rows,
        |s: &mut State| (&mut s.rows, &mut s.settings),
        KeyedActionPath::new(
            |a: &Action| match a {
                Action::Child(a) => Some(a),
                _ => None,
            },
            Action::Child,
        ),
        child,
    )
}

// The outer Store supplies a public context; borrowed values never cross its actor boundary.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one public consumer keeps borrowed domains, callbacks and effect mapping in the same lifetime fixture"
)]
fn synchronous_keyed_shared_reduction_accepts_borrowed_non_send_inputs() {
    /// Borrows local owners that never cross the outer store's actor boundary.
    type LocalState<'a> = (
        KeyedState<u8, &'a Rc<Cell<u8>>>,
        &'a Cell<u8>,
        &'a Cell<(u8, u8)>,
    );

    /// Carries non-`Send` input through synchronous extraction and effect mapping.
    enum LocalAction {
        Child(KeyedAction<u8, Rc<Cell<u8>>>),
        Other(Rc<Cell<u8>>),
    }

    /// Counts real child calls while borrowing private and shared local values.
    struct LocalChild<'a>(&'a Cell<usize>);

    impl<'a> SharedReducer for LocalChild<'a> {
        type State = &'a Rc<Cell<u8>>;
        type Shared = &'a Cell<u8>;
        type Action = Rc<Cell<u8>>;

        fn reduce(
            &mut self,
            state: &mut Self::State,
            shared: &mut Self::Shared,
            action: &Self::Action,
            _: &mut ReduceContext<'_>,
        ) -> Effect<Self::Action> {
            self.0.set(self.0.get() + 1);
            state.set(shared.get() + action.get());
            shared.set(shared.get() * 10 + action.get());
            Effect::send(action.get()).map(|value| Rc::new(Cell::new(value)))
        }
    }

    /// Transfers only owned scalar observations to the outer test assertion.
    #[derive(Clone, Debug, PartialEq)]
    struct Observation {
        private: u8,
        shared: u8,
        parent_observed: (u8, u8),
        projection_calls: [usize; 3],
        parent_calls: usize,
        child_calls: usize,
        mapped_actions: usize,
    }

    let outer = Reduce::new(
        |observations: &mut Vec<Observation>, (): &(), context: &mut ReduceContext<'_>| {
            let private = Rc::new(Cell::new(0));
            let shared = Cell::new(2);
            let parent_observed = Cell::new((0, 0));
            let parent_calls = Cell::new(0);
            let child_calls = Cell::new(0);
            let get_calls = Rc::new(Cell::new(0));
            let split_calls = Rc::new(Cell::new(0));
            let extract_calls = Rc::new(Cell::new(0));
            let mapped_actions = Arc::new(AtomicUsize::new(0));

            let parent = Reduce::new(
                |state: &mut LocalState<'_>, action: &LocalAction, _: &mut ReduceContext<'_>| {
                    parent_calls.set(parent_calls.get() + 1);
                    match action {
                        LocalAction::Child(child) => {
                            state.2.set((state.0[child.id()].get(), state.1.get()));
                        }
                        LocalAction::Other(value) => state.1.set(state.1.get() + value.get()),
                    }
                    Effect::none()
                },
            );
            let mut inner = for_each_shared(
                parent,
                |state: &LocalState<'_>| {
                    get_calls.set(get_calls.get() + 1);
                    &state.0
                },
                |state: &mut LocalState<'_>| {
                    split_calls.set(split_calls.get() + 1);
                    (&mut state.0, &mut state.1)
                },
                KeyedActionPath::new(
                    |action: &LocalAction| {
                        extract_calls.set(extract_calls.get() + 1);
                        match action {
                            LocalAction::Child(child) => Some(child),
                            LocalAction::Other(_) => None,
                        }
                    },
                    LocalAction::Child,
                ),
                LocalChild(&child_calls),
            );
            let mut state: LocalState<'_> = (
                [(7, &private)].into_iter().collect(),
                &shared,
                &parent_observed,
            );

            for action in [
                LocalAction::Child(KeyedAction::new(7, Rc::new(Cell::new(3)))),
                LocalAction::Other(Rc::new(Cell::new(4))),
            ] {
                let mapped_counter = Arc::clone(&mapped_actions);
                let effect = inner
                    .reduce(&mut state, &action, context)
                    .map(move |action| {
                        let LocalAction::Child(child) = action else {
                            panic!("the child emitted an unrelated action");
                        };
                        assert_eq!(*child.id(), 7);
                        assert_eq!(child.action().get(), 3);
                        mapped_counter.fetch_add(1, Ordering::Relaxed);
                    });
                drop(effect);
                observations.push(Observation {
                    private: private.get(),
                    shared: shared.get(),
                    parent_observed: parent_observed.get(),
                    projection_calls: [get_calls.get(), split_calls.get(), extract_calls.get()],
                    parent_calls: parent_calls.get(),
                    child_calls: child_calls.get(),
                    mapped_actions: mapped_actions.load(Ordering::Relaxed),
                });
            }
            Effect::none()
        },
    );

    block_on(async {
        let mut store = TestStore::new(Vec::new(), outer);
        let _ = store
            .send_with((), |observations| {
                *observations = vec![
                    Observation {
                        private: 5,
                        shared: 23,
                        parent_observed: (5, 23),
                        projection_calls: [2, 1, 1],
                        parent_calls: 1,
                        child_calls: 1,
                        mapped_actions: 1,
                    },
                    Observation {
                        private: 5,
                        shared: 27,
                        parent_observed: (5, 23),
                        projection_calls: [4, 1, 2],
                        parent_calls: 2,
                        child_calls: 1,
                        mapped_actions: 1,
                    },
                ];
            })
            .await
            .unwrap();
        store.finish().await.unwrap();
    });
}

// Copied settings, wrong-key routing or parent-before-child ordering breaks this.
#[test]
fn keyed_children_share_current_settings_before_parent_observes_both_inputs() {
    block_on(async {
        let mut store = TestStore::projected(state(), reducer(Child::default()), snapshot);
        let _ = store
            .send_with(action(1, ChildAction::Mutate(1)), |s| {
                *s = (vec![(1, 1), (2, 0)], 1, Some((1, 1)));
            })
            .await
            .unwrap();
        let _ = store
            .send_with(action(2, ChildAction::Mutate(2)), |s| {
                *s = (vec![(1, 1), (2, 3)], 12, Some((3, 12)));
            })
            .await
            .unwrap();
        let _ = store
            .send_with(action(1, ChildAction::Mutate(3)), |s| {
                *s = (vec![(1, 15), (2, 3)], 123, Some((15, 123)));
            })
            .await
            .unwrap();
        store.finish().await.unwrap();
    });
}

// Post-reconciliation must discard effects produced by a same-action removed child.
#[test]
fn same_action_parent_removal_discards_new_child_effect() {
    block_on(async {
        let launched = Arc::new(AtomicUsize::new(0));
        let child = Child {
            launched: Some(Arc::clone(&launched)),
            ..Child::default()
        };
        let mut store = TestStore::projected(state(), reducer(child), snapshot);
        let task = store
            .send_with(action(1, ChildAction::Delete), |s| {
                *s = (vec![(2, 0)], 9, Some((7, 9)));
            })
            .await
            .unwrap();
        task.finish().await.unwrap();
        assert_eq!(launched.load(Ordering::SeqCst), 0);
        store.finish().await.unwrap();
    });
}

// Equal user cancellation IDs must remain per-entry despite common shared settings.
#[test]
fn cancelling_one_entry_preserves_sibling_work_across_settings_only_change() {
    block_on(async {
        let (first_completion, first_response) = controlled_future();
        let (second_completion, second_response) = controlled_future();
        let child = Child {
            responses: VecDeque::from([first_response, second_response]),
            ..Child::default()
        };
        let mut store = TestStore::projected(state(), reducer(child), snapshot);
        let first = store
            .send_with(action(1, ChildAction::Start), |s| s.2 = Some((0, 0)))
            .await
            .unwrap();
        let second = store.send(action(2, ChildAction::Start)).await.unwrap();
        let _ = store
            .send_with(Action::Settings(5), |s| s.1 = 5)
            .await
            .unwrap();
        let _ = store
            .send_with(action(1, ChildAction::Cancel), |s| s.2 = Some((0, 5)))
            .await
            .unwrap();
        first.finish().await.unwrap();
        assert_eq!(first_completion.complete(7).unwrap_err().into_inner(), 7);
        second
            .finish()
            .await
            .expect_err("sibling work remains pending");
        second_completion.complete(2).unwrap();
        let _ = store
            .receive_with(
                |action| {
                    matches!(action, Action::Child(child)
                        if *child.id() == 2 && matches!(child.action(), ChildAction::Mutate(2)))
                },
                |state| *state = (vec![(1, 0), (2, 7)], 52, Some((7, 52))),
            )
            .await
            .unwrap();
        store.finish().await.unwrap();
    });
}

// Stale queued provenance must be rejected before either child or parent reduction.
#[test]
fn reinserted_id_rejects_queued_old_output_and_reads_current_settings() {
    let runtime = DeterministicRuntime::new();
    let (captured, receiver) = mpsc::channel();
    let child = Child {
        captured: Some(captured),
        ..Child::default()
    };
    let store = Store::new(state(), reducer(child), runtime.runtime());
    let driver = runtime.clone();
    block_on(driver.run(async move {
        let old_task = store.send(action(1, ChildAction::Capture)).await.unwrap();
        runtime.run_until_stalled().unwrap();
        let sender = receiver.try_recv().unwrap();
        let replacement = store.try_send(Action::Replace(1)).unwrap();
        let mut stale = Box::pin(sender.send(ChildAction::Mutate(99)));
        assert!(
            stale
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        replacement.finish().await.unwrap().finish().await;
        assert!(stale.await.unwrap_err().is_cancelled());
        old_task.finish().await;
        assert_eq!(
            store.read(snapshot).await.unwrap(),
            (vec![(2, 0), (1, 0)], 5, Some((0, 0)))
        );
        // Two root commits, no old output commit or parent mutation.
        assert_eq!(store.runtime_snapshot().await.unwrap().commit_version, 2);
        store
            .send(action(1, ChildAction::Mutate(2)))
            .await
            .unwrap()
            .finish()
            .await;
        assert_eq!(
            store.read(snapshot).await.unwrap(),
            (vec![(2, 0), (1, 7)], 52, Some((7, 52)))
        );
        store.shutdown().await.unwrap();
    }))
    .unwrap();
}

// An unrelated/settings-only action must not take mutable child/shared projections.
#[test]
fn nonmatching_action_does_not_invoke_split() {
    block_on(async {
        let reducer = for_each_shared(
            Parent,
            |s: &State| &s.rows,
            |_: &mut State| -> (&mut KeyedState<u8, Private>, &mut Settings) {
                panic!("nonmatching split")
            },
            KeyedActionPath::new(
                |a: &Action| match a {
                    Action::Child(a) => Some(a),
                    _ => None,
                },
                Action::Child,
            ),
            Child::default(),
        );
        assert_eq!(format!("{reducer:?}"), "ForEachShared { .. }");
        let mut store = TestStore::projected(state(), reducer, snapshot);
        let _ = store
            .send_with(Action::Settings(8), |s| s.1 = 8)
            .await
            .unwrap();
        store.finish().await.unwrap();
    });
}

// Omitting either selected input must fail exhaustive assertions independently.
#[test]
fn projected_snapshot_detects_omitted_private_or_shared_mutation() {
    for omit_private in [true, false] {
        let panic = catch_unwind(AssertUnwindSafe(|| {
            block_on(async {
                let mut store = TestStore::projected(state(), reducer(Child::default()), snapshot);
                let _ = store
                    .send_with(action(1, ChildAction::Mutate(1)), move |s| {
                        if !omit_private {
                            s.0[0].1 = 1;
                        }
                        if omit_private {
                            s.1 = 1;
                        }
                        s.2 = Some((1, 1));
                    })
                    .await
                    .unwrap();
            });
        }));
        let panic = panic.expect_err("omitted input must fail exhaustive comparison");
        let message = panic
            .downcast_ref::<String>()
            .expect("snapshot comparison supplies a diagnostic message");
        assert!(message.contains("state mismatch"), "{message}");
    }
}

// Ordinary outer scope identities must isolate equal child IDs and cancellation IDs.
#[test]
fn clearing_one_nested_group_cancels_only_its_keyed_descendants() {
    /// Gives equal child IDs independent outer scope identities.
    struct Groups {
        left: State,
        right: State,
    }
    /// Preserves the enclosing group when routing child output.
    enum GroupAction {
        Left(Action),
        Right(Action),
    }
    block_on(async {
        let (left_completion, left_response) = controlled_future();
        let (right_completion, right_response) = controlled_future();
        let child = |response| Child {
            responses: VecDeque::from([response]),
            ..Child::default()
        };
        let reducer = combine((
            scope(
                StatePath::new(|s: &Groups| &s.left, |s: &mut Groups| &mut s.left),
                ActionPath::new(
                    |a: &GroupAction| match a {
                        GroupAction::Left(a) => Some(a),
                        GroupAction::Right(_) => None,
                    },
                    GroupAction::Left,
                ),
                reducer(child(left_response)),
            ),
            scope(
                StatePath::new(|s: &Groups| &s.right, |s: &mut Groups| &mut s.right),
                ActionPath::new(
                    |a: &GroupAction| match a {
                        GroupAction::Right(a) => Some(a),
                        GroupAction::Left(_) => None,
                    },
                    GroupAction::Right,
                ),
                reducer(child(right_response)),
            ),
        ));
        let mut store = TestStore::projected(
            Groups {
                left: state(),
                right: state(),
            },
            reducer,
            |s: &Groups| (snapshot(&s.left), snapshot(&s.right)),
        );
        let left = store
            .send_with(GroupAction::Left(action(1, ChildAction::Start)), |s| {
                s.0.2 = Some((0, 0));
            })
            .await
            .unwrap();
        let right = store
            .send_with(GroupAction::Right(action(1, ChildAction::Start)), |s| {
                s.1.2 = Some((0, 0));
            })
            .await
            .unwrap();
        let _ = store
            .send_with(GroupAction::Left(Action::Clear), |s| s.0.0.clear())
            .await
            .unwrap();
        left.finish().await.unwrap();
        assert_eq!(left_completion.complete(7).unwrap_err().into_inner(), 7);
        right.finish().await.expect_err("sibling remains pending");
        right_completion.complete(2).unwrap();
        let _ = store
            .receive_with(
                |action| {
                    matches!(action, GroupAction::Right(Action::Child(child))
                        if *child.id() == 1 && matches!(child.action(), ChildAction::Mutate(2)))
                },
                |state| state.1 = (vec![(1, 2), (2, 0)], 2, Some((2, 2))),
            )
            .await
            .unwrap();
        store.finish().await.unwrap();
    });
}
