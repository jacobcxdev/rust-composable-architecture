//! Combines reducers sharing one state and action domain.

use crate::{Effect, ReduceContext, Reducer};

/// Holds reducers invoked in declaration order.
///
/// [`Reducer`] is implemented for tuples with 1 through 12 members and for
/// [`Vec<R>`] where `R: Reducer`. Every member shares exactly the same
/// [`Reducer::State`] and [`Reducer::Action`] types. A typed empty vector uses
/// `R`'s associated types and returns [`Effect::none`] without changing state.
/// An empty tuple still provides no state or action domain to infer.
///
/// Members remain owned between reductions, including their mutable captures.
/// Construction invokes no reducer. Synchronous reduction adds no `Clone`,
/// `Send` or `'static` bounds to the shared state and action domain.
/// Actor admission and asynchronous effects retain their own transport bounds.
///
/// Construct this type with [`combine`].
///
/// # Panics
///
/// If a member panics, later members do not run. Completed state mutations remain.
/// During unwinding, effects returned by earlier members drop without execution.
/// This wrapper does not catch the panic, roll back mutations or retry reduction.
#[derive(Debug)]
pub struct Combine<Reducers> {
    /// Owns the ordered members and retains their state across reductions.
    reducers: Reducers,
}

/// Combines reducers sharing one state and action domain.
///
/// Each reducer receives the same borrowed action. Returned effects are merged
/// in declaration order, not asynchronous completion order. Reducer tuples may
/// contain 1 through 12 members. A vector may contain any number of reducers of
/// one type, including zero; its element type supplies the state/action domain.
/// Empty tuples remain unsupported. Vector construction may allocate. `Combine`
/// adds no allocation for all-empty effects, but member reductions may allocate.
/// See [`Combine`] for ownership and panic behaviour during reduction.
///
/// # Examples
///
/// ```
/// use rca_core::{Effect, ReduceContext, Reducer, combine};
///
/// struct State;
/// enum Action {
///     Run,
/// }
///
/// struct First;
/// struct Second;
///
/// macro_rules! impl_reducer {
///     ($reducer:ty) => {
///         impl Reducer for $reducer {
///             type State = State;
///             type Action = Action;
///
///             fn reduce(
///                 &mut self,
///                 _state: &mut Self::State,
///                 _action: &Self::Action,
///                 _context: &mut ReduceContext<'_>,
///             ) -> Effect<Self::Action> {
///                 Effect::none()
///             }
///         }
///     };
/// }
///
/// impl_reducer!(First);
/// impl_reducer!(Second);
///
/// let reducer = combine((First, Second));
/// let _: &dyn Reducer<State = State, Action = Action> = &reducer;
/// let empty = combine(Vec::<First>::new());
/// let _: &dyn Reducer<State = State, Action = Action> = &empty;
/// let many = combine(vec![First, First]);
/// let _: &dyn Reducer<State = State, Action = Action> = &many;
/// ```
#[must_use]
pub const fn combine<Reducers>(reducers: Reducers) -> Combine<Reducers> {
    Combine { reducers }
}

impl<R: Reducer> Reducer for Combine<Vec<R>> {
    type State = R::State;
    type Action = R::Action;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        Effect::merge(
            self.reducers
                .iter_mut()
                .map(|reducer| reducer.reduce(state, action, context)),
        )
    }
}

/// Implements each tuple width without erasing member types or cloning actions.
macro_rules! impl_combine {
    ($first_index:tt => $first:ident $(, $index:tt => $reducer:ident)*) => {
        impl<$first, $($reducer),*> Reducer for Combine<($first, $($reducer,)*)>
        where
            $first: Reducer,
            $($reducer: Reducer<State = $first::State, Action = $first::Action>,)*
        {
            type State = $first::State;
            type Action = $first::Action;

            fn reduce(
                &mut self,
                state: &mut Self::State,
                action: &Self::Action,
                context: &mut ReduceContext<'_>,
            ) -> Effect<Self::Action> {
                Effect::merge([
                    self.reducers.$first_index.reduce(state, action, context),
                    $(self.reducers.$index.reduce(state, action, context),)*
                ])
            }
        }
    };
}

