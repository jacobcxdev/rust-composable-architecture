//! Composes reducers whose child state has a structural lifetime.
//!
//! Presence-only composition preserves a lifetime while the selected child stays
//! present. Identified composition also detects semantic identity changes. Child
//! reduction precedes parent reduction; reconciliation then observes the parent's
//! final state. Returned child work retains its original lifetime authority.

use core::fmt;
use core::hash::Hash;
use core::marker::PhantomData;

use crate::effect::Program;
use crate::store::scope::{ScopeIdentity, ScopeSlot};
use crate::{
    ActionPath, CaseStatePath, Effect, KeyedActionPath, KeyedStatePath, OptionalStatePath,
    ReduceContext, Reducer, StatePath,
};

use super::keyed::{ForEach, for_each};
use super::missing_child::{MissingChildPolicy, missing_child};
use super::scope::{ScopeIdentified, scope_identified};

/// Shares optional borrowing between optional-state and enum-case projections.
trait LifecycleStatePath<ParentState, ChildState> {
    /// Observes presence and, where required, identity before mutable entry.
    fn get<'a>(&self, state: &'a ParentState) -> Option<&'a ChildState>;

    /// Selects the child whose presence and identity must match that observation.
    fn get_mut<'a>(&self, state: &'a mut ParentState) -> Option<&'a mut ChildState>;
}

impl<ParentState, ChildState, Get, GetMut> LifecycleStatePath<ParentState, ChildState>
    for OptionalStatePath<ParentState, ChildState, Get, GetMut>
where
    Get: for<'a> Fn(&'a ParentState) -> Option<&'a ChildState>,
    GetMut: for<'a> Fn(&'a mut ParentState) -> Option<&'a mut ChildState>,
{
    fn get<'a>(&self, state: &'a ParentState) -> Option<&'a ChildState> {
        self.get(state)
    }

    fn get_mut<'a>(&self, state: &'a mut ParentState) -> Option<&'a mut ChildState> {
        self.get_mut(state)
    }
}

impl<ParentState, ChildState, Get, GetMut> LifecycleStatePath<ParentState, ChildState>
    for CaseStatePath<ParentState, ChildState, Get, GetMut>
where
    Get: for<'a> Fn(&'a ParentState) -> Option<&'a ChildState>,
    GetMut: for<'a> Fn(&'a mut ParentState) -> Option<&'a mut ChildState>,
{
    fn get<'a>(&self, state: &'a ParentState) -> Option<&'a ChildState> {
        self.get(state)
    }

    fn get_mut<'a>(&self, state: &'a mut ParentState) -> Option<&'a mut ChildState> {
        self.get_mut(state)
    }
}

/// Owns the common child-entry and lifetime policy for optional and case wrappers.
struct LifecycleCore<Child, State, Action> {
    /// Reduces matching actions before the parent can remove or replace state.
    child: Child,
    /// Borrows child state without retaining a parent-state reference.
    state: State,
    /// Extracts borrowed input and owns the embedding callback for returned work.
    action: Action,
    /// Identifies this composition site independently of its current child lifetime.
    slot: ScopeSlot,
    /// Controls absent matching input, but never recovers contradictory projections.
    missing_child_policy: MissingChildPolicy,
}

impl<Child, State, Action> LifecycleCore<Child, State, Action> {
    /// Retains callbacks without projecting state, reducing or mounting a child.
    const fn new(child: Child, state: State, action: Action) -> Self {
        Self {
            child,
            state,
            action,
            slot: ScopeSlot::new(),
            missing_child_policy: MissingChildPolicy::ReportAndBase,
        }
    }
}

impl<Child, State, ParentAction, ChildAction, Extract, Embed>
    LifecycleCore<Child, State, ActionPath<ParentAction, ChildAction, Extract, Embed>>
where
    ParentAction: 'static,
    ChildAction: 'static,
    Extract: for<'a> Fn(&'a ParentAction) -> Option<&'a ChildAction>,
    Embed: Fn(ChildAction) -> ParentAction + Clone + Send + Sync + 'static,
{
    /// Reduces child then parent, reconciling only changes in presence.
    fn reduce_presence<ParentState, Parent, ChildState>(
        &mut self,
        parent: &mut Parent,
        state: &mut ParentState,
        action: &ParentAction,
        context: &mut ReduceContext<'_>,
        combinator: &'static str,
    ) -> Effect<ParentAction>
    where
        Parent: Reducer<State = ParentState, Action = ParentAction>,
        Child: Reducer<State = ChildState, Action = ChildAction>,
        State: LifecycleStatePath<ParentState, ChildState>,
    {
        let before = self.state.get(state).is_some();
        let token = before.then(|| {
            let node = ReduceContext::node(&mut self.slot);
            context.mount(node, ScopeIdentity::unit())
        });

        let child_effect = self.reduce_child(state, action, token, context, combinator, |_| {});
        let parent_effect = parent.reduce(state, action, context);

        let after = self.state.get(state).is_some();
        reconcile_presence(&mut self.slot, after, token, context);
        Effect::merge([child_effect, parent_effect])
    }

    /// Validates child identity at entry and reconciles the final parent observation.
    fn reduce_identified<ParentState, Parent, ChildState, Identity, Id>(
        &mut self,
        parent: &mut Parent,
        identity: &Identity,
        state: &mut ParentState,
        action: &ParentAction,
        context: &mut ReduceContext<'_>,
        combinator: &'static str,
    ) -> Effect<ParentAction>
    where
        Parent: Reducer<State = ParentState, Action = ParentAction>,
        Child: Reducer<State = ChildState, Action = ChildAction>,
        State: LifecycleStatePath<ParentState, ChildState>,
        Identity: for<'a> Fn(&'a ChildState) -> &'a Id,
        Id: Eq + Hash + Clone + Send + Sync + 'static,
    {
        let before_id = self.state.get(state).map(|child| identity(child).clone());
        let lifetime = before_id.as_ref().map(|id| {
            let node = ReduceContext::node(&mut self.slot);
            (id, context.mount(node, ScopeIdentity::new(id.clone())))
        });
        let token = lifetime.map(|(_, token)| token);

        let child_effect = self.reduce_child(state, action, token, context, combinator, |child| {
            assert!(
                Some(identity(child)) == before_id.as_ref(),
                "identified lifecycle child must retain its identity through the mutable projection"
            );
        });
        let parent_effect = parent.reduce(state, action, context);

        let after_id = self.state.get(state).map(identity);
        reconcile_identified(&mut self.slot, lifetime, after_id, context);
        let effect = Effect::merge([child_effect, parent_effect]);
        // Keep effect payloads locally owned if destroying the snapshot panics.
        drop(before_id);
        effect
    }

    /// Validates a matching mutable child before entering its captured lifetime.
    ///
    /// Absence follows the selected missing-child policy. The validator is called
    /// only for matching, present input; presence-only callers need no extra check.
    /// Empty effects avoid cloning the embedding callback or building a mapping.
    fn reduce_child<ParentState, ChildState>(
        &mut self,
        state: &mut ParentState,
        action: &ParentAction,
        token: Option<crate::store::scope::ScopeToken>,
        context: &mut ReduceContext<'_>,
        combinator: &'static str,
        validate: impl FnOnce(&ChildState),
    ) -> Effect<ParentAction>
    where
        Child: Reducer<State = ChildState, Action = ChildAction>,
        State: LifecycleStatePath<ParentState, ChildState>,
    {
        match (self.action.extract(action), token) {
            (Some(child_action), Some(token)) => {
                let child_state = self
                    .state
                    .get_mut(state)
                    .expect("present lifecycle child must remain borrowable");
                validate(child_state);

                let effect = context.with_scope(token, |context| {
                    self.child.reduce(child_state, child_action, context)
                });
                if matches!(&effect.program, Program::None) {
                    Effect::none()
                } else {
                    effect.in_scope(token).map(self.action.clone_embedder())
                }
            }
            (Some(_), None) => {
                missing_child::<ParentState, ParentAction, ChildState, ChildAction>(
                    self.missing_child_policy,
                    context,
                    combinator,
                    None,
                );
                Effect::none()
            }
            (None, _) => Effect::none(),
        }
    }
}

