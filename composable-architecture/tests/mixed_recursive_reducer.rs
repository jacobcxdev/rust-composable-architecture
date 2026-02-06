// tests/mixed_recursive_reducer.rs
//
// Mixed recursive reducers: a parent that has both standard child state and keyed child state.
// Contract: routing and effects must not “cross” between standard and keyed children.

use std::hash::Hash;

use composable::*;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Id(u32);

#[derive(Clone, Debug, Default, PartialEq)]
struct StandardChildState {
    log: Vec<&'static str>,
}

#[derive(Clone, Debug, PartialEq)]
enum StandardChildAction {
    Ping,
    Pong,
}

impl Reducer for StandardChildState {
    type Action = StandardChildAction;
    type Output = Self;

    fn reduce(&mut self, action: StandardChildAction, send: impl Effects<StandardChildAction>) {
        use StandardChildAction::*;

        match action {
            Ping => {
                self.log.push("ping");
                send.action(Pong);
            }
            Pong => {
                self.log.push("pong");
            }
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
struct KeyedChildState {
    log: Vec<&'static str>,
}

#[derive(Clone, Debug, PartialEq)]
enum KeyedChildAction {
    Ping,
    Pong,
}

impl Reducer for KeyedChildState {
    type Action = KeyedChildAction;
    type Output = Self;

    fn reduce(&mut self, action: KeyedChildAction, send: impl Effects<KeyedChildAction>) {
        use KeyedChildAction::*;

        match action {
            Ping => {
                self.log.push("ping");
                send.action(Pong);
            }
            Pong => {
                self.log.push("pong");
            }
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, RecursiveReducer)]
struct State {
    standard: StandardChildState,
    keyed: KeyedState<Id, KeyedChildState>,

    #[reducer(skip)]
    parent_log: Vec<&'static str>,
}

#[derive(Clone, Debug, PartialEq, From, TryInto)]
enum Action {
    Standard(StandardChildAction),
    Keyed(Keyed<Id, KeyedChildAction>),
}

impl RecursiveReducer for State {
    type Action = Action;

    fn reduce(&mut self, action: Action, _send: impl Effects<Action>) {
        match action {
            Action::Standard(StandardChildAction::Pong) => {
                self.parent_log.push("parent:standard:pong");
            }
            Action::Keyed(Keyed {
                action: KeyedChildAction::Pong,
                ..
            }) => {
                self.parent_log.push("parent:keyed:pong");
            }
            _ => {}
        }
    }
}

#[test]
fn mixed_struct_routes_standard_actions_and_effects_without_touching_keyed_children() {
    let mut state = State::default();
    state.keyed.insert(Id(1), KeyedChildState::default());

    let mut store = TestStore::with_initial(state);

    store.send(Action::Standard(StandardChildAction::Ping), |state| {
        state.standard.log = vec!["ping"];
    });

    store.recv(Action::Standard(StandardChildAction::Pong), |state| {
        state.standard.log = vec!["ping", "pong"];
        state.parent_log = vec!["parent:standard:pong"];
    });

    let state = store.into_inner();
    assert_eq!(
        state.keyed.get(&Id(1)).unwrap().log,
        Vec::<&'static str>::new()
    );
}

#[test]
fn mixed_struct_routes_keyed_actions_and_effects_back_to_same_child_without_touching_standard_child(
) {
    let mut state = State::default();
    state.keyed.insert(Id(1), KeyedChildState::default());

    let mut store = TestStore::with_initial(state);

    store.send(
        Action::Keyed(Keyed::new(Id(1), KeyedChildAction::Ping)),
        |state| {
            state.keyed.get_mut(&Id(1)).unwrap().log = vec!["ping"];
        },
    );

    store.recv(
        Action::Keyed(Keyed::new(Id(1), KeyedChildAction::Pong)),
        |state| {
            state.keyed.get_mut(&Id(1)).unwrap().log = vec!["ping", "pong"];
            state.parent_log = vec!["parent:keyed:pong"];
        },
    );

    let state = store.into_inner();
    assert_eq!(state.standard.log, Vec::<&'static str>::new());
}
