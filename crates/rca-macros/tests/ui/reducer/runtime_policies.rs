//! Checks ephemeral policies and their interaction with generated child bindings.
//!
//! The `d54-policy-runtime` consumer checks ordering, borrowed domains and stale dismissal.

#![cfg(test)]

#[path = "../../../../rca-core/tests/support/thread_runtime.rs"]
mod thread_runtime;

use rca_core::{Dismiss, Effect, PresentationAction, ReduceContext, Reducer, Store};
use thread_runtime::ThreadRuntime;

struct Alert {
    id: u64,
    text: String,
}
enum AlertAction {
    Capture,
    Accept,
    Replace,
}
struct IgnoredAction(Vec<u8>);

#[rca_macros::reducer(
    state(generated(struct State {
        #[reducer(case = alert)] alert: Option<Alert>,
        #[reducer(case = ignored)] ignored: Vec<u8>,
        trace: Vec<(u64, &'static str)>,
        plain_outputs: usize,
    })),
    action(generated(enum Action {
        #[reducer(case = alert)] Alert(_),
        #[reducer(case = ignored)] Ignored(_),
        Remove,
        Insert(Alert),
        Replay,
        PlainOutput,
        CaptureOutput(std::sync::mpsc::Sender<rca_core::EffectSender<Action>>),
    })),
    body(generated),
    case(alert, policy = r#ephemeral, action = supplied(AlertAction),
        state_path = StatePaths::alert(), action_path = ActionPaths::alert(),
        identity = |alert: &Alert| &alert.id,
        predicate = |_: &Alert, action: &AlertAction| matches!(action, AlertAction::Accept | AlertAction::Replace),
        r#bind(fn bind_alert<P>(parent: P) where P: Reducer<State = State, Action = Action>),
    ),
    case(ignored, policy = ignored, action = supplied(IgnoredAction)),
)]
struct Feature<R> {
    #[reducer(case = alert)]
    body: R,
}

/// Records parent visibility and retains dismissal from an earlier installation.
struct Parent {
    retained: Option<Dismiss>,
}

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
            Action::Alert(PresentationAction::Presented(action)) => {
                let alert = state.alert.as_ref().expect("parent sees data before clear");
                assert_eq!(alert.text, "same visible data");
                state.trace.push((
                    alert.id,
                    match action {
                        AlertAction::Capture => "capture",
                        AlertAction::Accept => "accept",
                        AlertAction::Replace => "replace",
                    },
                ));
                match action {
                    AlertAction::Capture => self.retained = Some(context.dismiss()),
                    AlertAction::Replace => {
                        state.alert = Some(Alert {
                            id: alert.id + 1,
                            text: alert.text.clone(),
                        })
                    }
                    AlertAction::Accept => (),
                }
                Effect::send(Action::PlainOutput)
            }
            Action::Alert(PresentationAction::Dismiss) => {
                state.trace.push((
                    state
                        .alert
                        .as_ref()
                        .expect("parent sees data before dismiss")
                        .id,
                    "dismiss",
                ));
                Effect::none()
            }
            Action::Ignored(payload) => {
                state.ignored.extend_from_slice(&payload.0);
                state.trace.push((0, "ignored"));
                Effect::none()
            }
            Action::Remove => {
                state.alert = None;
                Effect::none()
            }
            Action::Insert(alert) => {
                state.alert = Some(Alert {
                    id: alert.id,
                    text: alert.text.clone(),
                });
                Effect::none()
            }
            Action::Replay => self.retained.as_ref().expect("captured dismissal").effect(),
            Action::PlainOutput => {
                state.plain_outputs += 1;
                Effect::none()
            }
            Action::CaptureOutput(captured) => {
                let captured = captured.clone();
                Effect::run(move |sender| async move {
                    captured.send(sender).unwrap();
                    core::future::pending::<()>().await;
                })
            }
        }
    }
}

