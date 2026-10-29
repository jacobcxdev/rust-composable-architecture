//! Borrows private child state alongside parent-owned shared state.

use core::fmt;
use core::hash::Hash;
use core::marker::PhantomData;

use super::keyed::KeyedLifecycle;
use super::missing_child::{MissingChildPolicy, missing_child};
use crate::effect::Program;
use crate::store::scope::{ScopeIdentity, ScopeSlot};
use crate::{ActionPath, Effect, KeyedAction, KeyedActionPath, KeyedState, ReduceContext, Reducer};

/// Mutates private and shared state during one synchronous reduction.
///
/// Both inputs are exclusively borrowed during reduction, including local
/// borrowed or non-`Send` domains. Actor-backed Store construction requires
/// owned `Send + 'static` values. Effects must own their captures; neither
/// mutable borrow can escape the reduction.
/// No copying, shared ownership or asynchronous aggregate view is provided.
pub trait SharedReducer {
    /// Private child state exclusively borrowed during reduction.
    type State;
    /// Parent-owned state borrowed by one child at a time.
    type Shared;
    /// Action borrowed during one reduction.
    type Action;

    /// Reduces one action using disjoint private and shared inputs.
    fn reduce(
        &mut self,
        state: &mut Self::State,
        shared: &mut Self::Shared,
        action: &Self::Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action>;
}

/// Lifts a shared child reducer into an ordinary parent reducer.
///
/// Matching actions borrow disjoint private and shared inputs. Each scope has
/// its own cancellation identity, even when siblings borrow the same shared
/// value. Nonmatching actions do not invoke the split projection. The first
/// match may allocate Store-local registration; warmed empty effects do not.
/// Compose this reducer with existing ordinary reducer combinators.
///
/// Panics retain the Store's abortive closure semantics, not state rollback.
pub struct SharedScope<ParentState, ParentAction, Child, Split, Action> {
    /// Reduces the two borrowed state inputs without retaining either borrow.
    child: Child,
    /// Selects disjoint private and shared inputs from the parent.
    split: Split,
    /// Borrows matching actions and embeds owned child output.
    action: Action,
    /// Declares this adapter's independent cancellation scope in each Store.
    slot: ScopeSlot,
    /// Relates the parent types without owning their values or restricting auto traits.
    marker: PhantomData<fn(ParentState, ParentAction)>,
}

impl<ParentState, ParentAction, Child, Split, Extract, Embed>
    SharedScope<
        ParentState,
        ParentAction,
        Child,
        Split,
        ActionPath<ParentAction, Child::Action, Extract, Embed>,
    >
where
    Child: SharedReducer,
    Split: for<'a> Fn(&'a mut ParentState) -> (&'a mut Child::State, &'a mut Child::Shared),
{
    /// Creates a required shared child scope with a disjoint split projection.
    #[must_use]
    pub const fn new(
        split: Split,
        action: ActionPath<ParentAction, Child::Action, Extract, Embed>,
        child: Child,
    ) -> Self {
        Self {
            child,
            split,
            action,
            slot: ScopeSlot::new(),
            marker: PhantomData,
        }
    }
}

impl<ParentState, ParentAction, Child, Split, Extract, Embed> Reducer
    for SharedScope<
        ParentState,
        ParentAction,
        Child,
        Split,
        ActionPath<ParentAction, Child::Action, Extract, Embed>,
    >
where
    ParentAction: 'static,
    Child: SharedReducer,
    Child::Action: 'static,
    Split: for<'a> Fn(&'a mut ParentState) -> (&'a mut Child::State, &'a mut Child::Shared),
    Extract: for<'a> Fn(&'a ParentAction) -> Option<&'a Child::Action>,
    Embed: Fn(Child::Action) -> ParentAction + Clone + Send + Sync + 'static,
{
    type State = ParentState;
    type Action = ParentAction;

    fn reduce(
        &mut self,
        state: &mut ParentState,
        action: &ParentAction,
        context: &mut ReduceContext<'_>,
    ) -> Effect<ParentAction> {
        let Some(child_action) = self.action.extract(action) else {
            return Effect::none();
        };
        let node = ReduceContext::node(&mut self.slot);
        let token = context.mount(node, ScopeIdentity::unit());
        let effect = context.with_scope(token, |context| {
            let (private, shared) = (self.split)(state);
            self.child.reduce(private, shared, child_action, context)
        });
        if matches!(&effect.program, Program::None) {
            Effect::none()
        } else {
            effect.in_scope(token).map(self.action.clone_embedder())
        }
    }
}

