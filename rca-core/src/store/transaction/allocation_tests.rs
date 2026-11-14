//! Measures transaction costs without the ordinary test harness.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::test_support::{Host, counter_host};
use super::{
    ControlWake, EffectValue, Launches, OperationAncestry, Program, ScopeRegistry, Shared,
    WorkOrigin,
};

#[derive(Clone, Copy, Debug)]
enum GenerationMerge {
    Flat,
    Mixed,
    Nested,
    Scoped,
    NonFresh,
}

/// Checks dense Runtime siblings share one captured generation backing.
pub(crate) fn dense_runtime_merge_shares_generation_snapshot() {
    const NAME: &str = "store::transaction::tests::dense_runtime_merge_shares_generation_snapshot";
    if crate::allocation_test::run_in_child(NAME) {
        return;
    }

    // Expected values name registered tasks, nonempty memberships and distinct
    // backings. Two nested groups of eight share only the first eligible group;
    // its launches make the second group's launch buffer nonfresh.
    for (shape, runtime_count, keyed, expected) in [
        (GenerationMerge::Flat, 0, true, (0, 0, 0)),
        (GenerationMerge::Flat, 1, true, (1, 1, 1)),
        (GenerationMerge::Flat, 16, false, (16, 0, 0)),
        (GenerationMerge::Flat, 16, true, (16, 16, 1)),
        (GenerationMerge::Flat, 7, true, (7, 7, 7)),
        (GenerationMerge::Mixed, 15, true, (16, 16, 16)),
        (GenerationMerge::Nested, 16, true, (16, 16, 9)),
        (GenerationMerge::Scoped, 16, true, (16, 16, 16)),
        (GenerationMerge::NonFresh, 16, true, (18, 16, 16)),
    ] {
        let mut host = counter_host();
        let scope = host.scopes.root();
        let factories = Arc::new(AtomicUsize::new(0));
        let mut effects: Vec<_> = (0..runtime_count)
            .map(|_| deferred_runtime(&factories))
            .collect();
        let mut launches = Launches::default();
        match shape {
            GenerationMerge::Flat => {}
            GenerationMerge::Mixed => effects.push(crate::Effect::send(1)),
            GenerationMerge::Nested => {
                let right = effects.split_off(8);
                effects = vec![crate::Effect::merge(effects), crate::Effect::merge(right)];
            }
            GenerationMerge::Scoped => {
                let node = ScopeRegistry::node(&mut crate::store::scope::ScopeSlot::new());
                let child =
                    host.scopes
                        .mount(scope, node, crate::store::scope::ScopeIdentity::unit());
                *effects[0].parts_mut().1 = Some(child);
            }
            GenerationMerge::NonFresh => host.parts().compile_effect(
                crate::Effect::merge((0..2).map(|_| deferred_runtime(&factories))),
                WorkOrigin {
                    scope,
                    lineage: &mut None,
                    generations: &mut Vec::new(),
                    operations: &OperationAncestry::default(),
                    inherited_generations: 0,
                    group: None,
                },
                &mut launches,
            ),
        }
        let effect = crate::Effect::merge(effects);
        let effect = if keyed {
            effect.cancellable("dense-runtime-merge")
        } else {
            effect
        };
        assert_generation_snapshot_registration(
            &mut host, effect, launches, &factories, expected, shape,
        );
    }
}

/// Builds a real Runtime leaf whose factory must remain deferred.
fn deferred_runtime(factories: &Arc<AtomicUsize>) -> crate::Effect<usize> {
    let factories = Arc::clone(factories);
    crate::Effect::run(move |_| {
        factories.fetch_add(1, Ordering::SeqCst);
        std::future::pending::<()>()
    })
}

