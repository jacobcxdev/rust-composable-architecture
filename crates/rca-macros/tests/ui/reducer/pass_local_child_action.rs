use rca::{Effect, ReduceContext, Reducer};
use std::{cell::Cell, rc::Rc};

struct LocalAction<'a> {
    amount: &'a usize,
    token: Rc<()>,
}
struct Child<'a>(&'a Cell<usize>);
impl<'a> Reducer for Child<'a> {
    type State = usize;
    type Action = LocalAction<'a>;
    fn reduce(
        &mut self,
        state: &mut usize,
        action: &LocalAction<'a>,
        _: &mut ReduceContext<'_>,
    ) -> Effect<LocalAction<'a>> {
        self.0.set(self.0.get() + 1);
        *state += *action.amount;
        assert_eq!(Rc::strong_count(&action.token), 1);
        Effect::none()
    }
}

#[rca_macros::reducer(
    state(supplied(usize)),
    action(generated(enum Action<'a> { #[reducer(child = local)] Local(_) })),
    body(supplied(local)),
    child(local, reducer = Child<'a>, kind = required,
        state = rca::StatePath::new(|state: &usize| state, |state: &mut usize| state),
        action = ActionPaths::local(),
        bind(fn bind_local<'a>(child: Child<'a>)),
    ),
)]
struct Feature<'a, C> {
    local: C,
    marker: core::marker::PhantomData<&'a ()>,
}

fn main() {
    let calls = Cell::new(0);
    let amount = 3;
    let feature = Feature {
        local: bind_local(Child(&calls)),
        marker: core::marker::PhantomData,
    };
    let _: &dyn Reducer<State = usize, Action = Action<'_>> = &feature;
    let action = ActionPaths::local().embed(LocalAction {
        amount: &amount,
        token: Rc::new(()),
    });
    let borrowed = ActionPaths::local().extract(&action).unwrap();
    assert!(core::ptr::eq(borrowed.amount, &amount));
}