impl<ParentState, ParentAction, Child, Split, Action> fmt::Debug
    for SharedScope<ParentState, ParentAction, Child, Split, Action>
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedScope").finish_non_exhaustive()
    }
}

/// Lifts disjoint private and shared inputs into a required child reducer.
///
/// `split` must return two nonoverlapping fields of the same parent state.
/// Full root snapshots cover both inputs. Projected test snapshots must
/// explicitly select both to make the same exhaustivity claim.
///
/// # Examples
///
/// ```
/// use rca_core::{ActionPath, Effect, ReduceContext, SharedReducer, scope_shared};
/// struct Child;
/// impl SharedReducer for Child {
///     type State = u64;
///     type Shared = u64;
///     type Action = ();
///     fn reduce(
///         &mut self,
///         state: &mut u64,
///         shared: &mut u64,
///         (): &(),
///         _: &mut ReduceContext<'_>,
///     ) -> Effect<()> {
///         *state += *shared;
///         *shared += 1;
///         Effect::none()
///     }
/// }
/// let reducer = scope_shared(
///     |state: &mut (u64, u64)| (&mut state.0, &mut state.1),
///     ActionPath::new(|action: &()| Some(action), |action| action),
///     Child,
/// );
/// ```
#[must_use]
#[expect(
    clippy::type_complexity,
    reason = "preserves the concrete static shared reducer type"
)]
pub const fn scope_shared<ParentState, ParentAction, Child, Split, Extract, Embed>(
    split: Split,
    action: ActionPath<ParentAction, Child::Action, Extract, Embed>,
    child: Child,
) -> SharedScope<
    ParentState,
    ParentAction,
    Child,
    Split,
    ActionPath<ParentAction, Child::Action, Extract, Embed>,
>
where
    Child: SharedReducer,
    Split: for<'a> Fn(&'a mut ParentState) -> (&'a mut Child::State, &'a mut Child::Shared),
{
    SharedScope::new(split, action, child)
}

/// Composes keyed private children with one parent-owned shared value.
///
/// Each entry retains its independent generation and cancellation identity.
/// Matching actions reduce the child before the parent, reconciling lifetimes
/// before and after both reductions. Settings-only changes do not remount entries.
/// Nonmatching actions do not invoke the mutable split projection.
///
/// State and synchronous projections can borrow local, non-`Send` values.
/// Action types remain `'static` because returned effects can retain them.
/// Identifiers and the output embedder retain their `Send + Sync + 'static`
/// bounds for stored scope identities and delayed output. Actor-backed
/// [`crate::Store`] construction keeps its own transport requirements.
///
/// # Panics
///
/// Missing active IDs follow [`MissingChildPolicy`]: by default one required
/// issue must be accepted before the base runs, without invoking `split`.
/// Fail-fast policy, rejected delivery, reporter panics, contradictory split
/// projections and reconciliation epoch overflow remain fatal. No rollback is
/// provided for mutations made before a panic.
pub struct ForEachShared<ParentState, ParentAction, Parent, Child, Get, Split, Action, Id> {
    /// Runs after the child, before the final lifetime reconciliation.
    parent: Parent,
    /// Borrows one selected private value and the parent's shared input.
    child: Child,
    /// Defines the active child insertions before and after reduction.
    get_children: Get,
    /// Selects the private collection and shared input without overlapping borrows.
    split: Split,
    /// Borrows keyed input and embeds owned output under the retained identifier.
    action: Action,
    /// Keeps each insertion's scope separate from siblings and replacement entries.
    lifecycle: KeyedLifecycle<Id>,
    /// Selects required reporting or a panic when no active insertion matches.
    missing_child_policy: MissingChildPolicy,
    /// Relates the parent types without owning their values or restricting auto traits.
    marker: PhantomData<fn(ParentState, ParentAction)>,
}