impl_combine!(0 => R0);
impl_combine!(0 => R0, 1 => R1);
impl_combine!(0 => R0, 1 => R1, 2 => R2);
impl_combine!(0 => R0, 1 => R1, 2 => R2, 3 => R3);
impl_combine!(0 => R0, 1 => R1, 2 => R2, 3 => R3, 4 => R4);
impl_combine!(0 => R0, 1 => R1, 2 => R2, 3 => R3, 4 => R4, 5 => R5);
impl_combine!(0 => R0, 1 => R1, 2 => R2, 3 => R3, 4 => R4, 5 => R5, 6 => R6);
impl_combine!(0 => R0, 1 => R1, 2 => R2, 3 => R3, 4 => R4, 5 => R5, 6 => R6, 7 => R7);
impl_combine!(0 => R0, 1 => R1, 2 => R2, 3 => R3, 4 => R4, 5 => R5, 6 => R6, 7 => R7, 8 => R8);
impl_combine!(0 => R0, 1 => R1, 2 => R2, 3 => R3, 4 => R4, 5 => R5, 6 => R6, 7 => R7, 8 => R8, 9 => R9);
impl_combine!(0 => R0, 1 => R1, 2 => R2, 3 => R3, 4 => R4, 5 => R5, 6 => R6, 7 => R7, 8 => R8, 9 => R9, 10 => R10);
impl_combine!(0 => R0, 1 => R1, 2 => R2, 3 => R3, 4 => R4, 5 => R5, 6 => R6, 7 => R7, 8 => R8, 9 => R9, 10 => R10, 11 => R11);

#[cfg(all(test, feature = "allocation-measurement"))]
#[global_allocator]
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

