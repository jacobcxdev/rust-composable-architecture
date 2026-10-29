//! Lifts required child reducers into parent domains.

use core::fmt;
use core::marker::PhantomData;

use crate::effect::Program;
use crate::store::scope::{ScopeIdentity, ScopeSlot};
use crate::{ActionPath, Effect, ReduceContext, Reducer, StatePath};

use super::lifecycle::reconcile_identified;

/// Holds a required child reducer projected into parent state and actions.
///
/// Construct this type with [`Scope::new`] or [`scope`]. Matching actions
/// borrow child state and action. Owned child effect outputs are embedded into
/// parent actions. Nonmatching paths allocate nothing. A first matching
/// reduction may allocate Store-local registration; warmed matching empty
/// effects and scope tagging add no allocation.
///
/// State may borrow local data, and owned parent and child actions need not be
/// `Send`. Both action types must be `'static` because total output mapping
/// uses the concrete [`Effect`] representation. This boundary is asymmetric:
/// local leaf reducers may accept arbitrary-lifetime actions, but those action
/// domains cannot be lifted through this required scope, even if a particular
/// reduction returns no effects. The owned embedding function separately
/// requires `Clone + Send + Sync + 'static`; these are not payload bounds.
/// Asynchronous captures and actor admission retain their own requirements.
pub struct Scope<ParentState, ParentAction, Child, State, Action> {
    /// Reduces only actions accepted by the child action path.
    child: Child,
    /// Retains synchronous projection captures, not a borrow of parent state.
    state: State,
    /// Borrows inputs and supplies independently owned output-routing metadata.
    action: Action,
    /// Identifies this declaration site across reductions and Store instances.
    slot: ScopeSlot,
    /// Records input domains without owning their values or imposing auto traits.
    marker: PhantomData<fn(ParentState, ParentAction)>,
}

impl<ParentState, ParentAction, Child, ChildState, ChildAction, Get, GetMut, Extract, Embed>
    Scope<
        ParentState,
        ParentAction,
        Child,
        StatePath<ParentState, ChildState, Get, GetMut>,
        ActionPath<ParentAction, ChildAction, Extract, Embed>,
    >
{
    /// Creates a required child reducer scope.
    #[must_use]
    pub const fn new(
        state: StatePath<ParentState, ChildState, Get, GetMut>,
        action: ActionPath<ParentAction, ChildAction, Extract, Embed>,
        child: Child,
    ) -> Self {
        Self {
            child,
            state,
            action,
            slot: ScopeSlot::new(),
            marker: PhantomData,
        }
    }
}

impl<ParentState, ParentAction, Child, ChildState, ChildAction, Get, GetMut, Extract, Embed> Reducer
    for Scope<
        ParentState,
        ParentAction,
        Child,
        StatePath<ParentState, ChildState, Get, GetMut>,
        ActionPath<ParentAction, ChildAction, Extract, Embed>,
    >
where
    ParentAction: 'static,
    Child: Reducer<State = ChildState, Action = ChildAction>,
    ChildAction: 'static,
    Get: for<'a> Fn(&'a ParentState) -> &'a ChildState,
    GetMut: for<'a> Fn(&'a mut ParentState) -> &'a mut ChildState,
    Extract: for<'a> Fn(&'a ParentAction) -> Option<&'a ChildAction>,
    Embed: Fn(ChildAction) -> ParentAction + Clone + Send + Sync + 'static,
{
    type State = ParentState;
    type Action = ParentAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        let Some(child_action) = self.action.extract(action) else {
            return Effect::none();
        };

        let node = ReduceContext::node(&mut self.slot);
        let token = context.mount(node, ScopeIdentity::unit());
        let effect = context.with_scope(token, |context| {
            self.child
                .reduce(self.state.get_mut(state), child_action, context)
        });
        if matches!(&effect.program, Program::None) {
            Effect::none()
        } else {
            effect.in_scope(token).map(self.action.clone_embedder())
        }
    }
}

