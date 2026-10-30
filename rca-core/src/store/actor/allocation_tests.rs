//! Checks actor allocation budgets without the ordinary test harness.

use std::future::pending;

use super::test_support::{LaunchDriver, launch_actor};
use super::{
    Actor, Arc, ControlWake, Effect, Launches, LineageState, Runtime, RuntimeFuture, Sequence,
    Shared, TaskStatus, TimerProvenance,
};
use crate::ActionTask;

/// Checks timer command allocations and the lifetime of its causal work.
///
/// # Panics
/// Panics if ownership or allocation contracts change.
#[expect(
    clippy::too_many_lines,
    reason = "keep constructor cost and its live-provenance controls in one isolated process"
)]
pub(crate) fn timer_command_avoids_discarded_reply_allocation() {
    if crate::allocation_test::run_in_marked_child(
        "store::actor::tests::timer_command_avoids_discarded_reply_allocation",
        "RCA_TIMER_REPLY_ALLOCATION_CHILD",
    ) {
        return;
    }

    let (mut actor, _ingress) = launch_actor(
        Effect::none(),
        Runtime::new(LaunchDriver {
            tasks: Arc::new(std::sync::Mutex::new(Vec::new())),
            panic_after: usize::MAX,
        }),
    );
    let lineage = LineageState::new(Arc::downgrade(&actor.control));
    let witness = Arc::downgrade(&lineage);
    let scope = actor.scopes.root();
    let prepared = actor.registry.prepare(scope, Arc::clone(&lineage), &[]);
    let id = prepared.id();
    let provenance = TimerProvenance {
        scope,
        lineage: Arc::clone(&lineage),
        task: prepared.sender_token(),
        generations: prepared.generation_tokens(),
        operations: prepared.operation_ancestry(),
    };
    actor.registry.register_actor(prepared);
    lineage.finish_registration();
    let task = ActionTask::live(Arc::clone(&lineage));
    assert!(provenance.is_live());
    assert!(!task.is_finished());

    let profiler = dhat::Profiler::builder().testing().build();
    let before = dhat::HeapStats::get();
    let command = Actor::<(), ()>::timer_action_command((), &provenance);
    let alive = dhat::HeapStats::get();
    drop(command);
    let released = dhat::HeapStats::get();
    drop(profiler);
    println!(
        "timer blocks={} bytes={} held_blocks={} held_bytes={} retained_blocks={} retained_bytes={} peak_blocks={} peak_bytes={}",
        alive.total_blocks - before.total_blocks,
        alive.total_bytes - before.total_bytes,
        alive.curr_blocks - before.curr_blocks,
        alive.curr_bytes - before.curr_bytes,
        released.curr_blocks - before.curr_blocks,
        released.curr_bytes - before.curr_bytes,
        released.max_blocks,
        released.max_bytes,
    );

    let profiler = dhat::Profiler::builder().testing().build();
    let ack_before = dhat::HeapStats::get();
    let (reply, mut receive) = crate::store::command::commit_channel();
    let ack_alive = dhat::HeapStats::get();
    reply
        .send(Ok(ActionTask::completed()))
        .expect("connected acknowledgement");
    let acknowledged = receive.try_recv().unwrap().unwrap().unwrap();
    drop(receive);
    let ack_released = dhat::HeapStats::get();
    drop(profiler);
    assert!(acknowledged.is_finished());
    assert!(!acknowledged.is_cancelled());
    println!(
        "connected blocks={} bytes={} held_blocks={} held_bytes={} retained_blocks={} retained_bytes={} peak_blocks={} peak_bytes={}",
        ack_released.total_blocks - ack_before.total_blocks,
        ack_released.total_bytes - ack_before.total_bytes,
        ack_alive.curr_blocks - ack_before.curr_blocks,
        ack_alive.curr_bytes - ack_before.curr_bytes,
        ack_released.curr_blocks - ack_before.curr_blocks,
        ack_released.curr_bytes - ack_before.curr_bytes,
        ack_released.max_blocks,
        ack_released.max_bytes,
    );

    // Discarding one command must not complete or cancel its timer authority.
    assert!(provenance.is_live());
    assert!(!task.is_finished());
    assert!(!task.is_cancelled());
    let turn = actor.commit_action(Actor::<(), ()>::timer_action_command((), &provenance));
    assert!(actor.launch(turn));
    assert_eq!(actor.commit_version, 1);
    assert!(provenance.is_live());
    assert!(!task.is_finished());
    actor.registry.complete_actor(id, TaskStatus::Completed);
    let turn = actor.process_control();
    assert!(actor.launch(turn));
    assert!(task.is_finished());
    assert!(!task.is_cancelled());
    assert!(!provenance.is_live());
    let turn = actor.commit_action(Actor::<(), ()>::timer_action_command((), &provenance));
    assert!(!actor.launch(turn));
    assert_eq!(actor.commit_version, 1, "stale timer cannot reduce again");
    let run = actor.run();
    println!(
        "actor_future size={} align={}",
        std::mem::size_of_val(&run),
        std::mem::align_of_val(&run)
    );
    drop(run);
    drop(provenance);
    drop(task);
    drop(lineage);
    assert!(witness.upgrade().is_none());

    assert_eq!(ack_released.total_blocks - ack_before.total_blocks, 1);
    assert!(ack_released.total_bytes > ack_before.total_bytes);
    assert_eq!(ack_released.curr_blocks, ack_before.curr_blocks);
    assert_eq!(ack_released.curr_bytes, ack_before.curr_bytes);
    assert_eq!(released.curr_blocks, before.curr_blocks);
    assert_eq!(released.curr_bytes, before.curr_bytes);
    // The real timer constructor must not allocate a discarded reply.
    assert_eq!(released.total_blocks - before.total_blocks, 0);
    assert_eq!(released.total_bytes - before.total_bytes, 0);
}

