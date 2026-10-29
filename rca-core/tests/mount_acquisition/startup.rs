use super::*;
use rca_core::testing::{TransactionGate, TransactionKind};
use rca_core::{MountSite, Task, TaskAccessReason};

/// Records a real readiness notification without driving the actor.
struct ReadyWake(std::sync::atomic::AtomicUsize);
impl std::task::Wake for ReadyWake {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}
/// Polls readiness once and returns the counter held by its registered waker.
fn register_ready_waker<F: Future>(future: std::pin::Pin<&mut F>) -> Arc<ReadyWake> {
    let notified = Arc::new(ReadyWake(std::sync::atomic::AtomicUsize::new(0)));
    let waker = Waker::from(std::sync::Arc::clone(&notified));
    assert!(future.poll(&mut Context::from_waker(&waker)).is_pending());
    notified
}
/// Checks that the actor notified a previously pending readiness waiter.
fn assert_readiness_woken(notified: &ReadyWake) {
    assert!(notified.0.load(std::sync::atomic::Ordering::SeqCst) > 0);
}

/// Builds the gated child whose construction, factory and body have distinct traces.
#[expect(
    clippy::type_complexity,
    reason = "The fixture returns its independently typed Store and child mount site"
)]
fn mounted(
    driver: &Driver,
    gate: &TransactionGate<Root, RootAction>,
    trace: &Arc<Mutex<Vec<&'static str>>>,
) -> (
    Store<Root, RootAction>,
    MountSite<Root, Cell<usize>, ChildAction, RootAction>,
) {
    let construct = std::sync::Arc::clone(trace);
    let initial = std::sync::Arc::clone(trace);
    let mount = OptionalMount::new(
        |root: &Root| root.child.as_ref(),
        |root: &mut Root| root.child.as_mut(),
        |root: &Root| {
            root.child.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| {
            construct.lock().unwrap().push("construct");
            ChildFeature(Cell::new(0))
        },
    )
    .on_initial(Cell::get, move |_, _| {
        initial.lock().unwrap().push("factory");
        let body = std::sync::Arc::clone(&initial);
        Task::run(move |_| async move {
            body.lock().unwrap().push("body");
        })
    });
    let (builder, site) = Store::feature_builder(
        Root {
            child: Some(Cell::new(1)),
        },
        RootFeature,
        Runtime::new(driver.clone()),
    )
    .mount_with_site(mount);
    (
        builder.transaction_gate(gate.clone()).build().unwrap(),
        site,
    )
}

#[test]
fn held_structural_frontier_rejects_skip_and_publishes_ready_before_acknowledgement() {
    let driver = Driver::default();
    let gate = TransactionGate::new();
    let trace = Arc::new(Mutex::new(Vec::new()));
    let (store, site) = mounted(&driver, &gate, &trace);
    let mut pre_ready = Box::pin(store.acquire(&site));
    assert!(
        matches!(poll(pre_ready.as_mut()), Poll::Ready(Err(error)) if error.reason() == TaskAccessReason::NotReady)
    );
    driver.turn();
    assert_eq!(gate.pending_kind(), Some(TransactionKind::Structural));
    let boundary = gate.boundary().unwrap();
    assert!(!gate.skip_pending());
    assert_eq!(gate.boundary(), Some(boundary));
    assert!(trace.lock().unwrap().is_empty());
    assert!(
        gate.inspect_pending(|_| panic!("Structural must not invoke action matcher"))
            .is_none()
    );
    let mut ready = Box::pin(store.ready());
    let notified = register_ready_waker(ready.as_mut());
    let receipt = gate.release_pending_with_receipt().unwrap();
    driver.turn();
    assert_eq!(*trace.lock().unwrap(), ["construct", "factory"]);
    assert_readiness_woken(&notified);
    assert!(matches!(poll(ready.as_mut()), Poll::Ready(Ok(()))));
    let mut acquisition = Box::pin(store.acquire(&site));
    assert!(poll(acquisition.as_mut()).is_pending());
    assert_eq!(gate.boundary(), Some(boundary));
    let mut receipt = Box::pin(receipt.finish());
    assert!(matches!(poll(receipt.as_mut()), Poll::Ready(Ok(_))));
    driver.turn();
    assert_eq!(*trace.lock().unwrap(), ["construct", "factory", "body"]);
    assert!(poll(acquisition.as_mut()).is_pending());
    assert!(gate.acknowledge(boundary));
    driver.turn();
    assert_eq!(*trace.lock().unwrap(), ["construct", "factory", "body"]);
    assert!(matches!(
        poll(acquisition.as_mut()),
        Poll::Ready(Ok(Some(_)))
    ));
    driver.turn();
    assert_eq!(*trace.lock().unwrap(), ["construct", "factory", "body"]);
}

#[test]
fn shutdown_at_admitted_head_before_first_poll_runs_no_mount_callbacks() {
    let driver = Driver::default();
    let gate = TransactionGate::new();
    let trace = Arc::new(Mutex::new(Vec::new()));
    let (store, _) = mounted(&driver, &gate, &trace);
    let mut ready = Box::pin(store.ready());
    let notified = register_ready_waker(ready.as_mut());
    let mut shutdown = Box::pin(store.shutdown());
    assert!(poll(shutdown.as_mut()).is_pending());
    driver.turn();
    assert_readiness_woken(&notified);
    assert!(
        matches!(poll(ready.as_mut()), Poll::Ready(Err(error)) if error.reason() == TaskAccessReason::StoreClosed)
    );
    assert!(matches!(poll(shutdown.as_mut()), Poll::Ready(Ok(()))));
    assert!(trace.lock().unwrap().is_empty());
    assert!(gate.boundary().is_none());
}

#[test]
fn shutdown_at_empty_held_startup_head_progresses_without_release() {
    let driver = Driver::default();
    let gate = TransactionGate::new();
    let trace = Arc::new(Mutex::new(Vec::new()));
    let (store, _) = mounted(&driver, &gate, &trace);
    driver.turn();
    assert_eq!(gate.pending_kind(), Some(TransactionKind::Structural));
    let boundary = gate.pending_boundary().unwrap();
    let mut shutdown = Box::pin(store.shutdown());
    assert!(poll(shutdown.as_mut()).is_pending());
    driver.turn();
    assert!(matches!(poll(shutdown.as_mut()), Poll::Ready(Ok(()))));
    assert!(trace.lock().unwrap().is_empty());
    assert!(!gate.has_pending());
    assert_eq!(
        gate.match_terminal(&boundary, |error| {
            error.reason() == TaskAccessReason::StoreClosed
                && error.progress() == rca_core::TaskProgress::NotStarted
        }),
        Some(true)
    );
    assert!(gate.acknowledge_terminal(&boundary));
    assert!(gate.boundary().is_none());
}

#[test]
fn ordinary_input_before_shutdown_retains_fifo_through_structural_gate() {
    let driver = Driver::default();
    let gate = TransactionGate::new();
    let trace = Arc::new(Mutex::new(Vec::new()));
    let (store, _) = mounted(&driver, &gate, &trace);
    let mut action = Box::pin(store.send(RootAction::Noop));
    let mut shutdown = Box::pin(store.shutdown());
    assert!(poll(action.as_mut()).is_pending());
    assert!(poll(shutdown.as_mut()).is_pending());
    driver.turn();
    assert_eq!(gate.pending_kind(), Some(TransactionKind::Structural));
    assert!(poll(shutdown.as_mut()).is_pending());
    assert!(trace.lock().unwrap().is_empty());
    assert!(gate.release_pending());
    driver.turn();
    assert!(gate.acknowledge(gate.boundary().unwrap()));
    driver.turn();
    assert_eq!(gate.pending_kind(), Some(TransactionKind::Action));
    assert!(poll(shutdown.as_mut()).is_pending());
    assert!(gate.release_pending());
    driver.turn();
    assert!(matches!(poll(action.as_mut()), Poll::Ready(Ok(_))));
    assert!(poll(shutdown.as_mut()).is_pending());
    assert!(gate.acknowledge(gate.boundary().unwrap()));
    driver.turn();
    assert!(matches!(poll(shutdown.as_mut()), Poll::Ready(Ok(()))));
}

#[test]
fn startup_query_close_rejects_released_installation_and_buffered_input() {
    let driver = Driver::default();
    let gate = TransactionGate::new();
    let trace = Arc::new(Mutex::new(Vec::new()));
    let (store, _) = mounted(&driver, &gate, &trace);
    let mut ready = Box::pin(store.ready());
    let notified = register_ready_waker(ready.as_mut());

    // Installation first holds an empty startup head. The later ordinary input
    // must remain buffered behind that Structural frontier, without callbacks.
    driver.turn();
    assert_eq!(gate.pending_kind(), Some(TransactionKind::Structural));
    let boundary_id = gate.boundary().unwrap();
    let boundary = gate.pending_boundary().unwrap();
    let action = store
        .try_send(RootAction::Noop)
        .unwrap_or_else(|_| panic!("ordinary input must enter the live mailbox"));
    let mut action = Box::pin(action.finish());
    driver.turn();
    assert!(poll(action.as_mut()).is_pending());
    assert_eq!(gate.boundary(), Some(boundary_id));
    assert!(trace.lock().unwrap().is_empty());

    // State inspection precedes the selected mutation. Closing the sole owner
    // here must reject installation even though its release is already queued.
    let mut query = Box::pin(
        gate.assert_state(move |root| {
            assert_eq!(root.child.as_ref().unwrap().get(), 1);
            drop(store);
        })
        .finish(),
    );
    assert!(poll(query.as_mut()).is_pending());
    let mut release = Box::pin(gate.release_pending_with_receipt().unwrap().finish());
    assert!(poll(release.as_mut()).is_pending());
    driver.turn();

    assert!(matches!(poll(query.as_mut()), Poll::Ready(Ok(()))));
    assert!(matches!(poll(release.as_mut()), Poll::Ready(Err(_))));
    assert!(matches!(poll(action.as_mut()), Poll::Ready(Err(_))));
    assert_readiness_woken(&notified);
    assert!(
        matches!(poll(ready.as_mut()), Poll::Ready(Err(error)) if error.reason() == TaskAccessReason::StoreClosed)
    );
    assert!(trace.lock().unwrap().is_empty());
    assert!(!gate.has_pending());
    assert_eq!(gate.boundary(), Some(boundary_id));
    assert_eq!(
        gate.match_terminal(&boundary, |error| {
            error.reason() == TaskAccessReason::StoreClosed
                && error.progress() == rca_core::TaskProgress::NotStarted
        }),
        Some(true)
    );
    assert!(driver.0.lock().unwrap().is_empty());
    assert!(gate.acknowledge_terminal(&boundary));
    assert!(gate.boundary().is_none());
}

#[test]
fn last_owner_abort_closes_held_structural_gate_without_callbacks() {
    let driver = Driver::default();
    let gate = TransactionGate::new();
    let trace = Arc::new(Mutex::new(Vec::new()));
    let (store, _) = mounted(&driver, &gate, &trace);
    let mut ready = Box::pin(store.ready());
    let notified = register_ready_waker(ready.as_mut());
    driver.turn();
    let boundary = gate.pending_boundary().unwrap();
    drop(store);
    driver.turn();
    assert_readiness_woken(&notified);
    assert!(
        matches!(poll(ready.as_mut()), Poll::Ready(Err(error)) if error.reason() == TaskAccessReason::StoreClosed)
    );
    assert!(!gate.has_pending());
    assert_eq!(
        gate.match_terminal(&boundary, |error| {
            error.reason() == TaskAccessReason::StoreClosed
                && error.progress() == rca_core::TaskProgress::NotStarted
        }),
        Some(true)
    );
    assert!(gate.acknowledge_terminal(&boundary));
    assert!(gate.boundary().is_none());
    assert!(trace.lock().unwrap().is_empty());
}

// A live close can occur inside entered user code even though the actor owns
// State exclusively. Later callback entries must observe it without rollback.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One callback-close sequence proves later user entries stop after the last owner drops"
)]
fn last_owner_dropped_by_mount_callback_stops_later_user_entries() {
    struct Value;
    impl rca_core::DependencyKey for Value {
        type Value = usize;
        fn live_value() -> usize {
            19
        }
    }
    for close_at in ["identity", "construct", "select", "factory"] {
        let driver = Driver::default();
        let owner = Arc::new(Mutex::new(None::<Store<Root, RootAction>>));
        let trace = Arc::new(Mutex::new(Vec::new()));
        let descriptor = |index| {
            let enter = |label| {
                let owner = std::sync::Arc::clone(&owner);
                let trace = std::sync::Arc::clone(&trace);
                move || {
                    trace.lock().unwrap().push((index, label));
                    if index == 0 && label == close_at {
                        drop(owner.lock().unwrap().take());
                    }
                }
            };
            let identity = enter("identity");
            let construct = enter("construct");
            let select = enter("select");
            let factory = enter("factory");
            let dependency = enter("dependency");
            let body = enter("body");
            OptionalMount::new(
                |root: &Root| root.child.as_ref(),
                |root: &mut Root| root.child.as_mut(),
                move |root: &Root| {
                    identity();
                    root.child.as_ref().map(|_| MountIdentity {
                        key: (),
                        replacement: 0,
                        configuration: 0,
                    })
                },
                move |_| {
                    construct();
                    ChildFeature(Cell::new(0))
                },
            )
            .on_initial(
                move |state| {
                    select();
                    state.get()
                },
                move |_, mut context| {
                    factory();
                    assert_eq!(*context.dependency::<Value>(), 19);
                    dependency();
                    let body = body.clone();
                    Task::run(move |_| async move {
                        body();
                    })
                },
            )
        };
        let store = Store::feature_builder(
            Root {
                child: Some(Cell::new(1)),
            },
            RootFeature,
            Runtime::new(driver.clone()),
        )
        .mount(descriptor(0))
        .mount(descriptor(1))
        .build()
        .unwrap();
        let mut ready = Box::pin(store.ready());
        *owner.lock().unwrap() = Some(store);
        driver.turn();
        driver.turn();
        let expected = match close_at {
            "identity" => vec![(0, "identity")],
            "construct" => vec![(0, "identity"), (0, "construct")],
            "select" => vec![
                (0, "identity"),
                (0, "construct"),
                (1, "identity"),
                (1, "construct"),
                (0, "select"),
            ],
            "factory" => vec![
                (0, "identity"),
                (0, "construct"),
                (1, "identity"),
                (1, "construct"),
                (0, "select"),
                (0, "factory"),
                (0, "dependency"),
            ],
            _ => unreachable!(),
        };
        assert_eq!(*trace.lock().unwrap(), expected, "close in {close_at}");
        assert!(
            matches!(poll(ready.as_mut()), Poll::Ready(Err(error)) if error.reason() == TaskAccessReason::StoreClosed)
        );
    }
}

