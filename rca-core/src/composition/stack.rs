//! Composes ordered headless routes with private entry lifetimes.
//!
//! Logical IDs identify entries, not payloads or runtime authority. Equal route
//! values can coexist. Removing and reinserting an ID starts a fresh lifetime.
//! Stack persistence encodes ordered values, never IDs or private generations.

use core::{
    fmt,
    hash::{Hash, Hasher},
    marker::PhantomData,
};
use std::{backtrace::Backtrace, sync::Arc};

use super::keyed::{BorrowedEntryStamp, KeyedLifecycle};
use super::missing_child::{MissingChildPolicy, missing_child};
use super::presentation::{DismissTarget, bind_dismiss};
use crate::{
    ActionPath, DuplicateId, Effect, Issue, IssueSeverity, KeyedState, ReduceContext, Reducer,
    StatePath,
};

/// Encodes route payloads without retaining runtime identities.
mod codec;

#[doc(inline)]
pub use codec::{EncodedStackRoute, StackDecodeError, StackEncoding};

/// Identifies one logical stack entry without granting runtime authority.
///
/// Each new ID owns one reference-counted allocation. Cloning retains the same
/// logical ID. Neither its address nor private lifetime stamps are serialised.
#[derive(Clone)]
pub struct StackId(
    /// Keeps this logical allocation identity alive while any ID clone exists.
    Arc<()>,
);

impl StackId {
    /// Creates a fresh logical identifier without an ambient generator.
    #[must_use]
    pub fn new() -> Self {
        Self(Arc::new(()))
    }
}

impl Default for StackId {
    fn default() -> Self {
        Self::new()
    }
}

impl PartialEq for StackId {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for StackId {}

impl Hash for StackId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.0).hash(state);
    }
}

impl fmt::Debug for StackId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("StackId(..)")
    }
}

/// Holds ordered independently identified route values.
///
/// Equality and hashes include ordered logical IDs and values, not private entry
/// generations. Mutable payload access preserves lifetime; removal and insertion
/// renew it. Clone preserves IDs and stamps, while decoding creates fresh ones.
///
/// # Examples
///
/// ```
/// use rca_core::StackState;
/// let mut path = StackState::new();
/// let first = path.push("detail");
/// let second = path.push("detail");
/// assert_ne!(first, second);
/// assert_eq!(path.try_pop_to(&first).unwrap(), vec![(second, "detail")]);
/// assert_eq!(path.get(&first), Some(&"detail"));
/// ```
pub struct StackState<State> {
    /// Preserves order and insertion identity without requiring cloned payloads.
    entries: KeyedState<StackId, State>,
}

impl<State> StackState<State> {
    /// Lends the keyed storage for mounted route selection and identity checks.
    pub(crate) fn entries(&self) -> &KeyedState<StackId, State> {
        &self.entries
    }

    /// Lends the same storage for checked mounted entry mutations.
    pub(crate) fn entries_mut(&mut self) -> &mut KeyedState<StackId, State> {
        &mut self.entries
    }