impl<ParentState, ParentAction, Parent, Child, Get, Split, Extract, Embed, Id>
    ForEachShared<
        ParentState,
        ParentAction,
        Parent,
        Child,
        Get,
        Split,
        KeyedActionPath<ParentAction, Id, Child::Action, Extract, Embed>,
        Id,
    >
where
    Child: SharedReducer,
    Get: for<'a> Fn(&'a ParentState) -> &'a KeyedState<Id, Child::State>,
    Split: for<'a> Fn(
        &'a mut ParentState,
    ) -> (&'a mut KeyedState<Id, Child::State>, &'a mut Child::Shared),
{
    /// Creates a keyed shared-child reducer with disjoint state projections.
    ///
    /// `get_children` defines active insertion lifetimes. The collection selected
    /// by `split` must preserve the matched child's insertion lifetime.
    /// A cloned collection can retain that lifetime despite state-only changes.
    #[must_use]
    pub fn new(
        parent: Parent,
        get_children: Get,
        split: Split,
        action: KeyedActionPath<ParentAction, Id, Child::Action, Extract, Embed>,
        child: Child,
    ) -> Self {
        Self {
            parent,
            child,
            get_children,
            split,
            action,
            lifecycle: KeyedLifecycle::new(),
            missing_child_policy: MissingChildPolicy::ReportAndBase,
            marker: PhantomData,
        }
    }
}

impl<ParentState, ParentAction, Parent, Child, Get, Split, Action, Id> fmt::Debug
    for ForEachShared<ParentState, ParentAction, Parent, Child, Get, Split, Action, Id>
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ForEachShared").finish_non_exhaustive()
    }
}

impl<ParentState, ParentAction, Parent, Child, Get, Split, Extract, Embed, Id> Reducer
    for ForEachShared<
        ParentState,
        ParentAction,
        Parent,
        Child,
        Get,
        Split,
        KeyedActionPath<ParentAction, Id, Child::Action, Extract, Embed>,
        Id,
    >
where
    ParentAction: 'static,
    Parent: Reducer<State = ParentState, Action = ParentAction>,
    Child: SharedReducer,
    Child::Action: 'static,
    Id: Eq + Hash + Clone + Send + Sync + 'static,
    Get: for<'a> Fn(&'a ParentState) -> &'a KeyedState<Id, Child::State>,
    Split: for<'a> Fn(
        &'a mut ParentState,
    ) -> (&'a mut KeyedState<Id, Child::State>, &'a mut Child::Shared),
    Extract: for<'a> Fn(&'a ParentAction) -> Option<&'a KeyedAction<Id, Child::Action>>,
    Embed: Fn(KeyedAction<Id, Child::Action>) -> ParentAction + Clone + Send + Sync + 'static,
{
    type State = ParentState;
    type Action = ParentAction;

    fn reduce(
        &mut self,
        state: &mut ParentState,
        action: &ParentAction,
        context: &mut ReduceContext<'_>,
    ) -> Effect<ParentAction> {
        let (_, route) =
            self.lifecycle
                .reconcile_with((self.get_children)(state), context, |snapshot| {
                    self.action
                        .extract(action)
                        .map(|(id, child_action)| (id, child_action, snapshot.active(id)))
                });
        let child_effect = if let Some((id, child_action, active)) = route {
            if let Some((active_id, token, stamp)) = active {
                let (children, shared) = (self.split)(state);
                let (private, selected_stamp) = children.get_mut_with_stamp(id).expect(
                    "active shared keyed child must remain borrowable through the split projection",
                );
                assert!(
                    stamp.matches(selected_stamp),
                    "active shared keyed child must retain its insertion lifetime through the split projection"
                );
                drop(stamp);
                let effect = context.with_scope(token, |context| {
                    self.child.reduce(private, shared, child_action, context)
                });
                if matches!(&effect.program, Program::None) {
                    Effect::none()
                } else {
                    let embed = self.action.clone_embedder();
                    effect.in_scope(token).map(move |action| {
                        embed(KeyedAction::new(active_id.as_ref().clone(), action))
                    })
                }
            } else {
                missing_child::<ParentState, ParentAction, Child::State, Child::Action>(
                    self.missing_child_policy,
                    context,
                    "ForEachShared",
                    Some(core::any::type_name::<Id>()),
                );
                Effect::none()
            }
        } else {
            Effect::none()
        };
        let parent_effect = self.parent.reduce(state, action, context);
        self.lifecycle
            .reconcile((self.get_children)(state), context);
        Effect::merge([child_effect, parent_effect])
    }
}