/// Holds a parent reducer with an optional child reducer.
///
/// Matching child actions reduce the child before the parent. Presence changes
/// reconcile the child's Store-local lifetime after parent reduction.
/// Replacing one present value with another preserves that lifetime; use
/// [`IfSomeIdentified`] when replacement must end the old child's authority.
///
/// # Panics
///
/// Missing matching children follow [`MissingChildPolicy`]: by default one
/// required issue must be accepted before the base runs. Fail-fast policy,
/// rejected diagnostic delivery, reporter panics and contradictory projections
/// panic before base reduction. No rollback of earlier mutations is provided.
/// Projection, reduction and cloning the embedding callback may also panic.
pub struct IfSome<ParentState, ParentAction, Parent, Child, State, Action> {
    /// Reduces after any matching child action.
    parent: Parent,
    /// Retains the child, borrowing paths and structural lifetime site.
    lifecycle: LifecycleCore<Child, State, Action>,
    /// Records domain types without owning domain values.
    marker: PhantomData<fn(ParentState, ParentAction)>,
}

impl<ParentState, ParentAction, Parent, Child, ChildState, ChildAction, Get, GetMut, Extract, Embed>
    IfSome<
        ParentState,
        ParentAction,
        Parent,
        Child,
        OptionalStatePath<ParentState, ChildState, Get, GetMut>,
        ActionPath<ParentAction, ChildAction, Extract, Embed>,
    >
{
    /// Creates an optional child-lifetime reducer.
    ///
    /// Stores its inputs without invoking callbacks. See [`IfSome`] for reduction
    /// order, lifetime rules and failure behaviour.
    #[must_use]
    pub const fn new(
        parent: Parent,
        state: OptionalStatePath<ParentState, ChildState, Get, GetMut>,
        action: ActionPath<ParentAction, ChildAction, Extract, Embed>,
        child: Child,
    ) -> Self {
        Self {
            parent,
            lifecycle: LifecycleCore::new(child, state, action),
            marker: PhantomData,
        }
    }
}

impl<ParentState, ParentAction, Parent, Child, ChildState, ChildAction, Get, GetMut, Extract, Embed>
    Reducer
    for IfSome<
        ParentState,
        ParentAction,
        Parent,
        Child,
        OptionalStatePath<ParentState, ChildState, Get, GetMut>,
        ActionPath<ParentAction, ChildAction, Extract, Embed>,
    >
where
    ParentAction: 'static,
    Parent: Reducer<State = ParentState, Action = ParentAction>,
    Child: Reducer<State = ChildState, Action = ChildAction>,
    ChildAction: 'static,
    Get: for<'a> Fn(&'a ParentState) -> Option<&'a ChildState>,
    GetMut: for<'a> Fn(&'a mut ParentState) -> Option<&'a mut ChildState>,
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
        self.lifecycle
            .reduce_presence(&mut self.parent, state, action, context, "IfSome")
    }
}

impl<ParentState, ParentAction, Parent, Child, State, Action> fmt::Debug
    for IfSome<ParentState, ParentAction, Parent, Child, State, Action>
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IfSome").finish_non_exhaustive()
    }
}

