//! Composes data-only ephemeral state with ordinary parent reduction.
//!
//! Dismissal owns a private lifetime; ordinary parent work retains its enclosing
//! lifetime. No child reducer is constructed or invoked for the data payload.

use core::{fmt, hash::Hash, marker::PhantomData};
use std::sync::Arc;

use crate::store::scope::{ScopeIdentity, ScopeSlot};
use crate::{ActionPath, Effect, PresentationAction, ReduceContext, Reducer, StatePath};

use super::lifecycle::reconcile_identified;
use super::missing_child::{MissingChildPolicy, missing_child};
use super::presentation::{DismissTarget, bind_dismiss};

/// Holds a parent reducer with identity-aware ephemeral data.
///
/// The parent receives each borrowed action once and inspects data before clear.
/// Selected presented actions and explicit dismissals clear only the identity
/// captured before reduction. A different replacement survives. Equal identities
/// mean the same presentation; removal and later reinsert begin a fresh lifetime.
///
/// Presented actions supply a captured [`crate::Dismiss`] through the context.
/// Only dismissal requests acquire the ephemeral lifetime. Other parent effects
/// and sibling reducers retain their enclosing ownership.
///
/// # Panics
/// Matching actions with absent data require accepted `rca.missing_child` issue
/// delivery before parent reduction. Disabled or closed reporting panics with
/// the original [`crate::IssueReportError`]; reporter callback panics propagate.
/// Contradictory projections panic. Earlier mutations are not rolled back.
pub struct Ephemeral<Parent, State, Action, Identity, Predicate, Id> {
    /// Handles each action once within its original enclosing lifetime.
    parent: Parent,
    /// Borrows the optional data slot, including mutable access for clearing it.
    state: State,
    /// Extracts borrowed input and embeds owned dismissal output.
    action: Action,
    /// Selects the semantic identity that must match before clearing.
    identity: Identity,
    /// Selects presented actions that clear the original data after parent reduction.
    predicate: Predicate,
    /// Identifies this structural site across successive data lifetimes.
    slot: ScopeSlot,
    /// Distinguishes this adapter's dismiss route from nested and sibling routes.
    route: Arc<()>,
    /// Records the identity type without owning another identity value.
    marker: PhantomData<fn() -> Id>,
}

/// Composes an ordinary parent with a data-only ephemeral presentation.
///
/// The predicate borrows the original data and action before parent reduction.
/// Select a semantic identity that changes when replacements should survive.
/// No child reducer or asynchronous action mapping is needed. Local domains and
/// path closures may borrow non-`Send`, non-`Clone` values; only the selected
/// owned identity retains the registry's transport requirements.
///
/// # Examples
///
/// ```
/// use rca_core::{
///     ActionPath, Effect, PresentationAction, ReduceContext, Reducer, StatePath,
///     ephemeral,
/// };
///
/// struct Parent;
///
/// impl Reducer for Parent {
///     type State = Option<u8>;
///     type Action = PresentationAction<()>;
///
///     fn reduce(
///         &mut self,
///         _: &mut Self::State,
///         _: &Self::Action,
///         _: &mut ReduceContext<'_>,
///     ) -> Effect<Self::Action> {
///         Effect::none()
///     }
/// }
///
/// let reducer = ephemeral(
///     Parent,
///     StatePath::new(|state: &Option<u8>| state, |state: &mut Option<u8>| state),
///     ActionPath::new(|action: &PresentationAction<()>| Some(action), |action| action),
///     |data: &u8| data,
///     |_: &u8, _: &()| true,
/// );
/// ```
#[must_use]
#[expect(
    clippy::type_complexity,
    reason = "preserves concrete reducer and path types"
)]
pub fn ephemeral<Parent, Data, DataAction, Get, GetMut, Extract, Embed, Identity, Predicate, Id>(
    parent: Parent,
    state: StatePath<Parent::State, Option<Data>, Get, GetMut>,
    action: ActionPath<Parent::Action, PresentationAction<DataAction>, Extract, Embed>,
    identity: Identity,
    predicate: Predicate,
) -> Ephemeral<
    Parent,
    StatePath<Parent::State, Option<Data>, Get, GetMut>,
    ActionPath<Parent::Action, PresentationAction<DataAction>, Extract, Embed>,
    Identity,
    Predicate,
    Id,