/// Composes keyed private children with a shared parent-owned input.
///
/// `get_children` defines active insertion lifetimes. The collection selected
/// by `split` must preserve the matched child's insertion lifetime.
/// A cloned collection can retain that lifetime despite state-only changes.
/// `split` must lend nonoverlapping child and shared fields. Full root snapshots
/// cover both inputs; projected test snapshots must explicitly select both.
/// Sharing does not merge child cancellation identities or entry generations.
///
/// # Examples
///
/// ```
/// use rca_core::{Effect, KeyedAction, KeyedActionPath, KeyedState, ReduceContext,
///     Reducer, SharedReducer, for_each_shared};
/// struct Child;
/// impl SharedReducer for Child {
///     type State = u64;
///     type Shared = u64;
///     type Action = ();
///     fn reduce(
///         &mut self,
///         private: &mut u64,
///         shared: &mut u64,
///         (): &(),
///         _: &mut ReduceContext<'_>,
///     ) -> Effect<()> {
///         *private += *shared;
///         *shared += 1;
///         Effect::none()
///     }
/// }
/// struct Parent;
/// impl Reducer for Parent {
///     type State = (KeyedState<u8, u64>, u64);
///     type Action = KeyedAction<u8, ()>;
///     fn reduce(
///         &mut self,
///         _: &mut Self::State,
///         _: &Self::Action,
///         _: &mut ReduceContext<'_>,
///     ) -> Effect<Self::Action> {
///         Effect::none()
///     }
/// }
/// let reducer = for_each_shared(
///     Parent,
///     |s: &(KeyedState<u8, u64>, u64)| &s.0,
///     |s: &mut (KeyedState<u8, u64>, u64)| (&mut s.0, &mut s.1),
///     KeyedActionPath::new(|a: &KeyedAction<u8, ()>| Some(a), |a| a),
///     Child,
/// );
/// ```
#[must_use]
#[expect(
    clippy::type_complexity,
    reason = "preserves the concrete static keyed shared reducer type"
)]
pub fn for_each_shared<ParentState, ParentAction, Parent, Child, Get, Split, Extract, Embed, Id>(
    parent: Parent,
    get_children: Get,
    split: Split,
    action: KeyedActionPath<ParentAction, Id, Child::Action, Extract, Embed>,
    child: Child,
) -> ForEachShared<
    ParentState,
    ParentAction,
    Parent,
    Child,
    Get,
    Split,
    KeyedActionPath<ParentAction, Id, Child::Action, Extract, Embed>,
    Id,
>
where
    Child: SharedReducer,
    Get: for<'a> Fn(&'a ParentState) -> &'a KeyedState<Id, Child::State>,
    Split: for<'a> Fn(
        &'a mut ParentState,
    ) -> (&'a mut KeyedState<Id, Child::State>, &'a mut Child::Shared),
{
    ForEachShared::new(parent, get_children, split, action, child)
}

#[cfg(test)]
ordinary_test_items! {
mod local_tests {
    use std::{cell::Cell, rc::Rc};

    use super::*;
    use crate::store::scope::ScopeRegistry;
    use crate::{DependencyValues, MonotonicInstant};

    struct Local<'a>(&'a Cell<usize>);

    impl<'a> SharedReducer for Local<'a> {
        type State = Rc<Cell<usize>>;
        type Shared = &'a Cell<usize>;
        type Action = &'a Rc<usize>;

        fn reduce(
            &mut self,
            state: &mut Self::State,
            shared: &mut Self::Shared,
            action: &Self::Action,
            _: &mut ReduceContext<'_>,
        ) -> Effect<Self::Action> {
            self.0.set(self.0.get() + 1);
            state.set(state.get() + ***action);
            shared.set(shared.get() + state.get());
            Effect::none()
        }
    }

    #[test]
    fn borrowed_non_send_shared_reducer_reduces_locally() {
        let calls = Cell::new(0);
        let shared = Cell::new(2);
        let amount = Rc::new(3);
        let mut state = Rc::new(Cell::new(4));
        let mut borrowed_shared = &shared;
        let mut reducer = Local(&calls);
        let mut dependencies = DependencyValues::new();
        let (_scopes_lifecycle, mut scopes) = ScopeRegistry::for_test();
        let mut context_transitions = crate::store::scope::ScopeTransitionBatch::default();
        let mut context = ReduceContext::new(
            &mut dependencies,
            MonotonicInstant::ZERO,
            &mut scopes,
            &mut context_transitions,
        );

        let effect = reducer.reduce(&mut state, &mut borrowed_shared, &&amount, &mut context);
        assert!(matches!(effect.program, Program::None));
        assert_eq!(calls.get(), 1);
        assert_eq!(state.get(), 7);
        assert_eq!(shared.get(), 9);
    }
}
}

