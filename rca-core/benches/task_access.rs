//! Measures root task access and completed tasks through a manually polled actor.
//!
//! Each access sample includes request creation, admission, actor work and the reply.
//! The lifecycle row also declares, registers, polls and collects a completed task.
//! Setup, origin cancellation/join and exact final-state checks are outside
//! timing. The originating task stays pending throughout all samples, so every
//! request uses real, live causal authority. This measures neither contention
//! nor an executor's scheduling cost.
//!
//! For paired comparisons, pass `--sample-size 256` to both binaries. A fixed
//! batch size limits timer rounding and keeps automatic batch choices out of
//! the comparison.

/// Supplies the manually controlled runtime used by each fixture.
#[path = "../tests/support/mod.rs"]
#[expect(
    dead_code,
    reason = "this benchmark only manually polls captured tasks"
)]
mod support;

use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use rca_core::{
    ActionTask, Effect, Feature, FeatureContext, ReduceContext, Reducer, RuntimeFuture, Store,
    Task, TaskContext, combine,
};

/// Counts allocations separately from uninstrumented timing runs.
#[cfg(feature = "allocation-measurement")]
#[global_allocator]
static ALLOCATOR: divan::AllocProfiler = divan::AllocProfiler::system();

/// Distinguishes an actual state read from a default-valued reply.
const INITIAL_STATE: u64 = 7;

fn main() {
    divan::main();
}

/// Starts causal work, declares child batches or commits an increment.
#[derive(Debug)]
enum Action {
    /// Captures the pending origin's live task context.
    Start,
    /// Declares one child task that completes when polled.
    CompleteTask,
    /// Declares an ordered batch and records one state change.
    CompleteBatch {
        /// Number of typed task bodies to register.
        tasks: usize,
        /// Number of ordinary effect bodies to register.
        effects: usize,
    },
    /// Runs a tuple of reducers through the ordinary feature adapter.
    Combined {
        /// Number of reducer members in the concrete tuple.
        width: usize,
        /// Selects completed runtime effects instead of empty effects.
        effectful: bool,
    },
    /// Adds the supplied value to actor-owned state.
    Add(u64),
}

/// Captures a genuine root task context without completing its origin task.
struct Capture(Arc<Mutex<Option<TaskContext<u64, Action>>>>);

impl Feature for Capture {
    type State = u64;
    type Action = Action;

    fn update(
        &mut self,
        state: &mut u64,
        action: &Action,
        context: &mut FeatureContext<'_, u64, Action>,
    ) {
        match action {
            Action::Start => {
                let captured = Arc::clone(&self.0);
                context.task(Task::run(move |context| async move {
                    *captured.lock().expect("capture lock") = Some(context);
                    std::future::pending::<()>().await;
                }));
            }
            Action::CompleteTask => context.task(Task::run(|context| async move {
                divan::black_box(context);
            })),
            Action::CompleteBatch { tasks, effects } => {
                *state += 1;

                for _ in 0..*effects {
                    context.effect(Effect::run(|sender| async move {
                        divan::black_box(sender);
                    }));
                }

                for _ in 0..*tasks {
                    context.task(Task::run(|context| async move {
                        divan::black_box(context);
                    }));
                }
            }
            Action::Combined { width, .. } => {
                macro_rules! reduce_members {
                    ($($member:expr),+ $(,)?) => {
                        Feature::update(&mut combine(($($member,)+)), state, action, context)
                    };
                }

                match *width {
                    1 => reduce_members!(CombinedLeaf),
                    3 => reduce_members!(CombinedLeaf, CombinedLeaf, CombinedLeaf),
                    12 => reduce_members!(
                        CombinedLeaf,
                        CombinedLeaf,
                        CombinedLeaf,
                        CombinedLeaf,
                        CombinedLeaf,
                        CombinedLeaf,
                        CombinedLeaf,
                        CombinedLeaf,
                        CombinedLeaf,
                        CombinedLeaf,
                        CombinedLeaf,
                        CombinedLeaf,
                    ),
                    _ => panic!("benchmark supplies only supported tuple widths"),
                }
            }
            Action::Add(value) => *state += value,
        }
    }
}

/// Records each member call and optionally declares one completed effect.
struct CombinedLeaf;

impl Reducer for CombinedLeaf {
    type State = u64;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut u64,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        *state += 1;
        if matches!(
            action,
            Action::Combined {
                effectful: true,
                ..
            }
        ) {
            Effect::run(|sender| async move {
                divan::black_box(sender);
            })
        } else {
            Effect::none()
        }
    }
}

/// Owns the actor and pending origin without spawning worker threads.
struct Fixture {
    /// Retains the root owner through ordered state extraction.
    store: Store<u64, Action>,
    /// Executes each request at its real mailbox position.
    actor: RuntimeFuture,
    /// Keeps the captured context's originating task live.
    origin: RuntimeFuture,
    /// Cancels and joins the originating causal lineage after measurement.
    task: ActionTask,
    /// Supplies the authority used by every measured request.
    context: TaskContext<u64, Action>,
    /// Owns the existing manual driver and checks that no new tasks escape.
    runtime: support::ManualRuntime,
}