/// Measures compilation before inspecting its retained registered tasks.
fn assert_generation_snapshot_registration(
    host: &mut Host<usize, usize, Shared>,
    effect: crate::Effect<usize>,
    mut launches: Launches,
    factories: &Arc<AtomicUsize>,
    expected: (usize, usize, usize),
    shape: GenerationMerge,
) {
    let scope = host.scopes.root();
    let mut lineage = None;
    let mut generations = Vec::new();
    let operations = OperationAncestry::default();
    let profiler = dhat::Profiler::builder().testing().build();
    let before = dhat::HeapStats::get();
    host.parts().compile_effect(
        effect,
        WorkOrigin {
            scope,
            lineage: &mut lineage,
            generations: &mut generations,
            operations: &operations,
            inherited_generations: 0,
            group: None,
        },
        &mut launches,
    );
    let after = dhat::HeapStats::get();
    let costs = (
        after.total_blocks - before.total_blocks,
        after.total_bytes - before.total_bytes,
    );
    drop(profiler);

    // This collection occurs after sampling and cannot affect measured costs.
    let backings = host.registry.generation_snapshot_backings();
    assert_eq!(backings.len(), expected.0);
    assert!(backings.windows(2).all(|pair| pair[0].0 != pair[1].0));
    let mut distinct: Vec<_> = backings
        .iter()
        .filter_map(|(_, backing)| *backing)
        .collect();
    assert_eq!(distinct.len(), expected.1);
    let membership_ids: Vec<_> = backings
        .iter()
        .filter_map(|(id, backing)| backing.is_some().then_some(*id))
        .collect();
    assert_eq!(
        host.registry.generation_membership_task_ids(),
        membership_ids
    );
    distinct.sort_unstable();
    distinct.dedup();
    assert!(host.timers.is_empty());
    assert!(generations.is_empty());
    assert_eq!(launches.into_iter().count(), expected.0);
    assert_eq!(factories.load(Ordering::SeqCst), 0);
    eprintln!(
        "shape={shape:?} tasks={} memberships={} snapshots={} blocks/bytes={costs:?}",
        backings.len(),
        expected.1,
        distinct.len(),
    );
    assert_eq!(
        distinct.len(),
        expected.2,
        "generation backing count for {shape:?}"
    );
}

/// Checks that dense effect batches avoid repeated launch-buffer allocations.
pub(crate) fn dense_effect_registration_avoids_launch_growth() {
    const NAME: &str = "store::transaction::tests::dense_effect_registration_avoids_launch_growth";
    if crate::allocation_test::run_in_child(NAME) {
        return;
    }

    for (runtime_count, timer_count, retired) in [
        (0, 16, false),
        (1, 15, false),
        (16, 0, true),
        (16, 0, false),
    ] {
        let separate = measure_launch_registration(runtime_count, timer_count, retired, false);
        let merged = measure_launch_registration(runtime_count, timer_count, retired, true);
        eprintln!(
            "runtime={runtime_count} timers={timer_count} retired={retired} separate={separate:?} merged={merged:?}"
        );
        if timer_count == 0 && !retired {
            assert!(
                merged.0 + 2 <= separate.0 && merged.1 < separate.1,
                "sixteen merged launches must save two allocation requests and bytes"
            );
        } else {
            assert_eq!(
                merged, separate,
                "timer work and retired children must not reserve launch storage"
            );
        }
    }
}

/// Measures registration after construction and before polling any launch.
fn measure_launch_registration(
    runtime_count: usize,
    timer_count: usize,
    retired: bool,
    grouped: bool,
) -> (u64, u64) {
    let mut host = counter_host();
    let scope = host.scopes.root();
    let retired = retired.then(|| {
        let node = ScopeRegistry::node(&mut crate::store::scope::ScopeSlot::new());
        let child = host
            .scopes
            .mount(scope, node, crate::store::scope::ScopeIdentity::unit());
        drop(host.scopes.leave(child));
        crate::Effect::send(usize::MAX).in_scope(child)
    });
    let effects = (0..runtime_count)
        .map(crate::Effect::send)
        .chain((0..timer_count).map(|_| crate::Effect::send_after(std::time::Duration::MAX, 0)))
        .chain(retired);
    let effects = if grouped {
        vec![crate::Effect::merge(effects)]
    } else {
        effects.collect()
    };
    let mut lineage = None;
    let mut generations = Vec::new();
    let operations = OperationAncestry::default();
    let mut launches = Launches::default();

    let profiler = dhat::Profiler::builder().testing().build();
    let before = dhat::HeapStats::get();
    for effect in effects {
        host.parts().compile_effect(
            effect,
            WorkOrigin {
                scope,
                lineage: &mut lineage,
                generations: &mut generations,
                operations: &operations,
                inherited_generations: 0,
                group: None,
            },
            &mut launches,
        );
    }
    let after = dhat::HeapStats::get();
    drop(profiler);

    assert_eq!(host.timers.len(), timer_count);
    assert_eq!(launches.into_iter().count(), runtime_count);
    (
        after.total_blocks - before.total_blocks,
        after.total_bytes - before.total_bytes,
    )
}