#[cfg(test)]
ordinary_test_items! {
mod keyed_tests {
    use super::*;
    use crate::store::scope::ScopeRegistry;
    use crate::{DependencyValues, MonotonicInstant};
    use std::panic::{AssertUnwindSafe, catch_unwind};

    struct State {
        rows: KeyedState<u8, u64>,
        other: KeyedState<u8, u64>,
        shared: u64,
        parent_calls: usize,
    }
    enum Action {
        Child(KeyedAction<u8, ()>),
        Settings,
    }
    struct Parent;
    impl Reducer for Parent {
        type State = State;
        type Action = Action;
        fn reduce(
            &mut self,
            state: &mut State,
            _: &Action,
            _: &mut ReduceContext<'_>,
        ) -> Effect<Action> {
            state.shared += 1;
            state.parent_calls += 1;
            Effect::none()
        }
    }
    struct Child;
    impl SharedReducer for Child {
        type State = u64;
        type Shared = u64;
        type Action = ();
        fn reduce(
            &mut self,
            state: &mut u64,
            shared: &mut u64,
            (): &(),
            _: &mut ReduceContext<'_>,
        ) -> Effect<()> {
            *state += *shared;
            Effect::none()
        }
    }
    fn state() -> State {
        State {
            rows: [(1, 0), (2, 0)].into_iter().collect(),
            other: KeyedState::new(),
            shared: 1,
            parent_calls: 0,
        }
    }

    // Changes to shared state must not trigger scans or replace either child token.
    #[test]
    fn settings_only_and_private_changes_preserve_reconciliation_and_entry_tokens() {
        let mut reducer = for_each_shared(
            Parent,
            |s: &State| &s.rows,
            |s: &mut State| (&mut s.rows, &mut s.shared),
            KeyedActionPath::new(
                |a: &Action| match a {
                    Action::Child(a) => Some(a),
                    Action::Settings => None,
                },
                Action::Child,
            ),
            Child,
        );
        let mut state = state();
        let mut dependencies = DependencyValues::new();
        let (_scopes_lifecycle, mut scopes) = ScopeRegistry::for_test();
        let mut context_transitions = crate::store::scope::ScopeTransitionBatch::default();
        let mut context = ReduceContext::new(
            &mut dependencies,
            MonotonicInstant::ZERO,
            &mut scopes,
            &mut context_transitions,
        );
        let _ = reducer.reduce(&mut state, &Action::Settings, &mut context);
        let (_, (first, second)) = reducer.lifecycle.reconcile_with(
            &state.rows,
            &mut context,
            |snapshot| (snapshot.active(&1).unwrap().1, snapshot.active(&2).unwrap().1),
        );
        for _ in 0..100 {
            let _ = reducer.reduce(&mut state, &Action::Settings, &mut context);
        }
        let _ = reducer.reduce(
            &mut state,
            &Action::Child(KeyedAction::new(1, ())),
            &mut context,
        );
        assert_eq!(reducer.lifecycle.full_reconciliations(), 1);
        let (_, current) = reducer.lifecycle.reconcile_with(
            &state.rows,
            &mut context,
            |snapshot| (snapshot.active(&1).unwrap().1, snapshot.active(&2).unwrap().1),
        );
        assert_eq!(current, (first, second));
        assert_eq!(state.rows[&1], 102);
        assert_eq!(state.rows[&2], 0);
        assert_eq!(state.shared, 103);
    }

    // Both absent active IDs and inconsistent projections must panic before parent.
    #[test]
    fn missing_active_or_projected_child_panics_before_parent_reduction() {
        for active_missing in [true, false] {
            let mut reducer = for_each_shared(
                Parent,
                |s: &State| &s.rows,
                |s: &mut State| (&mut s.other, &mut s.shared),
                KeyedActionPath::new(
                    |a: &Action| match a {
                        Action::Child(a) => Some(a),
                        Action::Settings => None,
                    },
                    Action::Child,
                ),
                Child,
            )
            .missing_child_policy(if active_missing {
                MissingChildPolicy::FailFast
            } else {
                MissingChildPolicy::ReportAndBase
            });
            let mut state = state();
            let mut dependencies = DependencyValues::new();
            let (_scopes_lifecycle, mut scopes) = ScopeRegistry::for_test();
            let mut context_transitions = crate::store::scope::ScopeTransitionBatch::default();
            let mut context = ReduceContext::new(
                &mut dependencies,
                MonotonicInstant::ZERO,
                &mut scopes,
                &mut context_transitions,
            );
            let panic = catch_unwind(AssertUnwindSafe(|| {
                let id = if active_missing { 3 } else { 1 };
                let _ = reducer.reduce(
                    &mut state,
                    &Action::Child(KeyedAction::new(id, ())),
                    &mut context,
                );
            }))
            .unwrap_err();
            let text = panic
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| panic.downcast_ref::<&str>().copied())
                .unwrap();
            if active_missing {
                assert!(text.contains("ForEachShared received keyed child action"));
                assert!(text.contains("was absent from KeyedState"));
            } else {
                assert!(text.contains("active shared keyed child must remain borrowable"));
            }
            assert_eq!(state.parent_calls, 0);
            assert_eq!(state.shared, 1);
        }
    }

    /// Checks the actual split selection before either private or shared state can change.
    fn assert_split_insertion_lifetime(
        rows: KeyedState<u8, u64>,
        other: KeyedState<u8, u64>,
        same_lifetime: bool,
    ) {
        let mut reducer = for_each_shared(
            Parent,
            |s: &State| &s.rows,
            |s: &mut State| (&mut s.other, &mut s.shared),
            KeyedActionPath::new(
                |a: &Action| match a {
                    Action::Child(a) => Some(a),
                    Action::Settings => None,
                },
                Action::Child,
            ),
            Child,
        );
        let mut state = State {
            rows,
            other,
            shared: 1,
            parent_calls: 0,
        };
        let mut dependencies = DependencyValues::new();
        let (_scopes_lifecycle, mut scopes) = ScopeRegistry::for_test();
        let mut transitions = crate::store::scope::ScopeTransitionBatch::default();
        let mut context = ReduceContext::new(
            &mut dependencies,
            MonotonicInstant::ZERO,
            &mut scopes,
            &mut transitions,
        );

        let result = catch_unwind(AssertUnwindSafe(|| {
            reducer.reduce(
                &mut state,
                &Action::Child(KeyedAction::new(1, ())),
                &mut context,
            )
        }));
        assert_eq!(state.rows[&1], 10);
        if same_lifetime {
            assert!(
                result.is_ok(),
                "a cloned insertion accepts state-only changes"
            );
            assert_eq!(state.other[&1], 21);
            assert_eq!(state.shared, 2);
            assert_eq!(state.parent_calls, 1);
        } else {
            assert!(
                result.is_err(),
                "a different insertion must fail before reduction"
            );
            assert_eq!(state.other[&1], 20);
            assert_eq!(state.shared, 1);
            assert_eq!(state.parent_calls, 0);
        }
    }

    #[test]
    fn split_rejects_equal_id_from_independent_collection_before_reduction() {
        assert_split_insertion_lifetime(
            [(1, 10)].into_iter().collect(),
            [(1, 20)].into_iter().collect(),
            false,
        );
    }

    #[test]
    fn split_rejects_reinserted_id_from_clone_branch_before_reduction() {
        let rows: KeyedState<_, _> = [(1, 10)].into_iter().collect();
        let mut other = rows.clone();
        assert_eq!(other.remove(&1), Some(10));
        other.insert(1, 20);
        assert_split_insertion_lifetime(rows, other, false);
    }

    #[test]
    fn split_accepts_same_insertion_with_state_only_change() {
        let rows: KeyedState<_, _> = [(1, 10)].into_iter().collect();
        let mut other = rows.clone();
        *other.get_mut(&1).unwrap() = 20;
        assert_split_insertion_lifetime(rows, other, true);
    }
}
}