/// Holds an identity-aware optional child reducer.
///
/// The identity projection defines replacement while child state remains
/// present. A changed identity ends the old lifetime and mounts a fresh one.
/// Matching actions require the mutable child to have the identity observed
/// before entry; equal identity does not require the same memory address.
/// The identity callback also observes state before and after parent reduction.
///
/// # Panics
///
/// Missing matching children follow [`MissingChildPolicy`]: by default one
/// required issue must be accepted before the base runs. Fail-fast policy,
/// rejected diagnostic delivery, reporter panics and contradictory projections
/// panic before base reduction. No rollback of earlier mutations is provided.
/// Projection, identity, reduction and cloning the embedding callback may also panic.
pub struct IfSomeIdentified<ParentState, ParentAction, Parent, Child, State, Action, Identity, Id> {
    /// Reduces after any matching child action.
    parent: Parent,
    /// Retains the child, borrowing paths and structural lifetime site.
    lifecycle: LifecycleCore<Child, State, Action>,
    /// Selects the semantic identity used for entry validation and replacement.
    identity: Identity,
    /// Records domain and identity types without owning their values.
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
    IfSomeIdentified<
        ParentState,
        ParentAction,
        Parent,
        Child,
        OptionalStatePath<ParentState, ChildState, Get, GetMut>,
        ActionPath<ParentAction, ChildAction, Extract, Embed>,
        Identity,
        Id,
    >
{
    /// Creates an identity-aware optional child reducer.
    ///
    /// Stores its inputs without invoking callbacks. See [`IfSomeIdentified`]
    /// for reduction order, identity validation and failure behaviour.
    #[must_use]
    pub const fn new(
        parent: Parent,
        state: OptionalStatePath<ParentState, ChildState, Get, GetMut>,
        action: ActionPath<ParentAction, ChildAction, Extract, Embed>,
        identity: Identity,
        child: Child,
    ) -> Self {
        Self {
            parent,
            lifecycle: LifecycleCore::new(child, state, action),
            identity,
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
    for IfSomeIdentified<
        ParentState,
        ParentAction,
        Parent,
        Child,
        OptionalStatePath<ParentState, ChildState, Get, GetMut>,
        ActionPath<ParentAction, ChildAction, Extract, Embed>,
        Identity,
        Id,
    >
where
    ParentAction: 'static,
    Parent: Reducer<State = ParentState, Action = ParentAction>,
    Child: Reducer<State = ChildState, Action = ChildAction>,
    ChildAction: 'static,
    Get: for<'a> Fn(&'a ParentState) -> Option<&'a ChildState>,
    GetMut: for<'a> Fn(&'a mut ParentState) -> Option<&'a mut ChildState>,
    Extract: for<'a> Fn(&'a ParentAction) -> Option<&'a ChildAction>,
    Embed: Fn(ChildAction) -> ParentAction + Clone + Send + Sync + 'static,
    Identity: for<'a> Fn(&'a ChildState) -> &'a Id,
    Id: Eq + Hash + Clone + Send + Sync + 'static,
{
    type State = ParentState;
    type Action = ParentAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        self.lifecycle.reduce_identified(
            &mut self.parent,
            &self.identity,
            state,
            action,
            context,
            "IfSomeIdentified",
        )
    }
}

impl<ParentState, ParentAction, Parent, Child, State, Action, Identity, Id> fmt::Debug
    for IfSomeIdentified<ParentState, ParentAction, Parent, Child, State, Action, Identity, Id>
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IfSomeIdentified").finish_non_exhaustive()
    }
}

/// Holds a parent reducer with one enum-case child reducer.
///
/// Matching child actions reduce the child before the parent. Entering or
/// leaving the projected case changes its Store-local lifetime.
/// Replacing the payload while remaining in that case preserves the lifetime;
/// use [`IfCaseIdentified`] when semantic replacement must end it.
///
/// # Panics
///
/// Missing matching children follow [`MissingChildPolicy`]: by default one
/// required issue must be accepted before the base runs. Fail-fast policy,
/// rejected diagnostic delivery, reporter panics and contradictory projections
/// panic before base reduction. No rollback of earlier mutations is provided.
/// Projection, reduction and cloning the embedding callback may also panic.
pub struct IfCase<ParentState, ParentAction, Parent, Child, State, Action> {
    /// Reduces after any matching child action.
    parent: Parent,
    /// Retains the child, borrowing paths and structural lifetime site.
    lifecycle: LifecycleCore<Child, State, Action>,
    /// Records domain types without owning domain values.
    marker: PhantomData<fn(ParentState, ParentAction)>,
}

impl<ParentState, ParentAction, Parent, Child, ChildState, ChildAction, Get, GetMut, Extract, Embed>
    IfCase<
        ParentState,
        ParentAction,
        Parent,
        Child,
        CaseStatePath<ParentState, ChildState, Get, GetMut>,
        ActionPath<ParentAction, ChildAction, Extract, Embed>,
    >
{
    /// Creates an enum-case child-lifetime reducer.
    ///
    /// Stores its inputs without invoking callbacks. See [`IfCase`] for reduction
    /// order, lifetime rules and failure behaviour.
    #[must_use]
    pub const fn new(
        parent: Parent,
        state: CaseStatePath<ParentState, ChildState, Get, GetMut>,
        action: ActionPath<ParentAction, ChildAction, Extract, Embed>,
        child: Child,
    ) -> Self
    where
        Child: Reducer<State = ChildState, Action = ChildAction>,
    {
        Self {
            parent,
            lifecycle: LifecycleCore::new(child, state, action),
            marker: PhantomData,
        }
    }
}

impl<ParentState, ParentAction, Parent, Child, ChildState, ChildAction, Get, GetMut, Extract, Embed>
    Reducer
    for IfCase<
        ParentState,
        ParentAction,
        Parent,
        Child,
        CaseStatePath<ParentState, ChildState, Get, GetMut>,
        ActionPath<ParentAction, ChildAction, Extract, Embed>,
    >
where
    ParentAction: 'static,
    Parent: Reducer<State = ParentState, Action = ParentAction>,
    Child: Reducer<State = ChildState, Action = ChildAction>,
    ChildAction: 'static,
    Get: for<'a> Fn(&'a ParentState) -> Option<&'a ChildState>,
    GetMut: for<'a> Fn(&'a mut ParentState) -> Option<&'a mut ChildState>,
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
        self.lifecycle
            .reduce_presence(&mut self.parent, state, action, context, "IfCase")
    }
}

impl<ParentState, ParentAction, Parent, Child, State, Action> fmt::Debug
    for IfCase<ParentState, ParentAction, Parent, Child, State, Action>
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IfCase").finish_non_exhaustive()
    }
}