/// Creates equal visible data with an independently selected identity.
fn data(id: u64) -> Alert {
    Alert {
        id,
        text: "same visible data".to_owned(),
    }
}
/// Starts each test with the same data and empty observation records.
fn state() -> State {
    State {
        alert: Some(data(7)),
        ignored: Vec::new(),
        trace: Vec::new(),
        plain_outputs: 0,
    }
}

#[test]
fn generated_state_paths_borrow_and_mutate_only_the_named_fields() {
    let mut state = state();
    state.ignored = vec![3, 5];
    state.trace = vec![(11, "before")];
    state.plain_outputs = 9;
    let alert_text = state.alert.as_ref().unwrap().text.as_ptr();

    let ignored = StatePaths::ignored();
    let trace = StatePaths::trace();
    let plain_outputs = StatePaths::plain_outputs();
    assert!(core::ptr::eq(ignored.get(&state), &state.ignored));
    assert!(core::ptr::eq(trace.get(&state), &state.trace));
    assert!(core::ptr::eq(
        plain_outputs.get(&state),
        &state.plain_outputs
    ));
    assert_eq!(ignored.get(&state), &[3, 5]);
    assert_eq!(trace.get(&state), &[(11, "before")]);
    assert_eq!(*plain_outputs.get(&state), 9);

    ignored.get_mut(&mut state).push(7);
    trace.get_mut(&mut state).push((13, "after"));
    *plain_outputs.get_mut(&mut state) = 23;

    assert_eq!(state.ignored, [3, 5, 7]);
    assert_eq!(state.trace, [(11, "before"), (13, "after")]);
    assert_eq!(state.plain_outputs, 23);
    let alert = state.alert.as_ref().unwrap();
    assert_eq!(alert.id, 7);
    assert_eq!(alert.text, "same visible data");
    assert_eq!(alert.text.as_ptr(), alert_text);
}

#[test]
fn generated_payload_action_paths_preserve_storage_and_reject_other_variants() {
    let ignored = ActionPaths::ignored();
    let payload = IgnoredAction(vec![17, 19]);
    let allocation = payload.0.as_ptr();
    let action = ignored.embed(payload);
    let Action::Ignored(original) = &action else {
        panic!("the ignored path must embed the Ignored variant");
    };
    let borrowed = ignored.extract(&action).unwrap();
    assert!(core::ptr::eq(borrowed, original));
    assert_eq!(borrowed.0, [17, 19]);
    assert_eq!(borrowed.0.as_ptr(), allocation);
    assert!(ignored.extract(&Action::Insert(data(29))).is_none());
    assert!(ignored.extract(&Action::Remove).is_none());

    let insert = ActionPaths::insert();
    let payload = Alert {
        id: 31,
        text: "insert payload".to_owned(),
    };
    let allocation = payload.text.as_ptr();
    let action = insert.embed(payload);
    let Action::Insert(original) = &action else {
        panic!("the insert path must embed the Insert variant");
    };
    let borrowed = insert.extract(&action).unwrap();
    assert!(core::ptr::eq(borrowed, original));
    assert_eq!(borrowed.id, 31);
    assert_eq!(borrowed.text, "insert payload");
    assert_eq!(borrowed.text.as_ptr(), allocation);
    assert!(
        insert
            .extract(&Action::Ignored(IgnoredAction(vec![41])))
            .is_none()
    );
    assert!(insert.extract(&Action::Remove).is_none());
    assert!(ActionPaths::alert().extract(&action).is_none());
    assert!(ActionPaths::alert().extract(&Action::Replay).is_none());
}

#[test]
fn generated_capture_output_path_borrows_the_embedded_sender() {
    let path = ActionPaths::capture_output();
    let (sender, receiver) = std::sync::mpsc::channel();
    let action = path.embed(sender);
    let Action::CaptureOutput(original) = &action else {
        panic!("the capture path must embed the CaptureOutput variant");
    };
    assert!(core::ptr::eq(path.extract(&action).unwrap(), original));
    assert!(path.extract(&Action::PlainOutput).is_none());
    assert!(path.extract(&Action::Remove).is_none());
    assert!(matches!(
        receiver.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    ));

    drop(action);
    assert!(matches!(
        receiver.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Disconnected)
    ));
}