/// Measures only launch-buffer storage; future boxes are prepared before profiling starts.
fn assert_launch_buffer_budget() {
    let widths = [0, 1, 2, 4, 5, 8, 9, 16, 17];
    let mut costs = [(0, 0); 9];
    for (width, cost) in widths.into_iter().zip(&mut costs) {
        let prepared: Vec<RuntimeFuture> = (0..width)
            .map(|_| Box::pin(std::future::ready(())) as RuntimeFuture)
            .collect();
        let profiler = dhat::Profiler::builder().testing().build();
        let before = dhat::HeapStats::get();
        let mut launches = Launches::default();
        for future in prepared {
            launches.push(future);
        }
        let allocated = dhat::HeapStats::get();
        drop(launches);
        let released = dhat::HeapStats::get();
        drop(profiler);
        *cost = (
            allocated.total_blocks - before.total_blocks,
            allocated.total_bytes - before.total_bytes,
        );
        assert_eq!(released.curr_blocks, before.curr_blocks);
        assert_eq!(released.curr_bytes, before.curr_bytes);
    }
    eprintln!(
        "launch buffer widths={widths:?} blocks/bytes={costs:?} inline_bytes={}",
        std::mem::size_of::<Launches>()
    );
    assert_eq!(
        costs.map(|(blocks, _)| blocks),
        [0, 0, 1, 1, 1, 2, 2, 3, 3],
        "the first launch stays inline when later launches spill"
    );
}