impl<ParentState, ParentAction, Child, State, Action> fmt::Debug
    for Scope<ParentState, ParentAction, Child, State, Action>
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Scope").finish_non_exhaustive()
    }
}

/// Lifts a required child reducer into parent state and action domains.
#[must_use]
#[expect(
    clippy::type_complexity,
    reason = "the constructor preserves Scope's concrete static reducer type"
)]
pub const fn scope<
    ParentState,
    ParentAction,
    Child,
    ChildState,
    ChildAction,
    Get,
    GetMut,
    Extract,
    Embed,
>(
    state: StatePath<ParentState, ChildState, Get, GetMut>,
    action: ActionPath<ParentAction, ChildAction, Extract, Embed>,
    child: Child,
) -> Scope<
    ParentState,
    ParentAction,
    Child,
    StatePath<ParentState, ChildState, Get, GetMut>,
    ActionPath<ParentAction, ChildAction, Extract, Embed>,
> {
    Scope::new(state, action, child)
}

/// Holds an identity-aware required child and its parent reducer.
///
/// Matching child actions reduce before the parent. A changed identity after
/// parent reduction ends the old required-child lifetime and mounts a fresh
/// generation without replaying the current action.
///
/// Before child reduction, the mutable projection must select the observed
/// semantic identity. It is evaluated once; a different identity panics before
/// either reducer runs. Equal identities need not occupy the same address.
pub struct ScopeIdentified<ParentState, ParentAction, Parent, Child, State, Action, Identity, Id> {
    /// Runs after the matching child and may replace its identity.
    parent: Parent,
    /// Runs within the selected child lifetime before parent reduction.
    child: Child,
    /// Selects required state for identity inspection and exclusive reduction.
    state: State,
    /// Borrows child inputs and supplies owned effect-output routing.
    action: Action,
    /// Borrows the semantic identifier that controls the child's lifetime.
    identity: Identity,
    /// Identifies the declaration site independently of the current child identity.
    slot: ScopeSlot,
    /// Records input and identity domains without owning their values.
    marker: PhantomData<fn(ParentState, ParentAction) -> Id>,
}

impl<
    ParentState,
    ParentAction,
    Parent,
    Child,
    ChildState,
    ChildAction,
    Get,
    GetMut,
    Extract,
    Embed,
    Identity,
    Id,
>
    ScopeIdentified<
        ParentState,
        ParentAction,
        Parent,
        Child,
        StatePath<ParentState, ChildState, Get, GetMut>,
        ActionPath<ParentAction, ChildAction, Extract, Embed>,
        Identity,
        Id,
    >
{
    /// Creates an identity-aware required child reducer.
    #[must_use]
    pub const fn new(
        parent: Parent,
        state: StatePath<ParentState, ChildState, Get, GetMut>,
        action: ActionPath<ParentAction, ChildAction, Extract, Embed>,
        identity: Identity,
        child: Child,
    ) -> Self {
        Self {
            parent,
            child,
            state,
            action,
            identity,
            slot: ScopeSlot::new(),
            marker: PhantomData,
        }
    }
}

impl<
    ParentState,
    ParentAction,
    Parent,
    Child,
    ChildState,
    ChildAction,
    Get,
    GetMut,
    Extract,
    Embed,
    Identity,
    Id,
> Reducer
    for ScopeIdentified<
        ParentState,
        ParentAction,
        Parent,
        Child,
        StatePath<ParentState, ChildState, Get, GetMut>,
        ActionPath<ParentAction, ChildAction, Extract, Embed>,
        Identity,
        Id,
    >