#[test]
fn generated_unit_action_paths_distinguish_remove_replay_and_plain_output() {
    let remove = ActionPaths::remove();
    assert_eq!(remove.extract(&Action::Remove), Some(()));
    assert!(remove.extract(&Action::Replay).is_none());
    assert!(remove.extract(&Action::PlainOutput).is_none());
    assert!(matches!(remove.embed(()), Action::Remove));

    let replay = ActionPaths::replay();
    assert_eq!(replay.extract(&Action::Replay), Some(()));
    assert!(replay.extract(&Action::Remove).is_none());
    assert!(replay.extract(&Action::PlainOutput).is_none());
    assert!(matches!(replay.embed(()), Action::Replay));

    let plain_output = ActionPaths::plain_output();
    assert_eq!(plain_output.extract(&Action::PlainOutput), Some(()));
    assert!(plain_output.extract(&Action::Remove).is_none());
    assert!(plain_output.extract(&Action::Replay).is_none());
    assert!(matches!(plain_output.embed(()), Action::PlainOutput));
}

#[test]
fn ignored_mapping_and_ephemeral_parent_before_clear_are_real_macro_consumers() {
    let runtime = ThreadRuntime::new();
    let store = Store::new(
        state(),
        Feature {
            body: bind_alert(Parent { retained: None }),
        },
        runtime.runtime(),
    );
    futures_executor::block_on(async {
        store
            .send(ActionPaths::ignored().embed(IgnoredAction(vec![3, 4])))
            .await
            .unwrap()
            .finish()
            .await;
        assert_eq!(
            store
                .read(|state| (state.ignored.clone(), state.trace.clone()))
                .await
                .unwrap(),
            (vec![3, 4], vec![(0, "ignored")])
        );
        store
            .send(ActionPaths::alert().embed(PresentationAction::Presented(AlertAction::Accept)))
            .await
            .unwrap()
            .finish()
            .await;
        assert_eq!(
            store
                .read(|state| (
                    state.alert.is_none(),
                    state.trace.clone(),
                    state.plain_outputs
                ))
                .await
                .unwrap(),
            (true, vec![(0, "ignored"), (7, "accept")], 1)
        );
        store.shutdown().await.unwrap();
    });
}

#[test]
fn different_same_action_replacement_survives_and_old_dismiss_is_stale() {
    let runtime = ThreadRuntime::new();
    let store = Store::new(
        state(),
        Feature {
            body: bind_alert(Parent { retained: None }),
        },
        runtime.runtime(),
    );
    futures_executor::block_on(async {
        store
            .send(Action::Alert(PresentationAction::Presented(
                AlertAction::Capture,
            )))
            .await
            .unwrap()
            .finish()
            .await;
        store
            .send(Action::Alert(PresentationAction::Presented(
                AlertAction::Replace,
            )))
            .await
            .unwrap()
            .finish()
            .await;
        store.send(Action::Replay).await.unwrap().finish().await;
        assert_eq!(
            store
                .read(|state| (
                    state.alert.as_ref().map(|alert| alert.id),
                    state.trace.clone(),
                    state.plain_outputs
                ))
                .await
                .unwrap(),
            (Some(8), vec![(7, "capture"), (7, "replace")], 2)
        );
        store
            .send(Action::Alert(PresentationAction::Dismiss))
            .await
            .unwrap()
            .finish()
            .await;
        assert!(store.read(|state| state.alert.is_none()).await.unwrap());
        assert_eq!(
            store.read(|state| state.trace.clone()).await.unwrap(),
            [(7, "capture"), (7, "replace"), (8, "dismiss")]
        );
        store.shutdown().await.unwrap();
    });
}

