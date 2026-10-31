use rca::{Effect, ReduceContext, Reducer};

#[rca_macros::reducer(state(r#empty), action(r#empty), body(generated))]
struct Empty {}

pub struct MoveOnly(Vec<u8>);

#[rca_macros::reducer(state(empty), action(supplied(MoveOnly)), body(generated))]
struct Noop {}

mod generated_state {
    use super::*;

    #[rca_macros::reducer(
        state(generated(pub struct State { pub count: usize })),
        action(supplied(MoveOnly)), body(supplied(body)),
    )]
    pub struct Feature<R> {
        pub body: R,
    }

    pub struct Body;
    impl Reducer for Body {
        type State = State;
        type Action = MoveOnly;
        fn reduce(
            &mut self,
            state: &mut State,
            action: &MoveOnly,
            _: &mut ReduceContext<'_>,
        ) -> Effect<MoveOnly> {
            state.count += action.0.len();
            Effect::none()
        }
    }
}

mod generated_action {
    use super::*;

    #[rca_macros::reducer(
        state(supplied(Vec<usize>)),
        action(generated(pub enum Action { Push(usize) })), body(supplied(body)),
    )]
    pub struct Feature<R> {
        pub body: R,
    }

    pub struct Body;
    impl Reducer for Body {
        type State = Vec<usize>;
        type Action = Action;
        fn reduce(
            &mut self,
            state: &mut Self::State,
            action: &Action,
            _: &mut ReduceContext<'_>,
        ) -> Effect<Action> {
            let Action::Push(value) = action;
            state.push(*value);
            Effect::none()
        }
    }
}

fn exact<R: Reducer<State = S, Action = A>, S, A>(_: R) {}

fn main() {
    exact::<_, (), core::convert::Infallible>(Empty {});
    exact::<_, (), MoveOnly>(Noop {});
    exact::<_, generated_state::State, MoveOnly>(generated_state::Feature {
        body: generated_state::Body,
    });
    exact::<_, Vec<usize>, generated_action::Action>(generated_action::Feature {
        body: generated_action::Body,
    });
    let mut state = generated_state::State { count: 3 };
    *generated_state::StatePaths::count().get_mut(&mut state) = 8;
    assert_eq!(state.count, 8);
    let action = generated_action::ActionPaths::push().embed(7);
    assert_eq!(
        generated_action::ActionPaths::push().extract(&action),
        Some(&7)
    );
}
