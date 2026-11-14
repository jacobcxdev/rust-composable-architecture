//! Authors typed asynchronous work alongside synchronous reduction.
//!
//! [`Feature`] appends effects and typed tasks through [`FeatureContext`].
//! Every [`Reducer`] is a `Feature` without an adapter or additional allocation.
//! Tasks start after the originating transaction commits, not during reduction.
//!
//! Mount reactions receive owned [`ReactionChange`] values and a [`ReactionContext`].
//! The context permits dependency capture but does not expose mutable state.

use std::fmt;

use crate::store::task_context::Task;
use crate::{Effect, ReduceContext, Reducer};

/// Owns independently selected values for a finite reaction.
///
/// A selector must preserve the observed value independently of later state changes.
/// Cloning a shared mutable handle does not create an independent snapshot.
/// Changed values describe one transaction, not the interval between factory calls.
#[derive(Debug)]
pub enum ReactionChange<V> {
    /// An opted-in hook for a newly installed incarnation.
    Initial {
        /// The value selected from the newly installed incarnation.
        current: V,
    },
    /// Distinct values paired within one unchanged registration and configuration.
    Changed {
        /// The value selected before the transaction mutates state.
        previous: V,
        /// The value selected after mutation and mount reconciliation.
        current: V,
    },
}

/// Chooses initial execution for one transaction reaction registration.
///
/// [`Self::Skip`] is the default value. Each `on_change` registration requires
/// an explicit policy, independently of other registrations.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum InitialPolicy {
    /// React only to later unequal committed values.
    #[default]
    Skip,
    /// Also react once when this incarnation is installed.
    Run,
}

/// Captures dependencies without granting state mutation or command admission.
///
/// The factory is synchronous and non-preemptive after authorised entry. Its
/// dependency borrows remain valid until return, even if it drops the last Store
/// owner. Store closure prevents later callbacks and new reaction work.
/// It does not interrupt user code that already started.
pub struct ReactionContext<'deps, State, Action> {
    /// Keeps dependency borrows valid throughout the synchronous factory call.
    reducer: ReduceContext<'deps>,
    /// Names the feature domain without owning its values or inheriting their auto-traits.
    domain: std::marker::PhantomData<fn(State, Action)>,
}

impl<'deps, S, A> ReactionContext<'deps, S, A> {
    /// Restricts the reducer context to dependency capture during a reaction factory.
    pub(crate) fn new(reducer: ReduceContext<'deps>) -> Self {
        Self {
            reducer,
            domain: std::marker::PhantomData,
        }
    }

    /// Borrows an authorised dependency for explicit capture into owned work.
    ///
    /// Clone or otherwise own the needed value before returning a task. This
    /// borrow cannot escape the synchronous factory or retain live State.
    ///
    /// # Panics
    /// Propagates a selected dependency default's panic.
    pub fn dependency<K: crate::DependencyKey>(&mut self) -> &K::Value {
        self.reducer.dependency::<K>()
    }
}

impl<S, A> fmt::Debug for ReactionContext<'_, S, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReactionContext").finish_non_exhaustive()
    }
}

/// Reduces actions and declares typed asynchronous work.
///
/// Reduction borrows state exclusively and cannot suspend. [`FeatureContext`]
/// collects work for the Store to register after the synchronous transaction.
/// Every [`Reducer`] implements this trait automatically. Reducers return
/// action-based effects and support static composition with borrowed state paths.
/// Implement `Feature` directly when the callback also declares typed tasks.
/// Both forms use the same Store and `TestStore` constructors.
///
/// A type defines its behaviour through either `Reducer` or `Feature`, not both.
/// Use separate wrapper types when the same data needs different behaviours.
///
/// # Choosing an authoring form
///
/// Start with [`Reducer`] for action-based inputs and results. An effect can
/// await a client and return a result action, so asynchronous work alone does
/// not require a direct `Feature` implementation.
///
/// Implement `Feature` directly when a [`Task`] must read current state or
/// submit an owned update between awaits. For example, a worker can inspect
/// current retry policy without an action for every internal step. Task access
/// still passes through the actor and can fail after cancellation or removal.
/// If a decision must use current state at commit, make that decision inside
/// the update closure. A separate earlier read can become stale.
///
/// A direct Feature can append both effects and tasks. It does not implement
/// `Reducer` and cannot enter reducer-only combinators such as [`crate::scope`].
/// Use installed children to compose direct Features with other Features or
/// reducers. [`crate::RootMount`] and [`crate::OptionalMount`] document that route.
///
/// Both forms remain exhaustively testable. Effect feedback is an action
/// transaction; a typed task update is an update transaction. `TestStore`
/// asserts each through its corresponding receive method.
pub trait Feature {
    /// State exclusively borrowed for this turn.
    type State;

