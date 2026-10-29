//! Checks façade exports under their actual feature gates.
//!
//! Constructor witnesses check types and paths. They do not establish actor execution.

#[test]
#[cfg(feature = "diagnostics")]
fn umbrella_exposes_typed_diagnostics() {
    use rca::ReducerExt as _;
    let reducer: rca::diagnostics::TraceChanges<_, _, _> = CaseChild.trace_changes(
        |state| *state,
        |change: rca::diagnostics::Change<'_, u8, u8>| {
            let _ = change.format();
        },
    );
    assert!(format!("{reducer:?}").contains("TraceChanges"));
    assert!(
        rca::diagnostics::format_snapshot_mismatch("hidden", "hidden").contains("semantically")
    );
}

/// Supplies a case-shaped child without additional state bounds.
struct CaseRootState {
    child: Option<u8>,
}

enum CaseRootAction {
    Child(u8),
    Parent,
}

/// No-op parent used only to check case-composition constructor types.
struct CaseRoot;

impl rca::Reducer for CaseRoot {
    type State = CaseRootState;
    type Action = CaseRootAction;

    fn reduce(
        &mut self,
        _state: &mut Self::State,
        _action: &Self::Action,
        _context: &mut rca::ReduceContext<'_>,
    ) -> rca::Effect<Self::Action> {
        rca::Effect::none()
    }
}

/// No-op child used by case-construction and diagnostic-wrapper witnesses.
struct CaseChild;

impl rca::Reducer for CaseChild {
    type State = u8;
    type Action = u8;

    fn reduce(
        &mut self,
        _state: &mut Self::State,
        _action: &Self::Action,
        _context: &mut rca::ReduceContext<'_>,
    ) -> rca::Effect<Self::Action> {
        rca::Effect::none()
    }
}

/// Supplies a keyed collection to each public composition constructor.
struct KeyedRootState {
    rows: rca::KeyedState<u8, u8>,
}

enum KeyedRootAction {
    Row(rca::KeyedAction<u8, u8>),
    Parent,
}

/// No-op parent whose type identifies the keyed composition result.
struct KeyedRoot;

impl rca::Reducer for KeyedRoot {
    type State = KeyedRootState;
    type Action = KeyedRootAction;

    fn reduce(
        &mut self,
        _state: &mut Self::State,
        _action: &Self::Action,
        _context: &mut rca::ReduceContext<'_>,
    ) -> rca::Effect<Self::Action> {
        rca::Effect::none()
    }
}

/// No-op child used only by keyed-composition constructor witnesses.
struct KeyedChild;

impl rca::Reducer for KeyedChild {
    type State = u8;
    type Action = u8;

    fn reduce(
        &mut self,
        _state: &mut Self::State,
        _action: &Self::Action,
        _context: &mut rca::ReduceContext<'_>,
    ) -> rca::Effect<Self::Action> {
        rca::Effect::none()
    }
}

#[test]
fn umbrella_exports_composition_paths() {
    struct State {
        required: u8,
        optional: Option<u8>,
        case: Option<u8>,
    }

    enum Action {
        Child(u8),
        Parent,
    }

    let required = rca::StatePath::new(
        |state: &State| &state.required,
        |state: &mut State| &mut state.required,
    );
    let optional = rca::OptionalStatePath::new(
        |state: &State| state.optional.as_ref(),
        |state: &mut State| state.optional.as_mut(),
    );
    let case = rca::CaseStatePath::new(
        |state: &State| state.case.as_ref(),
        |state: &mut State| state.case.as_mut(),
    );
    let action = rca::ActionPath::new(
        |action: &Action| match action {
            Action::Child(child) => Some(child),
            Action::Parent => None,
        },
        Action::Child,
    );
    let state = State {
        required: 1,
        optional: None,
        case: Some(2),
    };

    assert_eq!(*required.get(&state), 1);
    assert!(optional.get(&state).is_none());
    assert_eq!(case.get(&state), Some(&2));
    assert!(matches!(action.embed(3), Action::Child(3)));
    assert!(action.extract(&Action::Parent).is_none());

    let _: rca::Combine<(_,)> = rca::combine(((),));
}

#[test]
fn umbrella_exports_keyed_state_and_duplicate_error() {
    let mut rows = rca::KeyedState::new();
    rows.insert(1_u8, String::from("one"));
    let duplicate: rca::DuplicateId<u8, String> = rows
        .try_insert(1, String::from("duplicate"))
        .expect_err("duplicate ID is rejected");

    assert_eq!(duplicate.into_parts(), (1, String::from("duplicate")));
}

