//! Measures transaction costs without the ordinary test harness.

use super::test_support::counter_host;
use super::{
    ControlWake, EffectValue, Launches, OperationAncestry, Program, ScopeRegistry, WorkOrigin,
};

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
        assert_eq!(host.task_timers.len(), width);
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
        assert!(host.task_timers.is_empty());

        // Another retirement may run before terminal waits are collected.
        host.parts().prune_deferred_scopes();
        assert!(host.timers.is_empty());
        assert!(host.task_timers.is_empty());
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