/// Holds an identity-aware enum-case child reducer.
///
/// The identity projection defines replacement while the projected case stays
/// present. A changed identity ends the old lifetime and mounts a fresh one.
/// Matching actions require the mutable child to have the identity observed
/// before entry; equal identity does not require the same memory address.
/// The identity callback also observes state before and after parent reduction.
///
/// # Panics
///
/// Missing matching children follow [`MissingChildPolicy`]: by default one
/// required issue must be accepted before the base runs. Fail-fast policy,
/// rejected diagnostic delivery, reporter panics and contradictory projections
/// panic before base reduction. No rollback of earlier mutations is provided.
/// Projection, identity, reduction and cloning the embedding callback may also panic.
pub struct IfCaseIdentified<ParentState, ParentAction, Parent, Child, State, Action, Identity, Id> {
    /// Reduces after any matching child action.
    parent: Parent,
    /// Retains the child, borrowing paths and structural lifetime site.
    lifecycle: LifecycleCore<Child, State, Action>,
    /// Selects the semantic identity used for entry validation and replacement.
    identity: Identity,
    /// Records domain and identity types without owning their values.
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
    IfCaseIdentified<
        ParentState,
        ParentAction,
        Parent,
        Child,
        CaseStatePath<ParentState, ChildState, Get, GetMut>,
        ActionPath<ParentAction, ChildAction, Extract, Embed>,
        Identity,
        Id,
    >
{
    /// Creates an identity-aware enum-case child reducer.
    ///
    /// Stores its inputs without invoking callbacks. See [`IfCaseIdentified`]
    /// for reduction order, identity validation and failure behaviour.
    #[must_use]
    pub const fn new(
        parent: Parent,
        state: CaseStatePath<ParentState, ChildState, Get, GetMut>,
        action: ActionPath<ParentAction, ChildAction, Extract, Embed>,
        identity: Identity,
        child: Child,
    ) -> Self
    where
        Child: Reducer<State = ChildState, Action = ChildAction>,
        Identity: for<'a> Fn(&'a ChildState) -> &'a Id,
    {
        Self {
            parent,
            lifecycle: LifecycleCore::new(child, state, action),
            identity,
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
    for IfCaseIdentified<
        ParentState,
        ParentAction,
        Parent,
        Child,
        CaseStatePath<ParentState, ChildState, Get, GetMut>,
        ActionPath<ParentAction, ChildAction, Extract, Embed>,
        Identity,
        Id,
    >
where
    ParentAction: 'static,
    Parent: Reducer<State = ParentState, Action = ParentAction>,
    Child: Reducer<State = ChildState, Action = ChildAction>,
    ChildAction: 'static,
    Get: for<'a> Fn(&'a ParentState) -> Option<&'a ChildState>,
    GetMut: for<'a> Fn(&'a mut ParentState) -> Option<&'a mut ChildState>,
    Extract: for<'a> Fn(&'a ParentAction) -> Option<&'a ChildAction>,
    Embed: Fn(ChildAction) -> ParentAction + Clone + Send + Sync + 'static,
    Identity: for<'a> Fn(&'a ChildState) -> &'a Id,
    Id: Eq + Hash + Clone + Send + Sync + 'static,
{
    type State = ParentState;
    type Action = ParentAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        self.lifecycle.reduce_identified(
            &mut self.parent,
            &self.identity,
            state,
            action,
            context,
            "IfCaseIdentified",
        )
    }
}

impl<ParentState, ParentAction, Parent, Child, State, Action, Identity, Id> fmt::Debug
    for IfCaseIdentified<ParentState, ParentAction, Parent, Child, State, Action, Identity, Id>
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IfCaseIdentified").finish_non_exhaustive()
    }
}

/// Composes a parent reducer with an optional child reducer.
///
/// Stores callbacks without invoking them. See [`IfSome`] for its presence-only
/// lifetime and child-before-parent reduction contract.
#[must_use]
#[expect(
    clippy::type_complexity,
    reason = "the constructor preserves IfSome's concrete static reducer type"
)]
pub const fn if_some<
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
>(
    parent: Parent,
    state: OptionalStatePath<ParentState, ChildState, Get, GetMut>,
    action: ActionPath<ParentAction, ChildAction, Extract, Embed>,
    child: Child,
) -> IfSome<
    ParentState,
    ParentAction,
    Parent,
    Child,
    OptionalStatePath<ParentState, ChildState, Get, GetMut>,
    ActionPath<ParentAction, ChildAction, Extract, Embed>,
> {
    IfSome::new(parent, state, action, child)
}

/// Composes an identity-aware optional child reducer.
///
/// Stores callbacks without invoking them. See [`IfSomeIdentified`] for identity
/// validation, replacement and child-before-parent reduction.
#[must_use]
#[expect(
    clippy::type_complexity,
    reason = "the constructor preserves IfSomeIdentified's concrete static reducer type"
)]
pub const fn if_some_identified<
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
    state: OptionalStatePath<ParentState, ChildState, Get, GetMut>,
    action: ActionPath<ParentAction, ChildAction, Extract, Embed>,
    identity: Identity,
    child: Child,
) -> IfSomeIdentified<
    ParentState,
    ParentAction,
    Parent,
    Child,
    OptionalStatePath<ParentState, ChildState, Get, GetMut>,
    ActionPath<ParentAction, ChildAction, Extract, Embed>,
    Identity,
    Id,
>
where
    Identity: for<'a> Fn(&'a ChildState) -> &'a Id,
{
    IfSomeIdentified::new(parent, state, action, identity, child)
}

/// Composes a parent reducer with one enum-case child reducer.
///
/// Stores callbacks without invoking them. See [`IfCase`] for its presence-only
/// lifetime and child-before-parent reduction contract.
#[must_use]
#[expect(
    clippy::type_complexity,
    reason = "the constructor preserves IfCase's concrete static reducer type"
)]
pub const fn if_case<
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
>(
    parent: Parent,
    state: CaseStatePath<ParentState, ChildState, Get, GetMut>,
    action: ActionPath<ParentAction, ChildAction, Extract, Embed>,
    child: Child,
) -> IfCase<
    ParentState,
    ParentAction,
    Parent,
    Child,
    CaseStatePath<ParentState, ChildState, Get, GetMut>,
    ActionPath<ParentAction, ChildAction, Extract, Embed>,
>
where
    Child: Reducer<State = ChildState, Action = ChildAction>,
{
    IfCase::new(parent, state, action, child)
}

/// Composes an identity-aware enum-case child reducer.
///
/// Stores callbacks without invoking them. See [`IfCaseIdentified`] for identity
/// validation, replacement and child-before-parent reduction.
#[must_use]
#[expect(
    clippy::type_complexity,
    reason = "the constructor preserves IfCaseIdentified's concrete static reducer type"
)]
pub const fn if_case_identified<
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
    state: CaseStatePath<ParentState, ChildState, Get, GetMut>,
    action: ActionPath<ParentAction, ChildAction, Extract, Embed>,
    identity: Identity,
    child: Child,
) -> IfCaseIdentified<
    ParentState,
    ParentAction,
    Parent,
    Child,
    CaseStatePath<ParentState, ChildState, Get, GetMut>,
    ActionPath<ParentAction, ChildAction, Extract, Embed>,
    Identity,
    Id,