    /// Creates an empty stack.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: KeyedState::new(),
        }
    }

    /// Returns the number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns whether there are no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterates over logical IDs in stack order.
    #[must_use = "iterators are lazy and do nothing unless consumed"]
    pub fn ids(&self) -> impl ExactSizeIterator<Item = &StackId> {
        self.entries.ids()
    }

    /// Iterates over route values in stack order.
    #[must_use = "iterators are lazy and do nothing unless consumed"]
    pub fn values(&self) -> impl ExactSizeIterator<Item = &State> {
        self.entries.values()
    }

    /// Iterates over logical IDs and route values in stack order.
    #[must_use = "iterators are lazy and do nothing unless consumed"]
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&StackId, &State)> {
        self.entries.iter()
    }

    /// Borrows the value identified by an existing logical ID.
    #[must_use]
    pub fn get(&self, id: &StackId) -> Option<&State> {
        self.entries.get(id)
    }

    /// Mutably borrows a value without replacing its lifetime.
    pub fn get_mut(&mut self, id: &StackId) -> Option<&mut State> {
        self.entries.get_mut(id)
    }

    /// Appends a value with a fresh logical ID.
    ///
    /// # Panics
    /// Panics if private entry revisions overflow.
    pub fn push(&mut self, state: State) -> StackId {
        let id = StackId::new();
        self.entries.insert(id.clone(), state);
        id
    }

    /// Appends an explicitly identified value with a fresh insertion lifetime.
    ///
    /// # Errors
    /// Returns the original owned inputs if the logical ID already exists.
    /// # Panics
    /// Panics if private entry revisions overflow.
    pub fn try_push(
        &mut self,
        id: StackId,
        state: State,
    ) -> Result<(), DuplicateId<StackId, State>> {
        self.entries.try_insert(id, state)
    }

    /// Removes the final entry, or returns `None` when empty.
    ///
    /// # Panics
    /// Panics if private entry revisions overflow.
    pub fn pop(&mut self) -> Option<(StackId, State)> {
        self.entries.pop()
    }

    /// Removes a checked number of final entries in their original order.
    ///
    /// # Errors
    /// Rejects counts greater than the stack length without mutation.
    /// # Panics
    /// Panics if private entry revisions overflow.
    pub fn try_pop_count(
        &mut self,
        count: usize,
    ) -> Result<Vec<(StackId, State)>, StackMutationError> {
        let start = self
            .len()
            .checked_sub(count)
            .ok_or_else(|| StackMutationError::new(false))?;
        Ok(self
            .entries
            .try_remove_range(start..self.len())
            .expect("checked stack suffix"))
    }

    /// Removes the target entry and its suffix in original order.
    ///
    /// # Errors
    /// Rejects unknown logical IDs without mutation.
    /// # Panics
    /// Panics if private entry revisions overflow.
    pub fn try_pop_from(
        &mut self,
        id: &StackId,
    ) -> Result<Vec<(StackId, State)>, StackMutationError> {
        self.remove_suffix(id, false)
    }

    /// Preserves the target entry and removes its suffix in original order.
    ///
    /// # Errors
    /// Rejects unknown logical IDs without mutation.
    /// # Panics
    /// Panics if private entry revisions overflow.
    pub fn try_pop_to(
        &mut self,
        id: &StackId,
    ) -> Result<Vec<(StackId, State)>, StackMutationError> {
        self.remove_suffix(id, true)
    }

    /// Removes an owned suffix after resolving its target within the same borrow.
    ///
    /// `exclusive` preserves the target. Returned entries keep their original order.
    /// Unknown IDs fail before mutation; no user callback separates lookup and removal.
    fn remove_suffix(
        &mut self,
        id: &StackId,
        exclusive: bool,
    ) -> Result<Vec<(StackId, State)>, StackMutationError> {
        let index = self
            .entries
            .index_of(id)
            .ok_or_else(|| StackMutationError::new(true))?;
        let start = index + usize::from(exclusive);
        Ok(self
            .entries
            .try_remove_range(start..self.len())
            .expect("resolved stack suffix"))
    }

    /// Borrows insertion identity independently of equal IDs or payload values.
    pub(crate) fn entry_stamp(&self, id: &StackId) -> Option<BorrowedEntryStamp<'_>> {
        self.entries.entry_stamp(id)
    }

    /// Borrows a route and its insertion stamp from the same lookup.
    pub(crate) fn get_with_stamp(&self, id: &StackId) -> Option<(&State, BorrowedEntryStamp<'_>)> {
        self.entries.get_with_stamp(id)
    }
}

impl<State> Default for StackState<State> {
    fn default() -> Self {
        Self::new()
    }
}

impl<State: Clone> Clone for StackState<State> {
    fn clone(&self) -> Self {
        Self {
            entries: self.entries.clone(),
        }
    }
}

impl<State> fmt::Debug for StackState<State> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StackState")
            .field("len", &self.len())
            .finish_non_exhaustive()
    }
}

impl<State: PartialEq> PartialEq for StackState<State> {
    fn eq(&self, other: &Self) -> bool {
        self.entries == other.entries
    }
}

impl<State: Eq> Eq for StackState<State> {}

impl<State: Hash> Hash for StackState<State> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.entries.hash(state);
    }
}

/// Reports an unknown stack ID or an excessive removal count.
#[derive(Debug)]
pub struct StackMutationError {
    /// Distinguishes an absent ID from a removal count greater than the length.
    unknown_id: bool,
    /// Records the caller location when backtrace capture is enabled.
    backtrace: Backtrace,
}

impl StackMutationError {
    /// Captures one of the two checked stack-input failures without retaining payloads.
    fn new(unknown_id: bool) -> Self {
        Self {
            unknown_id,
            backtrace: Backtrace::capture(),
        }
    }

    /// Returns whether the requested ID was absent.
    #[must_use]
    pub const fn is_unknown_id(&self) -> bool {
        self.unknown_id
    }

    /// Returns whether the requested count exceeded the length.
    #[must_use]
    pub const fn is_over_pop(&self) -> bool {
        !self.unknown_id
    }

    /// Returns the captured error backtrace.
    pub const fn backtrace(&self) -> &Backtrace {
        &self.backtrace
    }
}

