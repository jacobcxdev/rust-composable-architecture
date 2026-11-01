//! Measures successful root task access through a manually polled actor.
//!
//! Each sample includes request creation, admission, actor work and the reply.
//! Setup, origin cancellation/join and exact final-state checks are outside
//! timing. The originating task stays pending throughout all samples, so every
//! request uses real, live causal authority. This measures neither contention
//! nor an executor's scheduling cost.
//!
//! For paired comparisons, pass `--sample-size 256` to both binaries. A fixed
//! batch size limits timer rounding and keeps automatic batch choices out of
//! the comparison.

#[path = "../tests/support/mod.rs"]
#[expect(
    dead_code,
    reason = "this benchmark only manually polls captured tasks"
)]
mod support;

use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use rca_core::{ActionTask, Feature, FeatureContext, RuntimeFuture, Store, Task, TaskContext};

/// Distinguishes an actual state read from a default-valued reply.
const INITIAL_STATE: u64 = 7;

fn main() {
    divan::main();
}

/// Starts the origin task or commits one increment.
#[derive(Debug)]
enum Action {
    Start,
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
            Action::Add(value) => *state += value,
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
    fn finish(mut self, expected: u64) {
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

        assert_eq!(self.runtime.spawn_count(), 2, "only actor and origin");
        assert!(self.runtime.take_queued_tasks().is_empty());
    }
}

#[divan::bench(sample_count = 50, max_time = 0.5)]
fn root_read(bencher: divan::Bencher) {
    let mut fixture = Fixture::new();
    bencher.bench_local(|| {
        let request = fixture.context.read(|state| *state);
        let value = fixture.complete(request).expect("live root read");
        assert_eq!(value, INITIAL_STATE);
        divan::black_box(value)
    });
    fixture.finish(INITIAL_STATE);
}

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
    fixture.finish(expected);
}

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
    fixture.finish(expected);
}