#[cfg(test)]
ordinary_test_items! {
mod tests {
    use super::*;
    use crate::effect::Program;
    use crate::store::scope::ScopeRegistry;
    use crate::{DependencyValues, MonotonicInstant};

    struct OrderFirst;

    struct OrderSecond;

    struct OrderState {
        action_addresses: Vec<usize>,
        calls: Vec<&'static str>,
    }

    enum OrderAction {
        Run(Vec<u8>),
        Output(u8),
    }

    impl Reducer for OrderFirst {
        type State = OrderState;
        type Action = OrderAction;

        fn reduce(
            &mut self,
            state: &mut Self::State,
            action: &Self::Action,
            _context: &mut ReduceContext<'_>,
        ) -> Effect<Self::Action> {
            let OrderAction::Run(bytes) = action else {
                return Effect::none();
            };
            assert_eq!(bytes, &[1]);
            state
                .action_addresses
                .push(std::ptr::from_ref(action).addr());
            state.calls.push("first");
            Effect::send(OrderAction::Output(1))
        }
    }

    impl Reducer for OrderSecond {
        type State = OrderState;
        type Action = OrderAction;

        fn reduce(
            &mut self,
            state: &mut Self::State,
            action: &Self::Action,
            _context: &mut ReduceContext<'_>,
        ) -> Effect<Self::Action> {
            let OrderAction::Run(bytes) = action else {
                return Effect::none();
            };
            assert_eq!(bytes, &[1]);
            state
                .action_addresses
                .push(std::ptr::from_ref(action).addr());
            state.calls.push("second");
            Effect::send(OrderAction::Output(2))
        }
    }

    #[test]
    fn combine_merges_effects_in_source_order_without_clone() {
        let mut reducer = combine((OrderFirst, OrderSecond));
        let mut state = OrderState {
            action_addresses: Vec::new(),
            calls: Vec::new(),
        };
        let action = OrderAction::Run(vec![1]);
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

        assert_eq!(state.calls, ["first", "second"]);
        assert_eq!(state.action_addresses.len(), 2);
        assert_eq!(state.action_addresses[0], state.action_addresses[1]);
        let Program::Merge(effects) = effect.program else {
            panic!("combined sends must produce a merge");
        };
        let mut effects = effects.into_iter();
        assert!(matches!(
            effects.next().map(|effect| effect.program),
            Some(Program::Send(OrderAction::Output(1)))
        ));
        assert!(matches!(
            effects.next().map(|effect| effect.program),
            Some(Program::Send(OrderAction::Output(2)))
        ));
        assert!(effects.next().is_none());
    }

    #[test]
    fn merge_discards_none_children_without_reordering() {
        let effect = Effect::merge([
            Effect::none(),
            Effect::send(OrderAction::Output(1)),
            Effect::none(),
            Effect::send(OrderAction::Output(2)),
        ]);

        let Program::Merge(effects) = effect.program else {
            panic!("non-empty effects must produce a merge");
        };
        let outputs: Vec<u8> = effects
            .into_iter()
            .map(|effect| match effect.program {
                Program::Send(OrderAction::Output(output)) => output,
                _ => panic!("merge children must be sends"),
            })
            .collect();
        assert_eq!(outputs, [1, 2]);
    }

    struct VectorReducer {
        digit: u8,
        calls: usize,
        emits: bool,
    }

    struct VectorState {
        value: u64,
        calls: Vec<(u8, usize)>,
        action_addresses: Vec<usize>,
        context_addresses: Vec<usize>,
    }

    impl Reducer for VectorReducer {
        type State = VectorState;
        type Action = OrderAction;

        fn reduce(
            &mut self,
            state: &mut Self::State,
            action: &Self::Action,
            context: &mut ReduceContext<'_>,
        ) -> Effect<Self::Action> {
            self.calls += 1;
            state.value = state.value * 10 + u64::from(self.digit);
            state.calls.push((self.digit, self.calls));
            state
                .action_addresses
                .push(std::ptr::from_ref(action).addr());
            state
                .context_addresses
                .push(std::ptr::from_ref(context).addr());
            if self.emits {
                Effect::send(OrderAction::Output(self.digit))
            } else {
                Effect::none()
            }
        }
    }

    #[test]
    fn typed_empty_vector_leaves_state_and_effect_unchanged() {
        let mut reducer = combine(Vec::<VectorReducer>::new());
        let mut state = VectorState {
            value: 42,
            calls: Vec::new(),
            action_addresses: Vec::new(),
            context_addresses: Vec::new(),
        };
        let action = OrderAction::Run(vec![1]);
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

        assert_eq!(state.value, 42);
        assert_eq!(state.calls, [] as [(u8, usize); 0]);
        assert_eq!(state.action_addresses, [] as [usize; 0]);
        assert_eq!(state.context_addresses, [] as [usize; 0]);
        assert!(matches!(effect.program, Program::None));
    }

    #[test]
    fn vector_reduces_once_in_order_with_shared_borrows_and_retained_members() {
        for (digits, expected_value, expected_calls, expected_outputs) in [
            (vec![1], 1, vec![(1, 1)], vec![1]),
            (vec![1, 2, 3], 123, vec![(1, 1), (2, 1), (3, 1)], vec![1, 3]),
        ] {
            let mut reducer = combine(
                digits
                    .into_iter()
                    .map(|digit| VectorReducer {
                        digit,
                        calls: 0,
                        emits: digit != 2,
                    })
                    .collect::<Vec<_>>(),
            );
            let mut state = VectorState {
                value: 0,
                calls: Vec::new(),
                action_addresses: Vec::new(),
                context_addresses: Vec::new(),
            };
            let action = OrderAction::Run(vec![1]);
            let action_address = std::ptr::from_ref(&action).addr();
            let mut dependencies = DependencyValues::new();
            let (_scopes_lifecycle, mut scopes) = ScopeRegistry::for_test();
            let mut context_transitions = crate::store::scope::ScopeTransitionBatch::default();
            let mut context = ReduceContext::new(
                &mut dependencies,
                MonotonicInstant::ZERO,
                &mut scopes,
                &mut context_transitions,
            );
            let context_address = std::ptr::from_ref(&context).addr();

            let effect = reducer.reduce(&mut state, &action, &mut context);

            assert_eq!(state.value, expected_value);
            assert_eq!(state.calls, expected_calls);
            assert!(
                state
                    .action_addresses
                    .iter()
                    .all(|address| *address == action_address)
            );
            assert!(
                state
                    .context_addresses
                    .iter()
                    .all(|address| *address == context_address)
            );
            let Program::Merge(effects) = effect.program else {
                panic!("vector sends must produce a merge");
            };
            let outputs: Vec<_> = effects
                .into_iter()
                .map(|effect| match effect.program {
                    Program::Send(OrderAction::Output(output)) => output,
                    _ => panic!("non-empty vector children must be sends"),
                })
                .collect();
            assert_eq!(outputs, expected_outputs);

            state.value = 0;
            state.calls.clear();
            let _effect = reducer.reduce(&mut state, &action, &mut context);
            assert_eq!(state.value, expected_value);
            assert!(state.calls.iter().all(|(_, calls)| *calls == 2));
            assert_eq!(state.calls.len(), expected_calls.len());
        }
    }

    #[test]
    fn vector_construction_omits_unselected_branches() {
        let mut members = Vec::new();
        for digit in 1..=3 {
            if digit != 2 {
                members.push(VectorReducer {
                    digit,
                    calls: 0,
                    emits: false,
                });
            }
        }
        #[cfg(any())]
        members.push(VectorReducer {
            digit: 9,
            calls: 0,
            emits: true,
        });
        let mut reducer = combine(members);
        let mut state = VectorState {
            value: 0,
            calls: Vec::new(),
            action_addresses: Vec::new(),
            context_addresses: Vec::new(),
        };
        let action = OrderAction::Run(vec![1]);
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

        assert_eq!(state.value, 13);
        assert_eq!(state.calls, [(1, 1), (3, 1)]);
        assert!(matches!(effect.program, Program::None));
    }

    struct DeferredReducer {
        output: u8,
        gate: Option<futures_channel::oneshot::Receiver<()>>,
        starts: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Reducer for DeferredReducer {
        type State = Vec<u8>;
        type Action = OrderAction;

        fn reduce(
            &mut self,
            state: &mut Self::State,
            _action: &Self::Action,
            _context: &mut ReduceContext<'_>,
        ) -> Effect<Self::Action> {
            state.push(self.output);
            let output = self.output;
            let gate = self.gate.take().expect("test reducer runs once");
            let starts = std::sync::Arc::clone(&self.starts);
            Effect::future(async move {
                starts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                gate.await.expect("test gate remains open");
                Some(OrderAction::Output(output))
            })
        }
    }

    #[test]
    fn vector_effects_defer_work_and_can_complete_out_of_reduction_order() {
        use std::sync::atomic::Ordering;
        use std::task::{Context, Poll};

        let (first_send, first_receive) = futures_channel::oneshot::channel();
        let (second_send, second_receive) = futures_channel::oneshot::channel();
        let starts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut reducer = combine(vec![
            DeferredReducer {
                output: 1,
                gate: Some(first_receive),
                starts: std::sync::Arc::clone(&starts),
            },
            DeferredReducer {
                output: 2,
                gate: Some(second_receive),
                starts: std::sync::Arc::clone(&starts),
            },
        ]);
        let mut state = Vec::new();
        let action = OrderAction::Run(vec![1]);
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

        assert_eq!(state, [1, 2]);
        assert_eq!(starts.load(Ordering::SeqCst), 0);
        let Program::Merge(effects) = effect.program else {
            panic!("deferred vector children must produce a merge");
        };
        let mut effects = effects.into_iter();
        let Program::Future(mut first) = effects.next().expect("first effect").program else {
            panic!("first effect must retain its future");
        };
        let Program::Future(mut second) = effects.next().expect("second effect").program else {
            panic!("second effect must retain its future");
        };
        assert!(effects.next().is_none());
        let mut poll_context = Context::from_waker(futures_util::task::noop_waker_ref());
        assert!(first.as_mut().poll(&mut poll_context).is_pending());
        second_send.send(()).expect("second receiver alive");
        assert!(matches!(
            second.as_mut().poll(&mut poll_context),
            Poll::Ready(Some(OrderAction::Output(2)))
        ));
        first_send.send(()).expect("first receiver alive");
        assert!(matches!(
            first.as_mut().poll(&mut poll_context),
            Poll::Ready(Some(OrderAction::Output(1)))
        ));
        assert_eq!(starts.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn tuple_and_vector_panics_stop_later_members_and_drop_pending_effects() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct DropProbe(Arc<AtomicUsize>);

        impl Drop for DropProbe {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        struct Member {
            position: u8,
            drops: Arc<AtomicUsize>,
            polls: Arc<AtomicUsize>,
        }

        impl Reducer for Member {
            type State = Vec<u8>;
            type Action = ();

            fn reduce(
                &mut self,
                state: &mut Vec<u8>,
                (): &(),
                _: &mut ReduceContext<'_>,
            ) -> Effect<()> {
                state.push(self.position);
                assert_ne!(self.position, 2, "second member panics after mutation");

                let probe = DropProbe(Arc::clone(&self.drops));
                let polls = Arc::clone(&self.polls);

                Effect::future(async move {
                    polls.fetch_add(1, Ordering::SeqCst);
                    drop(probe);
                    None
                })
            }
        }

        for vector in [false, true] {
            let drops = Arc::new(AtomicUsize::new(0));
            let polls = Arc::new(AtomicUsize::new(0));
            let member = |position| Member {
                position,
                drops: Arc::clone(&drops),
                polls: Arc::clone(&polls),
            };

            let mut reducer: Box<dyn Reducer<State = Vec<u8>, Action = ()>> = if vector {
                Box::new(combine(vec![member(1), member(2), member(3)]))
            } else {
                Box::new(combine((member(1), member(2), member(3))))
            };
            let mut state = Vec::new();
            let mut dependencies = DependencyValues::new();
            let (_scopes_lifecycle, mut scopes) = ScopeRegistry::for_test();
            let mut context_transitions = crate::store::scope::ScopeTransitionBatch::default();
            let mut context = ReduceContext::new(
                &mut dependencies,
                MonotonicInstant::ZERO,
                &mut scopes,
                &mut context_transitions,
            );

            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                reducer.reduce(&mut state, &(), &mut context)
            }));

            result.as_ref().expect_err("a member panic must propagate");
            assert_eq!(state, [1, 2], "vector={vector}");
            assert_eq!(polls.load(Ordering::SeqCst), 0, "vector={vector}");
            assert_eq!(drops.load(Ordering::SeqCst), 1, "vector={vector}");
        }
    }

}
}

