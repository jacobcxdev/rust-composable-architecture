//! Composes headless presentation, dismissal and ephemeral reduction.
//!
//! A presentation is an optional child with a distinct identity and lifetime.
//! It can represent a task, session or other headless feature. No UI is required.
//! Child actions run before parent reduction. Dismissal runs the parent first,
//! then clears only the selected identity. A captured dismiss client retains
//! its original lifetime, even inside delayed or named work.

use core::{fmt, hash::Hash, marker::PhantomData};
use std::sync::Arc;

use crate::effect::Program;
use crate::store::scope::{ScopeIdentity, ScopeSlot, ScopeToken};
use crate::{
    ActionPath, Effect, Issue, IssueReporter, IssueSeverity, ReduceContext, Reducer, StatePath,
};

use super::lifecycle::reconcile_identified;
use super::missing_child::{MissingChildPolicy, missing_child};

/// Routes a child action or requests parent-visible dismissal.
///
/// A directly sent action is fresh input. Historical output must retain its
/// effect or dismiss-client authority rather than reconstructing a fresh action.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PresentationAction<Action> {
    /// Reduces the presented child before the parent.
    Presented(Action),
    /// Lets the parent inspect the child before clearing its matching identity.
    Dismiss,
}

/// Binds dismissal to one Store, adapter route and installed lifetime.
#[derive(Clone)]
pub(crate) struct DismissTarget {
    /// Distinguishes independent Stores that share the same reducer instance.
    store: Arc<()>,
    /// Distinguishes nested or sibling presentation adapters within one Store.
    route: Arc<()>,
    /// Prevents an old request from controlling a recreated equal identity.
    scope: ScopeToken,
}

impl DismissTarget {
    /// Captures existing authority without retaining the presented state.
    pub(crate) const fn new(store: Arc<()>, route: Arc<()>, scope: ScopeToken) -> Self {
        Self {
            store,
            route,
            scope,
        }
    }

    /// Requires all three ownership boundaries to identify the same target.
    fn matches(&self, other: &Self) -> bool {
        self.scope == other.scope
            && Arc::ptr_eq(&self.store, &other.store)
            && Arc::ptr_eq(&self.route, &other.route)
    }
}

/// Owns dismissal authority for one captured presentation lifetime.
///
/// Obtain this client from [`ReduceContext::dismiss`]. Cloning preserves the
/// captured target; it never grants authority over a replacement or another Store.
/// Return [`Self::effect`] from the child reducer. Asynchronous work first sends
/// its completion action back to that reducer, which can then request dismissal.
///
/// # Examples
///
/// ```
/// use rca_core::{Effect, ReduceContext};
/// fn close(context: &mut ReduceContext<'_>) -> Effect<()> {
///     let dismiss = context.dismiss();
///     dismiss.effect()
/// }
/// ```
#[derive(Clone)]
pub struct Dismiss {
    /// Remains absent when the context has no enclosing presentation.
    target: Option<DismissTarget>,
    /// Delivers required diagnostics without granting dismissal authority.
    issues: IssueReporter,
}

impl Dismiss {
    /// Retains the captured target and its issue-reporting endpoint.
    pub(crate) const fn new(target: Option<DismissTarget>, issues: IssueReporter) -> Self {
        Self { target, issues }
    }

    /// Requests dismissal without granting new authority to the captured target.
    ///
    /// The nearest matching presentation turns this control into its parent-visible
    /// [`PresentationAction::Dismiss`]. Delays and effect composition preserve its
    /// original lifetime. A retired target produces no action.
    ///
    /// Outside presentation reduction, this reports required issue
    /// `rca.dismiss_unavailable` and returns no work after accepted delivery.
    ///
    /// # Panics
    /// Disabled or closed reporting panics with the original [`crate::IssueReportError`].
    /// Reporter callback panics propagate. A live control returned outside its
    /// matching adapter has the same required-reporting policy at compilation.
    #[must_use]
    #[track_caller]
    pub fn effect<Action>(&self) -> Effect<Action> {
        if let Some(target) = &self.target {
            Effect {
                program: Program::Dismiss(target.clone()),
                scope: Some(target.scope),
            }
        } else {
            report_unavailable_dismiss(&self.issues);
            Effect::none()
        }
    }
}