>
where
    Child: Reducer<State = ChildState, Action = ChildAction>,
    Identity: for<'a> Fn(&'a ChildState) -> &'a Id,
{
    IfCaseIdentified::new(parent, state, action, identity, child)
}

/// Adds statically dispatched composition and diagnostic methods to reducers.
///
/// These methods retain their concrete reducer and callback types. Constructing
/// a wrapper does not run reduction or observe state.
pub trait ReducerExt: Reducer + Sized {
    /// Reacts once to semantic changes selected around this reduction.
    ///
    /// Capture independent before/after values. The base runs once; unequal
    /// values construct one reaction reducer with the same state, action and
    /// context. Effects merge base first, without promising completion order.
    /// Reaction mutations do not trigger another comparison or reaction.
    ///
    /// # Panics
    ///
    /// Capture, comparison, base and reaction panics propagate without rollback.
    #[must_use]
    fn on_change<Value, Capture, MakeReaction, Reaction>(
        self,
        capture: Capture,
        make_reaction: MakeReaction,
    ) -> super::OnChange<Self, Capture, MakeReaction>
    where
        Capture: Fn(&Self::State) -> Value,
        Value: PartialEq,
        MakeReaction: FnMut(Value, Value) -> Reaction,
        Reaction: Reducer<State = Self::State, Action = Self::Action>,
    {
        super::on_change(self, capture, make_reaction)
    }

    /// Selects direct reduction without diagnostic capture or callback bounds.
    ///
    /// Returns this reducer unchanged. No snapshot, equality or sink is created
    /// or invoked, in any build profile. Required assertions remain independent
    /// of optional diagnostic observation.
    #[cfg(feature = "diagnostics")]
    #[must_use]
    fn trace_changes_disabled(self) -> Self {
        self
    }

    /// Observes every synchronous reduction through debug-only independent snapshots.
    ///
    /// Capture independent values rather than shared mutable handles. Every
    /// action reaches the sink, even when snapshots compare equal. Observation
    /// precedes Store commit acknowledgement and leaves the returned effect
    /// unchanged when callbacks complete normally. Without debug assertions,
    /// callbacks and equality are skipped but their captures remain owned.
    /// See [`crate::diagnostics`] for convenient Debug and redacted examples.
    ///
    /// # Panics
    ///
    /// Capture, equality, reducer, and sink panics propagate. State mutations
    /// are not rolled back; a post-reduction panic prevents effect delivery.
    #[cfg(feature = "diagnostics")]
    #[must_use]
    fn trace_changes<Snapshot, Capture, Sink>(
        self,
        snapshot: Capture,
        sink: Sink,
    ) -> crate::diagnostics::TraceChanges<Self, Capture, Sink>
    where
        Capture: Fn(&Self::State) -> Snapshot,
        Sink: for<'a> FnMut(crate::diagnostics::Change<'a, Self::Action, Snapshot>),
        Snapshot: PartialEq,
    {
        crate::diagnostics::TraceChanges::new(self, snapshot, sink)
    }

    /// Composes this parent reducer with one keyed child reducer.
    ///
    /// See [`ForEach`] for child insertion lifetimes and reconciliation rules.
    #[must_use]
    #[expect(
        clippy::type_complexity,
        reason = "the method preserves ForEach's concrete static reducer type"
    )]
    fn for_each<Child, ChildState, ChildAction, Id, Get, GetMut, Extract, Embed>(
        self,
        state: KeyedStatePath<Self::State, Id, ChildState, Get, GetMut>,
        action: KeyedActionPath<Self::Action, Id, ChildAction, Extract, Embed>,
        child: Child,
    ) -> ForEach<
        Self::State,
        Self::Action,
        Self,
        Child,
        KeyedStatePath<Self::State, Id, ChildState, Get, GetMut>,
        KeyedActionPath<Self::Action, Id, ChildAction, Extract, Embed>,
        Id,
    > {
        for_each(self, state, action, child)
    }

    /// Composes this parent reducer with an optional child reducer.
    ///
    /// See [`IfSome`] for presence-only lifetimes and child-first reduction.
    #[must_use]
    #[expect(
        clippy::type_complexity,
        reason = "the method preserves IfSome's concrete static reducer type"
    )]
    fn if_some<Child, ChildState, ChildAction, Get, GetMut, Extract, Embed>(
        self,
        state: OptionalStatePath<Self::State, ChildState, Get, GetMut>,
        action: ActionPath<Self::Action, ChildAction, Extract, Embed>,
        child: Child,
    ) -> IfSome<
        Self::State,
        Self::Action,
        Self,
        Child,
        OptionalStatePath<Self::State, ChildState, Get, GetMut>,
        ActionPath<Self::Action, ChildAction, Extract, Embed>,
    > {
        if_some(self, state, action, child)
    }

    /// Composes this parent with an identity-aware optional child reducer.
    ///
    /// See [`IfSomeIdentified`] for identity validation and replacement semantics.
    #[must_use]
    #[expect(
        clippy::type_complexity,
        reason = "the method preserves IfSomeIdentified's concrete static reducer type"
    )]
    fn if_some_identified<
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
        self,
        state: OptionalStatePath<Self::State, ChildState, Get, GetMut>,
        action: ActionPath<Self::Action, ChildAction, Extract, Embed>,
        identity: Identity,
        child: Child,
    ) -> IfSomeIdentified<
        Self::State,
        Self::Action,
        Self,
        Child,
        OptionalStatePath<Self::State, ChildState, Get, GetMut>,
        ActionPath<Self::Action, ChildAction, Extract, Embed>,
        Identity,
        Id,
    >
    where
        Identity: for<'a> Fn(&'a ChildState) -> &'a Id,
    {
        if_some_identified(self, state, action, identity, child)
    }

    /// Composes this parent reducer with one enum-case child reducer.
    ///
    /// See [`IfCase`] for presence-only lifetimes and child-first reduction.
    #[must_use]
    #[expect(
        clippy::type_complexity,
        reason = "the method preserves IfCase's concrete static reducer type"
    )]
    fn if_case<Child, ChildState, ChildAction, Get, GetMut, Extract, Embed>(
        self,
        state: CaseStatePath<Self::State, ChildState, Get, GetMut>,
        action: ActionPath<Self::Action, ChildAction, Extract, Embed>,
        child: Child,
    ) -> IfCase<
        Self::State,
        Self::Action,
        Self,
        Child,
        CaseStatePath<Self::State, ChildState, Get, GetMut>,
        ActionPath<Self::Action, ChildAction, Extract, Embed>,
    >
    where
        Child: Reducer<State = ChildState, Action = ChildAction>,
    {
        if_case(self, state, action, child)
    }

    /// Composes this parent with an identity-aware enum-case child reducer.
    ///
    /// See [`IfCaseIdentified`] for identity validation and replacement semantics.
    #[must_use]
    #[expect(
        clippy::type_complexity,
        reason = "the method preserves IfCaseIdentified's concrete static reducer type"
    )]
    fn if_case_identified<
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
        self,
        state: CaseStatePath<Self::State, ChildState, Get, GetMut>,
        action: ActionPath<Self::Action, ChildAction, Extract, Embed>,
        identity: Identity,
        child: Child,
    ) -> IfCaseIdentified<
        Self::State,
        Self::Action,
        Self,
        Child,
        CaseStatePath<Self::State, ChildState, Get, GetMut>,
        ActionPath<Self::Action, ChildAction, Extract, Embed>,
        Identity,
        Id,
    >
    where
        Child: Reducer<State = ChildState, Action = ChildAction>,
        Identity: for<'a> Fn(&'a ChildState) -> &'a Id,
    {
        if_case_identified(self, state, action, identity, child)
    }

    /// Composes this parent with an identity-aware required child reducer.
    ///
    /// See [`ScopeIdentified`] for identity validation and replacement semantics.
    #[must_use]
    #[expect(
        clippy::type_complexity,
        reason = "the method preserves ScopeIdentified's concrete static reducer type"
    )]
    fn scope_identified<Child, ChildState, ChildAction, Get, GetMut, Extract, Embed, Identity, Id>(
        self,
        state: StatePath<Self::State, ChildState, Get, GetMut>,
        action: ActionPath<Self::Action, ChildAction, Extract, Embed>,
        identity: Identity,
        child: Child,
    ) -> ScopeIdentified<
        Self::State,
        Self::Action,
        Self,
        Child,
        StatePath<Self::State, ChildState, Get, GetMut>,
        ActionPath<Self::Action, ChildAction, Extract, Embed>,
        Identity,
        Id,
    >
    where
        Identity: for<'a> Fn(&'a ChildState) -> &'a Id,
    {
        scope_identified(self, state, action, identity, child)
    }
}

