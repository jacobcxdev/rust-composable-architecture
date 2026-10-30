use std::cell::Cell;
use std::panic::Location;
use std::rc::Rc;
use std::thread::{self, ThreadId};
use std::time::Duration;

use futures_executor::block_on;
use rca_core::MonotonicInstant;
use rca_core::local::{ClockDriver, Feature, FeatureContext, Store, Task};
use rca_core::testing::local::TransactionGate;

use crate::local::{DeterministicRuntime, TestActionTask};

struct Owner {
    drops: Rc<Cell<usize>>,
    thread: ThreadId,
}

impl Drop for Owner {
    fn drop(&mut self) {
        assert_eq!(thread::current().id(), self.thread);
        self.drops.set(self.drops.get() + 1);
    }
}

struct Delayed {
    runtime: rca_core::local::Runtime,
    drops: Rc<Cell<usize>>,
    completed: Rc<Cell<usize>>,
}

impl Feature for Delayed {
    type State = Rc<Cell<usize>>;
    type Action = Rc<()>;

    fn update(
        &mut self,
        state: &mut Self::State,
        _: &Self::Action,
        context: &mut FeatureContext<'_, Self::State, Self::Action>,
    ) {
        state.set(state.get() + 1);
        let owner = Owner {
            drops: Rc::clone(&self.drops),
            thread: thread::current().id(),
        };
        let completed = Rc::clone(&self.completed);
        let sleep = self
            .runtime
            .sleep_until(MonotonicInstant::from_elapsed(Duration::from_secs(1)));
        context.task(Task::run(move |_| async move {
            sleep.await;
            completed.set(completed.get() + 1);
            drop(owner);
        }));
    }
}

#[test]
fn local_task_owns_causal_work_and_captures_real_gate_diagnostics() {
    for cancel in [false, true] {
        let host = DeterministicRuntime::with_step_budget(64);
        let gate = TransactionGate::new();
        let state = Rc::new(Cell::new(0));
        let drops = Rc::new(Cell::new(0));
        let completed = Rc::new(Cell::new(0));
        let store = Store::builder(
            Rc::clone(&state),
            Delayed {
                runtime: host.runtime(),
                drops: Rc::clone(&drops),
                completed: Rc::clone(&completed),
            },
            host.runtime(),
        )
        .transaction_gate(gate.clone())
        .build()
        .unwrap();
        block_on(host.run(store.ready())).unwrap().unwrap();
        assert!(gate.reserve_root());
        let causal = block_on(host.run(store.send(Rc::new(()))))
            .unwrap()
            .unwrap();
        assert!(gate.acknowledge(gate.boundary().unwrap()));
        host.run_until_stalled().unwrap();
        let origin = Location::caller();
        let task = TestActionTask::new(causal, host.clone(), origin, gate);
        let cloned = task.clone();
        drop(task);
        drop(cloned.cancel());
        drop(cloned.finish());
        assert!(!cloned.is_cancelled());
        assert!(!cloned.is_finished());
        assert!(std::ptr::eq(cloned.origin(), origin));
        assert_eq!(state.get(), 1);
        assert_eq!(drops.get(), 0);
        let error = block_on(cloned.finish()).unwrap_err();
        assert!(error.drive_error().is_none());
        assert_eq!(error.pending_sleeps().len(), 1);
        assert_eq!(error.current_time(), MonotonicInstant::ZERO);
        let snapshot = error.runtime_snapshot().expect("real actor diagnostics");
        assert_eq!(snapshot.commit_version, 1);
        assert_eq!(snapshot.live_task_ids, [0]);
        let saved_snapshot = snapshot.clone();
        assert!(std::ptr::eq(error.origin(), origin));
        let saved = error.to_string();
        if cancel {
            block_on(cloned.cancel()).unwrap();
        } else {
            host.advance(Duration::from_secs(1)).unwrap();
        }
        block_on(cloned.finish()).unwrap();
        assert!(cloned.is_finished());
        assert_eq!(cloned.is_cancelled(), cancel);
        assert_eq!(drops.get(), 1);
        assert_eq!(completed.get(), usize::from(!cancel));
        assert_eq!(error.to_string(), saved);
        assert_eq!(error.runtime_snapshot(), Some(&saved_snapshot));

        // Completion cannot hide scheduler failure: the drive precedes that check.
        let stop = Rc::new(Cell::new(false));
        let polls = Rc::new(Cell::new(0));
        let stopped = Rc::clone(&stop);
        let polled = Rc::clone(&polls);
        host.spawn_task(Box::pin(std::future::poll_fn(move |cx| {
            polled.set(polled.get() + 1);
            if stopped.get() {
                std::task::Poll::Ready(())
            } else {
                cx.waker().wake_by_ref();
                std::task::Poll::Pending
            }
        })));
        drop(cloned.finish());
        assert_eq!(polls.get(), 0, "unpolled finish must not drive");
        let drive_error = block_on(cloned.finish()).unwrap_err();
        assert!(drive_error.drive_error().is_some());
        let snapshot = drive_error
            .runtime_snapshot()
            .expect("real actor diagnostics");
        assert_eq!(snapshot.commit_version, 1);
        assert!(snapshot.live_task_ids.is_empty());
        assert!(std::ptr::eq(drive_error.origin(), origin));
        assert!(block_on(cloned.cancel()).is_err());
        stop.set(true);
        block_on(cloned.finish()).unwrap();
        block_on(host.run(store.shutdown())).unwrap().unwrap();
    }
}