impl fmt::Debug for Dismiss {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Dismiss")
            .field("bound", &self.target.is_some())
            .finish_non_exhaustive()
    }
}

/// Reports missing dismissal authority, preserving required-delivery failures.
#[cold]
#[track_caller]
pub(crate) fn report_unavailable_dismiss(issues: &IssueReporter) {
    let issue = Issue::new(
        "rca.dismiss_unavailable",
        "dismissal requires its matching presentation reducer and captured lifetime",
    )
    .with_severity(IssueSeverity::Error);
    if let Err(error) = issues.report(issue) {
        std::panic::panic_any(error);
    }
}

/// Holds a parent and an identity-aware presented child reducer.
///
/// Presented actions reduce the child before the parent. Dismiss actions skip the
/// child, let the parent inspect its value, then clear only the identity captured
/// before reduction. A different replacement survives. Identity equality means
/// the same presentation: select an identity that changes when replacement should
/// survive. Removing and later recreating an equal identity starts a fresh private
/// lifetime, including beneath optional, case and keyed composition.
///
/// Neither the whole state nor the action requires `Clone` or `Debug`. Only the
/// selected identity is cloned. Dismissal authority includes a private Store-owned
/// namespace, even when independent Stores share this reducer through a wrapper.
/// Synchronous state and actions can be non-`Send`. Owned action mapping requires
/// `'static` actions and an embedder that is `Clone + Send + Sync + 'static`.
/// Store admission keeps its separate transport requirements.
///
/// # Panics
/// Matching actions with absent state require accepted `rca.missing_child` issue
/// delivery before parent reduction. Disabled or closed reporting panics with the
/// original [`crate::IssueReportError`]; reporter callback panics propagate.
/// Contradictory state projections panic. The selected child must retain its
/// observed identity through the ephemeral predicate, until child reduction starts.
/// Earlier mutations are not rolled back.
pub struct Presentation<Parent, Child, State, Action, Identity, Ephemeral, Id> {
    /// Observes child changes before deciding whether to remove or replace it.
    parent: Parent,
    /// Reduces authorised presented actions within the captured child lifetime.
    child: Child,
    /// Borrows the optional child slot without owning parent state.
    state: State,
    /// Extracts borrowed input and embeds owned child output.
    action: Action,
    /// Selects the semantic identity used to distinguish replacements.
    identity: Identity,
    /// Decides whether the original presentation is cleared after reduction.
    ephemeral: Ephemeral,
    /// Identifies this structural site across successive child lifetimes.
    slot: ScopeSlot,
    /// Keeps this adapter's dismiss route distinct from sibling routes.
    route: Arc<()>,
    /// Records the identity type without owning another identity value.
    marker: PhantomData<fn() -> Id>,
}

/// Composes a parent with an identity-aware headless presentation.
///
/// The state path selects the actual optional slot so dismissal can clear it.
/// Select only the semantic identity, not the entire child state. Use
/// [`Presentation::ephemeral`] for matching actions that should also clear it.
#[must_use]
#[expect(
    clippy::type_complexity,
    reason = "preserves concrete reducer and path types"
)]
pub fn presentation<Parent, Child, Get, GetMut, Extract, Embed, Identity, Id>(
    parent: Parent,
    state: StatePath<Parent::State, Option<Child::State>, Get, GetMut>,
    action: ActionPath<Parent::Action, PresentationAction<Child::Action>, Extract, Embed>,
    identity: Identity,
    child: Child,
) -> Presentation<
    Parent,
    Child,
    StatePath<Parent::State, Option<Child::State>, Get, GetMut>,
    ActionPath<Parent::Action, PresentationAction<Child::Action>, Extract, Embed>,
    Identity,
    fn(&Child::State, &Child::Action) -> bool,
    Id,