impl<Parent> ReducerExt for Parent where Parent: Reducer {}

/// Retires or installs a child only when its final presence changed.
///
/// A newly installed child does not receive a replay of the current action.
fn reconcile_presence(
    slot: &mut ScopeSlot,
    after: bool,
    token: Option<crate::store::scope::ScopeToken>,
    context: &mut ReduceContext<'_>,
) {
    match (token, after) {
        (Some(token), false) => context.leave(token),
        (None, true) => {
            let node = ReduceContext::node(slot);
            let _ = context.mount(node, ScopeIdentity::unit());
        }
        (Some(_), true) | (None, false) => {}
    }
}

/// Reconciles the final observed identity without copying unchanged values.
///
/// A replacement is cloned before retiring the previous lifetime, so a clone
/// panic leaves its authority intact. Registry operations and their callbacks
/// retain their own panic semantics; reconciliation does not provide rollback.
pub(super) fn reconcile_identified<Id>(
    slot: &mut ScopeSlot,
    before: Option<(&Id, crate::store::scope::ScopeToken)>,
    after_id: Option<&Id>,
    context: &mut ReduceContext<'_>,
) where
    Id: Eq + Hash + Clone + Send + Sync + 'static,
{
    match (before, after_id) {
        (Some((before, _)), Some(after)) if before == after => {}
        (Some((_, token)), Some(after)) => {
            let after = after.clone();
            context.leave(token);
            let node = ReduceContext::node(slot);
            let _ = context.mount(node, ScopeIdentity::new(after));
        }
        (Some((_, token)), None) => context.leave(token),
        (None, Some(after)) => {
            let after = after.clone();
            let node = ReduceContext::node(slot);
            let _ = context.mount(node, ScopeIdentity::new(after));
        }
        (None, None) => {}
    }
}

impl<ParentState, ParentAction, Parent, Child, State, Action>
    IfSome<ParentState, ParentAction, Parent, Child, State, Action>
{
    /// Selects the response to an authorised matching action with absent child state.
    ///
    /// The default requires accepted issue delivery before base reduction.
    /// Contradictory projections and stale scope authority are never recovered.
    #[must_use]
    pub const fn missing_child_policy(mut self, policy: MissingChildPolicy) -> Self {
        self.lifecycle.missing_child_policy = policy;
        self
    }
}

impl<ParentState, ParentAction, Parent, Child, State, Action, Identity, Id>
    IfSomeIdentified<ParentState, ParentAction, Parent, Child, State, Action, Identity, Id>
{
    /// Selects the response to an authorised matching action with absent child state.
    ///
    /// The default requires accepted issue delivery before base reduction.
    /// Contradictory projections and stale scope authority are never recovered.
    #[must_use]
    pub const fn missing_child_policy(mut self, policy: MissingChildPolicy) -> Self {
        self.lifecycle.missing_child_policy = policy;
        self
    }
}

impl<ParentState, ParentAction, Parent, Child, State, Action>
    IfCase<ParentState, ParentAction, Parent, Child, State, Action>
{
    /// Selects the response to an authorised matching action with absent child state.
    ///
    /// The default requires accepted issue delivery before base reduction.
    /// Contradictory projections and stale scope authority are never recovered.
    #[must_use]
    pub const fn missing_child_policy(mut self, policy: MissingChildPolicy) -> Self {
        self.lifecycle.missing_child_policy = policy;
        self
    }
}

impl<ParentState, ParentAction, Parent, Child, State, Action, Identity, Id>
    IfCaseIdentified<ParentState, ParentAction, Parent, Child, State, Action, Identity, Id>
{
    /// Selects the response to an authorised matching action with absent child state.
    ///
    /// The default requires accepted issue delivery before base reduction.
    /// Contradictory projections and stale scope authority are never recovered.
    #[must_use]
    pub const fn missing_child_policy(mut self, policy: MissingChildPolicy) -> Self {
        self.lifecycle.missing_child_policy = policy;
        self
    }
}