impl fmt::Display for StackMutationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "stack mutation failed: {}\n{}",
            if self.unknown_id {
                "unknown ID"
            } else {
                "removal count exceeds length"
            },
            self.backtrace
        )
    }
}

impl std::error::Error for StackMutationError {}

/// Routes child input, typed push requests and ordered suffix removal.
///
/// A direct action is fresh input. Historical output must retain its effect or
/// scoped-client authority rather than reconstructing a fresh action.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StackAction<Push, Action> {
    /// Initialises and installs a route before parent reduction.
    Push {
        /// The fresh logical ID requested by the caller.
        id: StackId,
        /// Borrowed input passed to the route initialiser.
        input: Push,
    },
    /// Reduces the selected child before its parent.
    Element {
        /// The logical ID of the selected route.
        id: StackId,
        /// Input for the selected child reducer.
        action: Action,
    },
    /// Lets the parent inspect the target before inclusive suffix removal.
    PopFrom {
        /// The first logical ID to remove.
        id: StackId,
    },
    /// Lets the parent inspect the target before exclusive suffix removal.
    PopTo {
        /// The final logical ID to preserve.
        id: StackId,
    },
}

/// Composes a parent with one reducer shared across ordered stack entries.
///
/// Push initialisation happens once after duplicate rejection and before parent
/// reduction. Child actions reduce child before parent. Pop reduces parent first,
/// then removes only if the mutable projection retains the captured insertion.
/// An absent target or a new insertion leaves that selected stack unchanged.
/// Every reduction reconciles removed/replaced lifetimes. Outside reduction,
/// raw state mutation is observed only when this adapter next reduces.
/// Synchronous state, push inputs and actions can be non-`Send`.
/// Effect mappers still require `Send + Sync + 'static`.
/// Store admission retains its separate transport bounds.
///
/// # Panics
/// Missing children, duplicate pushes and unknown pops require accepted issue
/// delivery before parent reduction. Rejected delivery panics with the original
/// issue error. User callback panics propagate unchanged. Child projection
/// mismatches, contradictory pushes and private revision overflow panic.
/// Earlier mutations are not rolled back.
pub struct Stack<Parent, Child, State, Action, Initialise, Push> {
    /// Reduces after child actions and before deferred pop removal.
    parent: Parent,
    /// Handles every element while the context carries that entry's scope.
    child: Child,
    /// Borrows stack storage without owning or cloning application state.
    state: State,
    /// Extracts stack input and maps escaping child effects back to the parent.
    action: Action,
    /// Creates a route once after duplicate rejection and before parent reduction.
    initialise: Initialise,
    /// Keeps declaration slots; each Store registry owns its reconciliation cache.
    lifecycle: KeyedLifecycle<StackId>,
    /// Distinguishes this adapter's dismissal destination within the Store namespace.
    route: Arc<()>,
    /// Records the borrowed push-input type without owning or constraining a value.
    marker: PhantomData<fn(&Push)>,
}

/// Composes typed push initialisation, child reduction and ordered removal.
#[must_use]
#[expect(
    clippy::type_complexity,
    reason = "preserves concrete reducer and path types"
)]
pub fn stack<Parent: Reducer, Child: Reducer, Get, GetMut, Extract, Embed, Initialise, Push>(
    parent: Parent,
    state: StatePath<Parent::State, StackState<Child::State>, Get, GetMut>,
    action: ActionPath<Parent::Action, StackAction<Push, Child::Action>, Extract, Embed>,
    initialise: Initialise,
    child: Child,
) -> Stack<
    Parent,
    Child,
    StatePath<Parent::State, StackState<Child::State>, Get, GetMut>,
    ActionPath<Parent::Action, StackAction<Push, Child::Action>, Extract, Embed>,
    Initialise,
    Push,
>
where
    Initialise: FnMut(&Push, &mut ReduceContext<'_>) -> Child::State,
{
    Stack {
        parent,
        child,
        state,
        action,
        initialise,
        lifecycle: KeyedLifecycle::new(),
        route: Arc::new(()),
        marker: PhantomData,
    }
}

impl<Parent, Child, State, Action, Initialise, Push> fmt::Debug
    for Stack<Parent, Child, State, Action, Initialise, Push>
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Stack").finish_non_exhaustive()
    }
}

impl<Parent, Child, Get, GetMut, Extract, Embed, Initialise, Push> Reducer
    for Stack<
        Parent,
        Child,
        StatePath<Parent::State, StackState<Child::State>, Get, GetMut>,
        ActionPath<Parent::Action, StackAction<Push, Child::Action>, Extract, Embed>,
        Initialise,
        Push,
    >
