#![deny(warnings)]

mod public {
    #[expect(
        non_upper_case_globals,
        reason = "the value name tests generated type aliases"
    )]
    const __RcaStatePathsState: usize = 2;
    #[expect(
        non_upper_case_globals,
        reason = "the value name tests generated type aliases"
    )]
    const __RcaActionPathsAction: usize = 2;

    #[rca_macros::reducer(
        state(generated(pub struct State<State> {
            #[cfg(all())]
            pub value: State,
            #[cfg(all())]
            pub other: [State; __RcaStatePathsState],
        })),
        action(generated(pub enum Action<Action> {
            #[cfg(all())]
            Value(Action),
            #[cfg(all())]
            Pair(Action, [Action; __RcaActionPathsAction]),
        })),
        body(generated),
    )]
    pub struct Feature<State, Action> {
        #[reducer(data)]
        marker: ::core::marker::PhantomData<(State, Action)>,
    }

    pub fn feature() -> Feature<u8, u16> {
        Feature {
            marker: ::core::marker::PhantomData,
        }
    }
}

mod binding {
    use rca::{Effect, ReduceContext, Reducer};

    pub struct Child;

    impl Reducer for Child {
        type State = u8;
        type Action = u16;

        fn reduce(&mut self, _: &mut u8, _: &u16, _: &mut ReduceContext<'_>) -> Effect<u16> {
            Effect::none()
        }
    }

    #[rca_macros::reducer(
        state(generated(pub struct State {
            #[reducer(child = child)]
            pub value: _,
        })),
        action(generated(pub enum Action {
            #[reducer(child = child)]
            Value(_),
        })),
        body(supplied(body)),
        child(child, reducer = Child, kind = required,
            state = StatePaths::value(),
            action = ActionPaths::value(),
            bind(pub fn bind_child<State, Action>(child: Child)),
        ),
    )]
    pub struct Feature<R> {
        pub body: R,
    }
}

fn main() {
    let feature = binding::Feature {
        body: binding::bind_child::<u8, u16>(binding::Child),
    };
    let _: &dyn rca::Reducer<State = binding::State, Action = binding::Action> = &feature;
    let feature = public::feature();
    let _: &dyn rca::Reducer<State = public::State<u8>, Action = public::Action<u16>> = &feature;
    let mut state = public::State {
        value: 3,
        other: [5, 6],
    };
    *public::StatePaths::value().get_mut(&mut state) = 7;
    assert_eq!(public::StatePaths::value().get(&state), &7);
    assert_eq!(public::StatePaths::other().get(&state), &[5, 6]);
    let value = public::ActionPaths::value().embed(11);
    assert_eq!(public::ActionPaths::value().extract(&value), Some(&11));
    let pair = public::ActionPaths::pair().embed((13, [17, 19]));
    assert_eq!(
        public::ActionPaths::pair().extract(&pair),
        Some((&13, &[17, 19]))
    );
}