#[cfg(test)]
ordinary_test_items! {
mod tests {
    use std::cell::RefCell;
    use std::hash::{Hash, Hasher};
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{
        ActionPath, CaseStatePath, Effect, OptionalStatePath, ReduceContext, Reducer,
        if_case_identified, if_some_identified, reconcile_identified,
    };
    use crate::store::scope::{
        ScopeIdentity, ScopeRegistry, ScopeSlot, ScopeToken, ScopeTransitionBatch,
    };
    use crate::{DependencyValues, MonotonicInstant, Reduce};

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

    #[derive(Eq, Hash, PartialEq)]
    struct PanickingClone(u8);

    impl Clone for PanickingClone {
        fn clone(&self) -> Self {
            panic!("replacement identity clone");
        }
    }

    #[test]
    fn replacement_clone_panic_preserves_the_previous_lifetime() {
        let mut dependencies = DependencyValues::new();
        let (_owner, mut scopes) = ScopeRegistry::for_test();
        let mut transitions = ScopeTransitionBatch::default();
        let mut context = ReduceContext::new(
            &mut dependencies,
            MonotonicInstant::ZERO,
            &mut scopes,
            &mut transitions,
        );
        let mut slot = ScopeSlot::new();
        let node = ReduceContext::node(&mut slot);
        let token = context.mount(node, ScopeIdentity::new(PanickingClone(1)));
        let before = PanickingClone(1);

        reconcile_identified(
            &mut slot,
            Some((&before, token)),
            Some(&before),
            &mut context,
        );
        assert!(context.scopes().is_current(token));

        let after = PanickingClone(2);
        let panic = catch_unwind(AssertUnwindSafe(|| {
            reconcile_identified(
                &mut slot,
                Some((&before, token)),
                Some(&after),
                &mut context,
            );
        }))
        .expect_err("replacement must obtain ownership before retirement");
        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"replacement identity clone")
        );
        assert!(context.scopes().is_current(token));
    }

    enum Action {
        Observe,
        Replace(u8),
        Remove,
    }

    fn assert_identity_clone_counts(
        mut reducer: impl Reducer<State = Option<CloneCountId>, Action = Action>,
        clones: &Arc<AtomicUsize>,
        selected: &RefCell<Vec<ScopeToken>>,
    ) {
        let mut state = Some(CloneCountId {
            value: 7,
            clones: Arc::clone(clones),
        });
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

        for action in [
            Action::Observe,
            Action::Observe,
            Action::Replace(8),
            Action::Observe,
            Action::Remove,
            Action::Replace(8),
            Action::Observe,
        ] {
            clones.store(0, Ordering::Relaxed);
            let _ = reducer.reduce(&mut state, &action, &mut context);
            counts.push(clones.load(Ordering::Relaxed));
        }

        let selected = selected.borrow();
        assert_eq!(selected.len(), 4);
        assert_eq!(selected[0], selected[1]);
        assert_ne!(selected[0], selected[2]);
        assert_ne!(selected[2], selected[3]);
        assert!(!context.scopes().is_current(selected[0]));
        assert!(!context.scopes().is_current(selected[2]));
        assert!(context.scopes().is_current(selected[3]));
        assert_eq!(state.as_ref().unwrap().value, 8);
        eprintln!("identified_lifecycle/identity_clones: {counts:?}");
        assert_eq!(counts, [2, 2, 3, 2, 2, 1, 2]);
    }

    #[test]
    fn identified_lifecycle_clones_only_owned_snapshots_and_changed_identity() {
        for case in [false, true] {
            let clones = Arc::new(AtomicUsize::new(0));
            let selected = RefCell::new(Vec::new());
            let parent = Reduce::new(
                |state: &mut Option<CloneCountId>, action: &Action, _| {
                    match action {
                        Action::Observe => {}
                        Action::Replace(value) => {
                            *state = Some(CloneCountId {
                                value: *value,
                                clones: Arc::clone(&clones),
                            });
                        }
                        Action::Remove => *state = None,
                    }
                    Effect::none()
                },
            );
            let child = Reduce::new(
                |_: &mut CloneCountId, (): &(), context: &mut ReduceContext<'_>| {
                    selected.borrow_mut().push(context.scope_token());
                    Effect::none()
                },
            );
            let action = ActionPath::new(
                |action: &Action| matches!(action, Action::Observe).then_some(&()),
                |()| Action::Observe,
            );

            if case {
                let reducer = if_case_identified(
                    parent,
                    CaseStatePath::new(Option::as_ref, Option::as_mut),
                    action,
                    |identity: &CloneCountId| identity,
                    child,
                );
                assert_identity_clone_counts(reducer, &clones, &selected);
            } else {
                let reducer = if_some_identified(
                    parent,
                    OptionalStatePath::new(Option::as_ref, Option::as_mut),
                    action,
                    |identity: &CloneCountId| identity,
                    child,
                );
                assert_identity_clone_counts(reducer, &clones, &selected);
            }
        }
    }
}
}

#[cfg(all(test, feature = "allocation-measurement"))]
mod allocation_tests {
    use std::hint::black_box;

    use super::{
        ActionPath, CaseStatePath, Effect, OptionalStatePath, Program, ReduceContext, Reducer,
        if_case, if_case_identified, if_some, if_some_identified,
    };
    use crate::store::scope::ScopeRegistry;
    use crate::{DependencyValues, MonotonicInstant};

    struct ChildState {
        id: u8,
        calls: usize,
    }

    struct State {
        child: Option<ChildState>,
        parent_calls: usize,
    }

    #[derive(Clone, Copy)]
    enum Action {
        Child,
        Parent,
    }

    type OptionalPath = OptionalStatePath<
        State,
        ChildState,
        fn(&State) -> Option<&ChildState>,
        fn(&mut State) -> Option<&mut ChildState>,
    >;

    type CasePath = CaseStatePath<
        State,
        ChildState,
        fn(&State) -> Option<&ChildState>,
        fn(&mut State) -> Option<&mut ChildState>,
    >;

    type ChildPath = ActionPath<Action, (), fn(&Action) -> Option<&()>, fn(()) -> Action>;

    struct Parent;

    impl Reducer for Parent {
        type State = State;
        type Action = Action;

