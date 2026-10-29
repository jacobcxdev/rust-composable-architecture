//! Maps effect output actions without changing effect scheduling semantics.

use std::sync::Arc;

use super::leaves::{EffectLeaves, EffectValue, SharedEffects};
use super::{Effect, EffectSender, Program, Run};

/// Converts owned actions without requiring either action type to be `Send`.
type ActionMap<Source, Mapped> = dyn Fn(Source) -> Mapped + Send + Sync + 'static;

/// Maps shared output with allocation-free paths for direct eager and action-free variants.
pub(crate) fn map_effect<Source, Mapped, Map>(effect: Effect<Source>, map: Map) -> Effect<Mapped>
where
    Source: 'static,
    Mapped: 'static,
    Map: Fn(Source) -> Mapped + Send + Sync + 'static,
{
    map_with::<Source, Mapped, SharedEffects, SharedEffects, _, _>(effect, map, |map| {
        Arc::new(map) as Arc<ActionMap<Source, Mapped>>
    })
}

/// Maps direct eager variants without shared callback storage.
///
/// Nested and deferred programs use the selected shared callback storage.
/// `share` selects shared or owner-local callback storage. Empty work loses its
/// unused scope, just as a fresh empty effect does. Nonempty work retains scope.
pub(crate) fn map_with<Source, Mapped, C, D, Map, M>(
    effect: C::Effect,
    map: Map,
    share: impl FnOnce(Map) -> M,
) -> D::Effect
where
    Source: 'static,
    Mapped: 'static,
    C: EffectLeaves<Source>,
    D: EffectLeaves<Mapped>,
    Map: Fn(Source) -> Mapped,
    M: MapLeaves<Source, Mapped, C, D>,
{
    let (program, scope) = effect.into_parts();
    // Eager/action-free variants do not retain the mapper. Share it only when
    // deferred work or multiple children need owned access.
    let program = match program {
        Program::None => return D::Effect::from_parts(Program::None, None),
        Program::Dismiss(target) => Program::Dismiss(target),
        Program::Send(action) => Program::Send(map(action)),
        Program::Cancel(identity) => Program::Cancel(identity),
        Program::Wait(duration) => Program::Wait(duration),
        Program::SendAfter { duration, action } => Program::SendAfter {
            duration,
            action: map(action),
        },
        Program::SendAt { deadline, action } => Program::SendAt {
            deadline,
            action: map(action),
        },
        program => {
            return map_program::<Source, Mapped, C, D, M>(
                C::Effect::from_parts(program, scope),
                share(map),
            );
        }
    };
    D::Effect::from_parts(program, scope)
}

/// Adapts leaf ownership while one traversal preserves the effect structure.
///
/// Shared and owner-local callbacks have different thread bounds. Each adapter
/// supplies its family's erased leaves without strengthening action bounds.
pub(crate) trait MapLeaves<S, A, C: EffectLeaves<S>, D: EffectLeaves<A>>: Clone {
    /// Maps an owned action before any deferred work starts.
    fn action(&self, action: S) -> A;

    /// Gives deferred work a sender that maps each emitted action.
    fn run(self, run: C::Run) -> D::Run;

    /// Maps output while retaining the operation's provenance and cancellation.
    fn fallible(self, run: C::FallibleRun) -> D::FallibleRun;

    /// Maps optional output after its future completes.
    fn future(self, future: C::Future) -> D::Future;

    /// Maps each generated tick without changing the timer's cadence.
    fn interval(self, interval: C::Interval) -> D::Interval;
}

impl<S: 'static, A: 'static> MapLeaves<S, A, SharedEffects, SharedEffects>
    for Arc<ActionMap<S, A>>
{
    fn action(&self, action: S) -> A {
        self(action)
    }

    fn run(self, run: Run<S>) -> Run<A> {
        Box::new(move |sender| run(contramap(sender, self)))
    }

    fn fallible(
        self,
        run: <SharedEffects as EffectLeaves<S>>::FallibleRun,
    ) -> <SharedEffects as EffectLeaves<A>>::FallibleRun {
        Box::new(move |sender, provenance, cancellation| {
            run(contramap(sender, self), provenance, cancellation)
        })
    }

    fn future(
        self,
        future: <SharedEffects as EffectLeaves<S>>::Future,
    ) -> <SharedEffects as EffectLeaves<A>>::Future {
        Box::pin(async move { future.await.map(|action| self(action)) })
    }

    fn interval(
        self,
        mut interval: <SharedEffects as EffectLeaves<S>>::Interval,
    ) -> <SharedEffects as EffectLeaves<A>>::Interval {
        Box::new(move || self(interval()))
    }
}

