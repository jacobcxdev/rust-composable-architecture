//! Root task-send costs across cold, reused and full-mailbox admissions.
//!
//! The assay includes public protocol work, not just endpoint allocation.
//! Exact state and drop counts check that measured work actually completes.

#![cfg(feature = "allocation-measurement")]

#[path = "../src/allocation_test/child.rs"]
mod allocation_test;

#[expect(
    dead_code,
    reason = "this isolated assay manually polls captured tasks"
)]
mod support;

use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use rca_core::{Feature, FeatureContext, Store, Task, TaskContext};

#[global_allocator]
/// Counts requested and retained allocations inside the active profiler session.
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

#[derive(Debug)]
/// Keeps each action non-cloneable and counts its destruction independently.
struct Payload(Cell<u64>, Arc<AtomicUsize>);

impl Drop for Payload {
    fn drop(&mut self) {
        self.1.fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Debug)]
/// Starts the captured origin or adds one owned value to state.
enum Action {
    Start,
    Add(Payload),
}

#[derive(Debug)]
/// Counts additions and proves state release despite the retained task context.
struct State(Cell<u64>, Arc<AtomicUsize>);

impl Drop for State {
    fn drop(&mut self) {
        self.1.fetch_add(1, Ordering::Relaxed);
    }
}

/// Exports one actual root task context while its task remains pending.
struct Capture(Arc<Mutex<Option<TaskContext<State, Action>>>>);

impl Feature for Capture {
    type State = State;
    type Action = Action;