#[test]
fn equal_data_remove_reinsert_does_not_renew_retained_dismissal() {
    let runtime = ThreadRuntime::new();
    let store = Store::new(
        state(),
        Feature {
            body: bind_alert(Parent { retained: None }),
        },
        runtime.runtime(),
    );
    futures_executor::block_on(async {
        store
            .send(Action::Alert(PresentationAction::Presented(
                AlertAction::Capture,
            )))
            .await
            .unwrap()
            .finish()
            .await;
        store.send(Action::Remove).await.unwrap().finish().await;
        store
            .send(Action::Insert(data(7)))
            .await
            .unwrap()
            .finish()
            .await;
        store.send(Action::Replay).await.unwrap().finish().await;
        assert_eq!(
            store
                .read(|state| (
                    state.alert.as_ref().map(|alert| alert.id),
                    state.trace.clone()
                ))
                .await
                .unwrap(),
            (Some(7), vec![(7, "capture")])
        );
        store.shutdown().await.unwrap();
    });
}

mod local {
    //! Keeps borrowed, non-Send domains within a synchronous reduction.

    use super::*;
    use std::{cell::Cell, rc::Rc};

    struct Data<'a> {
        id: u8,
        value: &'a Rc<Cell<usize>>,
    }
    struct DataAction<'a> {
        amount: &'a usize,
        token: Rc<()>,
    }

    #[rca_macros::reducer(
        state(generated(struct State<'a> {
            #[reducer(case = data)] data: Option<Data<'a>>,
            seen: &'a Cell<usize>,
        })),
        action(generated(enum Action<'a> {
            #[reducer(case = data)] Data(_),
        })),
        body(supplied(body)),
        case(data, policy = ephemeral, action = supplied(DataAction<'a>),
            state_path = StatePaths::data(), action_path = ActionPaths::data(),
            identity = |data: &Data<'a>| &data.id,
            predicate = |_: &Data<'a>, _: &DataAction<'a>| true,
            bind(fn bind_data<'a, P>(parent: P) where P: Reducer<State = State<'a>, Action = Action<'a>>),
        ),
    )]
    struct Feature<'a, R> {
        body: R,
        marker: core::marker::PhantomData<&'a ()>,
    }

    #[test]
    fn generated_seen_state_path_replaces_only_the_borrowed_cell_reference() {
        let seen = Cell::new(11);
        let replacement = Cell::new(23);
        let value = Rc::new(Cell::new(7));
        let mut state = State {
            data: Some(Data {
                id: 5,
                value: &value,
            }),
            seen: &seen,
        };
        let path = StatePaths::seen();
        assert!(core::ptr::eq(path.get(&state), &state.seen));
        assert!(core::ptr::eq(*path.get(&state), &seen));
        assert_eq!(path.get(&state).get(), 11);

        *path.get_mut(&mut state) = &replacement;
        assert!(core::ptr::eq(state.seen, &replacement));
        path.get(&state).set(29);
        assert_eq!(seen.get(), 11);
        assert_eq!(replacement.get(), 29);
        let data = state.data.as_ref().unwrap();
        assert_eq!(data.id, 5);
        assert!(core::ptr::eq(data.value, &value));
        assert_eq!(value.get(), 7);
        assert_eq!(Rc::strong_count(&value), 1);
    }

    #[test]
    fn generated_data_action_embedding_preserves_borrowed_and_move_only_payloads() {
        let amount = 13;
        let token = Rc::new(());
        let allocation = Rc::as_ptr(&token);
        let path = ActionPaths::data();
        let action = path.embed(PresentationAction::Presented(DataAction {
            amount: &amount,
            token,
        }));
        let Action::Data(original) = &action;
        let borrowed = path.extract(&action).unwrap();
        assert!(core::ptr::eq(borrowed, original));
        let PresentationAction::Presented(payload) = borrowed else {
            panic!("the data path must preserve the Presented payload");
        };
        assert!(core::ptr::eq(payload.amount, &amount));
        assert_eq!(*payload.amount, 13);
        assert_eq!(Rc::as_ptr(&payload.token), allocation);
        assert_eq!(Rc::strong_count(&payload.token), 1);

        let dismissal = path.embed(PresentationAction::Dismiss);
        assert!(matches!(
            dismissal,
            Action::Data(PresentationAction::Dismiss)
        ));
        assert!(matches!(
            path.extract(&dismissal),
            Some(PresentationAction::Dismiss)
        ));
        assert!(matches!(
            path.extract(&action),
            Some(PresentationAction::Presented(_))
        ));
    }

    struct Parent<'a>(&'a Cell<usize>);
    impl<'a> Reducer for Parent<'a> {
        type State = State<'a>;
        type Action = Action<'a>;
        fn reduce(
            &mut self,
            state: &mut State<'a>,
            action: &Action<'a>,
            _: &mut ReduceContext<'_>,
        ) -> Effect<Action<'a>> {
            self.0.set(self.0.get() + 1);
            let Action::Data(PresentationAction::Presented(action)) = action else {
                panic!("presented action")
            };
            let data = state
                .data
                .as_ref()
                .expect("parent inspects borrowed data before clear");
            data.value.set(data.value.get() + *action.amount);
            state.seen.set(core::ptr::from_ref(action) as usize);
            assert_eq!(Rc::strong_count(&action.token), 1);
            Effect::none()
        }
    }

    struct Bridge;
    impl Reducer for Bridge {
        type State = usize;
        type Action = ();
        fn reduce(
            &mut self,
            output: &mut usize,
            _: &(),
            context: &mut ReduceContext<'_>,
        ) -> Effect<()> {
            let calls = Cell::new(0);
            let seen = Cell::new(0);
            let value = Rc::new(Cell::new(2));
            let amount = 3;
            let mut feature = Feature {
                body: bind_data(Parent(&calls)),
                marker: core::marker::PhantomData,
            };
            let mut state = State {
                data: Some(Data {
                    id: 7,
                    value: &value,
                }),
                seen: &seen,
            };
            let action = Action::Data(PresentationAction::Presented(DataAction {
                amount: &amount,
                token: Rc::new(()),
            }));
            let Action::Data(PresentationAction::Presented(payload)) = &action else {
                unreachable!()
            };
            let address = core::ptr::from_ref(payload) as usize;
            drop(feature.reduce(&mut state, &action, context));
            assert!(state.data.is_none());
            assert_eq!(calls.get(), 1);
            assert_eq!(seen.get(), address);
            *output = value.get();
            Effect::none()
        }
    }

    #[test]
    fn macro_ephemeral_binding_accepts_real_local_borrowed_move_only_domains() {
        let runtime = ThreadRuntime::new();
        let store = Store::new(0, Bridge, runtime.runtime());
        futures_executor::block_on(async {
            store.send(()).await.unwrap().finish().await;
            assert_eq!(store.read(|state| *state).await.unwrap(), 5);
            store.shutdown().await.unwrap();
        });
    }
}