    /// Action borrowed for this turn.
    type Action;

    /// Mutates state and appends follow-up work synchronously.
    ///
    /// Keep blocking I/O and long-running computation out of this callback.
    fn update(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        context: &mut FeatureContext<'_, Self::State, Self::Action>,
    );

    /// Returns the same state changes and work for root Store dispatch.
    ///
    /// Root Stores use this hook. Mounted features use [`Self::update`].
    /// Feature authors implement `update`. RCA uses this hook to adapt reducers.
    ///
    /// Overrides must preserve the state changes, work order and panic behaviour
    /// of `update`. Reducers return their effect without a temporary feature context.
    ///
    /// # Panics
    ///
    /// Propagates an update or reducer panic.
    #[doc(hidden)]
    fn reduce_for_store<'deps>(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        context: ReduceContext<'deps>,
    ) -> StoreReduction<'deps, Self::State, Self::Action> {
        let mut context = FeatureContext::new(context);
        self.update(state, action, &mut context);
        std::ops::ControlFlow::Continue(context)
    }
}

impl<R: Reducer + ?Sized> Feature for R {
    type State = R::State;
    type Action = R::Action;

    fn update(
        &mut self,
        state: &mut R::State,
        action: &R::Action,
        context: &mut FeatureContext<'_, R::State, R::Action>,
    ) {
        let effect = Reducer::reduce(self, state, action, context.reducer_context());
        context.effect(effect);
    }

    fn reduce_for_store<'deps>(
        &mut self,
        state: &mut R::State,
        action: &R::Action,
        mut context: ReduceContext<'deps>,
    ) -> StoreReduction<'deps, R::State, R::Action> {
        std::ops::ControlFlow::Break(Reducer::reduce(self, state, action, &mut context))
    }
}

/// Distinguishes an effect-only callback from an owned feature work collection.
type StoreReduction<'deps, S, A> = std::ops::ControlFlow<Effect<A>, FeatureContext<'deps, S, A>>;

/// Owns a turn's dependency borrow and ordered follow-up work.
///
/// The Store registers appended work in declaration order after synchronous reduction.
/// This order does not imply sequential completion of asynchronous work.
/// If reduction unwinds, the context drops its unregistered work and task captures.
pub struct FeatureContext<'deps, State, Action> {
    /// Owns synchronous dependency and scope authority until deferred work is extracted.
    reducer: ReduceContext<'deps>,
    /// Retains declarations and their captures until registration or context destruction.
    work: Work<Effect<Action>, Task<State, Action>>,
}

impl<'deps, S, A> FeatureContext<'deps, S, A> {
    /// Starts an empty work collection under this turn's synchronous authority.
    pub(crate) fn new(reducer: ReduceContext<'deps>) -> Self {
        Self {
            reducer,
            work: Work::default(),
        }
    }

    /// Transfers declarations while releasing the synchronous dependency and scope borrows.
    pub(crate) fn into_work(self) -> Work<Effect<A>, Task<S, A>> {
        self.work
    }

