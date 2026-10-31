#![deny(warnings)]
#![allow(
    non_camel_case_types,
    reason = "raw lower-case variant names deliberately exercise generated identifier hygiene"
)]

use rca::{Effect, ReduceContext, Reducer};
use std::{cell::Cell, rc::Rc};

mod nested {
    use super::*;

    #[rca_macros::reducer(
        state(generated(pub struct State<'a, T, const N: usize> where T: 'a {
            pub r#type: &'a [T; N],
            #[cfg(any())] pub missing: MissingState,
        })),
        action(generated(pub enum Action<'a, T> where T: 'a {
            r#gen(&'a T),
            #[cfg_attr(all(), cfg(any()))] Missing(MissingAction),
        })),
        body(supplied(r#type)),
    )]
    pub struct Feature<'a, T: 'a, R, const N: usize = 2> {
        r#type: R,
        marker: core::marker::PhantomData<(&'a T, [(); N])>,
    }

    pub struct Body<'a, T, const N: usize>(core::marker::PhantomData<(&'a T, [(); N])>);
    impl<'a, T, const N: usize> Reducer for Body<'a, T, N> {
        type State = State<'a, T, N>;
        type Action = Action<'a, T>;
        fn reduce(
            &mut self,
            _: &mut Self::State,
            _: &Self::Action,
            _: &mut ReduceContext<'_>,
        ) -> Effect<Self::Action> {
            Effect::none()
        }
    }

    pub fn verify<'a, T>(values: &'a [T; 2]) {
        let feature = Feature {
            r#type: Body::<T, 2>(core::marker::PhantomData),
            marker: core::marker::PhantomData,
        };
        let _: &dyn Reducer<State = State<'a, T, 2>, Action = Action<'a, T>> = &feature;
        let state = State { r#type: values };
        assert!(std::ptr::eq(
            *StatePaths::<T, 2>::r#type().get(&state),
            values
        ));
        let action = ActionPaths::<T>::r#gen().embed(&values[0]);
        assert!(std::ptr::eq(
            *ActionPaths::<T>::r#gen().extract(&action).unwrap(),
            &values[0]
        ));
    }
}

fn main() {
    let values = [Rc::new(Cell::new(1)), Rc::new(Cell::new(2))];
    nested::verify(&values);
}
