use rca_core::{ActionPath, CaseStatePath, Effect, IfCase, ReduceContext, Reducer};

struct DetailState;
struct SettingsState;

enum State {
    Detail(DetailState),
    Settings(SettingsState),
}

struct DetailAction;
struct SettingsAction;

enum Action {
    Detail(DetailAction),
    Settings(SettingsAction),
}

struct Parent;

impl Reducer for Parent {
    type State = State;
    type Action = Action;

    fn reduce(
        &mut self,
        _state: &mut Self::State,
        _action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        Effect::none()
    }
}

struct Settings;

impl Reducer for Settings {
    type State = SettingsState;
    type Action = SettingsAction;

    fn reduce(
        &mut self,
        _state: &mut Self::State,
        _action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        Effect::none()
    }
}

fn main() {
    let settings_state = CaseStatePath::new(
        |state: &State| match state {
            State::Settings(settings) => Some(settings),
            State::Detail(_) => None,
        },
        |state: &mut State| match state {
            State::Settings(settings) => Some(settings),
            State::Detail(_) => None,
        },
    );
    let detail_action = ActionPath::new(
        |action: &Action| match action {
            Action::Detail(detail) => Some(detail),
            Action::Settings(_) => None,
        },
        Action::Detail,
    );

    let _ = IfCase::new(Parent, settings_state, detail_action, Settings);
}
