#![deny(warnings)]

use rca::{Effect, ReduceContext, Reducer};

struct Child<T>(core::marker::PhantomData<T>);

struct Payload<T, const N: usize>(core::marker::PhantomData<T>);
trait PayloadType {
    type Value;
}
impl<T, const N: usize> PayloadType for Payload<T, N> {
    type Value = T;
}

impl<T> Reducer for Child<T> {
    type State = T;
    type Action = Vec<u8>;
    fn reduce(&mut self, _: &mut T, _: &Vec<u8>, _: &mut ReduceContext<'_>) -> Effect<Vec<u8>> {
        Effect::none()
    }
}

#[rca_macros::reducer(
    state(generated(struct State<T> {
        #[reducer(child = child)]
        value: <Payload<_, { let count: _ = 2usize; count }> as PayloadType>::Value,
    })),
    action(supplied(Vec<u8>)),
    body(supplied(body)),
    child(child, reducer = Child<T>, kind = required,
        state = StatePaths::<T>::value(),
        action = rca::ActionPath::new(|action: &Vec<u8>| Some(action), |action| action),
        r#bind(fn bind_child<T>(child: Child<T>) where T: Send + 'static),
    ),
)]
struct Feature<T, R> {
    body: R,
    marker: core::marker::PhantomData<T>,
}

struct MoveOnly(Vec<u8>);
fn main() {
    let feature = Feature {
        body: bind_child(Child::<MoveOnly>(core::marker::PhantomData)),
        marker: core::marker::PhantomData,
    };
    let _: &dyn Reducer<State = State<MoveOnly>, Action = Vec<u8>> = &feature;
    let mut state = State {
        value: MoveOnly(vec![1, 2]),
    };
    StatePaths::<MoveOnly>::value()
        .get_mut(&mut state)
        .0
        .push(3);
    assert_eq!(state.value.0, [1, 2, 3]);
}
