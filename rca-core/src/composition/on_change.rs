//! Reacts to semantic changes after one base reduction.

use core::fmt;

use crate::{Effect, ReduceContext, Reducer};

/// Adds a synchronous reaction to changes selected around a base reducer.
///
/// See [`on_change`] for ordering, snapshot and panic contracts. This wrapper
/// stores only the reducer and closures, not selected values.
/// Synchronous reduction adds no `Clone`, `Send` or `'static` requirements.
/// Store and effect transport keep their existing owned bounds.
/// Debug output omits reducers and captures.
pub struct OnChange<R, Capture, MakeReaction> {
    /// Owns the base reducer and retains its mutable captures between actions.
    reducer: R,
    /// Selects independent values before and after one base reduction.
    capture: Capture,
    /// Retains factory state, but constructs a fresh one-shot reducer per change.
    make_reaction: MakeReaction,
}

impl<R, Capture, MakeReaction> OnChange<R, Capture, MakeReaction> {
    /// Creates a reducer with a selected-value change reaction.
    ///
    /// Equivalent to [`on_change`]; constructing the wrapper invokes no closure.
    #[must_use]
    pub const fn new(reducer: R, capture: Capture, make_reaction: MakeReaction) -> Self {
        Self {
            reducer,
            capture,
            make_reaction,
        }
    }
}

impl<R, Capture, MakeReaction> fmt::Debug for OnChange<R, Capture, MakeReaction> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OnChange").finish_non_exhaustive()
    }
}

/// Adds one reaction when independently captured selected values differ.
///
/// Captures a value before reduction, invokes `reducer` once, then captures a
/// new value and compares them with [`PartialEq`]. Equal values return the base
/// effect unchanged. Unequal values are consumed by `make_reaction`, whose
/// reducer runs once with the same borrowed action, state and reduction context.
/// Base and reaction effects merge in that construction order for concurrent
/// execution, not completion order. Reaction mutations do not cause another
/// comparison or reaction. This behaviour is active in release builds too.
///
/// `capture` must produce independent values: a shared interior-mutable handle
/// does not freeze the old value. Select semantic data, not rendered strings or
/// private scope identities. Wrapping a child reducer does not replace the
/// enclosing composition's lifecycle or stale-action checks.
///
/// Typed edits remain ordinary action variants sent through [`crate::Store`]
/// or [`crate::ScopedStore`], not direct binding writes. Carry context in typed
/// action fields or existing dependencies. Explicit dependency overrides beat
/// feature overlays; inner/newer overlays beat outer/earlier overlays, then
/// Store defaults. No transaction or localisation engine is implied.
///
/// # Examples
///
/// ```
/// use rca_core::{Effect, ReduceContext, Reducer, on_change};
///
/// struct State {
///     name: String,
///     changes: Vec<(String, String)>,
/// }
///
/// enum Action {
///     SetName(String),
/// }
///
/// struct Edit;
///
/// impl Reducer for Edit {
///     type State = State;
///     type Action = Action;
///
///     fn reduce(
///         &mut self,
///         state: &mut State,
///         action: &Action,
///         _: &mut ReduceContext<'_>,
///     ) -> Effect<Action> {
///         let Action::SetName(name) = action;
///         state.name.clone_from(name);
///         Effect::none()
///     }
/// }
///
/// struct Record(String, String);
///
/// impl Reducer for Record {
///     type State = State;
///     type Action = Action;
///
///     fn reduce(
///         &mut self,
///         state: &mut State,
///         _: &Action,
///         _: &mut ReduceContext<'_>,
///     ) -> Effect<Action> {
///         state
///             .changes
///             .push((std::mem::take(&mut self.0), std::mem::take(&mut self.1)));
///         Effect::none()
///     }
/// }
///
/// let reducer = on_change(Edit, |state: &State| state.name.clone(), Record);
/// // Send Action::SetName through the Store that owns this reducer.
/// let _: &dyn Reducer<State = State, Action = Action> = &reducer;
/// ```
///
/// A reaction must share the base reducer's complete state and action types:
///
/// ```compile_fail
/// use rca_core::{Effect, ReduceContext, Reducer, on_change};
///
/// struct Base;
///
/// impl Reducer for Base {
///     type State = usize;
///     type Action = ();
///
///     fn reduce(
///         &mut self,
///         _: &mut usize,
///         _: &(),
///         _: &mut ReduceContext<'_>,
///     ) -> Effect<()> {
///         Effect::none()
///     }
/// }
///
/// struct Wrong;
///
/// impl Reducer for Wrong {
///     type State = String;
///     type Action = ();
///
///     fn reduce(
///         &mut self,
///         _: &mut String,
///         _: &(),
///         _: &mut ReduceContext<'_>,
///     ) -> Effect<()> {
///         Effect::none()
///     }
/// }
///
/// let _ = on_change(Base, |state: &usize| *state, |_, _| Wrong);
/// ```
///
/// A reaction cannot retain borrowed mutable state in an asynchronous effect:
///
/// ```compile_fail
/// use rca_core::{Effect, ReduceContext, Reducer, on_change};
///
/// struct Edit;
///
/// impl Reducer for Edit {
///     type State = String;
///     type Action = ();
///
///     fn reduce(
///         &mut self,
///         state: &mut String,
///         _: &(),
///         _: &mut ReduceContext<'_>,
///     ) -> Effect<()> {
///         state.push('x');
///         Effect::none()
///     }
/// }
///
/// struct BorrowingReaction;
///
/// impl Reducer for BorrowingReaction {
///     type State = String;
///     type Action = ();
///
///     fn reduce(
///         &mut self,
///         state: &mut String,
///         _: &(),
///         _: &mut ReduceContext<'_>,
///     ) -> Effect<()> {
///         Effect::future(async move {
///             state.push('y');
///             None
///         })
///     }
/// }
///
/// let _ = on_change(Edit, |state: &String| state.len(), |_, _| BorrowingReaction);
/// ```
///
/// # Panics
///
/// Caller captures, comparison, reducer and reaction construction may panic.
/// A before-capture panic prevents base reduction; a base panic prevents the
/// after capture and reaction. An after-capture, comparison or construction
/// panic leaves completed base mutations and drops its unreturned effect. A
/// reaction panic also leaves completed reaction mutations and drops pending
/// effects. There is no rollback, catch, retry or recursive reaction loop.
#[must_use]
pub fn on_change<R, Capture, MakeReaction, Value, Reaction>(
    reducer: R,
    capture: Capture,
    make_reaction: MakeReaction,
) -> OnChange<R, Capture, MakeReaction>
where
    R: Reducer,
    Capture: Fn(&R::State) -> Value,
    Value: PartialEq,
    MakeReaction: FnMut(Value, Value) -> Reaction,
    Reaction: Reducer<State = R::State, Action = R::Action>,
{
    OnChange::new(reducer, capture, make_reaction)
}