enum OptionalAction {
    Child(Action),
    Remove,
    Insert,
}
struct OptionalParent;
impl Reducer for OptionalParent {
    type State = Option<State>;
    type Action = OptionalAction;
    fn reduce(
        &mut self,
        value: &mut Self::State,
        action: &OptionalAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<OptionalAction> {
        match action {
            OptionalAction::Remove => *value = None,
            OptionalAction::Insert => *value = Some(state()),
            OptionalAction::Child(_) => (),
        }
        Effect::none()
    }
}

#[rca_macros::reducer(
    state(supplied(Option<State>)), action(supplied(OptionalAction)), body(supplied(body)),
    child(data, reducer = C, kind = optional,
        state = rca_core::OptionalStatePath::new(|state: &Option<State>| state.as_ref(), |state: &mut Option<State>| state.as_mut()),
        action = rca_core::ActionPath::new(|action: &OptionalAction| match action { OptionalAction::Child(action) => Some(action), _ => None }, OptionalAction::Child),
        bind(fn bind_optional<C>(parent: OptionalParent, child: C) where C: Reducer<State = State, Action = Action> + Send + 'static),
    ),
)]
struct OptionalFeature<R> {
    body: R,
}

enum CaseState {
    Child(State),
    Empty,
}
enum CaseAction {
    Child(Action),
    Remove,
    Insert,
}
struct CaseParent;
impl Reducer for CaseParent {
    type State = CaseState;
    type Action = CaseAction;
    fn reduce(
        &mut self,
        value: &mut CaseState,
        action: &CaseAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<CaseAction> {
        match action {
            CaseAction::Remove => *value = CaseState::Empty,
            CaseAction::Insert => *value = CaseState::Child(state()),
            CaseAction::Child(_) => (),
        }
        Effect::none()
    }
}

#[rca_macros::reducer(
    state(supplied(CaseState)), action(supplied(CaseAction)), body(supplied(body)),
    child(data, reducer = C, kind = case,
        state = rca_core::CaseStatePath::new(
            |state: &CaseState| match state { CaseState::Child(value) => Some(value), CaseState::Empty => None },
            |state: &mut CaseState| match state { CaseState::Child(value) => Some(value), CaseState::Empty => None },
        ),
        action = rca_core::ActionPath::new(|action: &CaseAction| match action { CaseAction::Child(action) => Some(action), _ => None }, CaseAction::Child),
        bind(fn bind_case<C>(parent: CaseParent, child: C) where C: Reducer<State = State, Action = Action> + Send + 'static),
    ),
)]
struct CaseFeature<R> {
    body: R,
}

