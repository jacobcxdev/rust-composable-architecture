use std::collections::HashMap;

use composable::*;

/// Keyed reducers route `Keyed<K, ChildAction>` into one child state selected by `K`.
/// Effects emitted by the child are scoped back through the parent action type while
/// preserving the same key.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Id(u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Name(&'static str);

#[derive(Clone, Debug, Default, PartialEq)]
struct ChildState {
    log: Vec<&'static str>,
}

#[derive(Clone, Debug, PartialEq)]
enum ChildAction {
    EmitPing,
    Ping,
}

impl Reducer for ChildState {
    type Action = ChildAction;
    type Output = Self;

    fn reduce(&mut self, action: ChildAction, send: impl Effects<ChildAction>) {
        use ChildAction::*;

        match action {
            EmitPing => {
                // Important: no state change here.
                // This lets us prove the "effect is queued" behaviour in TestStore.
                send.action(Ping);
            }
            Ping => {
                self.log.push("ping");
            }
        }
    }
}

#[test]
/// A keyed collection:
/// - routes actions by key
/// - routes effect-driven follow-up actions back to the same key
fn keyed_struct_routes_actions_and_effects_back_to_same_child() {
    #[derive(Clone, Debug, Default, PartialEq, RecursiveReducer)]
    struct State {
        children: KeyedState<Id, ChildState>,

        #[reducer(skip)]
        mirror: HashMap<Id, Vec<&'static str>>,
    }

    #[derive(Clone, From, TryInto, Debug, PartialEq)]
    enum Action {
        Child(Keyed<Id, ChildAction>),
    }

    impl RecursiveReducer for State {
        type Action = Action;

        fn reduce(&mut self, _action: Action, _send: impl Effects<Action>) {}
    }

    let mut state = State::default();
    state.children.insert(Id(1), ChildState::default());
    state.children.insert(Id(2), ChildState::default());

    let mut store = TestStore::with_initial(state);

    // 1) Send an action to child 1 that emits a follow-up effect.
    // TestStore does NOT automatically drain effects—so no "ping" yet.
    store.send(
        Action::Child(Keyed::new(Id(1), ChildAction::EmitPing)),
        |_| {},
    );

    // 2) The follow-up action should be queued as a *keyed parent action* for the same key.
    store.recv(
        Action::Child(Keyed::new(Id(1), ChildAction::Ping)),
        |state| {
            state.children.get_mut(&Id(1)).unwrap().log = vec!["ping"];
        },
    );

    // 3) Child 2 should remain untouched.
    assert_eq!(
        store.into_inner().children.get(&Id(2)).unwrap().log,
        Vec::<&'static str>::new(),
    );
    // And skipped fields should remain default/unmodified.
    // (If the derive attempted to recurse into `mirror`, this test would not compile.)
}

#[test]
/// Multiple keyed collections can coexist as long as their keyed payload types differ.
/// Using distinct key newtypes makes `Keyed<K, ChildAction>` distinct and avoids conversion ambiguity.
fn multiple_keyed_fields_work_when_keys_are_distinct_types() {
    #[derive(Clone, Debug, Default, PartialEq, RecursiveReducer)]
    struct State {
        by_id: KeyedState<Id, ChildState>,
        by_name: KeyedState<Name, ChildState>,
    }

    #[derive(Clone, From, TryInto, Debug, PartialEq)]
    enum Action {
        ById(Keyed<Id, ChildAction>),
        ByName(Keyed<Name, ChildAction>),
    }

    impl RecursiveReducer for State {
        type Action = Action;

        fn reduce(&mut self, _action: Action, _send: impl Effects<Action>) {}
    }

    let mut state = State::default();
    state.by_id.insert(Id(1), ChildState::default());
    state.by_name.insert(Name("A"), ChildState::default());

    let mut store = TestStore::with_initial(state);

    // Route via the `Id`-keyed field.
    store.send(
        Action::ById(Keyed::new(Id(1), ChildAction::EmitPing)),
        |_| {},
    );
    store.recv(
        Action::ById(Keyed::new(Id(1), ChildAction::Ping)),
        |state| {
            state.by_id.get_mut(&Id(1)).unwrap().log = vec!["ping"];
        },
    );

    // Route via the `Name`-keyed field.
    store.send(
        Action::ByName(Keyed::new(Name("A"), ChildAction::EmitPing)),
        |_| {},
    );
    store.recv(
        Action::ByName(Keyed::new(Name("A"), ChildAction::Ping)),
        |state| {
            state.by_name.get_mut(&Name("A")).unwrap().log = vec!["ping"];
        },
    );

    let state = store.into_inner();
    assert_eq!(state.by_id.get(&Id(1)).unwrap().log, vec!["ping"]);
    assert_eq!(state.by_name.get(&Name("A")).unwrap().log, vec!["ping"]);
}