where
    Parent: Reducer,
    Child: Reducer,
    Parent::Action: 'static,
    Child::Action: 'static,
    Push: 'static,
    Get: for<'a> Fn(&'a Parent::State) -> &'a StackState<Child::State>,
    GetMut: for<'a> Fn(&'a mut Parent::State) -> &'a mut StackState<Child::State>,
    Extract: for<'a> Fn(&'a Parent::Action) -> Option<&'a StackAction<Push, Child::Action>>,
    Embed: Fn(StackAction<Push, Child::Action>) -> Parent::Action + Clone + Send + Sync + 'static,
    Initialise: FnMut(&Push, &mut ReduceContext<'_>) -> Child::State,
{
    type State = Parent::State;
    type Action = Parent::Action;

    #[expect(
        clippy::too_many_lines,
        reason = "keeps stack routing, parent ordering and final reconciliation together"
    )]
    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        let (_, (stack_action, active)) =
            self.lifecycle
                .reconcile_with(&self.state.get(state).entries, context, |snapshot| {
                    let stack_action = self.action.extract(action);
                    let active = match stack_action {
                        Some(StackAction::Element { id, .. }) => snapshot.active(id),
                        _ => None,
                    };
                    (stack_action, active)
                });
        let mut child_effect = Effect::none();
        let mut pop = None;

        match stack_action {
            Some(StackAction::Push { id, input }) => {
                if self.state.get(state).get(id).is_some() {
                    report(
                        context,
                        "rca.stack_duplicate_id",
                        "stack push targeted an existing logical ID",
                    );
                } else {
                    let value = (self.initialise)(input, context);
                    assert!(
                        self.state
                            .get_mut(state)
                            .try_push(id.clone(), value)
                            .is_ok(),
                        "duplicate push checked before initialisation"
                    );
                }
            }
            Some(StackAction::Element { id, action }) => {
                if let Some((active_id, token, stamp)) = active {
                    let target = DismissTarget::new(
                        context.presentation_namespace(),
                        Arc::clone(&self.route),
                        token,
                    );
                    let (child, selected_stamp) = self
                        .state
                        .get_mut(state)
                        .entries
                        .get_mut_with_stamp(id)
                        .expect("active stack child remains borrowable");
                    assert!(
                        stamp.matches(selected_stamp),
                        "active stack child must retain its insertion lifetime through the mutable projection"
                    );
                    drop(stamp);
                    let effect = context.with_scope(token, |context| {
                        context.with_dismiss_target(target.clone(), |context| {
                            self.child.reduce(child, action, context)
                        })
                    });
                    let output_id = Arc::clone(&active_id);
                    let mut effect =
                        effect
                            .in_scope(token)
                            .map(move |action| StackAction::Element {
                                id: output_id.as_ref().clone(),
                                action,
                            });
                    bind_dismiss(&mut effect, &target, &|| StackAction::PopFrom {
                        id: active_id.as_ref().clone(),
                    });
                    child_effect = effect.map(self.action.clone_embedder());
                } else {
                    missing_child::<Parent::State, Parent::Action, Child::State, Child::Action>(
                        MissingChildPolicy::ReportAndBase,
                        context,
                        "Stack",
                        Some(core::any::type_name::<StackId>()),
                    );
                }
            }
            Some(pop_action @ (StackAction::PopFrom { id } | StackAction::PopTo { id })) => {
                if let Some(stamp) = self.state.get(state).entry_stamp(id) {
                    pop = Some((
                        id,
                        stamp.to_owned(),
                        matches!(pop_action, StackAction::PopTo { .. }),
                    ));
                } else {
                    report(
                        context,
                        "rca.stack_unknown_id",
                        "stack pop targeted an unknown logical ID",
                    );
                }
            }
            None => {}
        }

        let parent_effect = self.parent.reduce(state, action, context);
        if let Some((id, before, exclusive)) = pop {
            let selected = self.state.get_mut(state);
            if selected
                .entry_stamp(id)
                .is_some_and(|after| before.matches(after))
            {
                let _ = selected
                    .remove_suffix(id, exclusive)
                    .expect("captured stack incarnation remains present");
            }
        }

        self.lifecycle
            .reconcile(&self.state.get(state).entries, context);
        Effect::merge([child_effect, parent_effect])
    }
}

/// Requires diagnostic delivery before invalid stack input reaches the parent.
///
/// Delivery failures preserve their typed payload. Reporter panics propagate unchanged.
#[cold]
#[track_caller]
fn report(context: &ReduceContext<'_>, code: &'static str, message: &'static str) {
    if let Err(error) =
        context.report_issue(Issue::new(code, message).with_severity(IssueSeverity::Error))
    {
        std::panic::panic_any(error);
    }
}