/// Maps leaves while retaining scopes, order, timing and cancellation declarations.
///
/// Immediate actions map during this traversal. Deferred leaves own their mapper.
/// An immediate mapper panic propagates and drops the remaining owned work.
pub(crate) fn map_program<Source, Mapped, C, D, M>(effect: C::Effect, map: M) -> D::Effect
where
    Source: 'static,
    Mapped: 'static,
    C: EffectLeaves<Source>,
    D: EffectLeaves<Mapped>,
    M: MapLeaves<Source, Mapped, C, D>,
{
    let (program, scope) = effect.into_parts();
    let program = match program {
        Program::None => Program::None,
        Program::Dismiss(target) => Program::Dismiss(target),
        Program::Send(action) => Program::Send(map.action(action)),
        Program::Run(operation) => Program::Run(map.run(operation)),
        Program::FallibleRun(operation) => Program::FallibleRun(map.fallible(operation)),
        Program::Future(future) => Program::Future(map.future(future)),
        Program::Merge(effects) => Program::Merge(
            effects
                .into_iter()
                .map(|effect| map_program::<Source, Mapped, C, D, M>(effect, map.clone()))
                .collect(),
        ),
        Program::Concatenate(effects) => Program::Concatenate(
            effects
                .into_iter()
                .map(|effect| map_program::<Source, Mapped, C, D, M>(effect, map.clone()))
                .collect(),
        ),
        Program::Cancel(identity) => Program::Cancel(identity),
        Program::Wait(duration) => Program::Wait(duration),
        Program::WithoutInheritedCancellation(effect) => Program::WithoutInheritedCancellation(
            Box::new(map_program::<Source, Mapped, C, D, M>(*effect, map)),
        ),
        Program::SendAfter { duration, action } => Program::SendAfter {
            duration,
            action: map.action(action),
        },
        Program::SendAt { deadline, action } => Program::SendAt {
            deadline,
            action: map.action(action),
        },
        Program::Interval {
            period,
            make_action,
        } => Program::Interval {
            period,
            make_action: map.interval(make_action),
        },
        Program::Cancellable {
            effect,
            identity,
            cancel_in_flight,
        } => Program::Cancellable {
            effect: Box::new(map_program::<Source, Mapped, C, D, M>(*effect, map)),
            identity,
            cancel_in_flight,
        },
        Program::Named {
            effect,
            declaration,
        } => Program::Named {
            effect: Box::new(map_program::<Source, Mapped, C, D, M>(*effect, map)),
            declaration,
        },
    };

    D::Effect::from_parts(program, scope)
}

/// Adapts input without another future around the original admission future.
fn contramap<Source, Mapped>(
    target: EffectSender<Mapped>,
    map: Arc<ActionMap<Source, Mapped>>,
) -> EffectSender<Source>
where
    Source: 'static,
    Mapped: 'static,
{
    // The public borrowed send keeps this entire mapping chain alive while the
    // underlying owned future runs, so forwarding needs no additional future.
    EffectSender::new(move |source| (target.send)(map(source)))
}