impl Fixture {
    /// Starts the actor and captures its first task before measurement.
    fn new() -> Self {
        let captured = Arc::new(Mutex::new(None));
        let runtime = support::ManualRuntime::new();
        let store = Store::builder(
            INITIAL_STATE,
            Capture(Arc::clone(&captured)),
            runtime.runtime(),
        )
        .mailbox_capacity(1)
        .build()
        .expect("valid store");

        let mut actor = runtime.take_only_queued_task();
        let mut cx = Context::from_waker(Waker::noop());
        assert!(actor.as_mut().poll(&mut cx).is_pending());

        let start = store.try_send(Action::Start).expect("start fits");
        assert!(actor.as_mut().poll(&mut cx).is_pending());
        let task = {
            let mut finish = std::pin::pin!(start.finish());
            let Poll::Ready(Ok(task)) = finish.as_mut().poll(&mut cx) else {
                panic!("origin action must commit");
            };
            task
        };

        let mut origin = runtime.take_only_queued_task();
        assert!(origin.as_mut().poll(&mut cx).is_pending());
        let context = captured
            .lock()
            .expect("capture lock")
            .take()
            .expect("root context");
        assert!(!task.is_finished());

        Self {
            store,
            actor,
            origin,
            task,
            context,
            runtime,
        }
    }

    /// Polls one admitted request, its actor phase and its owned reply.
    fn complete<F: Future>(&mut self, request: F) -> F::Output {
        let mut cx = Context::from_waker(Waker::noop());
        let mut request = std::pin::pin!(request);
        assert!(request.as_mut().poll(&mut cx).is_pending());
        assert!(self.actor.as_mut().poll(&mut cx).is_pending());
        let Poll::Ready(result) = request.as_mut().poll(&mut cx) else {
            panic!("request must complete after its actor turn");
        };
        assert!(!self.task.is_finished(), "origin remains live");
        result
    }

    /// Joins the cancelled origin and checks every mutation outside timing.
    fn finish(mut self, expected: u64, completed_tasks: usize) {
        let mut cx = Context::from_waker(Waker::noop());
        self.task.cancel();
        assert!(self.actor.as_mut().poll(&mut cx).is_pending());
        assert!(self.origin.as_mut().poll(&mut cx).is_ready());
        drop(self.origin);

        assert!(self.actor.as_mut().poll(&mut cx).is_pending());
        assert!(self.task.is_finished());
        assert!(
            std::pin::pin!(self.task.finish())
                .as_mut()
                .poll(&mut cx)
                .is_ready()
        );

        let mut shutdown = std::pin::pin!(self.store.shutdown_into_state());
        assert!(shutdown.as_mut().poll(&mut cx).is_pending());
        assert!(self.actor.as_mut().poll(&mut cx).is_ready());
        let Poll::Ready(Ok(state)) = shutdown.as_mut().poll(&mut cx) else {
            panic!("ordered state extraction must complete");
        };
        assert_eq!(state, expected);

        assert_eq!(
            self.runtime.spawn_count(),
            completed_tasks + 2,
            "only actor, origin and measured completed tasks"
        );
        assert!(self.runtime.take_queued_tasks().is_empty());
    }
}

/// Measures an ordered read through the captured task context.
#[divan::bench(sample_count = 50, max_time = 0.5)]
fn root_read(bencher: divan::Bencher) {
    let mut fixture = Fixture::new();
    bencher.bench_local(|| {
        let request = fixture.context.read(|state| *state);
        let value = fixture.complete(request).expect("live root read");
        assert_eq!(value, INITIAL_STATE);
        divan::black_box(value)
    });
    fixture.finish(INITIAL_STATE, 0);
}

/// Measures an ordered mutation and its stamped receipt.
#[divan::bench(sample_count = 50, max_time = 0.5)]
fn root_update(bencher: divan::Bencher) {
    let mut fixture = Fixture::new();
    let mut expected = INITIAL_STATE;
    bencher.bench_local(|| {
        let request = fixture.context.update(|state| {
            *state += 1;
            *state
        });
        let committed = fixture.complete(request).expect("live root update");
        expected += 1;
        assert_eq!(*committed.value(), expected);
        assert_eq!(
            committed.revision(),
            expected - INITIAL_STATE + 1,
            "origin action committed first"
        );
        assert!(
            !committed.task().is_finished(),
            "receipt includes the live origin"
        );
        drop(divan::black_box(committed));
    });
    fixture.finish(expected, 0);
}

/// Measures action admission, state mutation and acknowledgement.
#[divan::bench(sample_count = 50, max_time = 0.5)]
fn root_send(bencher: divan::Bencher) {
    let mut fixture = Fixture::new();
    let mut expected = INITIAL_STATE;
    bencher.bench_local(|| {
        let request = fixture.context.send(Action::Add(1));
        let task = fixture.complete(request).expect("live root send");
        expected += 1;
        assert!(!task.is_finished(), "receipt includes the live origin");
        drop(divan::black_box(task));
    });
    fixture.finish(expected, 0);
}