#[test]
fn umbrella_exports_keyed_action_and_paths() {
    struct State {
        rows: rca::KeyedState<u8, String>,
    }

    enum Action {
        Row(rca::KeyedAction<u8, String>),
        Parent,
    }

    let state_path = rca::KeyedStatePath::new(
        |state: &State| &state.rows,
        |state: &mut State| &mut state.rows,
    );
    let action_path = rca::KeyedActionPath::new(
        |action: &Action| match action {
            Action::Row(action) => Some(action),
            Action::Parent => None,
        },
        Action::Row,
    );
    let mut state = State {
        rows: [(1, String::from("one"))].into_iter().collect(),
    };

    state_path
        .get_mut(&mut state)
        .get_mut(&1)
        .expect("row 1 exists")
        .push('!');
    assert_eq!(
        state_path.get(&state).get(&1).map(String::as_str),
        Some("one!")
    );

    let action = action_path.embed(1, String::from("tap"));
    let (id, child) = action_path.extract(&action).expect("row action matches");
    assert_eq!((*id, child.as_str()), (1, "tap"));
    assert!(action_path.extract(&Action::Parent).is_none());
}

#[test]
fn umbrella_exports_keyed_lifecycle_composition() {
    use rca::ReducerExt as _;

    fn assert_reducer<R>(_reducer: R)
    where
        R: rca::Reducer<State = KeyedRootState, Action = KeyedRootAction>,
    {
    }

    assert_reducer(rca::for_each(
        KeyedRoot,
        rca::KeyedStatePath::new(
            |state: &KeyedRootState| &state.rows,
            |state: &mut KeyedRootState| &mut state.rows,
        ),
        rca::KeyedActionPath::new(
            |action: &KeyedRootAction| match action {
                KeyedRootAction::Row(action) => Some(action),
                KeyedRootAction::Parent => None,
            },
            KeyedRootAction::Row,
        ),
        KeyedChild,
    ));
    assert_reducer(rca::ForEach::new(
        KeyedRoot,
        rca::KeyedStatePath::new(
            |state: &KeyedRootState| &state.rows,
            |state: &mut KeyedRootState| &mut state.rows,
        ),
        rca::KeyedActionPath::new(
            |action: &KeyedRootAction| match action {
                KeyedRootAction::Row(action) => Some(action),
                KeyedRootAction::Parent => None,
            },
            KeyedRootAction::Row,
        ),
        KeyedChild,
    ));
    assert_reducer(KeyedRoot.for_each(
        rca::KeyedStatePath::new(
            |state: &KeyedRootState| &state.rows,
            |state: &mut KeyedRootState| &mut state.rows,
        ),
        rca::KeyedActionPath::new(
            |action: &KeyedRootAction| match action {
                KeyedRootAction::Row(action) => Some(action),
                KeyedRootAction::Parent => None,
            },
            KeyedRootAction::Row,
        ),
        KeyedChild,
    ));

    let _ = KeyedRootAction::Parent;
}

#[test]
fn umbrella_exports_case_lifecycle_composition() {
    let state = rca::CaseStatePath::new(
        |state: &CaseRootState| state.child.as_ref(),
        |state: &mut CaseRootState| state.child.as_mut(),
    );
    let action = rca::ActionPath::new(
        |action: &CaseRootAction| match action {
            CaseRootAction::Child(child) => Some(child),
            CaseRootAction::Parent => None,
        },
        CaseRootAction::Child,
    );
    let reducer = rca::if_case(CaseRoot, state, action, CaseChild);
    let _: &dyn rca::Reducer<State = CaseRootState, Action = CaseRootAction> = &reducer;

    let _ = CaseRootAction::Parent;
}

#[cfg(feature = "macros")]
/// Checks that façade trait names and derive names coexist in their Rust namespaces.
mod macros {
    use rca::{ActionPaths, DependencyClient, DependencyKey, StatePaths, reducers};

    #[derive(DependencyClient)]
    struct Client {
        #[dependency(test = std::sync::Arc::new(|| 11))]
        read: std::sync::Arc<dyn Fn() -> u8 + Send + Sync>,
    }

    #[derive(DependencyKey)]
    #[dependency(value = u8, live = 7)]
    struct CountKey;

    #[derive(StatePaths)]
    struct MacroState {
        child: u8,
    }

    #[derive(ActionPaths)]
    enum MacroAction {
        Child(u8),
    }

    #[test]
    fn umbrella_exports_trait_and_derive_together() {
        assert_eq!((Client::test_value().read)(), 11);
        assert_eq!(CountKey::live_value(), 7);
        let _ = MacroStatePaths::child();
        let path = MacroActionPaths::child();
        assert!(matches!(path.embed(7), MacroAction::Child(7)));
        let _: rca::Combine<(_,)> = reducers![()];
    }
}

#[cfg(feature = "test-util")]
#[test]
fn umbrella_exports_deterministic_test_api() {
    fn accepts_runtime(_: &rca::DeterministicRuntime) {}

    let _: usize = rca::DEFAULT_STEP_BUDGET;
    accepts_runtime(&rca::DeterministicRuntime::new());
}

#[cfg(feature = "tokio")]
#[test]
fn umbrella_exports_tokio_adapter_and_error() {
    let error: rca::TokioRuntimeError =
        rca::TokioRuntime::current().expect_err("no active Tokio runtime");
    assert!(error.is_runtime_missing());
}