impl<ParentState, ParentAction, Parent, Child, Get, Split, Action, Id>
    ForEachShared<ParentState, ParentAction, Parent, Child, Get, Split, Action, Id>
{
    /// Selects the response to an authorised matching action with absent child state.
    ///
    /// The default requires accepted issue delivery before base reduction.
    /// Contradictory projections and stale scope authority are never recovered.
    #[must_use]
    pub const fn missing_child_policy(mut self, policy: MissingChildPolicy) -> Self {
        self.missing_child_policy = policy;
        self
    }
}

#[cfg(all(test, feature = "allocation-measurement"))]
mod allocation_tests {
    use std::hint::black_box;

    use super::{
        ActionPath, Effect, KeyedAction, KeyedActionPath, KeyedState, Program, ReduceContext,
        Reducer, SharedReducer, for_each_shared, scope_shared,
    };
    use crate::store::scope::ScopeRegistry;
    use crate::{DependencyValues, MonotonicInstant, StatePath, scope};

    struct Child;
    impl SharedReducer for Child {
        type State = u64;
        type Shared = u64;
        type Action = ();
        fn reduce(
            &mut self,
            state: &mut u64,
            shared: &mut u64,
            (): &(),
            _: &mut ReduceContext<'_>,
        ) -> Effect<()> {
            *state += 1;
            *shared += 1;
            Effect::none()
        }
    }
    impl Reducer for Child {
        type State = u64;
        type Action = ();
        fn reduce(&mut self, state: &mut u64, (): &(), _: &mut ReduceContext<'_>) -> Effect<()> {
            *state += 1;
            Effect::none()
        }
    }