    /// Borrows the ordinary synchronous reducer context.
    pub fn reducer_context(&mut self) -> &mut ReduceContext<'deps> {
        &mut self.reducer
    }

    /// Closes or reopens ordinary input to an installed direct child.
    ///
    /// Tasks, observations, outputs and fold contributions retain their lifetime.
    ///
    /// # Errors
    /// Rejects foreign, stale or non-child gates without selecting child State.
    pub fn set_input_open(
        &mut self,
        gate: &crate::InputGate,
        open: bool,
    ) -> Result<(), crate::TaskAccessError> {
        self.reducer.set_input_open(gate, open)
    }

    /// Appends an unchanged effect, discarding empty work.
    ///
    /// Effects and typed tasks share declaration order. Appending an effect does
    /// not execute its asynchronous work during reduction.
    pub fn effect(&mut self, effect: Effect<A>) {
        if !matches!(effect.program, crate::effect::Program::None) {
            self.work.push(WorkItem::Effect(effect));
        }
    }

    /// Appends one deferred typed task.
    ///
    /// The first poll after the transaction commits invokes the task factory.
    /// Dropping unregistered work releases its captures without invoking the factory.
    pub fn task(&mut self, task: Task<S, A>) {
        self.work.push(WorkItem::Task(task));
    }
}

impl<S, A> fmt::Debug for FeatureContext<'_, S, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FeatureContext").finish_non_exhaustive()
    }
}

/// Preserves declaration order across effect programs and typed task factories.
pub(crate) enum WorkItem<A, T> {
    /// An effect program for the carrier to compile after synchronous reduction.
    Effect(A),
    /// A typed task whose factory remains deferred until the registered task is polled.
    Task(T),
}

/// Owns a turn's declarations without allocating for an empty or singleton turn.
///
/// The first item stays inline after later items enter the vector. A non-empty
/// vector therefore always has an inline predecessor. Normal destruction drops
/// the inline item before the vector releases the remaining captures.
pub(crate) struct Work<A, T> {
    /// Owns the first declaration. Absence implies that `rest` is also empty.
    first: Option<WorkItem<A, T>>,
    /// Owns subsequent declarations in insertion order, never the inline item.
    rest: Vec<WorkItem<A, T>>,
}

impl<A, T> Default for Work<A, T> {
    fn default() -> Self {
        Self {
            first: None,
            rest: Vec::new(),
        }
    }
}

impl<A, T> Work<A, T> {
    /// Reports whether the turn has no retained work.
    ///
    /// The inline slot determines emptiness because every later item requires it.
    pub(crate) fn is_empty(&self) -> bool {
        self.first.is_none()
    }

    /// Visits effect declarations without visiting tasks or changing their positions.
    ///
    /// The transaction kernel uses this to prune stale effect scopes before registration.
    ///
    /// # Panics
    /// Propagates visitor panics. Earlier visitor mutations are not rolled back.
    pub(crate) fn for_each_effect(&mut self, mut visit: impl FnMut(&mut A)) {
        for item in self.first.iter_mut().chain(self.rest.iter_mut()) {
            if let WorkItem::Effect(effect) = item {
                visit(effect);
            }
        }
    }

    /// Appends an owned declaration without starting its work.
    ///
    /// The first declaration stays inline. Only later declarations enter the vector.
    pub(crate) fn push(&mut self, item: WorkItem<A, T>) {
        if self.first.is_none() {
            self.first = Some(item);
        } else {
            self.rest.push(item);
        }
    }
}

impl<A, T> IntoIterator for Work<A, T> {
    type Item = WorkItem<A, T>;
    type IntoIter =
        std::iter::Chain<std::option::IntoIter<Self::Item>, std::vec::IntoIter<Self::Item>>;

    fn into_iter(self) -> Self::IntoIter {
        self.first.into_iter().chain(self.rest)
    }
}