/// Checks delayed suffix retirement keeps a constant allocation budget.
pub(crate) fn retiring_delayed_suffixes_prunes_waits_with_constant_allocation_budget() {
    const NAME: &str = "store::transaction::tests::retiring_delayed_suffixes_prunes_waits_with_constant_allocation_budget";
    if crate::allocation_test::run_in_child(NAME) {
        return;
    }

    // The critical-section backend lazily allocates a process-global mutex.
    // Initialise it separately so the control measures a cold event, not global setup.
    let warm_control = ControlWake::new();
    warm_control.notify();
    drop(warm_control);

    // Cold event/Mutex storage differs across targets. Measure that same control
    // notification independently instead of pinning the macOS allocation count.
    let control = ControlWake::new();
    let profiler = dhat::Profiler::builder().testing().build();
    let before = dhat::HeapStats::get();
    control.notify();
    let after = dhat::HeapStats::get();
    let cold_notification = (
        after.total_blocks - before.total_blocks,
        after.total_bytes - before.total_bytes,
    );
    drop(profiler);

    let widths = [0, 1, 4, 16];
    let mut costs = [(0, 0); 4];
    for (index, width) in widths.into_iter().enumerate() {
        let mut host = counter_host();
        let root = host.scopes.root();
        let node = ScopeRegistry::node(&mut crate::store::scope::ScopeSlot::new());
        let child = host
            .scopes
            .mount(root, node, crate::store::scope::ScopeIdentity::unit());
        let mut lineage = None;
        let mut generations = Vec::new();
        let mut launches = Launches::default();
        for _ in 0..width {
            // The wait belongs to the live root; only its queued suffix retires.
            let wait = crate::Effect::from_parts(Program::Wait(std::time::Duration::MAX), None);
            host.parts().compile_effect(
                crate::Effect::concatenate([wait, crate::Effect::send(1).in_scope(child)]),
                WorkOrigin {
                    scope: root,
                    lineage: &mut lineage,
                    generations: &mut generations,
                    operations: &OperationAncestry::default(),
                    inherited_generations: 0,
                    group: None,
                },
                &mut launches,
            );
        }
        assert!(launches.into_iter().next().is_none());
        assert_eq!(host.sequences.len(), width);
        assert_eq!(host.sequence_waits.len(), width);
        assert_eq!(host.timers.len(), width);
        if let Some(lineage) = &lineage {
            lineage.finish_registration();
            assert!(!lineage.is_finished());
        }
        drop(host.scopes.leave(child));

        let profiler = dhat::Profiler::builder().testing().build();
        let before = dhat::HeapStats::get();
        host.parts().prune_deferred_scopes();
        let after = dhat::HeapStats::get();
        costs[index] = (
            after.total_blocks - before.total_blocks,
            after.total_bytes - before.total_bytes,
        );
        drop(profiler);

        assert!(host.timers.is_empty());

        // Another retirement may run before terminal waits are collected.
        host.parts().prune_deferred_scopes();
        assert!(host.timers.is_empty());
        assert_eq!(
            host.state, 0,
            "repeated pruning must not reduce retired work"
        );

        assert!(host.collect().into_iter().next().is_none());
        assert!(host.sequence_waits.is_empty());
        assert!(host.sequences.is_empty());
        assert!(host.task_groups.is_empty());
        assert!(lineage.as_ref().is_none_or(|lineage| lineage.is_finished()));
        assert_eq!(host.state, 0, "retired suffixes must not reduce");
    }
    eprintln!(
        "orphaned waits widths={widths:?} blocks/bytes={costs:?} cold_notification={cold_notification:?}"
    );
    // The first completion initialises ControlWake's event. Selecting orphaned
    // waits needs no additional allocation, regardless of the sequence count.
    assert_eq!(
        costs,
        [
            (0, 0),
            cold_notification,
            cold_notification,
            cold_notification
        ]
    );
}