    fn reduce<R: Reducer<State = (u64, u64), Action = bool>>(
        reducer: &mut R,
        state: &mut (u64, u64),
        action: bool,
        context: &mut ReduceContext<'_>,
    ) {
        let effect = black_box(reducer.reduce(state, &action, context));
        assert!(matches!(effect.program, Program::None));
    }

    pub(crate) fn warmed_shared_empty_effect_matches_ordinary_scope_allocations() {
        if crate::allocation_test::run_in_child(
            "composition::shared::allocation_tests::warmed_shared_empty_effect_matches_ordinary_scope_allocations",
        ) {
            return;
        }
        let mut ordinary = scope(
            StatePath::new(|s: &(u64, u64)| &s.0, |s: &mut (u64, u64)| &mut s.0),
            ActionPath::new(|a: &bool| a.then_some(&()), |()| true),
            Child,
        );
        let mut shared = scope_shared(
            |s: &mut (u64, u64)| (&mut s.0, &mut s.1),
            ActionPath::new(|a: &bool| a.then_some(&()), |()| true),
            Child,
        );
        let mut state = (0, 0);
        let mut dependencies = DependencyValues::new();
        let (_scopes_lifecycle, mut scopes) = ScopeRegistry::for_test();
        let mut context_transitions = crate::store::scope::ScopeTransitionBatch::default();
        let mut context = ReduceContext::new(
            &mut dependencies,
            MonotonicInstant::ZERO,
            &mut scopes,
            &mut context_transitions,
        );
        for _ in 0..20 {
            reduce(&mut ordinary, &mut state, true, &mut context);
            reduce(&mut shared, &mut state, true, &mut context);
        }
        let profiler = dhat::Profiler::builder().testing().build();
        let before = dhat::HeapStats::get();
        for _ in 0..100 {
            reduce(&mut ordinary, &mut state, true, &mut context);
            reduce(&mut ordinary, &mut state, false, &mut context);
        }
        let ordinary_stats = dhat::HeapStats::get();
        for _ in 0..100 {
            reduce(&mut shared, &mut state, true, &mut context);
            reduce(&mut shared, &mut state, false, &mut context);
        }
        let shared_stats = dhat::HeapStats::get();
        let ordinary_delta = (
            ordinary_stats.total_blocks - before.total_blocks,
            ordinary_stats.total_bytes - before.total_bytes,
        );
        let shared_delta = (
            shared_stats.total_blocks - ordinary_stats.total_blocks,
            shared_stats.total_bytes - ordinary_stats.total_bytes,
        );
        assert_eq!(ordinary_delta, (0, 0));
        assert_eq!(shared_delta, ordinary_delta);
        assert_eq!(state, (240, 120));
        drop(profiler);
        println!(
            "100 matching + 100 nonmatching: ordinary {ordinary_delta:?}, shared {shared_delta:?} (blocks, bytes)"
        );
    }

