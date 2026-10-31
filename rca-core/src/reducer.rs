//! Defines synchronous feature reduction.

use std::{fmt, marker::PhantomData};

use crate::{Effect, ReduceContext};

/// Mutates feature state and returns follow-up work.
///
/// Start with `Reducer` when actions describe the inputs and results of your
/// feature. Returned [`Effect`] values can perform asynchronous work and send
/// more actions. A network request alone does not require a direct
/// [`crate::Feature`] implementation. [`Reduce::new`] wraps a small closure
/// without a named trait implementation.
///
/// Use [`crate::Feature::update`] directly when a typed [`crate::Task`] needs
/// to read or update current state between asynchronous steps. Both callbacks
/// are synchronous: neither can retain borrowed state across an await.
///
/// [`crate::combine`], [`crate::scope`] and [`crate::for_each`] require reducers.
/// They route borrowed state and actions and map returned effects. Installed
/// children instead accept either reducers or direct Features. The choice of
/// trait does not decide whether a child can have its own lifetime.
///
/// Synchronous reduction may use borrowed or non-`Send` values. Actor-backed
/// [`crate::Store`] construction requires the reducer, state and action to be
/// `Send + 'static`; asynchronous effects retain their owned capture bounds.
/// Local leaf action borrowing does not imply that every composition accepts
/// the same domain: required [`crate::Scope`] keeps borrowed State support but
/// requires parent and child Action types to be `'static` for total effect
/// mapping. Owned non-`Send` actions remain valid there; arbitrary-lifetime
/// borrowed actions do not, even when the child returns no effects.
///
/// Every reducer also implements [`crate::Feature`] without an adapter.
/// Both forms use the same Store and `TestStore` constructors. Implement one
/// trait explicitly per type, not both: the automatic implementation would conflict.
pub trait Reducer {
    /// State exclusively borrowed during one reduction.
    type State;

    /// Action borrowed during one reduction.
    type Action;

    /// Reduces one action synchronously.
    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action>;
}

/// Adapts a persistent mutable closure to synchronous feature reduction.
///
/// Stores the concrete callback without allocation or erasure. State, action and
/// captures may be borrowed or non-`Send` for local reduction; actor-backed
/// [`crate::Store`] construction retains its transport bounds. The callback
/// receives the original borrowed action and context and returns its effect
/// unchanged. The framework supplies the context; this adapter adds no
/// standalone context constructor. Only the callback determines its auto traits.
///
/// # Examples
///
/// ```
/// use rca_core::{Effect, Reduce, Reducer};
///
/// let mut calls = 0;
/// let reducer = Reduce::new(move |state: &mut usize, action: &usize, _context| {
///     calls += 1;
///     *state += action + calls;
///     Effect::none()
/// });
/// let _: &dyn Reducer<State = usize, Action = usize> = &reducer;
/// ```
///
/// Local captures cannot cross an actor boundary without `Send`:
///
/// ```compile_fail
/// use rca_core::{Effect, Reduce, Runtime, Store};
/// use std::rc::Rc;
///
/// fn actor(runtime: Runtime) {
///     let capture = Rc::new(1);
///     let reducer = Reduce::new(move |state: &mut usize, _: &(), _| {
///         *state += *capture;
///         Effect::none()
///     });
///     let _ = Store::new(0, reducer, runtime);
/// }
/// ```
///
/// Borrowed captures also retain the actor's `'static` requirement:
///
/// ```compile_fail
/// use rca_core::{Effect, Reduce, Runtime, Store};
///
/// fn actor(runtime: Runtime) {
///     let value = 1;
///     let capture = &value;
///     let reducer = Reduce::new(move |state: &mut usize, _: &(), _| {
///         *state += *capture;
///         Effect::none()
///     });
///     let _ = Store::new(0, reducer, runtime);
/// }
/// ```
pub struct Reduce<State, Action, F> {
    /// Concrete callback retained across reductions, including its mutable captures.
    reduce: F,
    /// Names borrowed input types without owning them or inheriting their auto traits.
    marker: PhantomData<fn(State, Action)>,
}

impl<State, Action, F> Reduce<State, Action, F>
where
    F: FnMut(&mut State, &Action, &mut ReduceContext<'_>) -> Effect<Action>,
{
    /// Creates a reducer retaining one concrete mutable callback.
    #[must_use]
    pub const fn new(reduce: F) -> Self {
        Self {
            reduce,
            marker: PhantomData,
        }
    }
}

impl<State, Action, F> Reducer for Reduce<State, Action, F>
where
    F: FnMut(&mut State, &Action, &mut ReduceContext<'_>) -> Effect<Action>,
{
    type State = State;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut State,
        action: &Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        (self.reduce)(state, action, context)
    }
}

impl<State, Action, F> fmt::Debug for Reduce<State, Action, F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Reduce").finish_non_exhaustive()
    }
}

