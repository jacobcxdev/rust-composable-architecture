use composable::*;

#[test]
/// Composite struct reducers:
/// - Parent `RecursiveReducer::reduce` runs *before* child reducers.
/// - Parent runs for *every* action (including effect-driven follow-ups).
/// - Child effects are scoped back through the parent action type.
fn composite_struct_routes_child_actions_and_effects_back_to_same_child() {
    #[derive(Clone, Debug, PartialEq)]
    struct ChildState {
        log: Vec<&'static str>,
    }

    #[derive(Clone, Debug, PartialEq)]
    enum ChildAction {
        Ping,
        Pong,
    }

    impl Reducer for ChildState {
        type Action = ChildAction;
        type Output = Self;

        fn reduce(&mut self, action: ChildAction, send: impl Effects<ChildAction>) {
            use ChildAction::*;

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

    #[derive(Clone, Debug, PartialEq, RecursiveReducer)]
    struct State {
        child: ChildState,

        #[reducer(skip)]
        ignored: Vec<u32>,
    }

    #[derive(Clone, Debug, PartialEq, From, TryInto)]
    enum Action {
        Other,
        Child(ChildAction),
    }

    impl RecursiveReducer for State {
        type Action = Action;

        fn reduce(&mut self, _action: Action, _send: impl Effects<Action>) {
            // Parent runs first (pre-order traversal).
            // This should happen for the initial action *and* for any action
            // produced as an effect.
            self.child.log.push("parent");
        }
    }

    let mut store = TestStore::with_initial(State {
        child: ChildState { log: vec![] },
        ignored: vec![1, 2, 3],
    });

    store.send(ChildAction::Ping.into(), |state| {
        state.child.log = vec!["parent", "ping"];
    });

    store.recv(ChildAction::Pong.into(), |state| {
        state.child.log = vec!["parent", "ping", "parent", "pong"];
    });

    store.send(Action::Other, |state| {
        state.child.log = vec!["parent", "ping", "parent", "pong", "parent"];
    });
}

#[test]
/// Composite struct reducers:
/// - Only the targeted child reducer should run.
/// - Effects produced by a child should route back to that same child.
/// - Sibling state should not be touched.
fn composite_struct_does_not_route_actions_or_effects_to_the_wrong_child() {
    #[derive(Clone, Debug, PartialEq)]
    struct AChildState {
        log: Vec<&'static str>,
    }

    #[derive(Clone, Debug, PartialEq)]
    enum AChildAction {
        Ping,
        Pong,
    }

    impl Reducer for AChildState {
        type Action = AChildAction;
        type Output = Self;

        fn reduce(&mut self, action: AChildAction, send: impl Effects<AChildAction>) {
            use AChildAction::*;

            match action {
                Ping => {
                    self.log.push("a:ping");
                    send.action(Pong);
                }
                Pong => {
                    self.log.push("a:pong");
                }
            }
        }
    }

    #[derive(Clone, Debug, PartialEq)]
    struct BChildState {
        log: Vec<&'static str>,
    }

    #[derive(Clone, Debug, PartialEq)]
    enum BChildAction {
        Ping,
        Pong,
    }

    impl Reducer for BChildState {
        type Action = BChildAction;
        type Output = Self;

        fn reduce(&mut self, action: BChildAction, send: impl Effects<BChildAction>) {
            use BChildAction::*;

            match action {
                Ping => {
                    self.log.push("b:ping");
                    send.action(Pong);
                }
                Pong => {
                    self.log.push("b:pong");
                }
            }
        }
    }

    #[derive(Clone, Debug, PartialEq, RecursiveReducer)]
    struct State {
        a: AChildState,
        b: BChildState,

        #[reducer(skip)]
        parent_log: Vec<&'static str>,
    }

    #[derive(Clone, Debug, PartialEq, From, TryInto)]
    enum Action {
        A(AChildAction),
        B(BChildAction),
    }

    impl RecursiveReducer for State {
        type Action = Action;

        fn reduce(&mut self, _action: Action, _send: impl Effects<Action>) {
            self.parent_log.push("parent");
        }
    }

    let mut store = TestStore::with_initial(State {
        a: AChildState { log: vec![] },
        b: BChildState { log: vec![] },
        parent_log: vec![],
    });

    store.send(Action::A(AChildAction::Ping), |state| {
        state.a.log = vec!["a:ping"];
        state.b.log = vec![];
        state.parent_log = vec!["parent"];
    });

    store.recv(Action::A(AChildAction::Pong), |state| {
        state.a.log = vec!["a:ping", "a:pong"];
        state.b.log = vec![];
        state.parent_log = vec!["parent", "parent"];
    });

    store.send(Action::B(BChildAction::Ping), |state| {
        state.a.log = vec!["a:ping", "a:pong"];
        state.b.log = vec!["b:ping"];
        state.parent_log = vec!["parent", "parent", "parent"];
    });

    store.recv(Action::B(BChildAction::Pong), |state| {
        state.a.log = vec!["a:ping", "a:pong"];
        state.b.log = vec!["b:ping", "b:pong"];
        state.parent_log = vec!["parent", "parent", "parent", "parent"];
    });
}

#[test]
/// Alternate enum reducers:
/// - Only the active variant’s child reducer should receive routed actions.
/// - The parent `RecursiveReducer::reduce` still runs regardless of which action is sent.
fn alternate_enum_only_routes_actions_to_the_active_variant_but_parent_still_runs() {
    #[derive(Clone, Debug, PartialEq)]
    struct AState {
        log: Vec<&'static str>,
    }

    #[derive(Clone, Debug, PartialEq)]
    enum AAction {
        Ping,
        Pong,
    }

    impl Reducer for AState {
        type Action = AAction;
        type Output = Self;

        fn reduce(&mut self, action: AAction, send: impl Effects<AAction>) {
            use AAction::*;

            match action {
                Ping => {
                    self.log.push("a:ping");
                    send.action(Pong);
                }
                Pong => {
                    self.log.push("a:pong");
                }
            }
        }
    }

    #[derive(Clone, Debug, PartialEq)]
    struct BState {
        log: Vec<&'static str>,
    }

    #[derive(Clone, Debug, PartialEq)]
    enum BAction {
        Ping,
    }

    impl Reducer for BState {
        type Action = BAction;
        type Output = Self;

        fn reduce(&mut self, action: BAction, _send: impl Effects<BAction>) {
            match action {
                BAction::Ping => self.log.push("b:ping"),
            }
        }
    }

    #[derive(Clone, Debug, PartialEq, RecursiveReducer)]
    enum State {
        A(AState),
        B(BState),
    }

    #[derive(Clone, Debug, PartialEq, From, TryInto)]
    enum Action {
        A(AAction),
        B(BAction),
    }

    impl RecursiveReducer for State {
        type Action = Action;

        fn reduce(&mut self, _action: Action, _send: impl Effects<Action>) {
            match self {
                State::A(state) => state.log.push("parent"),
                State::B(state) => state.log.push("parent"),
            }
        }
    }

    let mut store = TestStore::with_initial(State::A(AState { log: vec![] }));

    store.send(AAction::Ping.into(), |state| {
        let State::A(state) = state else {
            panic!("expected State::A")
        };
        state.log = vec!["parent", "a:ping"];
    });

    store.recv(AAction::Pong.into(), |state| {
        let State::A(state) = state else {
            panic!("expected State::A")
        };
        state.log = vec!["parent", "a:ping", "parent", "a:pong"];
    });

    // Not for the active variant: the child reducer should not run, but the parent still runs.
    store.send(BAction::Ping.into(), |state| {
        // `BAction::Ping` cannot be routed into `AAction`, so the derived child routing does nothing,
        // but the parent `reduce` is still invoked for the action.
        let State::A(state) = state else {
            panic!("expected State::A")
        };
        state.log = vec!["parent", "a:ping", "parent", "a:pong", "parent"];
    });

    // Also validate the other variant works when it is active.
    let mut store = TestStore::with_initial(State::B(BState { log: vec![] }));

    store.send(BAction::Ping.into(), |state| {
        let State::B(state) = state else {
            panic!("expected State::B")
        };
        state.log = vec!["parent", "b:ping"];
    });
}