where
    ParentAction: 'static,
    Parent: Reducer<State = ParentState, Action = ParentAction>,
    Child: Reducer<State = ChildState, Action = ChildAction>,
    ChildAction: 'static,
    Get: for<'a> Fn(&'a ParentState) -> &'a ChildState,
    GetMut: for<'a> Fn(&'a mut ParentState) -> &'a mut ChildState,
    Extract: for<'a> Fn(&'a ParentAction) -> Option<&'a ChildAction>,
    Embed: Fn(ChildAction) -> ParentAction + Clone + Send + Sync + 'static,
    Identity: for<'a> Fn(&'a ChildState) -> &'a Id,
    Id: Eq + core::hash::Hash + Clone + Send + Sync + 'static,
{
    type State = ParentState;
    type Action = ParentAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        let before_id = (self.identity)(self.state.get(state)).clone();
        let node = ReduceContext::node(&mut self.slot);
        let token = context.mount(node, ScopeIdentity::new(before_id.clone()));
        let child_effect = if let Some(child_action) = self.action.extract(action) {
            let effect = context.with_scope(token, |context| {
                let child = self.state.get_mut(state);
                assert!(
                    (self.identity)(child) == &before_id,
                    "required child projection must preserve the observed identity"
                );
                self.child.reduce(child, child_action, context)
            });
            if matches!(&effect.program, Program::None) {
                Effect::none()
            } else {
                effect.in_scope(token).map(self.action.clone_embedder())
            }
        } else {
            Effect::none()
        };

        let parent_effect = self.parent.reduce(state, action, context);
        let after_id = (self.identity)(self.state.get(state));
        reconcile_identified(
            &mut self.slot,
            Some((&before_id, token)),
            Some(after_id),
            context,
        );
        let effect = Effect::merge([child_effect, parent_effect]);
        // Keep effect payloads locally owned if destroying the snapshot panics.
        drop(before_id);
        effect
    }
}

impl<ParentState, ParentAction, Parent, Child, State, Action, Identity, Id> fmt::Debug
    for ScopeIdentified<ParentState, ParentAction, Parent, Child, State, Action, Identity, Id>
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScopeIdentified").finish_non_exhaustive()
    }
}

/// Composes a parent with an identity-aware required child reducer.
#[must_use]
#[expect(
    clippy::type_complexity,
    reason = "the constructor preserves ScopeIdentified's concrete static reducer type"
)]
pub const fn scope_identified<
    ParentState,
    ParentAction,
    Parent,
    Child,
    ChildState,
    ChildAction,
    Get,
    GetMut,
    Extract,
    Embed,
    Identity,
    Id,
>(
    parent: Parent,
    state: StatePath<ParentState, ChildState, Get, GetMut>,
    action: ActionPath<ParentAction, ChildAction, Extract, Embed>,
    identity: Identity,
    child: Child,
) -> ScopeIdentified<
    ParentState,
    ParentAction,
    Parent,
    Child,
    StatePath<ParentState, ChildState, Get, GetMut>,
    ActionPath<ParentAction, ChildAction, Extract, Embed>,
    Identity,
    Id,
>
where
    Identity: for<'a> Fn(&'a ChildState) -> &'a Id,
{
    ScopeIdentified::new(parent, state, action, identity, child)
}