#[cfg(test)]
ordinary_test_items! {
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;
    use crate::MonotonicInstant;
    use crate::composition::DismissTarget;
    use crate::store::scope::ScopeRegistry;

    struct DropCount(Arc<AtomicUsize>);

    impl Drop for DropCount {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn eager_mapping_preserves_payload_timing_scope_and_mapper_drop() {
        let token = ScopeRegistry::for_test().1.root();
        let duration = Duration::from_secs(3);
        let deadline = MonotonicInstant::MAX;
        let effects = [
            Effect::send(String::from("one")),
            Effect::send_after(duration, String::from("two")),
            Effect::send_at(deadline, String::from("six")),
        ];
        for (index, effect) in effects.into_iter().enumerate() {
            let calls = Arc::new(AtomicUsize::new(0));
            let drops = Arc::new(AtomicUsize::new(0));
            let called = Arc::clone(&calls);
            let capture = DropCount(Arc::clone(&drops));
            let mapped = effect.in_scope(token).map(move |value| {
                let _ = &capture;
                called.fetch_add(1, Ordering::Relaxed);
                value.len()
            });
            assert_eq!(calls.load(Ordering::Relaxed), 1);
            assert_eq!(drops.load(Ordering::Relaxed), 1);
            assert_eq!(mapped.scope, Some(token));
            match (index, mapped.program) {
                (0, Program::Send(3)) => {}
                (
                    1,
                    Program::SendAfter {
                        duration: actual,
                        action: 3,
                    },
                ) => {
                    assert_eq!(actual, duration);
                }
                (
                    2,
                    Program::SendAt {
                        deadline: actual,
                        action: 3,
                    },
                ) => {
                    assert_eq!(actual, deadline);
                }
                _ => panic!("mapping must preserve effect kind and mapped payload"),
            }
        }
    }

    #[test]
    fn action_free_mapping_drops_mapper_without_calling_it() {
        let token = ScopeRegistry::for_test().1.root();
        let programs = [
            Program::None,
            Effect::<u8>::cancel(4_u8).program,
            Program::Wait(Duration::from_secs(5)),
            Program::Dismiss(DismissTarget::new(Arc::new(()), Arc::new(()), token)),
        ];
        for (index, program) in programs.into_iter().enumerate() {
            let drops = Arc::new(AtomicUsize::new(0));
            let capture = DropCount(Arc::clone(&drops));
            let mapped = Effect {
                program,
                scope: Some(token),
            }
            .map(move |_| -> u16 {
                let _ = &capture;
                panic!("action-free mapping must not call the mapper");
            });
            assert_eq!(drops.load(Ordering::Relaxed), 1);
            assert_eq!(mapped.scope, (index != 0).then_some(token));
            match (index, mapped.program) {
                (0, Program::None) | (1, Program::Cancel(_)) | (3, Program::Dismiss(_)) => {}
                (2, Program::Wait(duration)) => assert_eq!(duration, Duration::from_secs(5)),
                _ => panic!("mapping must preserve action-free effect kind"),
            }
        }
    }

    #[test]
    fn panicking_mapper_drops_payload_and_capture_once() {
        let payload_drops = Arc::new(AtomicUsize::new(0));
        let mapper_drops = Arc::new(AtomicUsize::new(0));
        let payload = DropCount(Arc::clone(&payload_drops));
        let capture = DropCount(Arc::clone(&mapper_drops));
        let result = std::panic::catch_unwind(move || {
            Effect::send(payload).map(move |_| -> () {
                let _ = &capture;
                panic!("mapper failure");
            })
        });
        result.unwrap_err();
        assert_eq!(payload_drops.load(Ordering::Relaxed), 1);
        assert_eq!(mapper_drops.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn deferred_mapping_retains_mapper_until_effect_drop() {
        let drops = Arc::new(AtomicUsize::new(0));
        let capture = DropCount(Arc::clone(&drops));
        let mapped = Effect::<u8>::run(|_| std::future::pending()).map(move |_| {
            let _ = &capture;
            1_u16
        });
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        drop(mapped);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }

    #[cfg(feature = "local")]
    #[test]
    fn local_eager_mapping_preserves_timing_scope_and_capture_drop() {
        use std::cell::Cell;
        use std::rc::Rc;

        use crate::local;

        let token = ScopeRegistry::for_test().1.root();
        let duration = Duration::from_secs(3);
        let deadline = MonotonicInstant::MAX;
        let effects = [
            local::Effect::send(String::from("one")),
            local::Effect::send_after(duration, String::from("two")),
            local::Effect::send_at(deadline, String::from("six")),
        ];

        for (index, effect) in effects.into_iter().enumerate() {
            let calls = Rc::new(Cell::new(0));
            let called = Rc::clone(&calls);
            let drops = Arc::new(AtomicUsize::new(0));
            let capture = DropCount(Arc::clone(&drops));
            let (program, _) = effect.into_parts();
            let scoped = local::Effect::from_parts(program, Some(token));
            let mapped = scoped.map(move |value| {
                let _ = &capture;
                called.set(called.get() + 1);
                value.len()
            });
            assert_eq!(calls.get(), 1);
            assert_eq!(drops.load(Ordering::Relaxed), 1);
            let (program, scope) = mapped.into_parts();
            assert_eq!(scope, Some(token));

            match (index, program) {
                (0, Program::Send(3)) => {}
                (1, Program::SendAfter { duration: actual, action: 3 }) => {
                    assert_eq!(actual, duration);
                }
                (2, Program::SendAt { deadline: actual, action: 3 }) => {
                    assert_eq!(actual, deadline);
                }
                _ => panic!("local mapping must preserve effect kind and mapped payload"),
            }
        }
    }

    #[cfg(feature = "local")]
    #[test]
    fn local_action_free_mapping_releases_capture_and_normalises_empty_scope() {
        use std::cell::Cell;
        use std::rc::Rc;

        use crate::local;
        use crate::store::local::LocalEffects;

        let token = ScopeRegistry::for_test().1.root();
        let (cancel, _) = local::Effect::<u8>::cancel(4_u8).into_parts();
        let programs = [
            Program::<u8, LocalEffects>::None,
            cancel,
            Program::Wait(Duration::from_secs(5)),
            Program::Dismiss(DismissTarget::new(Arc::new(()), Arc::new(()), token)),
        ];

        for (index, program) in programs.into_iter().enumerate() {
            let capture = Rc::new(Cell::new(0));
            let captured = Rc::clone(&capture);
            let effect = local::Effect::from_parts(program, Some(token));
            let mapped = effect.map(move |_| -> u16 {
                captured.set(captured.get() + 1);
                panic!("action-free local mapping must not call the mapper");
            });
            assert_eq!(capture.get(), 0);
            assert_eq!(Rc::strong_count(&capture), 1);
            let (program, scope) = mapped.into_parts();
            assert_eq!(scope, (index != 0).then_some(token));

            match (index, program) {
                (0, Program::None) | (1, Program::Cancel(_)) | (3, Program::Dismiss(_)) => {}
                (2, Program::Wait(duration)) => assert_eq!(duration, Duration::from_secs(5)),
                _ => panic!("local mapping must preserve action-free effect kind"),
            }
        }
    }
}
}