    fn update(
        &mut self,
        state: &mut State,
        action: &Action,
        context: &mut FeatureContext<'_, State, Action>,
    ) {
        match action {
            Action::Start => {
                let captured = Arc::clone(&self.0);
                context.task(Task::run(move |context| async move {
                    *captured.lock().expect("capture lock") = Some(context);
                    std::future::pending::<()>().await;
                }));
            }
            Action::Add(value) => state.0.set(state.0.get() + value.0.get()),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
/// Separates requested allocations from signed changes in retained storage.
struct Cost {
    blocks: u64,
    bytes: u64,
    live_blocks: i128,
    live_bytes: i128,
}

impl Cost {
    /// Computes a phase delta without losing negative retained-storage changes.
    fn between(before: &dhat::HeapStats, after: &dhat::HeapStats) -> Self {
        Self {
            blocks: after.total_blocks - before.total_blocks,
            bytes: after.total_bytes - before.total_bytes,
            live_blocks: after.curr_blocks as i128 - before.curr_blocks as i128,
            live_bytes: after.curr_bytes as i128 - before.curr_bytes as i128,
        }
    }

    /// Adds a completed sample to its cold, reused or full-mailbox category.
    fn add(&mut self, other: Self) {
        self.blocks += other.blocks;
        self.bytes += other.bytes;
        self.live_blocks += other.live_blocks;
        self.live_bytes += other.live_bytes;
    }
}

/// Checks cold, reused and full root-task send costs.
#[expect(
    clippy::too_many_lines,
    reason = "keep registration, send, cancellation and teardown ownership measurements in one schedule"
)]
fn root_task_send_reports_origin_cold_reused_and_full_costs() {
    if allocation_test::run_in_child("root_task_send_reports_origin_cold_reused_and_full_costs") {
        return;
    }

    let captured = Arc::new(Mutex::new(None));
    let payload_drops = Arc::new(AtomicUsize::new(0));
    let state_drops = Arc::new(AtomicUsize::new(0));
    let runtime = support::ManualRuntime::new();
    let store = Store::builder(
        State(Cell::new(0), Arc::clone(&state_drops)),
        Capture(Arc::clone(&captured)),
        runtime.runtime(),
    )
    .mailbox_capacity(1)
    .build()
    .expect("valid store");
    let mut actor = runtime.take_only_queued_task();
    let mut context = Context::from_waker(Waker::noop());
    assert!(actor.as_mut().poll(&mut context).is_pending());
    let start = store.try_send(Action::Start).expect("start fits");

    // Setup above is excluded. This phase is the complete origin registration
    // and first task poll, not a purported isolated endpoint allocation.
    let profiler = dhat::Profiler::builder().testing().build();
    let origin_before = dhat::HeapStats::get();
    assert!(actor.as_mut().poll(&mut context).is_pending());
    let task = {
        let finish = start.finish();
        let mut finish = std::pin::pin!(finish);
        let Poll::Ready(Ok(task)) = finish.as_mut().poll(&mut context) else {
            panic!("start committed");
        };
        task
    };
    let mut body = runtime.take_only_queued_task();
    assert!(body.as_mut().poll(&mut context).is_pending());
    let sender = captured
        .lock()
        .expect("capture lock")
        .take()
        .expect("root context");
    let origin_after = dhat::HeapStats::get();
    assert!(!task.is_finished());
    assert_eq!(runtime.spawn_count(), 2);

    // Exactly one cold, 63 reused fast sends and one full-mailbox send. The
    // latter has one external prefill. No action creates a descendant task.
    // Full complete includes prefill processing, but not prefill creation.
    // The second pending poll belongs only to each complete interval.
    let mut admission = [Cost::default(); 3];
    let mut commit = [Cost::default(); 3];
    let mut complete = [Cost::default(); 3];
    let mut lazy = [Cost::default(); 3];
    let mut retry = Cost::default();
    let mut prefill_cost = Cost::default();
    for index in 0..65 {
        let cell = if index == 0 {
            0
        } else if index == 64 {
            2
        } else {
            1
        };
        let prefill = (index == 64).then(|| {
            store
                .try_send(Action::Add(Payload(
                    Cell::new(1),
                    Arc::clone(&payload_drops),
                )))
                .expect("prefill fits")
        });
        let action = Action::Add(Payload(Cell::new(1), Arc::clone(&payload_drops)));
        let before = dhat::HeapStats::get();
        {
            let send = sender.send(action);
            let created = dhat::HeapStats::get();
            lazy[cell].add(Cost::between(&before, &created));
            let mut send = std::pin::pin!(send);
            assert!(send.as_mut().poll(&mut context).is_pending());
            let admitted = dhat::HeapStats::get();
            admission[cell].add(Cost::between(&created, &admitted));
            assert!(send.as_mut().poll(&mut context).is_pending());
            if let Some(prefill) = prefill {
                let before_prefill = dhat::HeapStats::get();
                assert!(actor.as_mut().poll(&mut context).is_pending());
                let prefill_task = {
                    let finish = prefill.finish();
                    let mut finish = std::pin::pin!(finish);
                    let Poll::Ready(Ok(task)) = finish.as_mut().poll(&mut context) else {
                        panic!("prefill committed");
                    };
                    task
                };
                assert!(prefill_task.is_finished());
                drop(prefill_task);
                let after_prefill = dhat::HeapStats::get();
                prefill_cost = Cost::between(&before_prefill, &after_prefill);
                assert!(send.as_mut().poll(&mut context).is_pending());
                retry = Cost::between(&after_prefill, &dhat::HeapStats::get());
            }
            let before_commit = dhat::HeapStats::get();
            assert!(actor.as_mut().poll(&mut context).is_pending());
            let after_commit = dhat::HeapStats::get();
            commit[cell].add(Cost::between(&before_commit, &after_commit));
            let Poll::Ready(Ok(committed)) = send.as_mut().poll(&mut context) else {
                panic!("root send committed");
            };
            assert!(
                !committed.is_finished(),
                "send reply shares the still-live origin lineage"
            );
            drop(committed);
        };
        complete[cell].add(Cost::between(&before, &dhat::HeapStats::get()));
        assert!(!task.is_finished(), "captured origin remains pending");
        assert_eq!(runtime.spawn_count(), 2, "no hidden descendant work");
        assert_eq!(
            payload_drops.load(Ordering::Relaxed),
            index + 1 + usize::from(index == 64)
        );
    }

    let before_teardown = dhat::HeapStats::get();
    task.cancel();
    assert!(actor.as_mut().poll(&mut context).is_pending());
    assert!(body.as_mut().poll(&mut context).is_ready());
    drop(body);
    assert!(actor.as_mut().poll(&mut context).is_pending());
    assert!(task.is_finished());
    drop(task);
    {
        let shutdown = store.shutdown_into_state();
        let mut shutdown = std::pin::pin!(shutdown);
        assert!(shutdown.as_mut().poll(&mut context).is_pending());
        assert!(actor.as_mut().poll(&mut context).is_ready());
        let Poll::Ready(Ok(state)) = shutdown.as_mut().poll(&mut context) else {
            panic!("ordered state extraction");
        };
        assert_eq!(state.0.get(), 66, "65 root sends and one prefill");
        drop(state);
    };
    assert_eq!(
        state_drops.load(Ordering::Relaxed),
        1,
        "retained context is metadata-only"
    );
    let context_only = dhat::HeapStats::get();
    drop(sender);
    drop(actor);
    // The closed store, runtime and observation counters still exist here. These
    // retained totals are not a claim that all fixture metadata has been freed.
    let end = dhat::HeapStats::get();
    assert_eq!(payload_drops.load(Ordering::Relaxed), 66);
    drop(profiler);

    eprintln!(
        "root task origin registration+first poll: {:?}",
        Cost::between(&origin_before, &origin_after)
    );
    for (index, name, count) in [(0, "cold", 1), (1, "reused", 63), (2, "full", 1)] {
        eprintln!(
            "root task {name} count={count}: lazy={:?} admission={:?} commit={:?} complete={:?}",
            lazy[index], admission[index], commit[index], complete[index]
        );
        assert_eq!(lazy[index].blocks, 0, "future construction remains lazy");
        assert_eq!(lazy[index].bytes, 0, "future construction remains lazy");
    }
    eprintln!(
        "root task full complete includes external-prefill processing (not creation): prefill={prefill_cost:?} retry={retry:?}"
    );
    eprintln!(
        "root task teardown to retained context={:?} final context/actor release={:?}; blocks_at_byte_peak={} session_peak_bytes={} final_live_blocks={} final_live_bytes={}",
        Cost::between(&before_teardown, &context_only),
        Cost::between(&context_only, &end),
        end.max_blocks,
        end.max_bytes,
        end.curr_blocks,
        end.curr_bytes
    );
}

/// Preserves the exact names of the isolated allocation cases.
const CASES: &[(&str, fn())] = &[(
    "root_task_send_reports_origin_cold_reused_and_full_costs",
    root_task_send_reports_origin_cold_reused_and_full_costs,
)];

include!("support/allocation_main.rs");