>
where
    Parent: Reducer,
    Identity: for<'a> Fn(&'a Data) -> &'a Id,
    Predicate: Fn(&Data, &DataAction) -> bool,
{
    Ephemeral {
        parent,
        state,
        action,
        identity,
        predicate,
        slot: ScopeSlot::new(),
        route: Arc::new(()),
        marker: PhantomData,
    }
}

impl<Parent, Data, DataAction, Get, GetMut, Extract, Embed, Identity, Predicate, Id> Reducer
    for Ephemeral<
        Parent,
        StatePath<Parent::State, Option<Data>, Get, GetMut>,
        ActionPath<Parent::Action, PresentationAction<DataAction>, Extract, Embed>,
        Identity,
        Predicate,
        Id,
    >
where
    Parent: Reducer,
    Get: for<'a> Fn(&'a Parent::State) -> &'a Option<Data>,
    GetMut: for<'a> Fn(&'a mut Parent::State) -> &'a mut Option<Data>,
    Extract: for<'a> Fn(&'a Parent::Action) -> Option<&'a PresentationAction<DataAction>>,
    Embed: Fn(PresentationAction<DataAction>) -> Parent::Action,
    Identity: for<'a> Fn(&'a Data) -> &'a Id,
    Predicate: Fn(&Data, &DataAction) -> bool,
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
        let data = self.state.get(state).as_ref();
        let before = data.map(|data| (self.identity)(data).clone());
        let lifetime = before.as_ref().map(|id| {
            let node = ReduceContext::node(&mut self.slot);
            (id, context.mount(node, ScopeIdentity::new(id.clone())))
        });
        let token = lifetime.map(|(_, token)| token);
        let target = token.map(|token| {
            DismissTarget::new(
                context.presentation_namespace(),
                Arc::clone(&self.route),
                token,
            )
        });
        let mut clear = false;
        let mut presented_target = None;

        match (self.action.extract(action), data) {
            (Some(PresentationAction::Presented(data_action)), Some(data)) => {
                clear = (self.predicate)(data, data_action);
                presented_target = target.as_ref();
            }
            (Some(PresentationAction::Dismiss), Some(_)) => clear = true,
            (Some(_), None) => missing_child::<Parent::State, Parent::Action, Data, DataAction>(
                MissingChildPolicy::ReportAndBase,
                context,
                "Ephemeral",
                None,
            ),
            (None, _) => {}
        }

        let mut effect = if let Some(target) = presented_target {
            context.with_dismiss_target(target.clone(), |context| {
                self.parent.reduce(state, action, context)
            })
        } else {
            self.parent.reduce(state, action, context)
        };
        if let Some(target) = &target {
            bind_dismiss(&mut effect, target, &|| {
                self.action.embed(PresentationAction::Dismiss)
            });
        }

        let after = self.state.get(state).as_ref().map(&self.identity);
        let after = if clear && before.as_ref() == after {
            let selected = self.state.get_mut(state);
            assert!(
                selected.as_ref().map(&self.identity) == before.as_ref(),
                "dismissed data must retain its observed identity through mutable projection",
            );
            *selected = None;
            None
        } else {
            after
        };

        reconcile_identified(&mut self.slot, lifetime, after, context);
        // Keep returned work owned locally if snapshot destruction panics.
        drop(before);
        effect
    }
}

impl<Parent, State, Action, Identity, Predicate, Id> fmt::Debug
    for Ephemeral<Parent, State, Action, Identity, Predicate, Id>
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ephemeral").finish_non_exhaustive()
    }
}

