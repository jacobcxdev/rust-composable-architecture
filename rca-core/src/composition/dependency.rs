//! Adds immutable lexical dependency overlays to reducers.

use std::fmt;
use std::marker::PhantomData;

use crate::dependency::SharedDependency;
use crate::{DependencyKey, Effect, ReduceContext, Reducer};

/// Reduces one feature beneath a lexical dependency override.
///
/// Construction owns the erased value. Reduction borrows that value and never formats
/// it through this type's [`Debug`](fmt::Debug) implementation.
pub struct DependencyReducer<Key, R> {
    /// Owns the immutable value that each reduction borrows through its context.
    dependency: SharedDependency,
    /// Receives the original state and action beneath this dependency layer.
    reducer: R,
    /// Records the dependency key type without owning a key value.
    key: PhantomData<fn() -> Key>,
}

impl<Key, R> DependencyReducer<Key, R>
where
    Key: DependencyKey,
{
    /// Creates a dependency-scoped reducer.
    #[must_use]
    pub fn new(value: Key::Value, reducer: R) -> Self {
        Self {
            dependency: SharedDependency::new::<Key>(value),
            reducer,
            key: PhantomData,
        }
    }
}

impl<Key, R> Reducer for DependencyReducer<Key, R>
where
    Key: DependencyKey,
    R: Reducer,
{
    type State = R::State;
    type Action = R::Action;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        let reducer = &mut self.reducer;
        context.with_feature_dependency(&self.dependency, |child| {
            reducer.reduce(state, action, child)
        })
    }
}

impl<Key, R> fmt::Debug for DependencyReducer<Key, R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DependencyReducer")
            .field("key", &std::any::type_name::<Key>())
            .field("reducer", &std::any::type_name::<R>())
            .finish_non_exhaustive()
    }
}

/// Wraps one reducer in a lexical dependency override.
///
/// Inner feature overlays take precedence over outer overlays. Explicit reduction overrides and
/// deterministic test overrides take precedence over this layer.
#[must_use]
pub fn with_dependency<Key, R>(value: Key::Value, reducer: R) -> DependencyReducer<Key, R>
where
    Key: DependencyKey,
    R: Reducer,
{
    DependencyReducer::new(value, reducer)
}

#[cfg(all(test, feature = "allocation-measurement"))]
mod allocation_tests {
    use std::hint::black_box;

    use super::{DependencyKey, Effect, ReduceContext, Reducer, with_dependency};
    use crate::store::scope::ScopeRegistry;
    use crate::{DependencyValues, MonotonicInstant};

    struct ValueKey;

    impl DependencyKey for ValueKey {
        type Value = usize;

        fn live_value() -> Self::Value {
            1
        }
    }

    struct UnitKey;

    impl DependencyKey for UnitKey {
        type Value = ();
    }

    fn feature_override_owns_only_the_value_storage() {
        let before = dhat::HeapStats::get();
        let scalar = black_box(with_dependency::<ValueKey, _>(black_box(2), Probe));
        let scalar_owned = dhat::HeapStats::get();
        let unit = black_box(with_dependency::<UnitKey, _>((), Probe));
        let both_owned = dhat::HeapStats::get();
        drop(black_box((scalar, unit)));
        let released = dhat::HeapStats::get();

        eprintln!(
            "feature override construction: scalar {} blocks/{} bytes; unit {} blocks/{} bytes",
            scalar_owned.total_blocks - before.total_blocks,
            scalar_owned.total_bytes - before.total_bytes,
            both_owned.total_blocks - scalar_owned.total_blocks,
            both_owned.total_bytes - scalar_owned.total_bytes,
        );
        assert_eq!(scalar_owned.total_blocks - before.total_blocks, 1);
        assert_eq!(
            scalar_owned.total_bytes - before.total_bytes,
            size_of::<usize>() as u64,
        );
        assert_eq!(both_owned.total_blocks, scalar_owned.total_blocks);
        assert_eq!(both_owned.total_bytes, scalar_owned.total_bytes);
        assert_eq!(released.curr_blocks, before.curr_blocks);
        assert_eq!(released.curr_bytes, before.curr_bytes);
    }

    struct Probe;

    impl Reducer for Probe {
        type State = usize;
        type Action = bool;

        fn reduce(
            &mut self,
            state: &mut Self::State,
            action: &Self::Action,
            context: &mut ReduceContext<'_>,
        ) -> Effect<Self::Action> {
            if *action {
                *state += context.with_dependency_override::<ValueKey, _>(3, |context| {
                    *context.dependency::<ValueKey>()
                });
            } else {
                *state += *context.dependency::<ValueKey>();
            }
            Effect::none()
        }
    }

    pub(crate) fn dependency_overlays_do_not_allocate_during_reduction() {
        if crate::allocation_test::run_in_child(
            "composition::dependency::allocation_tests::dependency_overlays_do_not_allocate_during_reduction",
        ) {
            return;
        }
        let mut reducer = with_dependency::<ValueKey, _>(2, Probe);
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

        drop(black_box(reducer.reduce(&mut state, &false, &mut context)));
        drop(black_box(reducer.reduce(&mut state, &true, &mut context)));

        let profiler = dhat::Profiler::builder().testing().build();
        let before = dhat::HeapStats::get();
        drop(black_box(reducer.reduce(&mut state, &false, &mut context)));
        drop(black_box(reducer.reduce(&mut state, &true, &mut context)));
        let after = dhat::HeapStats::get();

        assert_eq!(after.total_blocks, before.total_blocks);
        assert_eq!(after.total_bytes, before.total_bytes);
        feature_override_owns_only_the_value_storage();
        drop(profiler);
    }
}

#[cfg(all(test, feature = "allocation-measurement"))]
pub(crate) use allocation_tests::dependency_overlays_do_not_allocate_during_reduction;