    struct KeyedParent;
    impl Reducer for KeyedParent {
        type State = (KeyedState<u64, u64>, u64);
        type Action = Option<KeyedAction<u64, ()>>;
        fn reduce(
            &mut self,
            state: &mut Self::State,
            _: &Self::Action,
            _: &mut ReduceContext<'_>,
        ) -> Effect<Self::Action> {
            state.1 += 1;
            Effect::none()
        }
    }

    pub(crate) fn warmed_shared_keyed_paths_do_not_allocate_or_remount() {
        if crate::allocation_test::run_in_child(
            "composition::shared::allocation_tests::warmed_shared_keyed_paths_do_not_allocate_or_remount",
        ) {
            return;
        }
        let profiler = dhat::Profiler::builder().testing().build();
        for size in [10_u64, 1_000, 100_000] {
            let mut reducer = for_each_shared(
                KeyedParent,
                |s: &(KeyedState<u64, u64>, u64)| &s.0,
                |s: &mut (KeyedState<u64, u64>, u64)| (&mut s.0, &mut s.1),
                KeyedActionPath::new(|a: &Option<KeyedAction<u64, ()>>| a.as_ref(), Some),
                Child,
            );
            let mut state = ((0..size).map(|id| (id, 0)).collect(), 0);
            let mut dependencies = DependencyValues::new();
            let (_scopes_lifecycle, mut scopes) = ScopeRegistry::for_test();
            let mut context_transitions = crate::store::scope::ScopeTransitionBatch::default();
            let mut context = ReduceContext::new(
                &mut dependencies,
                MonotonicInstant::ZERO,
                &mut scopes,
                &mut context_transitions,
            );
            let _ = reducer.reduce(&mut state, &None, &mut context);
            let (_, token) = reducer
                .lifecycle
                .reconcile_with(&state.0, &mut context, |snapshot| {
                    snapshot.active(&(size / 2)).unwrap().1
                });
            for (label, action) in [
                ("settings_only", None),
                ("matching", Some(KeyedAction::new(size / 2, ()))),
            ] {
                let before = dhat::HeapStats::get();
                for _ in 0..100 {
                    let effect = black_box(reducer.reduce(&mut state, &action, &mut context));
                    assert!(matches!(effect.program, Program::None));
                }
                let after = dhat::HeapStats::get();
                let blocks = after.total_blocks - before.total_blocks;
                let bytes = after.total_bytes - before.total_bytes;
                assert_eq!((blocks, bytes), (0, 0));
                eprintln!("shared_keyed/{label}/{size}: blocks={blocks} bytes={bytes}");
            }
            assert_eq!(reducer.lifecycle.full_reconciliations(), 1);
            let (_, current) =
                reducer
                    .lifecycle
                    .reconcile_with(&state.0, &mut context, |snapshot| {
                        snapshot.active(&(size / 2)).unwrap().1
                    });
            assert_eq!(current, token);
            assert_eq!(state.0[&(size / 2)], 100);
            assert_eq!(state.1, 301);
        }
        drop(profiler);
    }
}

#[cfg(all(test, feature = "allocation-measurement"))]
pub(crate) use allocation_tests::{
    warmed_shared_empty_effect_matches_ordinary_scope_allocations,
    warmed_shared_keyed_paths_do_not_allocate_or_remount,
};