>
where
    Parent: Reducer,
    Child: Reducer,
    Identity: for<'a> Fn(&'a Child::State) -> &'a Id,
{
    Presentation {
        parent,
        child,
        state,
        action,
        identity,
        ephemeral: |_, _| false,
        slot: ScopeSlot::new(),
        route: Arc::new(()),
        marker: PhantomData,
    }
}

impl<Parent, Child, State, Action, Identity, Ephemeral, Id>
    Presentation<Parent, Child, State, Action, Identity, Ephemeral, Id>
where
    Child: Reducer,
{
    /// Clears the same presentation after selected child actions and parent reduction.
    ///
    /// The predicate borrows the child and action before child reduction. A different
    /// identity installed by either reducer survives, just as on explicit dismissal.
    /// The predicate must preserve the selected identity until child reduction starts.
    #[must_use]
    pub fn ephemeral<Predicate>(
        self,
        predicate: Predicate,
    ) -> Presentation<Parent, Child, State, Action, Identity, Predicate, Id>
    where
        Predicate: Fn(&Child::State, &Child::Action) -> bool,
    {
        Presentation {
            parent: self.parent,
            child: self.child,
            state: self.state,
            action: self.action,
            identity: self.identity,
            ephemeral: predicate,
            slot: self.slot,
            route: self.route,
            marker: PhantomData,
        }
    }
}

impl<Parent, Child, Get, GetMut, Extract, Embed, Identity, Ephemeral, Id> Reducer
    for Presentation<
        Parent,
        Child,
        StatePath<Parent::State, Option<Child::State>, Get, GetMut>,
        ActionPath<Parent::Action, PresentationAction<Child::Action>, Extract, Embed>,
        Identity,
        Ephemeral,
        Id,
    >
where
    Parent: Reducer,
    Child: Reducer,
    Parent::Action: 'static,
    Child::Action: 'static,
    Get: for<'a> Fn(&'a Parent::State) -> &'a Option<Child::State>,
    GetMut: for<'a> Fn(&'a mut Parent::State) -> &'a mut Option<Child::State>,
    Extract: for<'a> Fn(&'a Parent::Action) -> Option<&'a PresentationAction<Child::Action>>,
    Embed: Fn(PresentationAction<Child::Action>) -> Parent::Action + Clone + Send + Sync + 'static,
    Identity: for<'a> Fn(&'a Child::State) -> &'a Id,
    Ephemeral: Fn(&Child::State, &Child::Action) -> bool,
    Id: Eq + Hash + Clone + Send + Sync + 'static,
{
    type State = Parent::State;
    type Action = Parent::Action;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        let before = self
            .state
            .get(state)
            .as_ref()
            .map(|child| (self.identity)(child).clone());
        let lifetime = before.as_ref().map(|id| {
            let node = ReduceContext::node(&mut self.slot);
            (id, context.mount(node, ScopeIdentity::new(id.clone())))
        });
        let token = lifetime.map(|(_, token)| token);
        let mut clear = false;
        let mut child_effect = Effect::none();

        match (self.action.extract(action), token) {
            (Some(PresentationAction::Presented(action)), Some(token)) => {
                let child = self
                    .state
                    .get_mut(state)
                    .as_mut()
                    .expect("present child remains borrowable");
                assert!(
                    Some((self.identity)(child)) == before.as_ref(),
                    "present child must retain its observed identity through mutable projection",
                );
                clear = (self.ephemeral)(child, action);
                assert!(
                    Some((self.identity)(child)) == before.as_ref(),
                    "present child must retain its observed identity through the predicate",
                );
                let target = DismissTarget::new(
                    context.presentation_namespace(),
                    Arc::clone(&self.route),
                    token,
                );
                let effect = context.with_scope(token, |context| {
                    context.with_dismiss_target(target.clone(), |context| {
                        self.child.reduce(child, action, context)
                    })
                });
                let mut effect = effect.in_scope(token).map(PresentationAction::Presented);
                bind_dismiss(&mut effect, &target, &|| PresentationAction::Dismiss);
                child_effect = effect.map(self.action.clone_embedder());
            }
            (Some(PresentationAction::Dismiss), Some(_)) => clear = true,
            (Some(_), None) => {
                missing_child::<Parent::State, Parent::Action, Child::State, Child::Action>(
                    MissingChildPolicy::ReportAndBase,
                    context,
                    "Presentation",
                    None,
                );
            }
            (None, _) => {}
        }

        let parent_effect = self.parent.reduce(state, action, context);
        let after = self.state.get(state).as_ref().map(&self.identity);
        let after = if clear && before.as_ref() == after {
            let selected = self.state.get_mut(state);
            assert!(
                selected.as_ref().map(&self.identity) == before.as_ref(),
                "dismissed child must retain its observed identity through mutable projection",
            );
            *selected = None;
            None
        } else {
            after
        };

        reconcile_identified(&mut self.slot, lifetime, after, context);
        let effect = Effect::merge([child_effect, parent_effect]);
        // Keep returned work owned locally if snapshot destruction panics.
        drop(before);
        effect
    }
}