enum KeyedAction {
    Child(rca_core::KeyedAction<u8, Action>),
    Remove,
    Insert,
    Reorder,
}
struct KeyedParent;
impl Reducer for KeyedParent {
    type State = rca_core::KeyedState<u8, State>;
    type Action = KeyedAction;
    fn reduce(
        &mut self,
        value: &mut Self::State,
        action: &KeyedAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<KeyedAction> {
        match action {
            KeyedAction::Remove => {
                value.remove(&1);
            }
            KeyedAction::Insert => value.insert(1, state()),
            KeyedAction::Reorder => value.move_index(1, 0),
            KeyedAction::Child(_) => (),
        }
        Effect::none()
    }
}

#[rca_macros::reducer(
    state(supplied(rca_core::KeyedState<u8, State>)), action(supplied(KeyedAction)), body(supplied(body)),
    child(data, reducer = C, kind = keyed,
        state = rca_core::KeyedStatePath::new(|state: &rca_core::KeyedState<u8, State>| state, |state: &mut rca_core::KeyedState<u8, State>| state),
        action = rca_core::KeyedActionPath::new(|action: &KeyedAction| match action { KeyedAction::Child(action) => Some(action), _ => None }, KeyedAction::Child),
        bind(fn bind_keyed<C>(parent: KeyedParent, child: C) where C: Reducer<State = State, Action = Action> + Send + 'static),
    ),
)]
struct KeyedFeature<R> {
    body: R,
}

#[test]
fn generated_ephemeral_inside_generated_optional_loses_old_parent_authority() {
    let runtime = ThreadRuntime::new();
    let inner = Feature {
        body: bind_alert(Parent { retained: None }),
    };
    let store = Store::new(
        Some(state()),
        OptionalFeature {
            body: bind_optional(OptionalParent, inner),
        },
        runtime.runtime(),
    );
    let (send, receive) = std::sync::mpsc::channel();
    futures_executor::block_on(async {
        store
            .send(OptionalAction::Child(Action::Alert(
                PresentationAction::Presented(AlertAction::Capture),
            )))
            .await
            .unwrap()
            .finish()
            .await;
        let work = store
            .send(OptionalAction::Child(Action::CaptureOutput(send)))
            .await
            .unwrap();
        let old = receive
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        store
            .send(OptionalAction::Remove)
            .await
            .unwrap()
            .finish()
            .await;
        work.finish().await;
        store
            .send(OptionalAction::Insert)
            .await
            .unwrap()
            .finish()
            .await;
        assert!(
            old.send(Action::PlainOutput)
                .await
                .unwrap_err()
                .is_cancelled()
        );
        store
            .send(OptionalAction::Child(Action::Replay))
            .await
            .unwrap()
            .finish()
            .await;
        assert_eq!(
            store
                .read(|state| {
                    let state = state.as_ref().unwrap();
                    (
                        state.alert.as_ref().map(|data| data.id),
                        state.plain_outputs,
                    )
                })
                .await
                .unwrap(),
            (Some(7), 0)
        );
        store.shutdown().await.unwrap();
    });
}

