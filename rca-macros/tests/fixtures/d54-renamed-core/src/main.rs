use ::architecture::{Effect, ReduceContext, Reducer};

mod architecture {}
mod core {}
struct Option;
struct Result;
struct Some;
struct None;

struct Child;
impl Reducer for Child {
    type State = usize;
    type Action = Vec<u8>;
    fn reduce(
        &mut self,
        state: &mut usize,
        action: &Vec<u8>,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Vec<u8>> {
        *state += action.len();
        Effect::none()
    }
}

#[rca_macros::reducer(
    state(generated(struct State { #[reducer(child = child)] value: _ })),
    action(generated(enum Action { #[reducer(child = child)] Value(_) })),
    body(generated),
    child(child, reducer = Child, kind = required,
        state = StatePaths::value(), action = ActionPaths::value(),
        bind(fn bind_child(child: Child)),
    ),
)]
struct Feature<C> {
    #[reducer(child = child)]
    child: C,
}

fn main() {
    let _ = (Option, Result, Some, None);
    let feature = Feature {
        child: bind_child(Child),
    };
    let _: &dyn Reducer<State = State, Action = Action> = &feature;
    let mut state = State { value: 3 };
    *StatePaths::value().get_mut(&mut state) = 8;
    assert_eq!(state.value, 8);
    let action = ActionPaths::value().embed(vec![2, 3]);
    let Action::Value(direct) = &action;
    assert!(::core::ptr::eq(
        ActionPaths::value().extract(&action).unwrap(),
        direct
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_reducer_routes_to_child() {
        main();
        ::futures_executor::block_on(async {
            let feature = Feature {
                child: bind_child(Child),
            };
            let mut store =
                ::rca_test::TestStore::projected(State { value: 3 }, feature, |state| state.value);
            let task = store
                .send_with(ActionPaths::value().embed(vec![2, 3]), |value| *value = 5)
                .await
                .unwrap();
            task.finish().await.unwrap();
            let empty = store
                .send(ActionPaths::value().embed(Vec::new()))
                .await
                .unwrap();
            empty.finish().await.unwrap();
            store.finish().await.unwrap();
        });
    }
}