/// Replaces matching controls without changing enclosing work or its ownership.
///
/// A non-matching control stays intact for an outer adapter or stale-scope rejection.
pub(crate) fn bind_dismiss<Action>(
    effect: &mut Effect<Action>,
    target: &DismissTarget,
    make_action: &impl Fn() -> Action,
) {
    match &mut effect.program {
        Program::Dismiss(request) => {
            if request.matches(target) {
                effect.program = Program::Send(make_action());
            }
        }
        Program::Merge(effects) => {
            for effect in effects {
                bind_dismiss(effect, target, make_action);
            }
        }
        Program::Concatenate(effects) => {
            for effect in effects {
                bind_dismiss(effect, target, make_action);
            }
        }
        Program::Cancellable { effect, .. }
        | Program::Named { effect, .. }
        | Program::WithoutInheritedCancellation(effect) => {
            bind_dismiss(effect, target, make_action);
        }
        Program::None
        | Program::Send(_)
        | Program::Run(_)
        | Program::FallibleRun(_)
        | Program::Future(_)
        | Program::Cancel(_)
        | Program::Wait(_)
        | Program::SendAfter { .. }
        | Program::SendAt { .. }
        | Program::Interval { .. } => {}
    }
}

impl<Parent, Child, State, Action, Identity, Ephemeral, Id> fmt::Debug
    for Presentation<Parent, Child, State, Action, Identity, Ephemeral, Id>
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Presentation").finish_non_exhaustive()
    }
}