#[cfg(test)]
ordinary_test_items! {
mod state_tests {
    use super::StackState;

    #[test]
    fn pop_returns_owned_state_without_dropping_it_or_renewing_retained_entries() {
        struct MoveOnly(std::rc::Rc<std::cell::Cell<u8>>);
        impl Drop for MoveOnly {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }

        let drops = std::rc::Rc::new(std::cell::Cell::new(0));
        let mut stack = StackState::new();
        let first = stack.push(MoveOnly(std::rc::Rc::clone(&drops)));
        let last = stack.push(MoveOnly(std::rc::Rc::clone(&drops)));
        let retained = stack.entry_stamp(&first).unwrap().to_owned();
        let removed = stack.entry_stamp(&last).unwrap().to_owned();

        let (id, value) = stack.pop().unwrap();
        assert_eq!(id, last);
        assert_eq!(drops.get(), 0);
        assert_eq!(stack.len(), 1);
        assert_eq!(stack.ids().next(), Some(&first));
        assert!(retained == stack.entry_stamp(&first).unwrap().to_owned());
        assert!(stack.get(&last).is_none());

        assert!(stack.try_push(id, value).is_ok());
        assert!(removed != stack.entry_stamp(&last).unwrap().to_owned());
        drop(stack.pop().unwrap());
        assert_eq!(drops.get(), 1);
        drop(stack.pop().unwrap());
        assert_eq!(drops.get(), 2);
        assert!(stack.pop().is_none());
    }

}
}

