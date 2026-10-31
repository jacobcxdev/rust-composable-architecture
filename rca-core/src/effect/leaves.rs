//! Selects erased effect leaves without duplicating the recursive program.

use super::{Effect, Program};
use crate::store::scope::ScopeToken;

/// Supplies Shared or Local leaf storage for the same recursive effect program.
pub(crate) trait EffectLeaves<A>: Sized {
    /// Effect wrapper carrying both the program and its scope authority.
    type Effect: EffectValue<A, Self>;
    /// Erased asynchronous operation with this carrier's capture bounds.
    type Run;
    /// Erased fallible operation and its failure policy.
    type FallibleRun;
    /// Erased future that may produce one action.
    type Future;
    /// Reusable action factory for an interval.
    type Interval: FnMut() -> A;
}

/// Keeps the program and its scope together across carrier-independent operations.
pub(crate) trait EffectValue<A, C: EffectLeaves<A>>: Sized {
    /// Wraps a program without changing its supplied scope.
    fn from_parts(program: Program<A, C>, scope: Option<ScopeToken>) -> Self;

    /// Transfers the complete program and scope without cloning either.
    fn into_parts(self) -> (Program<A, C>, Option<ScopeToken>);

    /// Borrows both fields for in-place traversal and scope retirement.
    fn parts_mut(&mut self) -> (&mut Program<A, C>, &mut Option<ScopeToken>);

    /// Tests only whether the outer program is empty.
    fn is_none(&self) -> bool;

    /// Removes retired work while preserving the order and authority of live work.
    ///
    /// Returns whether retirement changed the program or its scope. Mutation is
    /// not rolled back if the predicate or a removed value's destructor panics.
    fn prune_scopes(&mut self, current: &impl Fn(ScopeToken) -> bool) -> bool {
        let (program, scope) = self.parts_mut();
        prune::<A, C>(program, scope, current)
    }
}

/// Collects non-empty effects in input order under an unscoped merge wrapper.
///
/// Even one child keeps the wrapper so its scope remains on the child. Empty
/// input allocates nothing. With non-empty input, the iterator is fully consumed
/// and dropped before the returned effect is built. If iterator cleanup panics,
/// the collected children are still locally owned and are dropped on unwinding.
pub(crate) fn merge<A, C: EffectLeaves<A>>(
    effects: impl IntoIterator<Item = C::Effect>,
) -> C::Effect {
    let mut effects = effects.into_iter();
    while let Some(effect) = effects.next() {
        if effect.is_none() {
            continue;
        }

        let capacity = super::initial_merge_capacity(effects.size_hint().0);
        let mut non_empty = Vec::with_capacity(capacity);
        non_empty.push(effect);
        // Keep iterator cleanup ahead of the retained children when unwinding.
        let mut tail = effects;
        while let Some(effect) = tail.next() {
            if effect.is_none() {
                continue;
            }

            if non_empty.len() == non_empty.capacity()
                && non_empty.capacity() >= super::MAX_MERGE_PREALLOCATION
            {
                // Align growth after the initial cap without reserving beyond
                // the remaining input. Loose hints cannot increase this bound.
                let growth = (non_empty.len() + 1).next_power_of_two() - non_empty.len();
                let remaining = tail.size_hint().1.unwrap_or(usize::MAX).saturating_add(1);
                non_empty.reserve_exact(growth.min(remaining));
            }
            non_empty.push(effect);
        }
        drop(tail);
        return C::Effect::from_parts(Program::Merge(non_empty), None);
    }
    C::Effect::from_parts(Program::None, None)
}

/// Selects sendable leaf storage without adding a runtime carrier value.
pub(crate) struct SharedEffects;

impl<A> EffectLeaves<A> for SharedEffects {
    type Effect = Effect<A>;
    type Run = super::Run<A>;
    type FallibleRun = super::fallible::FallibleRun<A>;
    type Future = std::pin::Pin<Box<dyn Future<Output = Option<A>> + Send>>;
    type Interval = Box<dyn FnMut() -> A + Send>;
}

impl<A> EffectValue<A, SharedEffects> for Effect<A> {
    fn from_parts(program: Program<A>, scope: Option<ScopeToken>) -> Self {
        Self { program, scope }
    }

    fn into_parts(self) -> (Program<A>, Option<ScopeToken>) {
        (self.program, self.scope)
    }

    fn parts_mut(&mut self) -> (&mut Program<A>, &mut Option<ScopeToken>) {
        (&mut self.program, &mut self.scope)
    }

    fn is_none(&self) -> bool {
        matches!(self.program, Program::None)
    }
}

/// Prunes recursive programs in place without flattening live child wrappers.
///
/// A wait left alone after retirement has no continuation, so it is discarded.
/// Other surviving singleton compositions keep their original representation.
fn prune<A, C: EffectLeaves<A>>(
    program: &mut Program<A, C>,
    scope: &mut Option<ScopeToken>,
    current: &impl Fn(ScopeToken) -> bool,
) -> bool {
    if scope.is_some_and(|scope| !current(scope)) {
        *program = Program::None;
        *scope = None;
        return true;
    }
    let (changed, empty) = match program {
        Program::Merge(effects) => {
            let mut changed = false;
            for effect in &mut *effects {
                changed |= effect.prune_scopes(current);
            }
            if changed {
                effects.retain(|effect| !effect.is_none());
            }
            (changed, effects.is_empty())
        }
        Program::Concatenate(effects) => {
            let mut changed = false;
            for effect in &mut *effects {
                changed |= effect.prune_scopes(current);
            }
            if changed {
                effects.retain(|effect| !effect.is_none());
                if effects.len() == 1
                    && effects
                        .front_mut()
                        .is_some_and(|effect| matches!(effect.parts_mut().0, Program::Wait(_)))
                {
                    effects.clear();
                }
            }
            (changed, effects.is_empty())
        }
        Program::Cancellable { effect, .. }
        | Program::Named { effect, .. }
        | Program::WithoutInheritedCancellation(effect) => {
            let changed = effect.prune_scopes(current);
            (changed, effect.is_none())
        }
        _ => (false, false),
    };
    if changed && empty {
        *program = Program::None;
        *scope = None;
    }
    changed
}
