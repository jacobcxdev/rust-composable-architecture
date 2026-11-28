use rca_core::{
    Effect, KeyedAction, KeyedActionPath, KeyedState, ReduceContext, Reducer, SharedReducer,
    for_each_shared,
};
struct Child;
impl SharedReducer for Child {
    type State = u64;
    type Shared = KeyedState<u8, u64>;
    type Action = ();
    fn reduce(
        &mut self,
        _: &mut u64,
        _: &mut Self::Shared,
        _: &(),
        _: &mut ReduceContext<'_>,
    ) -> Effect<()> {
        Effect::none()
    }
}
struct Parent;
impl Reducer for Parent {
    type State = KeyedState<u8, u64>;
    type Action = KeyedAction<u8, ()>;
    fn reduce(
        &mut self,
        _: &mut Self::State,
        _: &Self::Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        Effect::none()
    }
}
fn main() {
    let _ = for_each_shared(
        Parent,
        |s: &KeyedState<u8, u64>| s,
        |s: &mut KeyedState<u8, u64>| (s, s),
        KeyedActionPath::new(|a: &KeyedAction<u8, ()>| Some(a), |a| a),
        Child,
    );
}
