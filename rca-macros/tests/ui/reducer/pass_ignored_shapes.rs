use rca::Reducer;
use std::{cell::Cell, rc::Rc};

struct MoveOnly(Rc<Cell<usize>>);
struct MappedAction(Vec<u8>);

#[rca_macros::reducer(
    state(generated(enum State {
        #[reducer(case = idle)] Idle,
        #[reducer(case = pair)] Pair(MoveOnly, String),
        #[reducer(case = named)] Named { token: MoveOnly, count: usize },
    })),
    action(generated(enum Action {
        #[reducer(case = idle)] Idle(_),
        #[reducer(case = pair)] Pair(_),
        #[reducer(case = named)] Named(_),
    })),
    body(generated),
    case(idle, policy = ignored, action = empty),
    case(pair, policy = data, action = empty),
    case(named, policy = ignored, action = supplied(MappedAction)),
)]
struct Feature {}

fn main() {
    let _: fn(core::convert::Infallible) -> Action = Action::Idle;
    let _: fn(core::convert::Infallible) -> Action = Action::Pair;
    let _: &dyn Reducer<State = State, Action = Action> = &Feature {};
    assert_eq!(StatePaths::idle().get(&State::Idle), Some(()));
    let cell = Rc::new(Cell::new(3));
    let mut state = State::Pair(MoveOnly(Rc::clone(&cell)), String::from("before"));
    let path = StatePaths::pair();
    let (token, text) = path.get(&state).unwrap();
    let State::Pair(direct, _) = &state else {
        panic!("pair retained")
    };
    assert!(std::ptr::eq(token, direct));
    assert_eq!(text, "before");
    let (_, text) = path.get_mut(&mut state).unwrap();
    text.push_str(" after");
    assert_eq!(path.get(&state).unwrap().1, "before after");
    let named = StatePaths::named().embed((MoveOnly(cell), 9));
    let (token, count) = StatePaths::named().get(&named).unwrap();
    assert_eq!(token.0.get(), 3);
    assert_eq!(*count, 9);
    let action = ActionPaths::named().embed(MappedAction(vec![4, 5]));
    let payload = ActionPaths::named().extract(&action).unwrap();
    let Action::Named(direct) = &action else {
        panic!("named retained")
    };
    assert!(std::ptr::eq(payload, direct));
    assert_eq!(payload.0, [4, 5]);
    let impossible = |action: Action| match action {
        Action::Idle(never) | Action::Pair(never) => match never {},
        Action::Named(_) => (),
    };
    impossible(action);
}