#[test]
fn generated_ephemeral_inside_generated_case_loses_old_parent_authority() {
    let runtime = ThreadRuntime::new();
    let inner = Feature {
        body: bind_alert(Parent { retained: None }),
    };
    let store = Store::new(
        CaseState::Child(state()),
        CaseFeature {
            body: bind_case(CaseParent, inner),
        },
        runtime.runtime(),
    );
    let (send, receive) = std::sync::mpsc::channel();
    futures_executor::block_on(async {
        store
            .send(CaseAction::Child(Action::Alert(
                PresentationAction::Presented(AlertAction::Capture),
            )))
            .await
            .unwrap()
            .finish()
            .await;
        let work = store
            .send(CaseAction::Child(Action::CaptureOutput(send)))
            .await
            .unwrap();
        let old = receive
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        store.send(CaseAction::Remove).await.unwrap().finish().await;
        work.finish().await;
        store.send(CaseAction::Insert).await.unwrap().finish().await;
        assert!(
            old.send(Action::PlainOutput)
                .await
                .unwrap_err()
                .is_cancelled()
        );
        store
            .send(CaseAction::Child(Action::Replay))
            .await
            .unwrap()
            .finish()
            .await;
        assert_eq!(
            store
                .read(|state| {
                    let CaseState::Child(state) = state else {
                        panic!("replacement exists")
                    };
                    (
                        state.alert.as_ref().map(|data| data.id),
                        state.plain_outputs,
                    )
                })
                .await
                .unwrap(),
            (Some(7), 0)
        );
        store.shutdown().await.unwrap();
    });
}

#[test]
fn generated_ephemeral_inside_generated_keyed_parent_retains_reordered_survivor() {
    let runtime = ThreadRuntime::new();
    let inner = Feature {
        body: bind_alert(Parent { retained: None }),
    };
    let mut rows = rca_core::KeyedState::new();
    rows.insert(1, state());
    rows.insert(2, state());
    let store = Store::new(
        rows,
        KeyedFeature {
            body: bind_keyed(KeyedParent, inner),
        },
        runtime.runtime(),
    );
    let (send, receive) = std::sync::mpsc::channel();
    let (survivor_send, survivor_receive) = std::sync::mpsc::channel();
    futures_executor::block_on(async {
        let work = store
            .send(KeyedAction::Child(rca_core::KeyedAction::new(
                1,
                Action::CaptureOutput(send),
            )))
            .await
            .unwrap();
        let old = receive
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        let survivor_work = store
            .send(KeyedAction::Child(rca_core::KeyedAction::new(
                2,
                Action::CaptureOutput(survivor_send),
            )))
            .await
            .unwrap();
        let survivor = survivor_receive
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        store
            .send(KeyedAction::Remove)
            .await
            .unwrap()
            .finish()
            .await;
        work.finish().await;
        store
            .send(KeyedAction::Insert)
            .await
            .unwrap()
            .finish()
            .await;
        store
            .send(KeyedAction::Reorder)
            .await
            .unwrap()
            .finish()
            .await;
        assert!(
            old.send(Action::PlainOutput)
                .await
                .unwrap_err()
                .is_cancelled()
        );
        survivor.send(Action::PlainOutput).await.unwrap();
        assert_eq!(
            store
                .read(|rows| (rows[&1].plain_outputs, rows[&2].plain_outputs))
                .await
                .unwrap(),
            (0, 1)
        );
        store.shutdown().await.unwrap();
        survivor_work.finish().await;
    });
}

mod construction {
    //! Checks owned reducer cleanup when a caller's path expression panics.

    use super::*;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::{Arc, Mutex};