#[cfg(all(test, feature = "allocation-measurement"))]
pub(crate) use allocation_tests::{
    combine_all_none_widths_do_not_allocate, vector_all_none_reduction_does_not_allocate,
};

#[cfg(all(test, feature = "allocation-measurement"))]
mod allocation_tests {
    use std::hint::black_box;

    use super::{Effect, ReduceContext, Reducer, combine};
    use crate::effect::Program;
    use crate::store::scope::ScopeRegistry;
    use crate::{DependencyValues, MonotonicInstant};

    #[cfg(feature = "allocation-measurement")]
    struct NoneReducer;

    #[cfg(feature = "allocation-measurement")]
    impl Reducer for NoneReducer {
        type State = ();
        type Action = ();

        fn reduce(
            &mut self,
            _state: &mut Self::State,
            _action: &Self::Action,
            _context: &mut ReduceContext<'_>,
        ) -> Effect<Self::Action> {
            Effect::none()
        }
    }

    #[cfg(feature = "allocation-measurement")]
    macro_rules! for_each_combine_width {
        ($assertion:ident, $state:ident, $action:ident, $context:ident) => {
            for_each_combine_width!(
                @next
                $assertion,
                $state,
                $action,
                $context;
                [NoneReducer];
                1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12
            );
        };
        (
            @next
            $assertion:ident,
            $state:ident,
            $action:ident,
            $context:ident;
            [$($reducers:expr),+];
            $width:literal $(, $remaining:literal)*
        ) => {
            $assertion!($width, ($($reducers,)+), $state, $action, $context);
            for_each_combine_width!(
                @next
                $assertion,
                $state,
                $action,
                $context;
                [$($reducers,)+ NoneReducer];
                $($remaining),*
            );
        };
        (
            @next
            $assertion:ident,
            $state:ident,
            $action:ident,
            $context:ident;
            [$($reducers:expr),+];
        ) => {};
    }

