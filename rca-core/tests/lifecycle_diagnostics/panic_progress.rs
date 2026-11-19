#![cfg(feature = "test-util")]

use super::{Events, ordered, poll, support};
use rca_core::diagnostics::{CancellationCause, LifecycleKind};
use rca_core::testing::TransactionGate;
use rca_core::{
    Effect, Feature, FeatureContext, MountIdentity, OptionalMount, Store, Task, TaskAccessReason,
    TaskContext, TaskProgress, TaskSendError,
};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;

struct State {
    child: Option<()>,
    mutations: Arc<AtomicUsize>,
}
enum Action {
    Start,
    Mutate,
}
/// Chooses a pre-stamp installation event or a post-stamp cancellation event.
struct Root {
    after_stamp: bool,
    context: Arc<Mutex<Option<TaskContext<State, Action>>>>,
}
impl Feature for Root {
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
                let captured = Arc::clone(&self.context);
                context.task(Task::run(move |context| async move {
                    *captured.lock().unwrap() = Some(context);
                    std::future::pending::<()>().await;
                }));
            }
            Action::Mutate => {
                state.mutations.fetch_add(1, Ordering::SeqCst);
                if self.after_stamp {
                    context.effect(Effect::cancel("empty cancellation still applies"));
                } else {
                    state.child = Some(());
                }
            }
        }
    }
}
struct Child;
impl Feature for Child {
    type State = ();
    type Action = ();
    fn update(&mut self, (): &mut (), (): &(), _: &mut FeatureContext<'_, (), ()>) {}
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one ordered panic schedule preserves the relation between typed progress and assertion debt"
)]
fn lifecycle_callback_panic_preserves_typed_progress_and_cannot_satisfy_assertion_debt() {
    for after_stamp in [false, true] {
        let runtime = support::ManualRuntime::new();
        let gate = TransactionGate::new();
        let contexts = Arc::new(Mutex::new(None));
        let mutations = Arc::new(AtomicUsize::new(0));
        let events = Arc::new(Mutex::new(Events::default()));
        let sink = Arc::clone(&events);
        let armed = Arc::new(AtomicBool::new(false));
        let sink_armed = Arc::clone(&armed);
        let mount = OptionalMount::new(
            |state: &State| state.child.as_ref(),
            |state: &mut State| state.child.as_mut(),
            |state: &State| {
                state.child.map(|()| MountIdentity {
                    key: 0_u8,
                    replacement: 0,
                    configuration: 0,
                })
            },
            |()| Child,
        );
        let store = Store::builder(
            State {
                child: None,
                mutations: Arc::clone(&mutations),
            },
            Root {
                after_stamp,
                context: Arc::clone(&contexts),
            },
            runtime.runtime(),
        )
        .mount(mount)
        .transaction_gate(gate.clone())
        .with_lifecycle_diagnostics(move |event| {
            sink.lock().unwrap().record(event);
            let selected = if after_stamp {
                matches!(
                    event.kind(),
                    LifecycleKind::CancellationRequested {
                        cause: CancellationCause::Explicit,
                        ..
                    }
                )
            } else {
                matches!(event.kind(), LifecycleKind::Installed { lifetime, .. } if lifetime != 0)
            };
            assert!(
                !(sink_armed.load(Ordering::SeqCst) && selected),
                "lifecycle callback panic"
            );
        })
        .build()
        .unwrap();
        let mut actor = runtime.take_only_queued_task();
        assert!(poll(actor.as_mut()).is_pending());
        assert!(gate.release_pending());
        assert!(poll(actor.as_mut()).is_pending());
        assert!(gate.acknowledge(gate.boundary().unwrap()));
        assert!(gate.reserve_root());
        let initial = ordered(&mut actor, store.send(Action::Start)).unwrap();
        assert!(gate.acknowledge(gate.boundary().unwrap()));
        let mut body = runtime.take_only_queued_task();
        assert!(poll(body.as_mut()).is_pending());
        let context = contexts.lock().unwrap().take().unwrap();
        events.lock().unwrap().take();
        armed.store(true, Ordering::SeqCst);

        let mut request = Box::pin(context.send(Action::Mutate));
        assert!(poll(request.as_mut()).is_pending());
        assert!(poll(actor.as_mut()).is_pending());
        let boundary = gate.boundary().unwrap();
        assert!(gate.has_pending());
        let mut assertion = Box::pin(gate.release_pending_with_receipt().unwrap().finish());
        let panic = catch_unwind(AssertUnwindSafe(|| poll(actor.as_mut()))).unwrap_err();
        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"lifecycle callback panic")
        );
        let Poll::Ready(Err(TaskSendError::Failed { error })) = poll(request.as_mut()) else {
            panic!("entered typed send must publish a failure");
        };
        assert_eq!(error.reason(), TaskAccessReason::ActorFailed);
        assert_eq!(
            error.progress(),
            if after_stamp {
                TaskProgress::Committed { revision: 3 }
            } else {
                TaskProgress::MayHaveRun
            }
        );
        assert!(matches!(poll(assertion.as_mut()), Poll::Ready(Err(_))));
        assert!(
            !gate.acknowledge(boundary),
            "failure cannot acknowledge a successful boundary"
        );
        assert_eq!(
            mutations.load(Ordering::SeqCst),
            1,
            "user mutation is not rolled back"
        );
        let (observed, count) = events.lock().unwrap().take();
        assert_eq!(
            count, 1,
            "unwind must not fabricate terminal lifecycle facts"
        );
        assert_eq!(
            observed[0].unwrap().last_committed_version(),
            if after_stamp { 3 } else { 2 }
        );
        drop(actor);
        drop(body);
        drop(initial);
        drop(store);
        assert_eq!(events.lock().unwrap().length, 0);
    }
}