/// Keeps effect-only root returns independent of typed task construction.
pub(crate) enum RootWork<E, T, F> {
    /// No declarations or registration side effects.
    Empty,
    /// One effect retains the reducer's inline return path.
    Effect(E),
    /// Preserves the original inline item and spill buffer for ordered work.
    Full {
        /// Owns all declarations without moving the inline item into the buffer.
        work: Work<E, T>,
        /// Selects typed-task registration within this batch's declaration order.
        registrar: F,
    },
}

impl<E, T, F: Copy> RootWork<E, T, F> {
    /// Classifies a concrete root producer without visiting or invoking its work.
    #[inline]
    pub(crate) fn new(work: Work<E, T>, registrar: F) -> Self {
        if work.rest.is_empty() {
            match work.first {
                None => Self::Empty,
                Some(WorkItem::Effect(effect)) => Self::Effect(effect),
                Some(WorkItem::Task(task)) => Self::Full {
                    work: Work {
                        first: Some(WorkItem::Task(task)),
                        rest: work.rest,
                    },
                    registrar,
                },
            }
        } else {
            Self::Full { work, registrar }
        }
    }

    /// Reports whether registration has any returned work to visit.
    pub(crate) fn is_empty(&self) -> bool {
        matches!(self, Self::Empty)
    }

    /// Prunes effect scopes without changing task positions or invoking factories.
    pub(crate) fn for_each_effect(&mut self, mut visit: impl FnMut(&mut E)) {
        match self {
            Self::Empty => {}
            Self::Effect(effect) => visit(effect),
            Self::Full { work, .. } => work.for_each_effect(visit),
        }
    }
}

/// Unifies Shared and Local reduction without erasing their effect and task types.
pub(crate) trait ReduceDriver<S, A> {
    /// The carrier-specific effect program to compile after reduction.
    type Effect;
    /// The carrier-specific task that owns deferred factory captures.
    type Task;
    /// One typed-task interpreter selected while the concrete root producer is known.
    type RootRegistrar: Copy;

    /// Runs one synchronous reduction and returns its ordered, owned declarations.
    ///
    /// Implementations release the context and unregistered work if reduction unwinds.
    fn reduce(
        &mut self,
        state: &mut S,
        action: &A,
        context: ReduceContext<'_>,
    ) -> RootWork<Self::Effect, Self::Task, Self::RootRegistrar>;
}

impl<S: Send + 'static, A: Send + 'static, F: Feature<State = S, Action = A>> ReduceDriver<S, A>
    for F
{
    type Effect = Effect<A>;
    type Task = Task<S, A>;
    type RootRegistrar =
        crate::store::carriers::RootRegistrar<S, A, crate::store::carriers::Shared>;

    fn reduce(
        &mut self,
        state: &mut S,
        action: &A,
        context: ReduceContext<'_>,
    ) -> RootWork<Self::Effect, Self::Task, Self::RootRegistrar> {
        match Feature::reduce_for_store(self, state, action, context) {
            std::ops::ControlFlow::Break(effect) => {
                if matches!(effect.program, crate::effect::Program::None) {
                    RootWork::Empty
                } else {
                    RootWork::Effect(effect)
                }
            }
            std::ops::ControlFlow::Continue(context) => RootWork::new(
                context.into_work(),
                <crate::store::carriers::Shared as crate::store::carriers::Execution<S, A>>::ROOT_REGISTRAR,
            ),
        }
    }
}