    #[cfg(feature = "allocation-measurement")]
    macro_rules! warm_combine_width {
        ($width:literal, $reducers:expr, $state:ident, $action:ident, $context:ident) => {{
            let mut reducer = combine($reducers);
            let effect = black_box(reducer.reduce(&mut $state, &$action, &mut $context));
            assert!(matches!(effect.program, Program::None), "width {}", $width);
        }};
    }

    #[cfg(feature = "allocation-measurement")]
    macro_rules! measure_combine_width {
        ($width:literal, $reducers:expr, $state:ident, $action:ident, $context:ident) => {{
            let mut reducer = combine($reducers);
            let before = dhat::HeapStats::get();
            let effect = black_box(reducer.reduce(&mut $state, &$action, &mut $context));
            assert!(matches!(effect.program, Program::None), "width {}", $width);
            let after = dhat::HeapStats::get();
            assert_eq!(
                after.total_blocks, before.total_blocks,
                "Combine width {} allocated blocks",
                $width
            );
            assert_eq!(
                after.total_bytes, before.total_bytes,
                "Combine width {} allocated bytes",
                $width
            );
        }};
    }

    #[cfg(feature = "allocation-measurement")]
    pub(crate) fn combine_all_none_widths_do_not_allocate() {
        if crate::allocation_test::run_in_child(
            "composition::combine::tests::combine_all_none_widths_do_not_allocate",
        ) {
            return;
        }
        let mut state = ();
        let action = ();
        let mut dependencies = DependencyValues::new();
        let (_scopes_lifecycle, mut scopes) = ScopeRegistry::for_test();
        let mut context_transitions = crate::store::scope::ScopeTransitionBatch::default();
        let mut context = ReduceContext::new(
            &mut dependencies,
            MonotonicInstant::ZERO,
            &mut scopes,
            &mut context_transitions,
        );

        for_each_combine_width!(warm_combine_width, state, action, context);
        let profiler = dhat::Profiler::builder().testing().build();
        for_each_combine_width!(measure_combine_width, state, action, context);
        drop(profiler);
    }