        fn reduce(
            &mut self,
            state: &mut Self::State,
            _action: &Self::Action,
            _context: &mut ReduceContext<'_>,
        ) -> Effect<Self::Action> {
            state.parent_calls += 1;
            Effect::none()
        }
    }

    struct Child;

    impl Reducer for Child {
        type State = ChildState;
        type Action = ();

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

    fn optional_path() -> OptionalPath {
        OptionalStatePath::new(
            (|state: &State| state.child.as_ref()) as fn(&State) -> Option<&ChildState>,
            (|state: &mut State| state.child.as_mut()) as fn(&mut State) -> Option<&mut ChildState>,
        )
    }

    fn action_path() -> ChildPath {
        static CHILD: () = ();
        ActionPath::new(
            (|action: &Action| match action {
                Action::Child => Some(&CHILD),
                Action::Parent => None,
            }) as fn(&Action) -> Option<&()>,
            (|()| Action::Child) as fn(()) -> Action,
        )
    }

    fn case_path() -> CasePath {
        CaseStatePath::new(
            (|state: &State| state.child.as_ref()) as fn(&State) -> Option<&ChildState>,
            (|state: &mut State| state.child.as_mut()) as fn(&mut State) -> Option<&mut ChildState>,
        )
    }

    fn assert_no_allocation<R>(
        label: &'static str,
        reducer: &mut R,
        state: &mut State,
        action: Action,
        context: &mut ReduceContext<'_>,
    ) where
        R: Reducer<State = State, Action = Action>,
    {
        let before = dhat::HeapStats::get();
        let effect = black_box(reducer.reduce(state, &action, context));
        let after = dhat::HeapStats::get();
        assert!(matches!(effect.program, Program::None));
        assert_eq!(after.total_blocks, before.total_blocks, "{label} blocks");
        assert_eq!(after.total_bytes, before.total_bytes, "{label} bytes");
    }

    pub(crate) fn optional_lifecycle_hot_paths_do_not_allocate_after_warm_up() {
        if crate::allocation_test::run_in_child(
            "composition::lifecycle::allocation_tests::optional_lifecycle_hot_paths_do_not_allocate_after_warm_up",
        ) {
            return;
        }
        let mut present = if_some(Parent, optional_path(), action_path(), Child);
        let mut identified = if_some_identified(
            Parent,
            optional_path(),
            action_path(),
            |state: &ChildState| &state.id,
            Child,
        );
        let mut absent = if_some(Parent, optional_path(), action_path(), Child);
        let mut present_state = State {
            child: Some(ChildState { id: 1, calls: 0 }),
            parent_calls: 0,
        };
        let mut identified_state = State {
            child: Some(ChildState { id: 1, calls: 0 }),
            parent_calls: 0,
        };
        let mut absent_state = State {
            child: None,
            parent_calls: 0,
        };
        let mut dependencies = DependencyValues::new();
        let (_scopes_lifecycle, mut scopes) = ScopeRegistry::for_test();
        let mut context_transitions = crate::store::scope::ScopeTransitionBatch::default();
        let mut context = ReduceContext::new(
            &mut dependencies,
            MonotonicInstant::ZERO,
            &mut scopes,
            &mut context_transitions,
        );

        let _ = present.reduce(&mut present_state, &Action::Child, &mut context);
        let _ = identified.reduce(&mut identified_state, &Action::Child, &mut context);
        let _ = absent.reduce(&mut absent_state, &Action::Parent, &mut context);

        let profiler = dhat::Profiler::builder().testing().build();
        assert_no_allocation(
            "matching present",
            &mut present,
            &mut present_state,
            Action::Child,
            &mut context,
        );
        assert_no_allocation(
            "nonmatching present",
            &mut present,
            &mut present_state,
            Action::Parent,
            &mut context,
        );
        assert_no_allocation(
            "matching identified",
            &mut identified,
            &mut identified_state,
            Action::Child,
            &mut context,
        );
        assert_no_allocation(
            "nonmatching identified",
            &mut identified,
            &mut identified_state,
            Action::Parent,
            &mut context,
        );
        assert_no_allocation(
            "warmed absent",
            &mut absent,
            &mut absent_state,
            Action::Parent,
            &mut context,
        );
        drop(profiler);
    }

    pub(crate) fn case_lifecycle_hot_paths_do_not_allocate_after_warm_up() {
        if crate::allocation_test::run_in_child(
            "composition::lifecycle::allocation_tests::case_lifecycle_hot_paths_do_not_allocate_after_warm_up",
        ) {
            return;
        }
        let mut case = if_case(Parent, case_path(), action_path(), Child);
        let mut identified = if_case_identified(
            Parent,
            case_path(),
            action_path(),
            |state: &ChildState| &state.id,
            Child,
        );
        let mut case_state = State {
            child: Some(ChildState { id: 1, calls: 0 }),
            parent_calls: 0,
        };
        let mut identified_state = State {
            child: Some(ChildState { id: 1, calls: 0 }),
            parent_calls: 0,
        };
        let mut dependencies = DependencyValues::new();
        let (_scopes_lifecycle, mut scopes) = ScopeRegistry::for_test();
        let mut context_transitions = crate::store::scope::ScopeTransitionBatch::default();
        let mut context = ReduceContext::new(
            &mut dependencies,
            MonotonicInstant::ZERO,
            &mut scopes,
            &mut context_transitions,
        );

        let _ = case.reduce(&mut case_state, &Action::Child, &mut context);
        let _ = identified.reduce(&mut identified_state, &Action::Child, &mut context);

        let profiler = dhat::Profiler::builder().testing().build();
        assert_no_allocation(
            "matching case",
            &mut case,
            &mut case_state,
            Action::Child,
            &mut context,
        );
        assert_no_allocation(
            "nonmatching case",
            &mut case,
            &mut case_state,
            Action::Parent,
            &mut context,
        );
        assert_no_allocation(
            "matching identified case",
            &mut identified,
            &mut identified_state,
            Action::Child,
            &mut context,
        );
        assert_no_allocation(
            "nonmatching identified case",
            &mut identified,
            &mut identified_state,
            Action::Parent,
            &mut context,
        );
        drop(profiler);
    }
}

#[cfg(all(test, feature = "allocation-measurement"))]
pub(crate) use allocation_tests::{
    case_lifecycle_hot_paths_do_not_allocate_after_warm_up,
    optional_lifecycle_hot_paths_do_not_allocate_after_warm_up,
};
