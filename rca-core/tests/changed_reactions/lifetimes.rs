use super::Driver;
use rca_core::ReactionChange;
use rca_core::{
    Feature, FeatureContext, InitialPolicy, MountIdentity, OptionalMount, RootMount, Runtime,
    Store, Task, TaskAccessReason, TaskProgress,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::task::{Context, Poll, Waker};

struct Root;
struct Child;
impl Feature for Root {
    type State = Option<i32>;
    type Action = ();
    fn reduce(
        &mut self,
        _: &mut Option<i32>,
        (): &(),
        _: &mut FeatureContext<'_, Option<i32>, ()>,
    ) {
    }
}
impl Feature for Child {
    type State = i32;
    type Action = ();
    fn reduce(&mut self, state: &mut i32, (): &(), _: &mut FeatureContext<'_, i32, ()>) {
        *state += 1;
    }
}
type Owner = Arc<Mutex<Option<Store<Option<i32>, ()>>>>;
struct ClosingValue {
    value: i32,
    owner: Owner,
}
impl PartialEq for ClosingValue {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}
impl Drop for ClosingValue {
    fn drop(&mut self) {
        drop(self.owner.lock().unwrap().take());
    }
}
struct DropCount(Arc<AtomicUsize>);
impl Drop for DropCount {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn closing_initial_factories_drop_returned_tasks_without_creating_fresh_authority() {
    for changed_initial in [false, true] {
        let driver = Driver::default();
        let owner = Owner::default();
        let callback_owner = std::sync::Arc::clone(&owner);
        let bodies = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        let task_bodies = std::sync::Arc::clone(&bodies);
        let task_drops = std::sync::Arc::clone(&drops);
        let mount = OptionalMount::new(
            |state: &Option<i32>| state.as_ref(),
            |state: &mut Option<i32>| state.as_mut(),
            |state| {
                state.map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            |_| Child,
        );
        let factory = move |_: ReactionChange<i32>, _: rca_core::ReactionContext<'_, i32, ()>| {
            drop(callback_owner.lock().unwrap().take());
            let bodies = std::sync::Arc::clone(&task_bodies);
            let guard = DropCount(std::sync::Arc::clone(&task_drops));
            Task::run(move |_| async move {
                bodies.fetch_add(1, Ordering::SeqCst);
                drop(guard);
            })
        };
        let mount = if changed_initial {
            mount.on_change(|value| *value, InitialPolicy::Run, factory)
        } else {
            mount.on_initial(|value| *value, factory)
        };
        let store = Store::feature_builder(Some(4), Root, Runtime::new(driver.clone()))
            .mount(mount)
            .build()
            .unwrap();
        *owner.lock().unwrap() = Some(store);
        driver.turn();
        assert!(owner.lock().unwrap().is_none());
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        driver.turn();
        assert_eq!(bodies.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn unmatched_old_value_drops_before_final_liveness_and_discards_prior_factory_work() {
    let driver = Driver::default();
    let owner = Owner::default();
    let capture_owner = std::sync::Arc::clone(&owner);
    let factories = Arc::new(AtomicUsize::new(0));
    let bodies = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let factory_count = std::sync::Arc::clone(&factories);
    let task_bodies = std::sync::Arc::clone(&bodies);
    let task_drops = std::sync::Arc::clone(&drops);
    let child = OptionalMount::new(
        |state: &Option<i32>| state.as_ref(),
        |state: &mut Option<i32>| state.as_mut(),
        |state| {
            state.map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        |_| Child,
    )
    .on_change(
        move |value| ClosingValue {
            value: *value,
            owner: std::sync::Arc::clone(&capture_owner),
        },
        InitialPolicy::Skip,
        |_, _| panic!("removed child cannot receive Changed"),
    );
    let (send, receive) = futures_channel::oneshot::channel();
    let mut send = Some(send);
    let store = RootMount::new(Root)
        .on_initial(
            |_| (),
            move |_, _| {
                let send = send.take().unwrap();
                Task::run(move |context| async move {
                    let _ = send.send(context);
                    std::future::pending::<()>().await;
                })
            },
        )
        .on_change(Option::is_some, InitialPolicy::Skip, move |_, _| {
            factory_count.fetch_add(1, Ordering::SeqCst);
            let guard = DropCount(std::sync::Arc::clone(&task_drops));
            let bodies = std::sync::Arc::clone(&task_bodies);
            Task::run(move |_| async move {
                bodies.fetch_add(1, Ordering::SeqCst);
                drop(guard);
            })
        })
        .builder(Some(4), Runtime::new(driver.clone()))
        .mount(child)
        .build()
        .unwrap();
    driver.finish(store.ready()).unwrap();
    let context = driver.finish(receive).unwrap();
    *owner.lock().unwrap() = Some(store);
    let committed = driver
        .finish(context.update(|state| *state = None))
        .unwrap();
    assert!(committed.revision() > 0);
    assert_eq!(
        factories.load(Ordering::SeqCst),
        1,
        "ancestor factory ran before unmatched child history drained"
    );
    assert!(owner.lock().unwrap().is_none());
    assert_eq!(
        drops.load(Ordering::SeqCst),
        1,
        "late closure destroys already-returned unregistered task"
    );
    driver.turn();
    assert_eq!(bodies.load(Ordering::SeqCst), 0);
}

struct PanicValue {
    value: i32,
    phase: usize,
}
impl PartialEq for PanicValue {
    fn eq(&self, other: &Self) -> bool {
        assert_ne!(self.phase, 2, "equality panic");
        self.value == other.value
    }
}
impl Drop for PanicValue {
    fn drop(&mut self) {
        assert!(
            !(self.phase == 4 && self.value == 4),
            "old projection destructor panic"
        );
    }
}

#[test]
fn every_projection_equality_factory_and_history_drop_panic_reports_entered_progress() {
    for phase in 0..5 {
        let driver = Driver::default();
        let selections = Arc::new(AtomicUsize::new(0));
        let selected = std::sync::Arc::clone(&selections);
        let mutation_calls = Arc::new(AtomicUsize::new(0));
        let calls = std::sync::Arc::clone(&mutation_calls);
        let mount = OptionalMount::new(
            |state: &Option<i32>| state.as_ref(),
            |state: &mut Option<i32>| state.as_mut(),
            |state| {
                state.map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            |_| Child,
        )
        .on_change(
            move |value| {
                let index = selected.fetch_add(1, Ordering::SeqCst);
                assert!(
                    !((phase == 0 && index == 0) || (phase == 1 && index == 1)),
                    "projection panic"
                );
                PanicValue {
                    value: *value,
                    phase,
                }
            },
            InitialPolicy::Skip,
            move |change, _| {
                assert_ne!(phase, 3, "factory panic");
                let ReactionChange::Changed { previous, current } = change else {
                    panic!("non-initial")
                };
                drop((previous, current));
                Task::run(|_| async {})
            },
        );
        let (builder, site) = Store::feature_builder(Some(4), Root, Runtime::new(driver.clone()))
            .mount_with_site(mount);
        let store = builder.build().unwrap();
        driver.finish(store.ready()).unwrap();
        let child = driver.finish(store.acquire(&site)).unwrap().unwrap();
        let mut update = Box::pin(child.update(move |state| {
            calls.fetch_add(1, Ordering::SeqCst);
            *state = 5;
        }));
        assert!(matches!(
            update
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop())),
            Poll::Pending
        ));
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| driver.turn()));
        assert!(
            panic.is_err(),
            "phase {phase} reaches actual actor callback"
        );
        let error = driver.finish(update).unwrap_err();
        assert_eq!(
            error.reason(),
            TaskAccessReason::ActorFailed,
            "phase {phase}"
        );
        assert_eq!(error.progress(), TaskProgress::MayHaveRun, "phase {phase}");
        assert_eq!(
            mutation_calls.load(Ordering::SeqCst),
            usize::from(phase != 0)
        );
        drop(store);
        driver.turn();
    }
}

#[test]
fn pre_capture_closure_reports_store_closed_after_entry_and_returns_unconsumed_output() {
    for operation in 0..3 {
        let driver = Driver::default();
        let owner = Owner::default();
        let select_owner = std::sync::Arc::clone(&owner);
        let mutations = Arc::new(AtomicUsize::new(0));
        let callback_mutations = std::sync::Arc::clone(&mutations);
        let mount = OptionalMount::new(
            |state: &Option<i32>| state.as_ref(),
            |state: &mut Option<i32>| state.as_mut(),
            |state| {
                state.map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            |_| Child,
        )
        .on_change(
            move |value| {
                drop(select_owner.lock().unwrap().take());
                *value
            },
            InitialPolicy::Skip,
            |_, _| panic!("capture closure must stop before factory"),
        );
        let (builder, site) = Store::feature_builder(Some(4), Root, Runtime::new(driver.clone()))
            .mount_with_site(mount);
        let (builder, output) = builder.output::<Box<i32>>(&site, |handlers| {
            handlers.at_root(move |_, _, _| {
                callback_mutations.fetch_add(1, Ordering::SeqCst);
                rca_core::OutputFlow::Consumed
            });
        });
        let store = builder.build().unwrap();
        driver.finish(store.ready()).unwrap();
        let child = driver.finish(store.acquire(&site)).unwrap().unwrap();
        *owner.lock().unwrap() = Some(store);
        let error = match operation {
            0 => {
                let calls = std::sync::Arc::clone(&mutations);
                driver
                    .finish(child.update(move |_| {
                        calls.fetch_add(1, Ordering::SeqCst);
                    }))
                    .unwrap_err()
            }
            1 => match driver.finish(child.send(())) {
                Err(rca_core::TaskSendError::Failed { error }) => error,
                other => panic!("entered send must fail, not reject: {other:?}"),
            },
            _ => {
                let value = Box::new(17);
                let identity = std::ptr::from_ref(&*value);
                match driver.finish(child.output(&output, value)) {
                    Err(rca_core::OutputError::Returned { value, error }) => {
                        assert_eq!(std::ptr::from_ref(&*value), identity);
                        error
                    }
                    other => panic!("unconsumed output must be returned: {other:?}"),
                }
            }
        };
        assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
        assert_eq!(error.progress(), TaskProgress::MayHaveRun);
        assert_eq!(mutations.load(Ordering::SeqCst), 0);
        assert!(owner.lock().unwrap().is_none());
        driver.turn();
    }
}