#[cfg(test)]
ordinary_test_items! {
mod tests {
    use std::cell::{Cell, RefCell};
    use std::hash::{Hash, Hasher};
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::rc::Rc;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{ActionPath, Effect, ReduceContext, Reducer, StatePath, scope, scope_identified};
    use crate::store::scope::{ScopeRegistry, ScopeTransitionBatch};
    use crate::{DependencyValues, MonotonicInstant, Reduce};

    /// Counts owned identifier copies without making instrumentation part of equality.
    struct CloneCountId {
        value: u8,
        clones: Arc<AtomicUsize>,
    }

    impl Clone for CloneCountId {
        fn clone(&self) -> Self {
            self.clones.fetch_add(1, Ordering::Relaxed);
            Self {
                value: self.value,
                clones: Arc::clone(&self.clones),
            }
        }
    }

    impl PartialEq for CloneCountId {
        fn eq(&self, other: &Self) -> bool {
            self.value == other.value
        }
    }

    impl Eq for CloneCountId {}

    impl Hash for CloneCountId {
        fn hash<H: Hasher>(&self, state: &mut H) {
            self.value.hash(state);
        }
    }

    #[test]
    fn identified_scope_clones_only_owned_snapshots_and_changed_identity() {
        let clones = Arc::new(AtomicUsize::new(0));
        let selected = RefCell::new(Vec::new());
        let mut state = CloneCountId {
            value: 7,
            clones: Arc::clone(&clones),
        };
        let mut reducer = scope_identified(
            Reduce::new(
                |state: &mut CloneCountId, action: &Option<u8>, _: &mut ReduceContext<'_>| {
                    if let Some(value) = action {
                        state.value = *value;
                    }
                    Effect::none()
                },
            ),
            StatePath::new(
                |state: &CloneCountId| state,
                |state: &mut CloneCountId| state,
            ),
            ActionPath::new(|_: &Option<u8>| Some(&()), |()| None),
            |state: &CloneCountId| state,
            Reduce::new(
                |_: &mut CloneCountId, (): &(), context: &mut ReduceContext<'_>| {
                    selected.borrow_mut().push(context.scope_token());
                    Effect::none()
                },
            ),
        );
        let mut dependencies = DependencyValues::new();
        let (_owner, mut scopes) = ScopeRegistry::for_test();
        let mut transitions = ScopeTransitionBatch::default();
        let mut context = ReduceContext::new(
            &mut dependencies,
            MonotonicInstant::ZERO,
            &mut scopes,
            &mut transitions,
        );
        let mut counts = Vec::new();

        for action in [None, None, Some(8), None, Some(7), None] {
            clones.store(0, Ordering::Relaxed);
            let _ = reducer.reduce(&mut state, &action, &mut context);
            counts.push(clones.load(Ordering::Relaxed));
        }

        let selected = selected.borrow();
        assert_eq!(selected[0], selected[1]);
        assert_eq!(selected[1], selected[2]);
        assert_eq!(selected[3], selected[4]);
        assert_ne!(selected[0], selected[3]);
        assert_ne!(selected[0], selected[5]);
        assert_ne!(selected[3], selected[5]);
        assert!(!context.scopes().is_current(selected[0]));
        assert!(!context.scopes().is_current(selected[3]));
        assert!(context.scopes().is_current(selected[5]));
        assert_eq!(state.value, 7);
        eprintln!("identified_scope/identity_clones: {counts:?}");
        assert_eq!(counts, [2, 2, 3, 2, 3, 2]);
    }

    #[test]
    fn local_scope_inputs_do_not_escape_with_non_send_effect_outputs() {
        let value = Rc::new(Cell::new(4_u8));
        let projection_calls = Rc::new(Cell::new(0));
        let extraction_calls = Rc::new(Cell::new(0));
        let effect = {
            let mut reducer = scope(
                StatePath::new(
                    |state: &(&Rc<Cell<u8>>, ())| &state.0,
                    |state: &mut (&Rc<Cell<u8>>, ())| {
                        projection_calls.set(projection_calls.get() + 1);
                        &mut state.0
                    },
                ),
                ActionPath::new(
                    |action: &Rc<u8>| {
                        extraction_calls.set(extraction_calls.get() + 1);
                        Some(action)
                    },
                    core::convert::identity,
                ),
                Reduce::new(
                    |state: &mut &Rc<Cell<u8>>, action: &Rc<u8>, _: &mut ReduceContext<'_>| {
                        state.set(state.get() + **action);
                        Effect::future(async { Some(9_u8) }).map(Rc::new)
                    },
                ),
            );
            let mut state = (&value, ());
            let mut dependencies = DependencyValues::new();
            let (_owner, mut scopes) = ScopeRegistry::for_test();
            let mut transitions = ScopeTransitionBatch::default();
            let mut context = ReduceContext::new(
                &mut dependencies,
                MonotonicInstant::ZERO,
                &mut scopes,
                &mut transitions,
            );
            reducer.reduce(&mut state, &Rc::new(3), &mut context)
        };

        assert_eq!(value.get(), 7);
        assert_eq!(projection_calls.get(), 1);
        assert_eq!(extraction_calls.get(), 1);
        assert_eq!(Rc::strong_count(&value), 1);
        assert!(effect.scope.is_some());
        let super::Program::Future(future) = effect.program else {
            panic!("the scoped output retains its deferred mapping");
        };
        let output = futures_executor::block_on(future).unwrap();
        assert_eq!(*output, 9);
        assert_eq!(Rc::strong_count(&output), 1);
        let weak = Rc::downgrade(&output);
        drop(output);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn identified_scope_rejects_a_different_mutable_identity_before_reduction() {
        for selected_id in [7, 8] {
            let parent_calls = Cell::new(0);
            let mutable_selections = Cell::new(0);
            let mut reducer = scope_identified(
                Reduce::new(
                    |_: &mut ((u8, usize), (u8, usize)), (): &(), _: &mut ReduceContext<'_>| {
                        parent_calls.set(parent_calls.get() + 1);
                        Effect::none()
                    },
                ),
                StatePath::new(
                    |state: &((u8, usize), (u8, usize))| &state.0,
                    |state: &mut ((u8, usize), (u8, usize))| {
                        mutable_selections.set(mutable_selections.get() + 1);
                        &mut state.1
                    },
                ),
                ActionPath::new(|action: &()| Some(action), core::convert::identity),
                |state: &(u8, usize)| &state.0,
                Reduce::new(
                    |state: &mut (u8, usize), (): &(), _: &mut ReduceContext<'_>| {
                        state.1 += 1;
                        Effect::none()
                    },
                ),
            );
            let mut state = ((7, 0), (selected_id, 0));
            let mut dependencies = DependencyValues::new();
            let (_owner, mut scopes) = ScopeRegistry::for_test();
            let mut transitions = ScopeTransitionBatch::default();
            let mut context = ReduceContext::new(
                &mut dependencies,
                MonotonicInstant::ZERO,
                &mut scopes,
                &mut transitions,
            );

            let root = context.scope_token();
            let result = catch_unwind(AssertUnwindSafe(|| {
                reducer.reduce(&mut state, &(), &mut context)
            }));

            assert_eq!(context.scope_token(), root);
            assert_eq!(mutable_selections.get(), 1);

            if selected_id == 7 {
                let _ = result.expect("equal semantic identities need not share an address");
                assert_eq!(state, ((7, 0), (7, 1)));
                assert_eq!(parent_calls.get(), 1);
            } else {
                result.expect_err("a different identity must not inherit observed authority");
                assert_eq!(state, ((7, 0), (8, 0)));
                assert_eq!(parent_calls.get(), 0);
            }
        }
    }
}
}