#[cfg(all(test, feature = "test-util"))]
ordinary_test_items! {
mod tests {
    use super::*;
    use crate::store::scope::ScopeRegistry;
    use crate::{
        DependencyValues, IssueCollector, IssueReportError, IssueReporter, MonotonicInstant,
    };
    use std::{
        cell::Cell,
        panic::{AssertUnwindSafe, catch_unwind},
        rc::Rc,
    };

    struct Parent(Rc<Cell<u8>>);
    impl Reducer for Parent {
        type State = StackState<Rc<Cell<u8>>>;
        type Action = StackAction<(), ()>;
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
    struct Child;
    impl Reducer for Child {
        type State = Rc<Cell<u8>>;
        type Action = ();
        fn reduce(
            &mut self,
            state: &mut Self::State,
            (): &(),
            _: &mut ReduceContext<'_>,
        ) -> Effect<()> {
            state.set(state.get() + 1);
            Effect::none()
        }
    }
    fn reducer(
        parent: Rc<Cell<u8>>,
    ) -> impl Reducer<State = StackState<Rc<Cell<u8>>>, Action = StackAction<(), ()>> {
        stack(
            Parent(parent),
            StatePath::new(
                |state: &StackState<Rc<Cell<u8>>>| state,
                |state: &mut StackState<Rc<Cell<u8>>>| state,
            ),
            ActionPath::new(|action: &StackAction<(), ()>| Some(action), |action| action),
            |(): &(), _: &mut ReduceContext<'_>| Rc::new(Cell::new(0)),
            Child,
        )
    }

    #[test]
    fn synchronous_stack_accepts_non_send_state_without_transport_bounds() {
        let parent = Rc::new(Cell::new(0));
        let mut reducer = reducer(Rc::clone(&parent));
        let value = Rc::new(Cell::new(4));
        let mut state = StackState::new();
        let id = state.push(Rc::clone(&value));
        let mut dependencies = DependencyValues::new();
        let (_scopes_lifecycle, mut scopes) = ScopeRegistry::for_test();
        let mut context_transitions = crate::store::scope::ScopeTransitionBatch::default();
        let mut context = ReduceContext::new(
            &mut dependencies,
            MonotonicInstant::ZERO,
            &mut scopes,
            &mut context_transitions,
        );
        let _ = reducer.reduce(
            &mut state,
            &StackAction::Element { id, action: () },
            &mut context,
        );
        assert_eq!(value.get(), 5);
        assert_eq!(parent.get(), 1);
    }

    #[test]
    fn synchronous_stack_accepts_non_send_push_and_mapped_child_output() {
        type Action = StackAction<Rc<Cell<u8>>, Rc<Cell<u8>>>;
        let parent_calls = Rc::new(Cell::new(0));
        let parent_count = Rc::clone(&parent_calls);
        let initializer_calls = Rc::new(Cell::new(0));
        let initializer_count = Rc::clone(&initializer_calls);
        let mut reducer = stack(
            crate::Reduce::new(
                move |_: &mut StackState<u8>, _: &Action, _: &mut ReduceContext<'_>| {
                    parent_count.set(parent_count.get() + 1);
                    Effect::none()
                },
            ),
            crate::identity_state_path(),
            crate::identity_action_path(),
            move |input: &Rc<Cell<u8>>, _: &mut ReduceContext<'_>| {
                initializer_count.set(initializer_count.get() + 1);
                input.get()
            },
            crate::Reduce::new(
                |state: &mut u8, action: &Rc<Cell<u8>>, _: &mut ReduceContext<'_>| {
                    *state += action.get();
                    Effect::future(async { Some(9_u8) })
                        .map(|value| Rc::new(Cell::new(value)))
                },
            ),
        );
        let mut state = StackState::new();
        let id = StackId::new();
        let mut dependencies = DependencyValues::new();
        let (_scopes_lifecycle, mut scopes) = ScopeRegistry::for_test();
        let mut transitions = crate::store::scope::ScopeTransitionBatch::default();
        let mut context = ReduceContext::new(
            &mut dependencies,
            MonotonicInstant::ZERO,
            &mut scopes,
            &mut transitions,
        );

        let push = StackAction::Push {
            id: id.clone(),
            input: Rc::new(Cell::new(3)),
        };
        let _ = reducer.reduce(&mut state, &push, &mut context);
        assert_eq!(initializer_calls.get(), 1);
        assert_eq!(state.get(&id), Some(&3));
        let input = StackAction::Element {
            id: id.clone(),
            action: Rc::new(Cell::new(2)),
        };
        let effect = reducer.reduce(&mut state, &input, &mut context);
        assert_eq!(state.get(&id), Some(&5));
        assert_eq!(parent_calls.get(), 2);

        drop((context, reducer, state, input, push));
        let crate::effect::Program::Merge(mut children) = effect.program else {
            panic!("the merged child must retain its deferred output");
        };
        assert_eq!(children.len(), 1);
        let child = children.pop().unwrap();
        assert!(child.scope.is_some());
        let crate::effect::Program::Future(future) = child.program else {
            panic!("the nonempty mapped child future must remain deferred");
        };
        let Some(StackAction::Element {
            id: output_id,
            action,
        }) = futures_executor::block_on(future)
        else {
            panic!("the mapped child output must keep its stack route");
        };
        assert_eq!(output_id, id);
        assert_eq!(action.get(), 9);
        assert_eq!(Rc::strong_count(&action), 1);
        let weak = Rc::downgrade(&action);
        drop(action);
        assert!(weak.upgrade().is_none());
    }

    /// Checks insertion authority without letting rejected routes mutate either stack.
    fn assert_element_insertion_lifetime(
        observed: StackState<Rc<Cell<u8>>>,
        selected: StackState<Rc<Cell<u8>>>,
        same_lifetime: bool,
    ) {
        type State = (StackState<Rc<Cell<u8>>>, StackState<Rc<Cell<u8>>>, usize);
        let id = observed.ids().next().unwrap().clone();
        let mut reducer = stack(
            crate::Reduce::new(
                |state: &mut State, _: &StackAction<(), ()>, _: &mut ReduceContext<'_>| {
                    state.2 += 1;
                    Effect::none()
                },
            ),
            StatePath::new(|state: &State| &state.0, |state: &mut State| &mut state.1),
            ActionPath::new(|action: &StackAction<(), ()>| Some(action), |action| action),
            |(): &(), _: &mut ReduceContext<'_>| Rc::new(Cell::new(0)),
            Child,
        );
        let mut state = (observed, selected, 0);
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
                &StackAction::Element {
                    id: id.clone(),
                    action: (),
                },
                &mut context,
            )
        }));
        assert_eq!(state.0.get(&id).unwrap().get(), 10);
        if same_lifetime {
            assert!(
                result.is_ok(),
                "a cloned insertion accepts state-only changes"
            );
            assert_eq!(state.1.get(&id).unwrap().get(), 21);
            assert_eq!(state.2, 1);
        } else {
            assert!(
                result.is_err(),
                "a different insertion must fail before reduction"
            );
            assert_eq!(state.1.get(&id).unwrap().get(), 20);
            assert_eq!(state.2, 0);
        }
    }

    #[test]
    fn element_rejects_equal_id_from_independent_stack_before_reduction() {
        let mut observed = StackState::new();
        let id = observed.push(Rc::new(Cell::new(10)));
        let mut selected = StackState::new();
        selected.try_push(id, Rc::new(Cell::new(20))).unwrap();
        assert_element_insertion_lifetime(observed, selected, false);
    }

    #[test]
    fn element_rejects_reinserted_id_from_clone_branch_before_reduction() {
        let mut observed = StackState::new();
        let id = observed.push(Rc::new(Cell::new(10)));
        let mut selected = observed.clone();
        let (removed_id, value) = selected.pop().unwrap();
        assert_eq!(removed_id, id);
        assert_eq!(value.get(), 10);
        selected.try_push(id, Rc::new(Cell::new(20))).unwrap();
        assert_element_insertion_lifetime(observed, selected, false);
    }

    #[test]
    fn element_accepts_same_insertion_with_state_only_change() {
        let mut observed = StackState::new();
        let id = observed.push(Rc::new(Cell::new(10)));
        let mut selected = observed.clone();
        *selected.get_mut(&id).unwrap() = Rc::new(Cell::new(20));
        assert_element_insertion_lifetime(observed, selected, true);
    }

    #[derive(Clone, Copy)]
    enum PopSelection {
        Independent,
        Reinserted,
        Cloned,
        Absent,
    }

    /// Exercises suffix removal through the public adapter with distinct projections.
    fn assert_pop_insertion_lifetime(exclusive: bool, selection: PopSelection) {
        type State = (StackState<u8>, StackState<u8>, usize, usize);
        let mut observed = StackState::new();
        let id = observed.push(10);
        observed.push(11);
        let same_lifetime = matches!(selection, PopSelection::Cloned);
        let mut selected = match selection {
            PopSelection::Independent => {
                let mut selected = StackState::new();
                selected.try_push(id.clone(), 20).unwrap();
                selected.push(21);
                selected
            }
            PopSelection::Reinserted => {
                let mut selected = observed.clone();
                selected.try_pop_from(&id).unwrap();
                selected.try_push(id.clone(), 20).unwrap();
                selected.push(21);
                selected
            }
            PopSelection::Cloned => observed.clone(),
            PopSelection::Absent => StackState::new(),
        };
        if same_lifetime {
            *selected.get_mut(&id).unwrap() = 20;
        }
        let before: Vec<_> = selected
            .iter()
            .map(|(id, value)| (id.clone(), *value))
            .collect();
        // Initial reconciliation adds exactly one cached logical-ID owner.
        let expected_id_owners = Arc::strong_count(&id.0) + 1;
        let mut reducer = stack(
            crate::Reduce::new(
                |state: &mut State,
                 action: &StackAction<(), ()>,
                 _: &mut ReduceContext<'_>| {
                    state.2 += 1;
                    let (StackAction::PopFrom { id } | StackAction::PopTo { id }) = action
                    else {
                        panic!("this fixture only sends pop actions");
                    };
                    state.3 = Arc::strong_count(&id.0);
                    Effect::none()
                },
            ),
            StatePath::new(|state: &State| &state.0, |state: &mut State| &mut state.1),
            ActionPath::new(|action: &StackAction<(), ()>| Some(action), |action| action),
            |(): &(), _: &mut ReduceContext<'_>| 0,
            crate::Reduce::new(
                |_: &mut u8, (): &(), _: &mut ReduceContext<'_>| Effect::none(),
            ),
        );
        let mut state = (observed, selected, 0, 0);
        let mut dependencies = DependencyValues::new();
        let (_scopes_lifecycle, mut scopes) = ScopeRegistry::for_test();
        let mut transitions = crate::store::scope::ScopeTransitionBatch::default();
        let mut context = ReduceContext::new(
            &mut dependencies,
            MonotonicInstant::ZERO,
            &mut scopes,
            &mut transitions,
        );
        let action = if exclusive {
            StackAction::PopTo { id }
        } else {
            StackAction::PopFrom { id }
        };

        let result = catch_unwind(AssertUnwindSafe(|| {
            reducer.reduce(&mut state, &action, &mut context)
        }));

        eprintln!(
            "pop target ID owners: actual={}, required={expected_id_owners}",
            state.3,
        );
        assert_eq!(state.3, expected_id_owners);
        assert!(result.is_ok(), "post-parent expiry must not panic");
        assert_eq!(state.2, 1, "the parent reduces before suffix removal");
        assert_eq!(state.0.values().copied().collect::<Vec<_>>(), [10, 11]);
        let after: Vec<_> = state
            .1
            .iter()
            .map(|(id, value)| (id.clone(), *value))
            .collect();
        if same_lifetime {
            assert_eq!(after, before[..usize::from(exclusive)]);
        } else {
            assert_eq!(after, before, "another insertion must retain its whole suffix");
        }
    }

    #[test]
    fn pop_from_preserves_an_independent_equal_id_insertion() {
        assert_pop_insertion_lifetime(false, PopSelection::Independent);
    }

    #[test]
    fn pop_to_preserves_an_independent_equal_id_insertion() {
        assert_pop_insertion_lifetime(true, PopSelection::Independent);
    }

    #[test]
    fn pop_from_preserves_a_reinserted_clone_branch() {
        assert_pop_insertion_lifetime(false, PopSelection::Reinserted);
    }

    #[test]
    fn pop_to_preserves_a_reinserted_clone_branch() {
        assert_pop_insertion_lifetime(true, PopSelection::Reinserted);
    }

    #[test]
    fn pop_from_accepts_a_clone_with_the_same_insertion() {
        assert_pop_insertion_lifetime(false, PopSelection::Cloned);
    }

    #[test]
    fn pop_to_accepts_a_clone_with_the_same_insertion() {
        assert_pop_insertion_lifetime(true, PopSelection::Cloned);
    }

    #[test]
    fn pop_from_accepts_an_absent_mutable_target_after_parent_reduction() {
        assert_pop_insertion_lifetime(false, PopSelection::Absent);
    }

    #[test]
    fn pop_to_accepts_an_absent_mutable_target_after_parent_reduction() {
        assert_pop_insertion_lifetime(true, PopSelection::Absent);
    }

    #[test]
    fn rejected_required_stack_diagnostics_preserve_typed_error_without_running_base() {
        for duplicate in [false, true] {
            for disabled in [false, true] {
                let parent = Rc::new(Cell::new(0));
                let mut reducer = reducer(Rc::clone(&parent));
                let mut state = StackState::new();
                let id = state.push(Rc::new(Cell::new(4)));
                let collector = IssueCollector::new();
                let closed = collector.reporter();
                assert!(collector.finish().is_empty());
                let reporter = if disabled {
                    IssueReporter::default()
                } else {
                    closed
                };
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
                let action = if duplicate {
                    StackAction::Push { id, input: () }
                } else {
                    StackAction::PopFrom { id: StackId::new() }
                };
                let failure = catch_unwind(AssertUnwindSafe(|| {
                    let _ = reducer.reduce(&mut state, &action, &mut context);
                }))
                .unwrap_err();
                let failure = failure
                    .downcast::<IssueReportError>()
                    .expect("original required delivery error");
                assert_eq!(failure.is_disabled(), disabled);
                assert_eq!(failure.is_closed(), !disabled);
                assert_eq!(
                    failure.into_issue().code,
                    if duplicate {
                        "rca.stack_duplicate_id"
                    } else {
                        "rca.stack_unknown_id"
                    }
                );
                assert_eq!(parent.get(), 0);
                assert_eq!(state.len(), 1);
            }
        }
    }

    #[test]
    fn stack_diagnostic_callback_panic_propagates_before_base() {
        let parent = Rc::new(Cell::new(0));
        let mut reducer = reducer(Rc::clone(&parent));
        let mut state = StackState::new();
        let mut dependencies = DependencyValues::new();
        let (_scopes_lifecycle, mut scopes) = ScopeRegistry::for_test();
        let mut context_transitions = crate::store::scope::ScopeTransitionBatch::default();
        let mut context = ReduceContext::new(
            &mut dependencies,
            MonotonicInstant::ZERO,
            &mut scopes,
            &mut context_transitions,
        )
        .with_issue_reporter(IssueReporter::new(|_| std::panic::panic_any(23_u8)));
        let failure = catch_unwind(AssertUnwindSafe(|| {
            let _ = reducer.reduce(
                &mut state,
                &StackAction::PopFrom { id: StackId::new() },
                &mut context,
            );
        }))
        .unwrap_err();
        assert_eq!(*failure.downcast::<u8>().unwrap(), 23);
        assert_eq!(parent.get(), 0);
    }
}
}

#[cfg(all(test, feature = "allocation-measurement"))]
pub(crate) use allocation_tests::pop_does_not_allocate;

#[cfg(all(test, feature = "allocation-measurement"))]
mod allocation_tests {
    use super::StackState;

    #[cfg(feature = "allocation-measurement")]
    pub(crate) fn pop_does_not_allocate() {
        if crate::allocation_test::run_in_child(
            "composition::stack::state_tests::pop_does_not_allocate",
        ) {
            return;
        }
        let mut stack = StackState::new();
        let first = stack.push(10);
        let last = stack.push(20);
        let profiler = dhat::Profiler::builder().testing().build();
        let before = dhat::HeapStats::get();
        let removed = std::hint::black_box(&mut stack).pop();
        let after = dhat::HeapStats::get();
        let blocks = after.total_blocks - before.total_blocks;
        let bytes = after.total_bytes - before.total_bytes;
        drop(profiler);
        eprintln!("single stack pop: {blocks} allocations, {bytes} bytes");
        assert_eq!(blocks, 0);
        assert_eq!(bytes, 0);
        assert_eq!(removed, Some((last, 20)));
        assert_eq!(stack.get(&first), Some(&10));
        assert_eq!(stack.len(), 1);
    }
}