    /// Records the parent reducer's execution and disposal.
    struct Parent(Arc<Mutex<Vec<&'static str>>>);

    impl Reducer for Parent {
        type State = Option<usize>;
        type Action = usize;

        fn reduce(
            &mut self,
            state: &mut Self::State,
            _: &usize,
            _: &mut ReduceContext<'_>,
        ) -> Effect<usize> {
            self.0.lock().unwrap().push("parent reduce");
            *state.as_mut().unwrap() += 100;
            Effect::none()
        }
    }

    impl Drop for Parent {
        fn drop(&mut self) {
            self.0.lock().unwrap().push("parent drop");
        }
    }

    /// Supplies a construction fault without changing the generated binder.
    struct Child {
        trace: Arc<Mutex<Vec<&'static str>>>,
        fail: bool,
    }

    impl Reducer for Child {
        type State = usize;
        type Action = usize;

        fn reduce(
            &mut self,
            state: &mut usize,
            action: &usize,
            _: &mut ReduceContext<'_>,
        ) -> Effect<usize> {
            self.trace.lock().unwrap().push("child reduce");
            *state += action;
            Effect::none()
        }
    }

    impl Drop for Child {
        fn drop(&mut self) {
            self.trace.lock().unwrap().push("child drop");
        }
    }

    #[rca_macros::reducer(
        state(supplied(Option<usize>)), action(supplied(usize)), body(supplied(body)),
        child(value, reducer = Child, kind = optional,
            state = {
                child.trace.lock().unwrap().push("state path");
                assert!(!child.fail, "binder state path fault");
                rca_core::OptionalStatePath::new(
                    |state: &Option<usize>| state.as_ref(),
                    |state: &mut Option<usize>| state.as_mut(),
                )
            },
            action = {
                child.trace.lock().unwrap().push("action path");
                rca_core::ActionPath::new(|action: &usize| Some(action), |action| action)
            },
            bind(fn bind_value(parent: Parent, child: Child)),
        ),
    )]
    struct Feature<R> {
        body: R,
    }

    #[test]
    fn panicking_path_factory_releases_owned_reducers_and_stops_later_evaluation() {
        let trace = Arc::new(Mutex::new(Vec::new()));
        let parent = Parent(Arc::clone(&trace));
        let child = Child {
            trace: Arc::clone(&trace),
            fail: true,
        };
        let panic = catch_unwind(AssertUnwindSafe(|| bind_value(parent, child)))
            .err()
            .expect("the state path must stop binder construction");
        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"binder state path fault")
        );
        {
            let events = trace.lock().unwrap();
            assert_eq!(events[0], "state path");
            assert_eq!(events.len(), 3);
            assert_eq!(
                events
                    .iter()
                    .filter(|&&event| event == "parent drop")
                    .count(),
                1
            );
            assert_eq!(
                events
                    .iter()
                    .filter(|&&event| event == "child drop")
                    .count(),
                1
            );
        }
        trace.lock().unwrap().clear();

        let feature = Feature {
            body: bind_value(
                Parent(Arc::clone(&trace)),
                Child {
                    trace: Arc::clone(&trace),
                    fail: false,
                },
            ),
        };
        let events = trace.lock().unwrap().clone();
        assert_eq!(events, ["state path", "action path"]);
        let runtime = ThreadRuntime::new();
        let store = Store::new(Some(3), feature, runtime.runtime());
        futures_executor::block_on(async {
            store.send(7).await.unwrap().finish().await;
            assert_eq!(store.read(|state| *state).await.unwrap(), Some(110));
            let events = trace.lock().unwrap().clone();
            assert_eq!(
                events,
                ["state path", "action path", "child reduce", "parent reduce"]
            );
            store.shutdown().await.unwrap();
        });
        let events = trace.lock().unwrap();
        assert_eq!(events.len(), 6);
        assert_eq!(
            events
                .iter()
                .filter(|&&event| event == "parent drop")
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|&&event| event == "child drop")
                .count(),
            1
        );
    }
}