#[cfg(test)]
ordinary_test_items! {
mod tests {
    use super::*;
    use crate::store::scope::{ScopeRegistry, ScopeTransitionBatch};
    use crate::{DependencyValues, Effect, MonotonicInstant, Reduce, ReduceContext};

    #[test]
    fn tagged_update_preserves_direct_feature_order_and_unwind() {
        use std::panic::{AssertUnwindSafe, catch_unwind};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct Capture(Arc<AtomicUsize>);

        impl Drop for Capture {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        struct Direct {
            drops: Arc<AtomicUsize>,
            panic: bool,
        }

        impl Feature for Direct {
            type State = usize;
            type Action = usize;

            fn update(
                &mut self,
                state: &mut usize,
                action: &usize,
                context: &mut FeatureContext<'_, usize, usize>,
            ) {
                *state += action;
                context.effect(Effect::send(1));
                let capture = Capture(Arc::clone(&self.drops));
                context.task(crate::Task::run(move |_| async move {
                    drop(capture);
                }));
                context.effect(Effect::send(2));
                assert!(!self.panic, "direct feature panic");
            }
        }

        for owned in [false, true] {
            for panic in [false, true] {
                let drops = Arc::new(AtomicUsize::new(0));
                let mut feature = Direct {
                    drops: Arc::clone(&drops),
                    panic,
                };
                let mut dependencies = DependencyValues::new();
                let (_lifecycle, mut scopes) = ScopeRegistry::for_test();
                let mut transitions = ScopeTransitionBatch::default();
                let context = ReduceContext::new(
                    &mut dependencies,
                    MonotonicInstant::ZERO,
                    &mut scopes,
                    &mut transitions,
                );
                let mut state = 3;
                let result = catch_unwind(AssertUnwindSafe(|| {
                    let feature: &mut dyn Feature<State = usize, Action = usize> = &mut feature;
                    if owned {
                        match Feature::reduce_for_store(feature, &mut state, &4, context) {
                            std::ops::ControlFlow::Continue(context) => context.into_work(),
                            std::ops::ControlFlow::Break(_) => {
                                panic!("direct feature must retain its work");
                            }
                        }
                    } else {
                        let mut context = FeatureContext::new(context);
                        Feature::update(feature, &mut state, &4, &mut context);
                        context.into_work()
                    }
                }));
                assert_eq!(state, 7);
                assert!(transitions.is_empty());
                if panic {
                    let payload = result.err().expect("direct feature must panic");
                    assert_eq!(payload.downcast_ref::<&str>(), Some(&"direct feature panic"));
                } else {
                    let mut items = result.unwrap().into_iter();
                    assert!(matches!(items.next(), Some(WorkItem::Effect(effect))
                        if matches!(effect.program, crate::effect::Program::Send(1))));
                    assert!(matches!(items.next(), Some(WorkItem::Task(_))));
                    assert!(matches!(items.next(), Some(WorkItem::Effect(effect))
                        if matches!(effect.program, crate::effect::Program::Send(2))));
                    assert!(items.next().is_none());
                }
                assert_eq!(drops.load(Ordering::SeqCst), 1);
            }
        }
    }

    #[test]
    fn tagged_update_matches_reducer_effect_none_and_panic() {
        use std::panic::{AssertUnwindSafe, catch_unwind};

        for owned in [false, true] {
            for mode in 0..3 {
                let reports = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let count = std::sync::Arc::clone(&reports);
                let reporter = crate::IssueReporter::new(move |_| {
                    count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                });
                let mut feature = Reduce::new(
                    move |state: &mut usize, action: &usize, context: &mut ReduceContext<'_>| {
                        *state += action;
                        context
                            .issue_reporter()
                            .report(crate::Issue::new("tag", "observed"))
                            .unwrap();
                        if mode == 2 {
                            panic!("reducer panic");
                        }
                        if mode == 0 {
                            Effect::none()
                        } else {
                            Effect::send(9)
                        }
                    },
                );
                let mut dependencies = DependencyValues::new();
                let (_lifecycle, mut scopes) = ScopeRegistry::for_test();
                let mut transitions = ScopeTransitionBatch::default();
                let context = ReduceContext::new(
                    &mut dependencies,
                    MonotonicInstant::ZERO,
                    &mut scopes,
                    &mut transitions,
                )
                .with_issue_reporter(reporter);
                let mut state = 2;
                let result = catch_unwind(AssertUnwindSafe(|| {
                    if owned {
                        match Feature::reduce_for_store(&mut feature, &mut state, &3, context) {
                            std::ops::ControlFlow::Break(effect) => {
                                let mut work = crate::feature::Work::default();
                                if !matches!(effect.program, crate::effect::Program::None) {
                                    work.push(WorkItem::Effect(effect));
                                }
                                work
                            }
                            std::ops::ControlFlow::Continue(_) => {
                                panic!("reducer must return its effect");
                            }
                        }
                    } else {
                        let mut context = FeatureContext::new(context);
                        Feature::update(&mut feature, &mut state, &3, &mut context);
                        context.into_work()
                    }
                }));
                assert_eq!(state, 5);
                assert_eq!(reports.load(std::sync::atomic::Ordering::SeqCst), 1);
                assert_eq!(std::sync::Arc::strong_count(&reports), 1);
                assert!(transitions.is_empty());
                if mode == 2 {
                    let payload = result.err().expect("reducer must panic");
                    assert_eq!(payload.downcast_ref::<&str>(), Some(&"reducer panic"));
                } else {
                    let mut items = result.unwrap().into_iter();
                    if mode == 1 {
                        assert!(matches!(items.next(), Some(WorkItem::Effect(effect))
                            if matches!(effect.program, crate::effect::Program::Send(9))));
                    }
                    assert!(items.next().is_none());
                }
            }
        }
    }

    #[test]
    fn root_work_preserves_spill_storage_and_ordered_panic_cleanup() {
        use std::cell::RefCell;
        use std::panic::{AssertUnwindSafe, catch_unwind};
        use std::rc::Rc;

        struct Captured(usize, Rc<RefCell<Vec<usize>>>);
        impl Drop for Captured {
            fn drop(&mut self) {
                self.1.borrow_mut().push(self.0);
            }
        }

        fn deferred_constructor() {
            panic!("classifying work must not construct a context");
        }

        let drops = Rc::new(RefCell::new(Vec::new()));
        let mut work = Work::default();
        work.push(WorkItem::Effect(Captured(0, Rc::clone(&drops))));
        work.push(WorkItem::Task(Captured(1, Rc::clone(&drops))));
        work.push(WorkItem::Effect(Captured(2, Rc::clone(&drops))));
        work.push(WorkItem::Task(Captured(3, Rc::clone(&drops))));
        let buffer = work.rest.as_ptr();
        let capacity = work.rest.capacity();

        let mut root = super::RootWork::new(work, deferred_constructor as fn());
        let super::RootWork::Full { work, .. } = &root else {
            panic!("multiple declarations retain their original storage");
        };
        assert_eq!(work.rest.as_ptr(), buffer);
        assert_eq!(work.rest.capacity(), capacity);
        assert_eq!(work.rest.len(), 3);
        assert!(matches!(&work.first, Some(WorkItem::Effect(Captured(0, _)))));
        assert!(drops.borrow().is_empty());

        let panic = catch_unwind(AssertUnwindSafe(move || {
            root.for_each_effect(|capture| {
                assert_ne!(capture.0, 2, "second effect visit panics");
            });
        }));
        assert!(panic.is_err());
        assert_eq!(*drops.borrow(), [0, 1, 2, 3]);
    }

    // Catches an adapter borrowing the dependency context for the whole turn,
    // losing the returned effect, or adding work for Effect::none().
    #[test]
    fn owning_context_releases_real_reducer_borrows_and_preserves_effect() {
        let mut dependencies = DependencyValues::new();
        let (_scopes_lifecycle, mut scopes) = ScopeRegistry::for_test();
        let mut transitions = ScopeTransitionBatch::default();
        let context = ReduceContext::new(
            &mut dependencies, MonotonicInstant::ZERO, &mut scopes, &mut transitions,
        );
        let mut context = FeatureContext::new(context);
        let mut feature = Reduce::new(
            |state: &mut usize, action: &usize, _| {
                *state += action;
                Effect::send(7)
            },
        );
        let mut state = 2;
        Feature::update(&mut feature, &mut state, &3, &mut context);
        context.effect(Effect::none());
        let work = context.into_work();
        assert_eq!(state, 5);
        assert!(transitions.is_empty());
        assert!(scopes.is_current(scopes.root()));
        let mut items = work.into_iter();
        assert!(matches!(items.next(), Some(WorkItem::Effect(effect)) if matches!(effect.program, crate::effect::Program::Send(7))));
        assert!(items.next().is_none());
    }

    // Catches overwrite of the first task or eager invocation of user factories.
    #[test]
    fn work_appends_tasks_and_effects_in_declaration_order() {
        let mut dependencies = DependencyValues::new();
        let (_scopes_lifecycle, mut scopes) = ScopeRegistry::for_test();
        let mut transitions = ScopeTransitionBatch::default();
        let mut context = FeatureContext::<usize, usize>::new(ReduceContext::new(
            &mut dependencies, MonotonicInstant::ZERO, &mut scopes, &mut transitions,
        ));
        context.task(Task::run(|_| { panic!("factory must remain deferred"); #[allow(unreachable_code, reason = "panic sentinel must still type-check as a deferred future factory")] async {} }));
        context.effect(Effect::send(4));
        context.task(Task::run(|_| { panic!("second factory must remain deferred"); #[allow(unreachable_code, reason = "panic sentinel must still type-check as a deferred future factory")] async {} }));
        let mut items = context.into_work().into_iter();
        assert!(matches!(items.next(), Some(WorkItem::Task(_))));
        assert!(matches!(items.next(), Some(WorkItem::Effect(effect)) if matches!(effect.program, crate::effect::Program::Send(4))));
        assert!(matches!(items.next(), Some(WorkItem::Task(_))));
        assert!(items.next().is_none());
    }

    #[test]
    fn work_keeps_one_item_inline_and_visits_only_effects() {
        let mut work = Work::<usize, usize>::default();
        assert!(work.is_empty());
        assert_eq!(work.rest.capacity(), 0);

        work.push(WorkItem::Effect(0));
        assert!(!work.is_empty());
        assert_eq!(work.rest.capacity(), 0);
        work.push(WorkItem::Task(1));
        work.push(WorkItem::Effect(2));
        work.push(WorkItem::Task(3));
        work.push(WorkItem::Effect(4));

        // This storage contract avoids moving the first item into the heap on spill.
        assert!(matches!(work.first, Some(WorkItem::Effect(0))));
        assert_eq!(work.rest.len(), 4);
        work.for_each_effect(|value| *value += 10);
        let actual: Vec<_> = work.into_iter().map(|item| match item {
            WorkItem::Effect(value) => ("effect", value),
            WorkItem::Task(value) => ("task", value),
        }).collect();
        assert_eq!(actual, [("effect", 10), ("task", 1), ("effect", 12), ("task", 3), ("effect", 14)]);
    }

    #[test]
    fn work_releases_inline_and_spilled_items_when_a_visitor_panics() {
        use std::cell::RefCell;
        use std::panic::{AssertUnwindSafe, catch_unwind};
        use std::rc::Rc;

        struct Captured(usize, Rc<RefCell<Vec<usize>>>);
        impl Drop for Captured {
            fn drop(&mut self) {
                self.1.borrow_mut().push(self.0);
            }
        }

        let drops = Rc::new(RefCell::new(Vec::new()));
        let mut work = Work::default();
        work.push(WorkItem::Effect(Captured(0, Rc::clone(&drops))));
        work.push(WorkItem::Task(Captured(1, Rc::clone(&drops))));
        work.push(WorkItem::Effect(Captured(2, Rc::clone(&drops))));
        work.push(WorkItem::Task(Captured(3, Rc::clone(&drops))));

        let panic = catch_unwind(AssertUnwindSafe(move || {
            work.for_each_effect(|capture| {
                assert_ne!(capture.0, 2, "second effect visit panics");
            });
        }));
        assert!(panic.is_err());
        assert_eq!(*drops.borrow(), [0, 1, 2, 3]);
    }
}
}
