//! Checks that both facade names expose macros and the traits their code implements.

macro_rules! check_facade {
    ($facade:path) => {
        use api::{DependencyKey as _, Snapshot as _};
        use $facade as api;

        #[derive(api::DependencyKey)]
        #[dependency(value = u8, live = 3)]
        struct Seed;

        #[derive(api::Snapshot, api::StatePaths)]
        struct State {
            count: u8,
        }

        #[derive(api::ActionPaths)]
        enum Action {
            Add(u8),
        }

        #[api::reducer(state(supplied(State)), action(supplied(Action)), body(supplied(body)))]
        struct Counter<R> {
            body: R,
        }

        #[test]
        fn generated_code_uses_its_facade_types() {
            let state = State {
                count: Seed::live_value(),
            };
            assert_eq!(*StatePaths::count().get(&state), 3);
            assert_eq!(state.snapshot().count, 3);
            assert!(matches!(ActionPaths::add().embed(7), Action::Add(7)));

            let body = api::Reduce::new(|state: &mut State, action: &Action, _| {
                let Action::Add(amount) = action;
                state.count += amount;
                api::Effect::none()
            });
            let _: &dyn api::Reducer<State = State, Action = Action> =
                &api::reducers![Counter { body }];
        }
    };
}

mod short {
    check_facade!(rca);
}

mod long {
    check_facade!(crate);
}