#[cfg(test)]
pub(crate) trait ErasedReducer<State, Action>: Send {
    fn reduce(
        &mut self,
        state: &mut State,
        action: &Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Action>;
}

#[cfg(test)]
impl<State, Action, R> ErasedReducer<State, Action> for R
where
    R: Reducer<State = State, Action = Action> + Send,
    State: Send + 'static,
    Action: Send + 'static,
{
    fn reduce(
        &mut self,
        state: &mut State,
        action: &Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        Reducer::reduce(self, state, action, context)
    }
}

#[cfg(test)]
ordinary_test_items! {
    mod tests {
        use std::{cell::Cell, rc::Rc};

        use super::*;
        use crate::store::scope::ScopeRegistry;
        use crate::{ActionPath, DependencyValues, MonotonicInstant, StatePath};

        struct LocalState<'a> {
            value: &'a Rc<Cell<usize>>,
        }
        struct LocalAction<'a> {
            amount: &'a usize,
            identity: Rc<()>,
        }
        struct LocalReducer<'a>(&'a Cell<usize>);

        impl<'a> Reducer for LocalReducer<'a> {
            type State = LocalState<'a>;
            type Action = LocalAction<'a>;

            fn reduce(
                &mut self,
                state: &mut Self::State,
                action: &Self::Action,
                _: &mut ReduceContext<'_>,
            ) -> Effect<Self::Action> {
                self.0.set(self.0.get() + 1);
                state.value.set(state.value.get() + *action.amount);
                Effect::none()
            }
        }

        #[test]
        fn borrowed_non_send_reducer_paths_and_payloads_reduce_locally() {
            let value = Rc::new(Cell::new(2));
            let calls = Cell::new(0);
            let amount = 3;
            let identity = Rc::new(());
            let mut reducer = LocalReducer(&calls);
            let mut state = LocalState { value: &value };
            let action = LocalAction {
                amount: &amount,
                identity: Rc::clone(&identity),
            };
            let state_path = StatePath::new(
                |state: &LocalState<'_>| &state.value,
                |state: &mut LocalState<'_>| &mut state.value,
            );
            let action_path = ActionPath::new(
                |action: &LocalAction<'_>| Some(&action.identity),
                |identity| LocalAction {
                    amount: &amount,
                    identity,
                },
            );
            let mut dependencies = DependencyValues::new();
            let (_scopes_lifecycle, mut scopes) = ScopeRegistry::for_test();
            let mut context_transitions = crate::store::scope::ScopeTransitionBatch::default();
            let mut context = ReduceContext::new(
                &mut dependencies,
                MonotonicInstant::ZERO,
                &mut scopes,
                &mut context_transitions,
            );

            let effect = reducer.reduce(&mut state, &action, &mut context);
            assert!(matches!(effect.program, crate::effect::Program::None));
            assert_eq!(state_path.get(&state).get(), 5);
            state_path.get_mut(&mut state).set(7);
            assert_eq!(value.get(), 7);
            assert_eq!(calls.get(), 1);
            assert!(Rc::ptr_eq(action_path.extract(&action).unwrap(), &identity));
            let embedded = action_path.embed(Rc::clone(&identity));
            assert!(Rc::ptr_eq(&embedded.identity, &identity));
            assert_eq!(*embedded.amount, 3);
        }

        #[test]
        fn closure_reducer_keeps_captures_borrows_context_and_effects() {
            struct Step;
            impl crate::DependencyKey for Step {
                type Value = usize;
            }
            let value = Rc::new(Cell::new(2));
            let calls = Cell::new(0);
            let amount = 3;
            let identity = Rc::new(());
            let mut dependencies = DependencyValues::new();
            dependencies.set::<Step>(4);
            let (_scopes_lifecycle, mut scopes) = ScopeRegistry::for_test();
            let now = MonotonicInstant::ZERO;
            let mut context_transitions = crate::store::scope::ScopeTransitionBatch::default();
            let mut context = ReduceContext::new(
                &mut dependencies,
                now,
                &mut scopes,
                &mut context_transitions,
            );
            let context_identity = std::ptr::from_mut(&mut context).cast::<()>();
            let action = LocalAction {
                amount: &amount,
                identity,
            };
            let action_identity = std::ptr::from_ref(&action);
            let mut state = LocalState { value: &value };
            let capture = Rc::new(());
            let capture_identity = Rc::as_ptr(&capture);
            let mut sequence = 0;
            {
                let mut reducer =
                    Reduce::<LocalState<'_>, LocalAction<'_>, _>::new(|state, action, context| {
                        sequence += 1;
                        calls.set(calls.get() + 1);
                        assert_eq!(std::ptr::from_ref(action), action_identity);
                        assert_eq!(std::ptr::from_mut(context).cast::<()>(), context_identity);
                        assert_eq!(Rc::as_ptr(&capture), capture_identity);
                        assert_eq!(context.now(), now);
                        state
                            .value
                            .set(state.value.get() + *action.amount + *context.dependency::<Step>());
                        Effect::none()
                    });
                for expected in [9, 16] {
                    let effect = reducer.reduce(&mut state, &action, &mut context);
                    assert!(matches!(effect.program, crate::effect::Program::None));
                    assert_eq!(value.get(), expected);
                }
            }
            assert_eq!(calls.get(), 2);
            assert_eq!(sequence, 2);
        }

        #[test]
        fn closure_reducers_preserve_combination_source_and_effect_order() {
            let first = Reduce::<usize, usize, _>::new(|state, action, _| {
                *state += action;
                Effect::send(11)
            });
            let second = Reduce::<usize, usize, _>::new(|state, _, _| {
                *state *= 3;
                Effect::send(22)
            });
            let mut reducer = crate::combine((first, second));
            let mut dependencies = DependencyValues::new();
            let (_scopes_lifecycle, mut scopes) = ScopeRegistry::for_test();
            let mut context_transitions = crate::store::scope::ScopeTransitionBatch::default();
            let mut context = ReduceContext::new(
                &mut dependencies,
                MonotonicInstant::ZERO,
                &mut scopes,
                &mut context_transitions,
            );
            let mut state = 1;
            let effect = Reducer::reduce(&mut reducer, &mut state, &2, &mut context);
            assert_eq!(state, 9);
            let crate::effect::Program::Merge(effects) = effect.program else {
                panic!("lost merge")
            };
            assert!(matches!(
                effects[0].program,
                crate::effect::Program::Send(11)
            ));
            assert!(matches!(
                effects[1].program,
                crate::effect::Program::Send(22)
            ));
        }
    }
}