    #[cfg(feature = "allocation-measurement")]
    pub(crate) fn vector_all_none_reduction_does_not_allocate() {
        if crate::allocation_test::run_in_child(
            "composition::combine::tests::vector_all_none_reduction_does_not_allocate",
        ) {
            return;
        }
        let mut reducers = [
            combine(Vec::<NoneReducer>::new()),
            combine(vec![NoneReducer]),
            combine((0..17).map(|_| NoneReducer).collect::<Vec<_>>()),
        ];
        let mut dependencies = DependencyValues::new();
        let (_scopes_lifecycle, mut scopes) = ScopeRegistry::for_test();
        let mut context_transitions = crate::store::scope::ScopeTransitionBatch::default();
        let mut context = ReduceContext::new(
            &mut dependencies,
            MonotonicInstant::ZERO,
            &mut scopes,
            &mut context_transitions,
        );
        for reducer in &mut reducers {
            let effect = black_box(reducer.reduce(&mut (), &(), &mut context));
            assert!(matches!(effect.program, Program::None));
        }
        // Vector construction is outside the measured reduction boundary.
        let profiler = dhat::Profiler::builder().testing().build();
        for reducer in &mut reducers {
            let before = dhat::HeapStats::get();
            let effect = black_box(reducer.reduce(&mut (), &(), &mut context));
            assert!(matches!(effect.program, Program::None));
            let after = dhat::HeapStats::get();
            assert_eq!(after.total_blocks, before.total_blocks);
            assert_eq!(after.total_bytes, before.total_bytes);
        }
        drop(profiler);
    }
}