/// Measures one child task from declaration through completion and retirement.
#[divan::bench(sample_count = 50, max_time = 0.5)]
fn root_task_lifecycle(bencher: divan::Bencher) {
    let mut fixture = Fixture::new();
    let mut completed = 0;
    bencher.bench_local(|| {
        let request = fixture.context.send(Action::CompleteTask);
        let task = fixture.complete(request).expect("live task declaration");
        let mut cx = Context::from_waker(Waker::noop());
        let mut work = fixture.runtime.take_only_queued_task();
        assert!(work.as_mut().poll(&mut cx).is_ready());
        drop(work);

        assert!(fixture.actor.as_mut().poll(&mut cx).is_pending());
        assert!(
            !task.is_finished(),
            "origin remains live after child completion"
        );
        completed += 1;
        drop(divan::black_box(task));
    });
    fixture.finish(INITIAL_STATE, completed);
}

/// Measures ordered registration, execution and retirement of one full batch.
fn root_batch(bencher: divan::Bencher, tasks: usize, effects: usize) {
    let mut fixture = Fixture::new();
    let mut expected = INITIAL_STATE;
    let mut completed = 0;
    bencher.bench_local(|| {
        let request = fixture
            .context
            .send(Action::CompleteBatch { tasks, effects });
        let task = fixture.complete(request).expect("live batch declaration");
        let children = fixture.runtime.take_queued_tasks();
        assert_eq!(
            children.len(),
            tasks + effects,
            "every declaration launches"
        );
        let mut cx = Context::from_waker(Waker::noop());
        for mut child in children {
            assert!(child.as_mut().poll(&mut cx).is_ready());
            drop(child);
        }
        assert!(fixture.actor.as_mut().poll(&mut cx).is_pending());
        assert!(!task.is_finished(), "origin remains live after the batch");
        expected += 1;
        completed += tasks + effects;
        drop(divan::black_box(task));
    });
    fixture.finish(expected, completed);
    println!(
        "RCA_ROOT_BATCH {{\"tasks\":{tasks},\"effects\":{effects},\"batches\":{},\"children\":{completed}}}",
        expected - INITIAL_STATE,
    );
}

#[divan::bench(args = [1, 16], sample_count = 50, max_time = 0.5)]
/// Measures full batches containing typed tasks only.
fn root_task_batch(bencher: divan::Bencher, width: usize) {
    root_batch(bencher, width, 0);
}

#[divan::bench(args = [2, 16], sample_count = 50, max_time = 0.5)]
/// Measures the added batch dispatch when no task context is needed.
fn root_effect_batch(bencher: divan::Bencher, width: usize) {
    root_batch(bencher, 0, width);
}

#[divan::bench(args = [1, 8], sample_count = 50, max_time = 0.5)]
/// Measures batches that register ordinary effects before typed tasks.
fn root_mixed_batch(bencher: divan::Bencher, width: usize) {
    root_batch(bencher, width, width);
}

/// Measures tuple reduction and exact child completion through the same actor.
fn root_combined_batch(bencher: divan::Bencher, width: usize, effectful: bool) {
    let mut fixture = Fixture::new();
    let mut batches = 0;
    let children_per_batch = if effectful { width } else { 0 };
    bencher.bench_local(|| {
        let task = fixture
            .complete(fixture.context.send(Action::Combined { width, effectful }))
            .expect("live tuple declaration");
        let children = fixture.runtime.take_queued_tasks();
        assert_eq!(children.len(), children_per_batch, "every effect launches");
        let mut cx = Context::from_waker(Waker::noop());
        for mut child in children {
            assert!(child.as_mut().poll(&mut cx).is_ready());
            drop(child);
        }
        assert!(fixture.actor.as_mut().poll(&mut cx).is_pending());
        assert!(!task.is_finished(), "origin remains live after the tuple");
        batches += 1;
        drop(divan::black_box(task));
    });
    let children = batches * children_per_batch;
    fixture.finish(INITIAL_STATE + batches as u64 * width as u64, children);
    println!(
        "RCA_COMBINED_BATCH {{\"width\":{width},\"effectful\":{effectful},\"batches\":{batches},\"children\":{children}}}",
    );
}

/// Measures every tuple member while each returns empty work.
#[divan::bench(args = [1, 3, 12], sample_count = 50, max_time = 0.5)]
fn root_combine_empty(bencher: divan::Bencher, width: usize) {
    root_combined_batch(bencher, width, false);
}

/// Measures every tuple member and the runtime effect it returns.
#[divan::bench(args = [1, 3, 12], sample_count = 50, max_time = 0.5)]
fn root_combine_effects(bencher: divan::Bencher, width: usize) {
    root_combined_batch(bencher, width, true);
}