#[test]
fn identity_clone_closure_is_rechecked_before_child_path() {
    use std::hash::{Hash, Hasher};
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct State(Option<Arc<AtomicUsize>>);
    struct Root;
    impl Feature for Root {
        type State = State;
        type Action = ();
        fn reduce(&mut self, _: &mut State, (): &(), _: &mut FeatureContext<'_, State, ()>) {}
    }
    struct Child;
    impl Feature for Child {
        type State = Arc<AtomicUsize>;
        type Action = u8;
        fn reduce(
            &mut self,
            _: &mut Self::State,
            _: &u8,
            _: &mut FeatureContext<'_, Self::State, u8>,
        ) {
        }
    }
    struct Key(Arc<Mutex<Option<Store<State, ()>>>>);
    impl Clone for Key {
        fn clone(&self) -> Self {
            drop(self.0.lock().unwrap().take());
            Self(std::sync::Arc::clone(&self.0))
        }
    }
    impl PartialEq for Key {
        fn eq(&self, _: &Self) -> bool {
            true
        }
    }
    impl Eq for Key {}
    impl Hash for Key {
        fn hash<H: Hasher>(&self, state: &mut H) {
            0_u8.hash(state);
        }
    }
    let driver = Driver::default();
    let owner = Arc::new(Mutex::new(None));
    let selected = Arc::new(AtomicUsize::new(0));
    let closing = std::sync::Arc::clone(&owner);
    let mount = OptionalMount::new(
        |state: &State| {
            state.0.as_ref().unwrap().fetch_add(1, Ordering::SeqCst);
            state.0.as_ref()
        },
        |state: &mut State| state.0.as_mut(),
        move |_| {
            Some(MountIdentity {
                key: Key(std::sync::Arc::clone(&closing)),
                replacement: 0,
                configuration: 0,
            })
        },
        |_| -> Child { panic!("closed identity clone cannot enter constructor") },
    );
    let store = Store::feature_builder(
        State(Some(std::sync::Arc::clone(&selected))),
        Root,
        Runtime::new(driver.clone()),
    )
    .mount(mount)
    .build()
    .unwrap();
    let mut ready = Box::pin(store.ready());
    *owner.lock().unwrap() = Some(store);
    driver.turn();
    assert_eq!(selected.load(Ordering::SeqCst), 0);
    assert!(
        matches!(poll(ready.as_mut()), Poll::Ready(Err(error)) if error.reason() == TaskAccessReason::StoreClosed)
    );
}

#[test]
fn later_root_factory_closure_discards_earlier_child_initial_work() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct WorkDrop(Arc<AtomicUsize>);
    impl Drop for WorkDrop {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let driver = Driver::default();
    let owner = Arc::new(Mutex::new(None::<Store<Root, RootAction>>));
    let drops = Arc::new(AtomicUsize::new(0));
    let child_drops = std::sync::Arc::clone(&drops);
    let mount = OptionalMount::new(
        |root: &Root| root.child.as_ref(),
        |root: &mut Root| root.child.as_mut(),
        |_| {
            Some(MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        |_| ChildFeature(Cell::new(0)),
    )
    .on_initial(Cell::get, move |_, _| {
        let guard = WorkDrop(std::sync::Arc::clone(&child_drops));
        Task::run(move |_| async move {
            let _guard = guard;
            panic!("earlier Initial body must remain unpolled");
        })
    });
    let closing = std::sync::Arc::clone(&owner);
    let root_drops = std::sync::Arc::clone(&drops);
    let store = rca_core::RootMount::new(RootFeature)
        .on_initial(
            |_| (),
            move |_, _| {
                drop(closing.lock().unwrap().take());
                let guard = WorkDrop(std::sync::Arc::clone(&root_drops));
                Task::run(move |_| async move {
                    let _guard = guard;
                    panic!("closing Initial body must remain unpolled");
                })
            },
        )
        .builder(
            Root {
                child: Some(Cell::new(1)),
            },
            Runtime::new(driver.clone()),
        )
        .mount(mount)
        .build()
        .unwrap();
    let mut ready = Box::pin(store.ready());
    *owner.lock().unwrap() = Some(store);
    driver.turn();
    driver.turn();
    assert_eq!(drops.load(Ordering::SeqCst), 2);
    assert!(
        matches!(poll(ready.as_mut()), Poll::Ready(Err(error)) if error.reason() == TaskAccessReason::StoreClosed)
    );
}