#[cfg(all(test, feature = "allocation-measurement"))]
pub(crate) use allocation_tests::closure_reducer_and_identity_paths_do_not_allocate;

#[cfg(all(test, feature = "allocation-measurement"))]
mod allocation_tests {
    use super::{Effect, Reduce, ReduceContext, Reducer};
    use crate::store::scope::ScopeRegistry;
    use crate::{DependencyValues, MonotonicInstant};

    #[cfg(feature = "allocation-measurement")]
    pub(crate) fn closure_reducer_and_identity_paths_do_not_allocate() {
        if crate::allocation_test::run_in_child(
            "reducer::tests::closure_reducer_and_identity_paths_do_not_allocate",
        ) {
            return;
        }
        let mut count = 0;
        let callback = move |state: &mut usize, action: &usize, _: &mut ReduceContext<'_>| {
            count += 1;
            *state += action + count;
            Effect::none()
        };
        let callback_size = std::mem::size_of_val(&callback);
        let mut reducer = Reduce::new(callback);
        assert_eq!(std::mem::size_of_val(&reducer), callback_size);
        let state_path = crate::identity_state_path::<usize>();
        let action_path = crate::identity_action_path::<usize>();
        let keyed_state_path = crate::identity_keyed_state_path::<usize, usize>();
        let keyed_action_path = crate::identity_keyed_action_path::<usize, usize>();
        assert_eq!(std::mem::size_of_val(&state_path), 0);
        assert_eq!(std::mem::size_of_val(&action_path), 0);
        assert_eq!(std::mem::size_of_val(&keyed_state_path), 0);
        assert_eq!(std::mem::size_of_val(&keyed_action_path), 0);
        let mut state = 0;
        let mut rows = crate::KeyedState::<usize, usize>::new();
        let action = crate::KeyedAction::new(7, 3);
        let mut dependencies = DependencyValues::new();
        let (_scopes_lifecycle, mut scopes) = ScopeRegistry::for_test();
        let mut context_transitions = crate::store::scope::ScopeTransitionBatch::default();
        let mut context = ReduceContext::new(
            &mut dependencies,
            MonotonicInstant::ZERO,
            &mut scopes,
            &mut context_transitions,
        );
        let profiler = dhat::Profiler::builder().testing().build();
        let before = dhat::HeapStats::get();
        for _ in 0..1024 {
            let effect = Reducer::reduce(
                &mut reducer,
                std::hint::black_box(&mut state),
                &3,
                &mut context,
            );
            assert!(matches!(effect.program, crate::effect::Program::None));
            std::hint::black_box(state_path.get(&state));
            std::hint::black_box(state_path.get_mut(&mut state));
            std::hint::black_box(action_path.extract(&3));
            std::hint::black_box(action_path.embed(4));
            std::hint::black_box(keyed_state_path.get(&rows));
            std::hint::black_box(keyed_state_path.get_mut(&mut rows));
            std::hint::black_box(keyed_action_path.extract(&action));
            std::hint::black_box(keyed_action_path.embed(7, 4));
        }
        let after = dhat::HeapStats::get();
        assert_eq!(after.total_blocks, before.total_blocks);
        assert_eq!(after.total_bytes, before.total_bytes);
        assert_eq!(state, 527_872);
        drop(profiler);
    }
}