impl<R, Capture, MakeReaction, Value, Reaction> Reducer for OnChange<R, Capture, MakeReaction>
where
    R: Reducer,
    Capture: Fn(&R::State) -> Value,
    Value: PartialEq,
    MakeReaction: FnMut(Value, Value) -> Reaction,
    Reaction: Reducer<State = R::State, Action = R::Action>,
{
    type State = R::State;
    type Action = R::Action;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        let before = (self.capture)(state);
        let base = self.reducer.reduce(state, action, context);
        let after = (self.capture)(state);

        if before == after {
            base
        } else {
            let reaction = (self.make_reaction)(before, after).reduce(state, action, context);
            Effect::merge([base, reaction])
        }
    }
}

#[cfg(test)]
ordinary_test_items! {
mod tests {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    use super::*;
    use crate::store::scope::ScopeRegistry;
    use crate::{DependencyValues, MonotonicInstant};

    type Trace = Rc<RefCell<Vec<&'static str>>>;

    struct Value {
        number: usize,
        trace: Trace,
    }

    impl PartialEq for Value {
        fn eq(&self, other: &Self) -> bool {
            self.trace.borrow_mut().push("eq");
            self.number == other.number
        }
    }

    struct State<'a> {
        number: usize,
        calls: &'a Cell<usize>,
        addresses: Vec<usize>,
    }

    struct Action<'a>(&'a Rc<usize>);

    struct Base<'a>(&'a Trace);

    impl<'a> Reducer for Base<'a> {
        type State = State<'a>;
        type Action = Action<'a>;

        fn reduce(
            &mut self,
            state: &mut State<'a>,
            action: &Action<'a>,
            _: &mut ReduceContext<'_>,
        ) -> Effect<Action<'a>> {
            self.0.borrow_mut().push("base");
            state.calls.set(state.calls.get() + 1);
            state.number += **action.0;
            state.addresses.push(std::ptr::from_ref(action).addr());
            Effect::none()
        }
    }

    struct Reaction<'a> {
        trace: &'a Trace,
        old: Value,
        new: Value,
    }

    impl<'a> Reducer for Reaction<'a> {
        type State = State<'a>;
        type Action = Action<'a>;

        fn reduce(
            &mut self,
            state: &mut State<'a>,
            action: &Action<'a>,
            _: &mut ReduceContext<'_>,
        ) -> Effect<Action<'a>> {
            self.trace.borrow_mut().push("reaction");
            assert_eq!((self.old.number, self.new.number), (1, 3));
            assert_eq!(state.number, 3);
            state.number += 10;
            state.addresses.push(std::ptr::from_ref(action).addr());
            Effect::none()
        }
    }

    #[test]
    fn local_move_only_values_follow_exact_oracle_and_reaction_does_not_loop() {
        let trace = Rc::new(RefCell::new(Vec::new()));
        let calls = Cell::new(0);
        let amount = Rc::new(2);
        let action = Action(&amount);
        let mut state = State {
            number: 1,
            calls: &calls,
            addresses: Vec::new(),
        };
        let mut reducer = on_change(
            Base(&trace),
            |state: &State<'_>| {
                trace.borrow_mut().push("capture");
                Value {
                    number: state.number,
                    trace: Rc::clone(&trace),
                }
            },
            |old, new| {
                trace.borrow_mut().push("make");
                Reaction {
                    trace: &trace,
                    old,
                    new,
                }
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
        assert_eq!(
            *trace.borrow(),
            ["capture", "base", "capture", "eq", "make", "reaction"]
        );
        assert_eq!(calls.get(), 1);
        assert_eq!(state.number, 13);
        assert_eq!(state.addresses, [std::ptr::from_ref(&action).addr(); 2]);
        assert!(matches!(effect.program, crate::effect::Program::None));
    }

    struct SendReducer(u8);

    impl Reducer for SendReducer {
        type State = usize;
        type Action = u8;
        fn reduce(&mut self, state: &mut usize, _: &u8, _: &mut ReduceContext<'_>) -> Effect<u8> {
            *state += 1;
            Effect::send(self.0)
        }
    }

    #[test]
    fn base_and_reaction_effects_keep_source_order() {
        let mut reducer = on_change(
            SendReducer(11),
            |state: &usize| *state,
            |old, new| {
                assert_eq!((old, new), (0, 1));
                SendReducer(23)
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
        let mut state = 0;
        let effect = reducer.reduce(&mut state, &0, &mut context);
        assert_eq!(state, 2);
        let crate::effect::Program::Merge(effects) = effect.program else {
            panic!("both sends retained")
        };
        let mut effects = effects.into_iter();
        assert!(matches!(
            effects.next().unwrap().program,
            crate::effect::Program::Send(11)
        ));
        assert!(matches!(
            effects.next().unwrap().program,
            crate::effect::Program::Send(23)
        ));
        assert!(effects.next().is_none());
    }

    #[test]
    fn no_change_returns_base_effect_without_constructing_a_reaction() {
        let mut reducer = on_change(
            SendReducer(11),
            |_: &usize| (),
            |(), ()| {
                panic!("unchanged projection must not construct reaction");
                #[expect(
                    unreachable_code,
                    reason = "pins the reaction's Reducer type after panic"
                )]
                SendReducer(23)
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
        let effect = reducer.reduce(&mut 0, &0, &mut context);
        assert!(matches!(effect.program, crate::effect::Program::Send(11)));
    }

    #[test]
    fn debug_does_not_require_or_render_captured_secrets() {
        let secret = "private-capture";
        let reducer = on_change(SendReducer(11), |_: &usize| secret, |_, _| SendReducer(23));
        let rendered = format!("{reducer:?}");
        assert!(rendered.contains("OnChange"));
        assert!(!rendered.contains(secret));
    }

    #[derive(Clone, Copy, PartialEq)]
    enum PanicAt {
        Before,
        Base,
        After,
        Equality,
        Make,
        Reaction,
    }

    struct DropProbe(std::sync::Arc<std::sync::atomic::AtomicUsize>);
    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    struct PanicValue(usize, PanicAt);
    impl PartialEq for PanicValue {
        fn eq(&self, other: &Self) -> bool {
            assert!(self.1 != PanicAt::Equality, "equality panic");
            self.0 == other.0
        }
    }

    struct PanicBase(PanicAt, std::sync::Arc<std::sync::atomic::AtomicUsize>);
    impl Reducer for PanicBase {
        type State = usize;
        type Action = ();
        fn reduce(&mut self, state: &mut usize, (): &(), _: &mut ReduceContext<'_>) -> Effect<()> {
            *state += 1;
            assert!(self.0 != PanicAt::Base, "base panic");
            let probe = DropProbe(std::sync::Arc::clone(&self.1));
            Effect::future(async move {
                drop(probe);
                None
            })
        }
    }

    struct PanicReaction;
    impl Reducer for PanicReaction {
        type State = usize;
        type Action = ();
        fn reduce(&mut self, state: &mut usize, (): &(), _: &mut ReduceContext<'_>) -> Effect<()> {
            *state += 10;
            panic!("reaction panic")
        }
    }

    #[test]
    fn panic_stages_preserve_completed_mutations_and_drop_unreturned_base_effect() {
        for (phase, expected_state, expected_drops) in [
            (PanicAt::Before, 0, 0),
            (PanicAt::Base, 1, 0),
            (PanicAt::After, 1, 1),
            (PanicAt::Equality, 1, 1),
            (PanicAt::Make, 1, 1),
            (PanicAt::Reaction, 11, 1),
        ] {
            let drops = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let captures = Cell::new(0);
            let mut reducer = on_change(
                PanicBase(phase, std::sync::Arc::clone(&drops)),
                |state: &usize| {
                    captures.set(captures.get() + 1);
                    assert!(
                        !(phase == PanicAt::Before && captures.get() == 1),
                        "before panic"
                    );
                    assert!(
                        !(phase == PanicAt::After && captures.get() == 2),
                        "after panic"
                    );
                    PanicValue(*state, phase)
                },
                |_, _| {
                    assert!(phase != PanicAt::Make, "construction panic");
                    PanicReaction
                },
            );
            let mut state = 0;
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
            result.unwrap_err();
            assert_eq!(state, expected_state);
            assert_eq!(
                drops.load(std::sync::atomic::Ordering::SeqCst),
                expected_drops
            );
        }
    }
}
}