#[cfg(all(test, feature = "allocation-measurement"))]
mod allocation_tests {
    use std::hint::black_box;

    use super::{ActionPath, Effect, Program, ReduceContext, Reducer, StatePath, scope};
    use crate::store::scope::ScopeRegistry;
    use crate::{DependencyValues, MonotonicInstant};

    struct State {
        calls: usize,
    }

    struct Action {
        depth: usize,
    }

    struct Leaf;

    impl Reducer for Leaf {
        type State = State;
        type Action = Action;

        fn reduce(
            &mut self,
            state: &mut Self::State,
            _action: &Self::Action,
            _context: &mut ReduceContext<'_>,
        ) -> Effect<Self::Action> {
            state.calls += 1;
            Effect::none()
        }
    }

    fn level<Child>(
        required_depth: usize,
        child: Child,
    ) -> impl Reducer<State = State, Action = Action>
    where
        Child: Reducer<State = State, Action = Action>,
    {
        scope(
            StatePath::new(|state: &State| state, |state: &mut State| state),
            ActionPath::new(
                move |action: &Action| (action.depth >= required_depth).then_some(action),
                |action| action,
            ),
            child,
        )
    }

    fn depth_1() -> impl Reducer<State = State, Action = Action> {
        level(1, Leaf)
    }

    fn depth_4() -> impl Reducer<State = State, Action = Action> {
        level(1, level(2, level(3, level(4, Leaf))))
    }