#[cfg(all(test, feature = "test-util"))]
ordinary_test_items! {
mod tests {
    use super::*;
    use crate::store::scope::ScopeRegistry;
    use crate::{DependencyValues, IssueCollector, IssueReportError, MonotonicInstant};
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::atomic::{AtomicUsize, Ordering};

    type Slots = (Option<(u8, usize)>, Option<(u8, usize)>, usize);

    // Only the first owned snapshot panics. The original state and the registry's
    // identity remain safe to destroy after the assertion catches that panic.
    struct SnapshotDropId {
        clones: Arc<AtomicUsize>,
        panic_on_drop: bool,
    }

    impl Clone for SnapshotDropId {
        fn clone(&self) -> Self {
            Self {
                panic_on_drop: self.clones.fetch_add(1, Ordering::Relaxed) == 0,
                clones: Arc::clone(&self.clones),
            }
        }
    }

    impl PartialEq for SnapshotDropId {
        fn eq(&self, _: &Self) -> bool {
            true
        }
    }

    impl Eq for SnapshotDropId {}

    impl Hash for SnapshotDropId {
        fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
            1_u8.hash(state);
        }
    }

    impl Drop for SnapshotDropId {
        fn drop(&mut self) {
            assert!(!self.panic_on_drop, "identity snapshot drop");
        }
    }

    struct EffectPayload(Arc<AtomicUsize>);

    impl Drop for EffectPayload {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[derive(Default)]
    struct EffectDropCounts {
        parent: Arc<AtomicUsize>,
        child: Arc<AtomicUsize>,
    }

    impl EffectDropCounts {
        fn parent(
            &self,
        ) -> impl Reducer<State = Option<SnapshotDropId>, Action = PresentationAction<()>> + use<> {
            let drops = Arc::clone(&self.parent);
            crate::Reduce::new(
                move |_: &mut Option<SnapshotDropId>, _: &PresentationAction<()>, _| {
                    let payload = EffectPayload(Arc::clone(&drops));
                    Effect::future(async move {
                        drop(payload);
                        None
                    })
                },
            )
        }

        fn child(&self) -> impl Reducer<State = SnapshotDropId, Action = ()> + use<> {
            let drops = Arc::clone(&self.child);
            crate::Reduce::new(move |_: &mut SnapshotDropId, (): &(), _| {
                let payload = EffectPayload(Arc::clone(&drops));
                Effect::future(async move {
                    drop(payload);
                    None
                })
            })
        }

        fn assert_released(
            &self,
            mut reducer: impl Reducer<State = Option<SnapshotDropId>, Action = PresentationAction<()>>,
            expected_child_drops: usize,
        ) {
            let mut state = Some(SnapshotDropId {
                clones: Arc::new(AtomicUsize::new(0)),
                panic_on_drop: false,
            });
            let mut dependencies = DependencyValues::new();
            let (_owner, mut scopes) = ScopeRegistry::for_test();
            let mut transitions = crate::store::scope::ScopeTransitionBatch::default();
            let mut context = ReduceContext::new(
                &mut dependencies,
                MonotonicInstant::ZERO,
                &mut scopes,
                &mut transitions,
            );

            let panic = catch_unwind(AssertUnwindSafe(|| {
                reducer.reduce(&mut state, &PresentationAction::Presented(()), &mut context)
            }))
            .expect_err("the first owned identity snapshot must panic in Drop");
            assert_eq!(panic.downcast_ref::<&str>(), Some(&"identity snapshot drop"));
            assert_eq!(state.as_ref().unwrap().clones.load(Ordering::Relaxed), 2);
            assert_eq!(self.parent.load(Ordering::Relaxed), 1);
            assert_eq!(self.child.load(Ordering::Relaxed), expected_child_drops);
        }
    }

    fn presented_child(action: &PresentationAction<()>) -> Option<&()> {
        match action {
            PresentationAction::Presented(action) => Some(action),
            PresentationAction::Dismiss => None,
        }
    }

    #[test]
    fn captured_identity_drop_releases_optional_effects() {
        let drops = EffectDropCounts::default();
        let reducer = crate::if_some_identified(
            drops.parent(),
            crate::OptionalStatePath::new(Option::as_ref, Option::as_mut),
            ActionPath::new(presented_child, PresentationAction::Presented),
            |identity: &SnapshotDropId| identity,
            drops.child(),
        );
        drops.assert_released(reducer, 1);
    }

    #[test]
    fn captured_identity_drop_releases_case_effects() {
        let drops = EffectDropCounts::default();
        let reducer = crate::if_case_identified(
            drops.parent(),
            crate::CaseStatePath::new(Option::as_ref, Option::as_mut),
            ActionPath::new(presented_child, PresentationAction::Presented),
            |identity: &SnapshotDropId| identity,
            drops.child(),
        );
        drops.assert_released(reducer, 1);
    }

    #[test]
    fn captured_identity_drop_releases_required_scope_effects() {
        let drops = EffectDropCounts::default();
        let reducer = crate::scope_identified(
            drops.parent(),
            StatePath::new(
                |state: &Option<SnapshotDropId>| state.as_ref().unwrap(),
                |state: &mut Option<SnapshotDropId>| state.as_mut().unwrap(),
            ),
            ActionPath::new(presented_child, PresentationAction::Presented),
            |identity: &SnapshotDropId| identity,
            drops.child(),
        );
        drops.assert_released(reducer, 1);
    }

    #[test]
    fn captured_identity_drop_releases_presentation_effects() {
        let drops = EffectDropCounts::default();
        let reducer = presentation(
            drops.parent(),
            StatePath::new(
                |state: &Option<SnapshotDropId>| state,
                |state: &mut Option<SnapshotDropId>| state,
            ),
            ActionPath::new(
                |action: &PresentationAction<()>| Some(action),
                |action| action,
            ),
            |identity: &SnapshotDropId| identity,
            drops.child(),
        );
        drops.assert_released(reducer, 1);
    }

    #[test]
    fn captured_identity_drop_releases_ephemeral_effects() {
        let drops = EffectDropCounts::default();
        let reducer = crate::ephemeral(
            drops.parent(),
            StatePath::new(
                |state: &Option<SnapshotDropId>| state,
                |state: &mut Option<SnapshotDropId>| state,
            ),
            ActionPath::new(
                |action: &PresentationAction<()>| Some(action),
                |action| action,
            ),
            |identity: &SnapshotDropId| identity,
            |_: &SnapshotDropId, (): &()| false,
        );
        drops.assert_released(reducer, 0);
    }

    fn assert_projection_rejected(
        mut reducer: impl Reducer<State = Slots, Action = PresentationAction<()>>,
        action: PresentationAction<()>,
        selected_present: bool,
        expected_parent_calls: usize,
    ) {
        let selected = selected_present.then_some((2, 20));
        let mut state = (Some((1, 10)), selected, 0);
        let mut dependencies = DependencyValues::new();
        let (_lifecycle, mut scopes) = ScopeRegistry::for_test();
        let mut transitions = crate::store::scope::ScopeTransitionBatch::default();
        let mut context = ReduceContext::new(
            &mut dependencies,
            MonotonicInstant::ZERO,
            &mut scopes,
            &mut transitions,
        );

        let result = catch_unwind(AssertUnwindSafe(|| {
            reducer.reduce(&mut state, &action, &mut context)
        }));

        // Check state before checking the panic, so the red identifies mutation.
        assert_eq!(state.0, Some((1, 10)));
        assert_eq!(state.1, selected, "another identity must remain untouched");
        assert_eq!(state.2, expected_parent_calls);
        result.expect_err("contradictory identities must be rejected");
    }

    fn mismatched_presentation() -> impl Reducer<State = Slots, Action = PresentationAction<()>> {
        presentation(
            crate::Reduce::new(
                |state: &mut Slots, _: &PresentationAction<()>, _: &mut ReduceContext<'_>| {
                    state.2 += 1;
                    Effect::none()
                },
            ),
            StatePath::new(|state: &Slots| &state.0, |state: &mut Slots| &mut state.1),
            ActionPath::new(
                |action: &PresentationAction<()>| Some(action),
                |action| action,
            ),
            |child: &(u8, usize)| &child.0,
            crate::Reduce::new(
                |state: &mut (u8, usize), (): &(), _: &mut ReduceContext<'_>| {
                    state.1 += 1;
                    Effect::none()
                },
            ),
        )
    }

    #[test]
    fn projection_mismatch_rejects_child_before_reduction() {
        for selected_present in [true, false] {
            assert_projection_rejected(
                mismatched_presentation(),
                PresentationAction::Presented(()),
                selected_present,
                0,
            );
        }
    }

    #[test]
    fn projection_mismatch_does_not_clear_another_presentation() {
        for selected_present in [true, false] {
            assert_projection_rejected(
                mismatched_presentation(),
                PresentationAction::Dismiss,
                selected_present,
                1,
            );
        }
    }

    #[test]
    fn projection_mismatch_does_not_clear_other_ephemeral_data() {
        for selected_present in [true, false] {
            for action in [PresentationAction::Presented(()), PresentationAction::Dismiss] {
                let reducer = crate::ephemeral(
                    crate::Reduce::new(
                        |state: &mut Slots,
                         _: &PresentationAction<()>,
                         _: &mut ReduceContext<'_>| {
                            state.2 += 1;
                            Effect::none()
                        },
                    ),
                    StatePath::new(|state: &Slots| &state.0, |state: &mut Slots| &mut state.1),
                    ActionPath::new(
                        |action: &PresentationAction<()>| Some(action),
                        |action| action,
                    ),
                    |data: &(u8, usize)| &data.0,
                    |_: &(u8, usize), (): &()| true,
                );
                assert_projection_rejected(reducer, action, selected_present, 1);
            }
        }
    }

    #[test]
    fn equal_identity_at_another_address_reduces_and_clears_the_selected_slot() {
        for (action, selected_after) in [
            (PresentationAction::Presented(()), Some((1, 21))),
            (PresentationAction::Dismiss, None),
        ] {
            let mut reducer = mismatched_presentation();
            let mut state = (Some((1, 10)), Some((1, 20)), 0);
            let mut dependencies = DependencyValues::new();
            let (_lifecycle, mut scopes) = ScopeRegistry::for_test();
            let mut transitions = crate::store::scope::ScopeTransitionBatch::default();
            let mut context = ReduceContext::new(
                &mut dependencies,
                MonotonicInstant::ZERO,
                &mut scopes,
                &mut transitions,
            );

            let effect = reducer.reduce(&mut state, &action, &mut context);

            assert!(matches!(effect.program, Program::None));
            assert_eq!(state.0, Some((1, 10)), "the immutable view remains unchanged");
            assert_eq!(state.1, selected_after);
            assert_eq!(state.2, 1);
        }
    }

    #[test]
    fn predicate_identity_change_rejects_child_before_reduction() {
        struct Child {
            original: u8,
            replacement: u8,
            changed: std::cell::Cell<bool>,
            calls: usize,
        }

        type State = (Option<Child>, usize);
        let mut reducer = presentation(
            crate::Reduce::new(
                |state: &mut State, _: &PresentationAction<()>, _: &mut ReduceContext<'_>| {
                    state.1 += 1;
                    Effect::none()
                },
            ),
            StatePath::new(|state: &State| &state.0, |state: &mut State| &mut state.0),
            ActionPath::new(
                |action: &PresentationAction<()>| Some(action),
                |action| action,
            ),
            |child: &Child| {
                if child.changed.get() {
                    &child.replacement
                } else {
                    &child.original
                }
            },
            crate::Reduce::new(|child: &mut Child, (): &(), _: &mut ReduceContext<'_>| {
                child.calls += 1;
                Effect::none()
            }),
        )
        .ephemeral(|child: &Child, (): &()| {
            child.changed.set(true);
            false
        });
        let mut state = (
            Some(Child {
                original: 1,
                replacement: 2,
                changed: std::cell::Cell::new(false),
                calls: 0,
            }),
            0,
        );
        let mut dependencies = DependencyValues::new();
        let (_lifecycle, mut scopes) = ScopeRegistry::for_test();
        let mut transitions = crate::store::scope::ScopeTransitionBatch::default();
        let mut context = ReduceContext::new(
            &mut dependencies,
            MonotonicInstant::ZERO,
            &mut scopes,
            &mut transitions,
        );

        let result = catch_unwind(AssertUnwindSafe(|| {
            reducer.reduce(&mut state, &PresentationAction::Presented(()), &mut context)
        }));

        let child = state.0.as_ref().unwrap();
        assert!(child.changed.get(), "the predicate must run before the rejection");
        assert_eq!(child.calls, 0, "the changed identity must not inherit authority");
        assert_eq!(state.1, 0);
        result.expect_err("a predicate cannot transfer child authority");
    }

    #[test]
    fn unavailable_dismiss_preserves_required_disabled_and_closed_delivery_errors() {
        let collector = IssueCollector::new();
        let closed = collector.reporter();
        assert!(collector.finish().is_empty());
        for (issues, disabled) in [(IssueReporter::default(), true), (closed, false)] {
            let dismiss = Dismiss::new(None, issues);
            let failure = catch_unwind(AssertUnwindSafe(|| dismiss.effect::<()>())).unwrap_err();
            let failure = failure
                .downcast::<IssueReportError>()
                .expect("original typed rejection");
            assert_eq!(failure.is_disabled(), disabled);
            assert_eq!(failure.is_closed(), !disabled);
            assert_eq!(failure.into_issue().code, "rca.dismiss_unavailable");
        }
    }

    #[test]
    fn unavailable_dismiss_propagates_reporter_callback_panic() {
        let dismiss = Dismiss::new(None, IssueReporter::new(|_| std::panic::panic_any(17_u8)));
        let failure = catch_unwind(AssertUnwindSafe(|| dismiss.effect::<()>())).unwrap_err();
        assert_eq!(*failure.downcast::<u8>().unwrap(), 17);
    }

    #[test]
    fn synchronous_presentation_accepts_non_send_state_actions_and_output() {
        use std::{cell::Cell, rc::Rc};
        struct Child;
        impl Reducer for Child {
            type State = (u8, Rc<Cell<u8>>);
            type Action = Rc<u8>;
            fn reduce(
                &mut self,
                state: &mut Self::State,
                action: &Rc<u8>,
                _: &mut ReduceContext<'_>,
            ) -> Effect<Self::Action> {
                state.1.set(state.1.get() + **action);
                Effect::send(7_u8).map(Rc::new)
            }
        }
        struct Parent;
        impl Reducer for Parent {
            type State = Option<(u8, Rc<Cell<u8>>)>;
            type Action = PresentationAction<Rc<u8>>;
            fn reduce(
                &mut self,
                _: &mut Self::State,
                _: &Self::Action,
                _: &mut ReduceContext<'_>,
            ) -> Effect<Self::Action> {
                Effect::none()
            }
        }
        let value = Rc::new(Cell::new(4));
        let mut state = Some((1, Rc::clone(&value)));
        let mut reducer = presentation(
            Parent,
            StatePath::new(
                |state: &Option<(u8, Rc<Cell<u8>>)>| state,
                |state: &mut Option<(u8, Rc<Cell<u8>>)>| state,
            ),
            ActionPath::new(
                |action: &PresentationAction<Rc<u8>>| Some(action),
                |action| action,
            ),
            |child: &(u8, Rc<Cell<u8>>)| &child.0,
            Child,
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
        let effect = reducer.reduce(
            &mut state,
            &PresentationAction::Presented(Rc::new(1)),
            &mut context,
        );
        assert_eq!(value.get(), 5);
        let Program::Merge(mut effects) = effect.program else {
            panic!("presentation preserves its child effect boundary");
        };
        assert_eq!(effects.len(), 1);
        let child_effect = effects.pop().unwrap();
        assert!(child_effect.scope.is_some());
        let Program::Send(PresentationAction::Presented(output)) = child_effect.program else {
            panic!("presentation preserves owned non-Send output");
        };
        assert_eq!(*output, 7);
        assert_eq!(Rc::strong_count(&output), 1);
        let _ = reducer.reduce(&mut state, &PresentationAction::Dismiss, &mut context);
        assert!(state.is_none());
    }
}
}
