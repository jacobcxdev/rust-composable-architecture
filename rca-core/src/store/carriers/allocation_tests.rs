//! Measures actual registration and first-poll costs without copying the kernel.

use super::{EffectLeaf, Execution, Provenance, Shared, TaskRegistry};
use crate::effect::CancellationIdentity;
use crate::effect::leaves::SharedEffects;
use crate::effect::lineage::{ControlWake, LineageState};
use crate::store::local::{Local, LocalEffects};
use crate::store::scope::ScopeRegistry;
use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::task::{Context, Waker};

/// Compares registration across cancellation depths and authored run first-poll costs.
pub(crate) fn effect_registration_costs() {
    const NAME: &str = "store::carriers::allocation_tests::effect_registration_costs";
    if crate::allocation_test::run_in_child(NAME) {
        return;
    }

    let _profiler = dhat::Profiler::builder().testing().build();
    let mut baseline = [0; 3];
    for depth in [0, 1, 4, 16] {
        let shared = [
            measure::<Shared>(depth, || EffectLeaf::Send(())),
            measure::<Shared>(depth, || {
                EffectLeaf::<(), SharedEffects>::Run(Box::new(|_| Box::pin(std::future::pending())))
            }),
            measure::<Shared>(depth, || {
                EffectLeaf::<(), SharedEffects>::Future(Box::pin(std::future::pending()))
            }),
        ];
        let local = [
            measure::<Local>(depth, || EffectLeaf::Send(())),
            measure::<Local>(depth, || {
                EffectLeaf::<(), LocalEffects>::Run(Box::new(|_| Box::pin(std::future::pending())))
            }),
            measure::<Local>(depth, || {
                EffectLeaf::<(), LocalEffects>::Future(Box::pin(std::future::pending()))
            }),
        ];

        if depth == 0 {
            baseline = shared.map(|cost| cost.0);
        }

        for (index, name) in ["send", "run", "future"].into_iter().enumerate() {
            eprintln!(
                "{name} registration depth={depth}: shared={:?}, local={:?}",
                shared[index], local[index]
            );
            assert_eq!(
                shared[index].0, baseline[index],
                "shared {name} registration must not copy generation storage at depth {depth}"
            );
            assert_eq!(
                local[index].0, shared[index].0,
                "local {name} registration must not copy generation storage at depth {depth}"
            );
        }
    }

    let shared_calls = Arc::new(AtomicUsize::new(0));
    let local_calls = Rc::new(Cell::new(0));
    let shared = measure_run_first_poll::<Shared>(|| {
        let calls = Arc::clone(&shared_calls);
        let effect = crate::Effect::run(move |_| {
            calls.fetch_add(1, Ordering::Relaxed);
            std::future::pending()
        });
        assert_eq!(shared_calls.load(Ordering::Relaxed), 0);
        let crate::effect::Program::Run(run) = effect.program else {
            panic!("run constructor must retain its deferred factory");
        };
        EffectLeaf::Run(run)
    });
    let local = measure_run_first_poll::<Local>(|| {
        let calls = Rc::clone(&local_calls);
        let effect = crate::store::local::Effect::run(move |_| {
            calls.set(calls.get() + 1);
            std::future::pending()
        });
        assert_eq!(local_calls.get(), 0);
        let crate::effect::Program::Run(run) = effect.program else {
            panic!("run constructor must retain its deferred factory");
        };
        EffectLeaf::Run(run)
    });
    assert_eq!(shared_calls.load(Ordering::Relaxed), 1);
    assert_eq!(local_calls.get(), 1);
    eprintln!("authored run first poll: shared={shared:?}, local={local:?}");
    assert_eq!(shared, (0, 0), "a zero-sized future needs no allocation");
    assert_eq!(
        local, shared,
        "local run must not allocate an extra future wrapper"
    );
}

/// Measures first polling of authored work after actual registration, then checks release.
fn measure_run_first_poll<C: Execution<(), ()>>(
    make_leaf: impl FnOnce() -> EffectLeaf<(), C::Effects>,
) -> (u64, u64) {
    let initial = dhat::HeapStats::get();
    let cost = {
        let control = ControlWake::new();
        let mut registry = TaskRegistry::new(Arc::clone(&control));
        let (_lifecycle, scopes) = ScopeRegistry::for_test();
        let scope = scopes.root();
        let lineage = LineageState::new(Arc::downgrade(&control));
        let prepared = registry.prepare(scope, Arc::clone(&lineage), &[]);
        let provenance = Provenance::Effect {
            scope,
            lineage: Arc::clone(&lineage),
            task: prepared.sender_token(),
            generations: prepared.generation_tokens(),
            operations: prepared.operation_ancestry(),
        };
        let leaf = make_leaf();
        let mut future = C::register_effect(&mut registry, prepared, leaf, Weak::new(), provenance);
        lineage.finish_registration();

        let before = dhat::HeapStats::get();
        assert!(
            std::pin::Pin::new(&mut future)
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        let after = dhat::HeapStats::get();
        let cost = (
            after.total_blocks - before.total_blocks,
            after.total_bytes - before.total_bytes,
        );

        drop(future);
        assert_eq!(registry.process_control().terminal.len(), 1);
        cost
    };
    let released = dhat::HeapStats::get();
    assert_eq!(released.curr_blocks, initial.curr_blocks);
    assert_eq!(released.curr_bytes, initial.curr_bytes);
    cost
}

/// Measures registration alone, then checks that every retained allocation is released.
fn measure<C: Execution<(), ()>>(
    depth: usize,
    make_leaf: impl FnOnce() -> EffectLeaf<(), C::Effects>,
) -> (u64, u64) {
    let initial = dhat::HeapStats::get();
    let cost = {
        let control = ControlWake::new();
        let mut registry = TaskRegistry::new(Arc::clone(&control));
        let (_lifecycle, scopes) = ScopeRegistry::for_test();
        let scope = scopes.root();
        let lineage = LineageState::new(Arc::downgrade(&control));
        let generations: Vec<_> = (0..depth)
            .map(|id| registry.register_generation(CancellationIdentity::local(id).in_scope(scope)))
            .collect();
        let prepared = registry.prepare(scope, Arc::clone(&lineage), &generations);
        let provenance = Provenance::Effect {
            scope,
            lineage: Arc::clone(&lineage),
            task: prepared.sender_token(),
            generations: prepared.generation_tokens(),
            operations: prepared.operation_ancestry(),
        };
        let leaf = make_leaf();

        let before = dhat::HeapStats::get();
        let future = C::register_effect(&mut registry, prepared, leaf, Weak::new(), provenance);
        let after = dhat::HeapStats::get();
        let cost = (
            after.total_blocks - before.total_blocks,
            after.total_bytes - before.total_bytes,
        );

        lineage.finish_registration();
        drop(future);
        assert_eq!(registry.process_control().terminal.len(), 1);
        for generation in &generations {
            registry.release_generation(generation);
        }
        cost
    };

    let released = dhat::HeapStats::get();
    assert_eq!(released.curr_blocks, initial.curr_blocks);
    assert_eq!(released.curr_bytes, initial.curr_bytes);
    cost
}