    fn depth_16() -> impl Reducer<State = State, Action = Action> {
        level(
            1,
            level(
                2,
                level(
                    3,
                    level(
                        4,
                        level(
                            5,
                            level(
                                6,
                                level(
                                    7,
                                    level(
                                        8,
                                        level(
                                            9,
                                            level(
                                                10,
                                                level(
                                                    11,
                                                    level(
                                                        12,
                                                        level(
                                                            13,
                                                            level(14, level(15, level(16, Leaf))),
                                                        ),
                                                    ),
                                                ),
                                            ),
                                        ),
                                    ),
                                ),
                            ),
                        ),
                    ),
                ),
            ),
        )
    }

    fn reduce_once<R>(
        reducer: &mut R,
        state: &mut State,
        depth: usize,
        context: &mut ReduceContext<'_>,
    ) where
        R: Reducer<State = State, Action = Action>,
    {
        let effect = black_box(reducer.reduce(state, &Action { depth }, context));
        assert!(matches!(effect.program, Program::None));
    }

    fn assert_no_allocation<R>(
        label: &'static str,
        reducer: &mut R,
        state: &mut State,
        depth: usize,
        context: &mut ReduceContext<'_>,
    ) where
        R: Reducer<State = State, Action = Action>,
    {
        let before = dhat::HeapStats::get();
        reduce_once(reducer, state, depth, context);
        let after = dhat::HeapStats::get();
        assert_eq!(after.total_blocks, before.total_blocks, "{label} blocks");
        assert_eq!(after.total_bytes, before.total_bytes, "{label} bytes");
    }

    pub(crate) fn scope_none_paths_do_not_allocate_after_warm_up() {
        if crate::allocation_test::run_in_child(
            "composition::scope::allocation_tests::scope_none_paths_do_not_allocate_after_warm_up",
        ) {
            return;
        }
        let mut direct = Leaf;
        let mut depth_1 = depth_1();
        let mut depth_4 = depth_4();
        let mut depth_16 = depth_16();
        let mut state = State { calls: 0 };
        let mut dependencies = DependencyValues::new();
        let (_scopes_lifecycle, mut scopes) = ScopeRegistry::for_test();
        let mut context_transitions = crate::store::scope::ScopeTransitionBatch::default();
        let mut context = ReduceContext::new(
            &mut dependencies,
            MonotonicInstant::ZERO,
            &mut scopes,
            &mut context_transitions,
        );

        reduce_once(&mut direct, &mut state, 1, &mut context);
        reduce_once(&mut depth_1, &mut state, 1, &mut context);
        reduce_once(&mut depth_1, &mut state, 0, &mut context);
        reduce_once(&mut depth_4, &mut state, 4, &mut context);
        reduce_once(&mut depth_4, &mut state, 3, &mut context);
        reduce_once(&mut depth_16, &mut state, 16, &mut context);
        reduce_once(&mut depth_16, &mut state, 15, &mut context);

        let profiler = dhat::Profiler::builder().testing().build();
        assert_no_allocation("direct", &mut direct, &mut state, 1, &mut context);
        assert_no_allocation(
            "matching depth 1",
            &mut depth_1,
            &mut state,
            1,
            &mut context,
        );
        assert_no_allocation(
            "nonmatching depth 1",
            &mut depth_1,
            &mut state,
            0,
            &mut context,
        );
        assert_no_allocation(
            "matching depth 4",
            &mut depth_4,
            &mut state,
            4,
            &mut context,
        );
        assert_no_allocation(
            "nonmatching depth 4",
            &mut depth_4,
            &mut state,
            3,
            &mut context,
        );
        assert_no_allocation(
            "matching depth 16",
            &mut depth_16,
            &mut state,
            16,
            &mut context,
        );
        assert_no_allocation(
            "nonmatching depth 16",
            &mut depth_16,
            &mut state,
            15,
            &mut context,
        );
        drop(profiler);
    }
}

#[cfg(all(test, feature = "allocation-measurement"))]
pub(crate) use allocation_tests::scope_none_paths_do_not_allocate_after_warm_up;