/// Panics from exactly one selected actor-owned destructor after it is armed.
struct DropBomb {
    armed: Arc<AtomicBool>,
    selected: bool,
    drops: Arc<AtomicUsize>,
}
impl Drop for DropBomb {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        assert!(
            !(self.selected && self.armed.load(Ordering::SeqCst)),
            "actor-owned destructor panic"
        );
    }
}
struct DropFeature(DropBomb);
impl Feature for DropFeature {
    type State = DropBomb;
    type Action = ();
    fn update(&mut self, _: &mut DropBomb, (): &(), _: &mut FeatureContext<'_, DropBomb, ()>) {
        let _ = &self.0;
    }
}
/// Counts disposal of the lifecycle sink capture after actor failure.
struct SinkDrop(Arc<AtomicUsize>);
impl Drop for SinkDrop {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn actor_owned_state_or_feature_drop_panic_suppresses_terminal_facts_and_success() {
    for state_panics in [false, true] {
        let runtime = support::ManualRuntime::new();
        let armed = Arc::new(AtomicBool::new(false));
        let state_drops = Arc::new(AtomicUsize::new(0));
        let feature_drops = Arc::new(AtomicUsize::new(0));
        let sink_drops = Arc::new(AtomicUsize::new(0));
        let capture = SinkDrop(Arc::clone(&sink_drops));
        let events = Arc::new(Mutex::new(Events::default()));
        let sink_events = Arc::clone(&events);
        let store = Store::builder(
            DropBomb {
                armed: Arc::clone(&armed),
                selected: state_panics,
                drops: Arc::clone(&state_drops),
            },
            DropFeature(DropBomb {
                armed: Arc::clone(&armed),
                selected: !state_panics,
                drops: Arc::clone(&feature_drops),
            }),
            runtime.runtime(),
        )
        .with_lifecycle_diagnostics(move |event| {
            let _ = &capture;
            sink_events.lock().unwrap().record(event);
        })
        .build()
        .unwrap();
        let mut actor = runtime.take_only_queued_task();
        assert!(poll(actor.as_mut()).is_pending());
        assert_eq!(events.lock().unwrap().take().1, 1);
        armed.store(true, Ordering::SeqCst);
        let mut shutdown = Box::pin(store.shutdown());
        assert!(poll(shutdown.as_mut()).is_pending());
        let panic = catch_unwind(AssertUnwindSafe(|| poll(actor.as_mut()))).unwrap_err();
        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"actor-owned destructor panic")
        );
        assert!(matches!(poll(shutdown.as_mut()), Poll::Ready(Err(_))));
        assert!(matches!(
            poll(Box::pin(store.shutdown()).as_mut()),
            Poll::Ready(Err(_))
        ));
        assert_eq!(state_drops.load(Ordering::SeqCst), 1);
        assert_eq!(feature_drops.load(Ordering::SeqCst), 1);
        assert_eq!(sink_drops.load(Ordering::SeqCst), 1);
        assert_eq!(events.lock().unwrap().length, 0);
        drop(actor);
        assert_eq!(events.lock().unwrap().length, 0);
    }
}