#[cfg(all(test, feature = "test-util"))]
ordinary_test_items! {
mod tests {
    use super::*;
    use crate::effect::Program;
    use crate::store::scope::ScopeRegistry;
    use crate::{
        DependencyValues, IssueCollector, IssueReportError, IssueReporter, MonotonicInstant,
    };
    use std::{
        cell::Cell,
        panic::{AssertUnwindSafe, catch_unwind},
        rc::Rc,
    };

    struct LocalData<'a> {
        id: u8,
        value: &'a Rc<Cell<u8>>,
    }
    struct LocalAction<'a> {
        amount: &'a Rc<Cell<u8>>,
    }
    struct LocalParent<'a> {
        calls: &'a Cell<u8>,
        parent_action: &'a Cell<*const LocalAction<'a>>,
    }
    impl<'a> Reducer for LocalParent<'a> {
        type State = Option<LocalData<'a>>;
        type Action = PresentationAction<LocalAction<'a>>;
        fn reduce(
            &mut self,
            state: &mut Self::State,
            action: &Self::Action,
            _: &mut ReduceContext<'_>,
        ) -> Effect<Self::Action> {
            self.calls.set(self.calls.get() + 1);
            if let PresentationAction::Presented(action) = action {
                self.parent_action.set(core::ptr::from_ref(action));
                let value = state.as_ref().unwrap().value;
                value.set(value.get() + action.amount.get());
            }
            Effect::none()
        }
    }

    // Added domain/closure transport bounds or cloned/reconstructed actions break this.
    #[test]
    fn local_borrowed_move_only_domain_preserves_the_action_address_and_parent_mutation() {
        let value = Rc::new(Cell::new(7));
        let amount = Rc::new(Cell::new(3));
        let calls = Cell::new(0);
        let parent_action = Cell::new(core::ptr::null());
        let predicate_action = Cell::new(core::ptr::null());
        let mut reducer = ephemeral(
            LocalParent {
                calls: &calls,
                parent_action: &parent_action,
            },
            StatePath::new(
                |state: &Option<LocalData<'_>>| state,
                |state: &mut Option<LocalData<'_>>| state,
            ),
            ActionPath::new(
                |action: &PresentationAction<LocalAction<'_>>| Some(action),
                |action| {
                    std::hint::black_box(&value);
                    action
                },
            ),
            |data: &LocalData<'_>| &data.id,
            |data: &LocalData<'_>, action: &LocalAction<'_>| {
                assert_eq!(data.value.get(), 7);
                predicate_action.set(core::ptr::from_ref(action));
                true
            },
        );
        let mut state = Some(LocalData {
            id: 1,
            value: &value,
        });
        let action = PresentationAction::Presented(LocalAction { amount: &amount });
        let PresentationAction::Presented(original) = &action else {
            unreachable!()
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
        let effect = reducer.reduce(&mut state, &action, &mut context);
        assert!(matches!(effect.program, Program::None));
        assert!(state.is_none());
        assert_eq!(value.get(), 10);
        assert_eq!(calls.get(), 1);
        assert_eq!(predicate_action.get(), core::ptr::from_ref(original));
        assert_eq!(parent_action.get(), core::ptr::from_ref(original));
    }

    struct CountParent(Rc<Cell<u8>>);
    impl Reducer for CountParent {
        type State = Option<u8>;
        type Action = PresentationAction<()>;
        fn reduce(
            &mut self,
            _: &mut Self::State,
            _: &Self::Action,
            _: &mut ReduceContext<'_>,
        ) -> Effect<Self::Action> {
            self.0.set(self.0.get() + 1);
            Effect::none()
        }
    }
    fn counting(
        calls: Rc<Cell<u8>>,
    ) -> impl Reducer<State = Option<u8>, Action = PresentationAction<()>> {
        ephemeral(
            CountParent(calls),
            StatePath::new(|state: &Option<u8>| state, |state: &mut Option<u8>| state),
            ActionPath::new(
                |action: &PresentationAction<()>| Some(action),
                |action| action,
            ),
            |data: &u8| data,
            |_: &u8, (): &()| true,
        )
    }

    // Suppressing a failed required report, or running the parent first, breaks this.
    #[test]
    fn absent_data_preserves_disabled_closed_and_callback_failures_before_parent() {
        for action in [
            PresentationAction::Presented(()),
            PresentationAction::Dismiss,
        ] {
            let collector = IssueCollector::new();
            let closed = collector.reporter();
            assert_eq!(collector.finish(), [] as [crate::Issue; 0]);
            for (reporter, disabled) in [(IssueReporter::default(), true), (closed, false)] {
                let calls = Rc::new(Cell::new(0));
                let mut reducer = counting(Rc::clone(&calls));
                let mut dependencies = DependencyValues::new();
                let (_scopes_lifecycle, mut scopes) = ScopeRegistry::for_test();
                let mut context_transitions = crate::store::scope::ScopeTransitionBatch::default();
                let mut context = ReduceContext::new(
                    &mut dependencies,
                    MonotonicInstant::ZERO,
                    &mut scopes,
                    &mut context_transitions,
                )
                .with_issue_reporter(reporter);
                let panic = catch_unwind(AssertUnwindSafe(|| {
                    reducer.reduce(&mut None, &action, &mut context)
                }))
                .expect_err("required report rejects delivery");
                let error = panic
                    .downcast::<IssueReportError>()
                    .expect("original typed error");
                assert_eq!(error.is_disabled(), disabled);
                assert_eq!(error.is_closed(), !disabled);
                assert_eq!(error.into_issue().code, "rca.missing_child");
                assert_eq!(calls.get(), 0);
            }
            let calls = Rc::new(Cell::new(0));
            let mut reducer = counting(Rc::clone(&calls));
            let mut dependencies = DependencyValues::new();
            let (_scopes_lifecycle, mut scopes) = ScopeRegistry::for_test();
            let mut context_transitions = crate::store::scope::ScopeTransitionBatch::default();
            let mut context = ReduceContext::new(
                &mut dependencies,
                MonotonicInstant::ZERO,
                &mut scopes,
                &mut context_transitions,
            )
            .with_issue_reporter(IssueReporter::new(|_| std::panic::panic_any(17_u8)));
            let panic = catch_unwind(AssertUnwindSafe(|| {
                reducer.reduce(&mut None, &action, &mut context)
            }))
            .expect_err("reporter panics");
            assert_eq!(*panic.downcast::<u8>().unwrap(), 17);
            assert_eq!(calls.get(), 0);
        }
    }

    // Accepted reporting must allow exactly one parent call, not silently drop the action.
    #[test]
    fn accepted_missing_data_report_precedes_one_parent_call() {
        for action in [
            PresentationAction::Presented(()),
            PresentationAction::Dismiss,
        ] {
            let calls = Rc::new(Cell::new(0));
            let mut reducer = counting(Rc::clone(&calls));
            let collector = IssueCollector::new();
            let mut dependencies = DependencyValues::new();
            let (_scopes_lifecycle, mut scopes) = ScopeRegistry::for_test();
            let mut context_transitions = crate::store::scope::ScopeTransitionBatch::default();
            let mut context = ReduceContext::new(
                &mut dependencies,
                MonotonicInstant::ZERO,
                &mut scopes,
                &mut context_transitions,
            )
            .with_issue_reporter(collector.reporter());
            let effect = reducer.reduce(&mut None, &action, &mut context);
            assert!(matches!(effect.program, Program::None));
            assert_eq!(calls.get(), 1);
            let issues = collector.finish();
            assert_eq!(issues.len(), 1);
            assert_eq!(issues[0].code, "rca.missing_child");
        }
    }
}
}