/// Checks direct-future launch and sequence-storage allocation budgets.
///
/// # Panics
/// Panics if ownership or allocation contracts change.
pub(crate) fn direct_future_launch_avoids_erased_sender_allocation() {
    if crate::allocation_test::run_in_marked_child(
        "store::actor::tests::direct_future_launch_avoids_erased_sender_allocation",
        "RCA_DIRECT_FUTURE_ALLOCATION_CHILD",
    ) {
        return;
    }

    assert_launch_buffer_budget();
    assert_sequence_storage_budget();

    let tasks = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (mut actor, _ingress) = launch_actor(
        Effect::none(),
        Runtime::new(LaunchDriver {
            tasks,
            panic_after: usize::MAX,
        }),
    );
    let effect = Effect::future(std::future::ready(None));
    let scope = actor.scopes.root();
    let mut lineage = None;
    let mut generations = Vec::new();
    let mut launches = Launches::default();
    let profiler = dhat::Profiler::builder().testing().build();
    let before = dhat::HeapStats::get();
    actor.transaction_parts().compile_effect(
        effect,
        super::super::mounted::WorkOrigin {
            scope,
            lineage: &mut lineage,
            generations: &mut generations,
            operations: &crate::effect::operation::OperationAncestry::default(),
            inherited_generations: 0,
            group: None,
        },
        &mut launches,
    );
    let blocks = dhat::HeapStats::get().total_blocks - before.total_blocks;
    drop(profiler);
    let lineage = lineage.expect("future must own a lineage");
    lineage.finish_registration();
    assert!(!lineage.is_finished());
    let mut launches = launches.into_iter();
    let mut launch = launches.next().expect("future must register a launch");
    assert!(
        launch
            .as_mut()
            .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
            .is_ready()
    );
    assert!(launches.next().is_none());
    drop(launch);
    drop(actor);
    assert!(lineage.is_finished());
    // The original erased sender used six blocks. Direct futures need no
    // public sender allocation, so their cold launch must save one block.
    assert!(
        blocks <= 5,
        "direct future launch allocated {blocks} blocks"
    );
}

/// Measures retained generations through real sequence compilation and full cleanup.
fn assert_sequence_storage_budget() {
    // The critical-section backend lazily allocates a process-global mutex.
    // Initialise it separately so the measurement includes only per-object costs.
    let warm_control = ControlWake::new();
    warm_control.notify();
    drop(warm_control);

    for (depth, blocks) in [(0_u32, 8), (1, 17), (4, 27), (16, 67)] {
        let effect = (0..depth).fold(
            Effect::concatenate([
                Effect::future(pending()),
                Effect::future(std::future::ready(None)),
            ]),
            Effect::cancellable,
        );
        let (mut actor, ingress) = launch_actor(
            Effect::none(),
            Runtime::new(LaunchDriver {
                tasks: Arc::default(),
                panic_after: usize::MAX,
            }),
        );
        let scope = actor.scopes.root();
        let mut lineage = None;
        let mut generations = Vec::new();
        let mut launches = Launches::default();
        let profiler = dhat::Profiler::builder().testing().build();
        let before = dhat::HeapStats::get();
        actor.transaction_parts().compile_effect(
            effect,
            super::super::mounted::WorkOrigin {
                scope,
                lineage: &mut lineage,
                generations: &mut generations,
                operations: &crate::effect::operation::OperationAncestry::default(),
                inherited_generations: 0,
                group: None,
            },
            &mut launches,
        );
        let retained = dhat::HeapStats::get();
        assert_eq!(actor.sequences.len(), 1);
        assert_eq!(
            actor.sequences.values().next().unwrap().generations.len(),
            depth as usize,
        );
        drop(launches);
        drop(actor);
        drop(ingress);
        drop(lineage);
        drop(generations);
        let released = dhat::HeapStats::get();
        drop(profiler);
        eprintln!(
            "sequence depth={depth} blocks={} bytes={} held_blocks={} held_bytes={} inline_bytes={}",
            retained.total_blocks - before.total_blocks,
            retained.total_bytes - before.total_bytes,
            retained.curr_blocks - before.curr_blocks,
            retained.curr_bytes - before.curr_bytes,
            std::mem::size_of::<Sequence<super::super::mounted::DeferredQueue<(), (), Shared>>>(),
        );
        assert_eq!(released.curr_blocks, before.curr_blocks);
        assert_eq!(released.curr_bytes, before.curr_bytes);
        assert_eq!(retained.total_blocks - before.total_blocks, blocks);
    }
    #[cfg(target_pointer_width = "64")]
    assert!(
        std::mem::size_of::<Sequence<super::super::mounted::DeferredQueue<(), (), Shared>>>()
            <= 144,
        "retained sequence storage must not regain unused generation capacity",
    );
}
