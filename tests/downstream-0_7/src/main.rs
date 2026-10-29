use composable::*;

#[derive(Default)]
struct State(usize);

#[derive(Clone, Debug)]
enum Action {
    Inc,
}

impl Reducer for State {
    type Action = Action;
    type Output = usize;

    fn reduce(&mut self, action: Action, _send: impl Effects<Action>) {
        match action {
            Action::Inc => self.0 += 1,
        }
    }
}

impl From<State> for usize {
    fn from(value: State) -> Self {
        value.0
    }
}

fn main() {
    let store = Store::with_initial(State::default());
    store.send(Action::Inc);
    let out = store.into_inner();
    assert_eq!(out, 1);
}
