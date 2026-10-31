//! Callback re-entry is queued, and terminal failures never return partial values.
use rca_core::{
    Feature, FeatureContext, Fold, FoldKey, MountIdentity, OptionalMount, Runtime, Store,
    TaskAccessError, TaskAccessReason, TaskProgress,
};
use std::cell::{Cell, RefCell};
use std::ops::ControlFlow;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
#[path = "fold/support.rs"]
mod support;
use support::{Driver, poll};

/// Carries a non-`Sync` value through the Shared fold without cloning it.
struct Owned(Cell<usize>);

/// Adds contributions without introducing a terminal condition itself.
struct Sum;
impl FoldKey for Sum {
    type Value = Owned;
    fn identity() -> Owned {
        Owned(Cell::new(0))
    }
    fn reduce(accumulator: Owned, next: Owned) -> ControlFlow<Owned, Owned> {
        accumulator.0.set(accumulator.0.get() + next.0.get());
        ControlFlow::Continue(accumulator)
    }
}
/// Holds the optional child used by callback and shutdown schedules.
struct State(Option<usize>);
/// Leaves the root unchanged while tests exercise fold callbacks.
struct Root;
impl Feature for Root {
    type State = State;
    type Action = ();
    fn update(&mut self, _: &mut State, (): &(), _: &mut FeatureContext<'_, State, ()>) {}
}
/// Supplies a mounted child without unrelated reducer behaviour.
struct Leaf;
impl Feature for Leaf {
    type State = usize;
    type Action = ();
    fn update(&mut self, _: &mut usize, (): &(), _: &mut FeatureContext<'_, usize, ()>) {}
}
/// Gives the optional child one fixed identity throughout each schedule.
fn mount() -> OptionalMount<State, Leaf, ()> {
    OptionalMount::new(
        |root: &State| root.0.as_ref(),
        |root: &mut State| root.0.as_mut(),
        |root: &State| {
            root.0.map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        |_| Leaf,
    )
}

#[test]
fn callback_first_poll_queues_a_later_read_without_recursive_entry() {
    type Pending = Pin<Box<dyn Future<Output = Result<Owned, TaskAccessError>> + Send>>;
    let driver = Driver::default();
    let fold = Fold::<State, Sum>::new();
    let declared = fold.clone();
    let owner = Arc::new(Mutex::new(None::<Store<State, ()>>));
    let callback_owner = std::sync::Arc::clone(&owner);
    let later = Arc::new(Mutex::new(None::<Pending>));
    let callback_later = std::sync::Arc::clone(&later);
    let calls = Arc::new(AtomicUsize::new(0));
    let callback_calls = std::sync::Arc::clone(&calls);
    let (builder, _) = Store::builder(State(Some(3)), Root, Runtime::new(driver.clone()))
        .mount_with_children(mount(), move |_, children| {
            let owner = std::sync::Arc::clone(&callback_owner);
            let later = std::sync::Arc::clone(&callback_later);
            let calls = std::sync::Arc::clone(&callback_calls);
            let fold = declared.clone();
            let entered = Cell::new(false);
            children.contribute(&declared, move |value| {
                assert!(!entered.replace(true), "callback entry is never recursive");
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    let store = owner.lock().unwrap().as_ref().unwrap().clone();
                    let mut nested = Box::pin(store.fold(&fold));
                    assert!(
                        poll(nested.as_mut()).is_pending(),
                        "nested read cannot run inline"
                    );
                    *later.lock().unwrap() = Some(nested);
                }
                entered.set(false);
                Owned(Cell::new(*value))
            });
        });
    let store = builder.build().unwrap();
    *owner.lock().unwrap() = Some(store.clone());
    driver.finish(store.ready()).unwrap();
    assert_eq!(driver.finish(store.fold(&fold)).unwrap().0.get(), 3);
    let nested = later.lock().unwrap().take().unwrap();
    assert_eq!(driver.finish(nested).unwrap().0.get(), 3);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    drop(owner.lock().unwrap().take());
    drop(store);
    driver.turn();
}

/// Counts destruction of a callback capture independently of actor replies.
struct Dropped(Arc<AtomicUsize>);
impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
/// Fails before the first contribution can be evaluated.
struct IdentityPanics;
impl FoldKey for IdentityPanics {
    type Value = Owned;
    fn identity() -> Owned {
        panic!("identity sentinel")
    }
    fn reduce(accumulator: Owned, _: Owned) -> ControlFlow<Owned, Owned> {
        ControlFlow::Continue(accumulator)
    }
}
/// Fails after a contributed value reaches reduction.
struct ReducePanics;
impl FoldKey for ReducePanics {
    type Value = Owned;
    fn identity() -> Owned {
        Owned(Cell::new(0))
    }
    fn reduce(_: Owned, _: Owned) -> ControlFlow<Owned, Owned> {
        panic!("reduce sentinel")
    }
}
/// Checks actor failure and capture cleanup at the selected callback stage.
fn panic_case<K: FoldKey<Value = Owned>>(kind: u8, expected_panic: &str) {
    let driver = Driver::default();
    let fold = Fold::<State, K>::new();
    let declared = fold.clone();
    let drops = Arc::new(AtomicUsize::new(0));
    let declared_drops = std::sync::Arc::clone(&drops);
    let (builder, _) = Store::builder(State(Some(3)), Root, Runtime::new(driver.clone()))
        .mount_with_children(mount(), move |_, children| {
            let capture = Dropped(std::sync::Arc::clone(&declared_drops));
            children.contribute_transformed(
                &declared,
                move |value| {
                    let _keep = &capture;
                    assert_ne!(kind, 1, "contribution sentinel");
                    Owned(Cell::new(*value))
                },
                move |value| {
                    assert_ne!(kind, 2, "transform sentinel");
                    value
                },
            );
        });
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    let mut read = Box::pin(store.fold(&fold));
    assert!(poll(read.as_mut()).is_pending());
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| driver.turn()))
        .expect_err("the selected fold callback must panic");
    let message = panic
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| panic.downcast_ref::<String>().map(String::as_str));
    assert_eq!(
        message.and_then(|message| message.lines().next()),
        Some(expected_panic)
    );
    let Err(error) = driver.finish(read) else {
        panic!("failed actor reply")
    };
    assert_eq!(error.reason(), TaskAccessReason::ActorFailed);
    assert_eq!(error.progress(), TaskProgress::MayHaveRun);
    assert_eq!(
        drops.load(Ordering::SeqCst),
        1,
        "lease unwinds before actor payload drain"
    );
    let Err(error) = driver.finish(store.fold(&fold)) else {
        panic!("failed actor rejects new read")
    };
    assert_eq!(error.reason(), TaskAccessReason::ActorFailed);
    assert_eq!(error.progress(), TaskProgress::NotStarted);
}
#[test]
fn identity_contribution_reduce_and_transform_panics_publish_conservative_progress() {
    panic_case::<IdentityPanics>(0, "identity sentinel");
    panic_case::<Sum>(1, "assertion `left != right` failed: contribution sentinel");
    panic_case::<ReducePanics>(0, "reduce sentinel");
    panic_case::<Sum>(2, "assertion `left != right` failed: transform sentinel");
}

#[test]
fn closure_inside_contribution_returns_error_instead_of_a_partial_success() {
    let driver = Driver::default();
    let fold = Fold::<State, Sum>::new();
    let declared = fold.clone();
    let owner = Arc::new(Mutex::new(None::<Store<State, ()>>));
    let callback_owner = std::sync::Arc::clone(&owner);
    let (builder, _) = Store::builder(State(Some(3)), Root, Runtime::new(driver.clone()))
        .mount_with_children(mount(), move |_, children| {
            let owner = std::sync::Arc::clone(&callback_owner);
            children.contribute(&declared, move |value| {
                drop(owner.lock().unwrap().take());
                Owned(Cell::new(*value))
            });
        });
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    let read = store.fold(&fold);
    *owner.lock().unwrap() = Some(store);
    let Err(error) = driver.finish(read) else {
        panic!("no partial fold after closure")
    };
    assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), TaskProgress::MayHaveRun);
}

#[test]
fn closure_inside_transform_stops_the_next_root_sibling() {
    let driver = Driver::default();
    let fold = Fold::<State, Sum>::new();
    let declared = fold.clone();
    let owner = Arc::new(Mutex::new(None::<Store<State, ()>>));
    let callback_owner = Arc::clone(&owner);
    let transforms = Arc::new(AtomicUsize::new(0));
    let callback_transforms = Arc::clone(&transforms);
    let (builder, _) = Store::builder(State(Some(3)), Root, Runtime::new(driver.clone()))
        .mount_with_children(mount(), move |_, children| {
            let owner = Arc::clone(&callback_owner);
            let transforms = Arc::clone(&callback_transforms);
            children.contribute_transformed(
                &declared,
                |value| Owned(Cell::new(*value)),
                move |value| {
                    transforms.fetch_add(1, Ordering::SeqCst);
                    let store = owner.lock().unwrap().take();
                    drop(store);
                    value
                },
            );
        });
    let sibling_fold = fold.clone();
    let sibling_calls = Arc::new(AtomicUsize::new(0));
    let callback_calls = Arc::clone(&sibling_calls);
    let (builder, _) = builder.mount_with_children(mount(), move |_, children| {
        let calls = Arc::clone(&callback_calls);
        children.contribute(&sibling_fold, move |value| {
            calls.fetch_add(1, Ordering::SeqCst);
            Owned(Cell::new(*value))
        });
    });
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    let read = store.fold(&fold);
    *owner.lock().unwrap() = Some(store);
    let Err(error) = driver.finish(read) else {
        panic!("no partial fold after transform closes the store")
    };
    assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), TaskProgress::MayHaveRun);
    assert_eq!(transforms.load(Ordering::SeqCst), 1);
    assert_eq!(sibling_calls.load(Ordering::SeqCst), 0);
}

/// Runs a contributed close action during reduction, not during contribution.
struct ReduceCloses;
impl FoldKey for ReduceCloses {
    type Value = Option<Box<dyn FnOnce() + Send>>;
    fn identity() -> Self::Value {
        None
    }
    fn reduce(
        accumulator: Self::Value,
        next: Self::Value,
    ) -> ControlFlow<Self::Value, Self::Value> {
        if let Some(close) = next {
            close();
        }
        ControlFlow::Continue(accumulator)
    }
}

/// Checks both later siblings and a pending transform after reduction closes the Store.
fn reduce_close(with_sibling: bool, transformed: bool) {
    let driver = Driver::default();
    let fold = Fold::<State, ReduceCloses>::new();
    let declared = fold.clone();
    let owner = Arc::new(Mutex::new(None::<Store<State, ()>>));
    let callback_owner = Arc::clone(&owner);
    let reductions = Arc::new(AtomicUsize::new(0));
    let callback_reductions = Arc::clone(&reductions);
    let transforms = Arc::new(AtomicUsize::new(0));
    let callback_transforms = Arc::clone(&transforms);
    let (builder, _) = Store::builder(State(Some(3)), Root, Runtime::new(driver.clone()))
        .mount_with_children(mount(), move |_, children| {
            let owner = Arc::clone(&callback_owner);
            let reductions = Arc::clone(&callback_reductions);
            let contribute = move |_: &usize| -> <ReduceCloses as FoldKey>::Value {
                let owner = Arc::clone(&owner);
                let reductions = Arc::clone(&reductions);
                Some(Box::new(move || {
                    reductions.fetch_add(1, Ordering::SeqCst);
                    let store = owner.lock().unwrap().take();
                    drop(store);
                }))
            };
            if transformed {
                let transforms = Arc::clone(&callback_transforms);
                children.contribute_transformed(&declared, contribute, move |value| {
                    transforms.fetch_add(1, Ordering::SeqCst);
                    value
                });
            } else {
                children.contribute(&declared, contribute);
            }
        });
    let sibling_calls = Arc::new(AtomicUsize::new(0));
    let builder = if with_sibling {
        let sibling_fold = fold.clone();
        let callback_calls = Arc::clone(&sibling_calls);
        let (builder, _) = builder.mount_with_children(mount(), move |_, children| {
            let calls = Arc::clone(&callback_calls);
            children.contribute(&sibling_fold, move |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                None
            });
        });
        builder
    } else {
        builder
    };
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    let read = store.fold(&fold);
    *owner.lock().unwrap() = Some(store);
    let Err(error) = driver.finish(read) else {
        panic!("no partial fold after reduce closes the store")
    };
    assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), TaskProgress::MayHaveRun);
    assert_eq!(reductions.load(Ordering::SeqCst), 1);
    assert_eq!(sibling_calls.load(Ordering::SeqCst), 0);
    assert_eq!(transforms.load(Ordering::SeqCst), 0);
}

#[test]
fn closure_inside_reduce_stops_the_next_owner() {
    reduce_close(true, false);
}

#[test]
fn closure_inside_the_last_reduce_returns_error_instead_of_a_partial_success() {
    reduce_close(false, false);
}

#[test]
fn closure_inside_frame_reduce_stops_its_transform() {
    reduce_close(false, true);
}

/// Closes the last Store owner through field destruction after its drop counter runs.
struct CloseOwnerOnDrop {
    _owner: Store<State, ()>,
    drops: Arc<AtomicUsize>,
}

impl Drop for CloseOwnerOnDrop {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

/// Discards close-bearing contributions so destruction supplies the terminal event.
struct ValueDropCloses;

impl FoldKey for ValueDropCloses {
    type Value = Option<CloseOwnerOnDrop>;

    fn identity() -> Self::Value {
        None
    }

    fn reduce(
        accumulator: Self::Value,
        _next: Self::Value,
    ) -> ControlFlow<Self::Value, Self::Value> {
        // The unused owned contribution drops after this return value is formed.
        // Its Store field closes the owner without calling an explicit closure.
        ControlFlow::Continue(accumulator)
    }
}

#[test]
fn value_destructor_closure_stops_later_callbacks_and_partial_success() {
    for (with_sibling, transformed) in [(false, false), (true, false), (false, true), (true, true)]
    {
        let driver = Driver::default();
        let fold = Fold::<State, ValueDropCloses>::new();
        let declared = fold.clone();
        let owner = Arc::new(Mutex::new(None::<Store<State, ()>>));
        let callback_owner = Arc::clone(&owner);
        let drops = Arc::new(AtomicUsize::new(0));
        let callback_drops = Arc::clone(&drops);
        let transforms = Arc::new(AtomicUsize::new(0));
        let callback_transforms = Arc::clone(&transforms);
        let (builder, _) = Store::builder(State(Some(3)), Root, Runtime::new(driver.clone()))
            .mount_with_children(mount(), move |_, children| {
                let owner = Arc::clone(&callback_owner);
                let drops = Arc::clone(&callback_drops);
                let contribute = move |_: &usize| {
                    Some(CloseOwnerOnDrop {
                        _owner: owner
                            .lock()
                            .unwrap()
                            .take()
                            .expect("one owned contribution"),
                        drops: Arc::clone(&drops),
                    })
                };
                if transformed {
                    let transforms = Arc::clone(&callback_transforms);
                    children.contribute_transformed(&declared, contribute, move |value| {
                        transforms.fetch_add(1, Ordering::SeqCst);
                        value
                    });
                } else {
                    children.contribute(&declared, contribute);
                }
            });

        let sibling_calls = Arc::new(AtomicUsize::new(0));
        let builder = if with_sibling {
            let declared = fold.clone();
            let calls = Arc::clone(&sibling_calls);
            let (builder, _) = builder.mount_with_children(mount(), move |_, children| {
                let calls = Arc::clone(&calls);
                children.contribute(&declared, move |_| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    None
                });
            });
            builder
        } else {
            builder
        };
        let store = builder.build().unwrap();
        driver.finish(store.ready()).unwrap();
        let read = store.fold(&fold);
        *owner.lock().unwrap() = Some(store);

        let Err(error) = driver.finish(read) else {
            panic!("no partial fold after value destruction closes the Store")
        };
        assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
        assert_eq!(error.progress(), TaskProgress::MayHaveRun);
        driver.turn();
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(sibling_calls.load(Ordering::SeqCst), 0);
        assert_eq!(transforms.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn closure_inside_child_transform_stops_parent_reduce_and_transform() {
    let driver = Driver::default();
    let fold = Fold::<State, ReduceCloses>::new();
    let declared = fold.clone();
    let owner = Arc::new(Mutex::new(None::<Store<State, ()>>));
    let callback_owner = Arc::clone(&owner);
    let child_transforms = Arc::new(AtomicUsize::new(0));
    let callback_child_transforms = Arc::clone(&child_transforms);
    let parent_reductions = Arc::new(AtomicUsize::new(0));
    let callback_parent_reductions = Arc::clone(&parent_reductions);
    let parent_transforms = Arc::new(AtomicUsize::new(0));
    let callback_parent_transforms = Arc::clone(&parent_transforms);
    let (builder, _) = Store::builder(State(Some(3)), Root, Runtime::new(driver.clone()))
        .mount_with_children(mount(), move |_, children| {
            let transforms = Arc::clone(&callback_parent_transforms);
            children.transform_fold(&declared, move |value| {
                transforms.fetch_add(1, Ordering::SeqCst);
                value
            });
            let child_fold = declared.clone();
            let owner = Arc::clone(&callback_owner);
            let transforms = Arc::clone(&callback_child_transforms);
            let reductions = Arc::clone(&callback_parent_reductions);
            children.optional_with_children(
                OptionalMount::new(
                    |value: &usize| Some(value),
                    |value: &mut usize| Some(value),
                    |_: &usize| {
                        Some(MountIdentity {
                            key: (),
                            replacement: 0,
                            configuration: 0,
                        })
                    },
                    |_| Leaf,
                ),
                move |_, children| {
                    let owner = Arc::clone(&owner);
                    let transforms = Arc::clone(&transforms);
                    let reductions = Arc::clone(&reductions);
                    children.transform_fold(&child_fold, move |_| {
                        transforms.fetch_add(1, Ordering::SeqCst);
                        let store = owner.lock().unwrap().take();
                        drop(store);
                        let reductions = Arc::clone(&reductions);
                        Some(Box::new(move || {
                            reductions.fetch_add(1, Ordering::SeqCst);
                        }))
                    });
                },
            );
        });
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    let read = store.fold(&fold);
    *owner.lock().unwrap() = Some(store);
    let Err(error) = driver.finish(read) else {
        panic!("no parent callback after child transform closes the store")
    };
    assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), TaskProgress::MayHaveRun);
    assert_eq!(child_transforms.load(Ordering::SeqCst), 1);
    assert_eq!(parent_reductions.load(Ordering::SeqCst), 0);
    assert_eq!(parent_transforms.load(Ordering::SeqCst), 0);
}

thread_local! {
    /// Holds the last owner for the identity callback, which cannot capture test state.
    static IDENTITY_CLOSE_OWNER: RefCell<Option<Store<State, ()>>> = const { RefCell::new(None) };
    /// Distinguishes the root identity from the transformed subtree identity.
    static IDENTITY_CLOSE_CALLS: Cell<usize> = const { Cell::new(0) };
}

/// Closes the Store when traversal creates the transformed subtree's identity.
struct IdentityCloses;
impl FoldKey for IdentityCloses {
    type Value = Owned;
    fn identity() -> Owned {
        let call = IDENTITY_CLOSE_CALLS.with(|calls| {
            let next = calls.get() + 1;
            calls.set(next);
            next
        });
        if call == 2 {
            let store = IDENTITY_CLOSE_OWNER.with(|owner| owner.borrow_mut().take());
            drop(store);
        }
        Owned(Cell::new(0))
    }
    fn reduce(accumulator: Owned, next: Owned) -> ControlFlow<Owned, Owned> {
        Sum::reduce(accumulator, next)
    }
}

#[test]
fn closure_inside_subtree_identity_stops_contribution_and_transform() {
    /// Clears thread-local test state on both normal return and assertion unwinding.
    struct ResetIdentityClose;
    impl Drop for ResetIdentityClose {
        fn drop(&mut self) {
            let store = IDENTITY_CLOSE_OWNER.with(|owner| owner.borrow_mut().take());
            drop(store);
            IDENTITY_CLOSE_CALLS.with(|calls| calls.set(0));
        }
    }
    let _reset = ResetIdentityClose;
    let driver = Driver::default();
    let fold = Fold::<State, IdentityCloses>::new();
    let declared = fold.clone();
    let contributions = Arc::new(AtomicUsize::new(0));
    let callback_contributions = Arc::clone(&contributions);
    let transforms = Arc::new(AtomicUsize::new(0));
    let callback_transforms = Arc::clone(&transforms);
    let (builder, _) = Store::builder(State(Some(3)), Root, Runtime::new(driver.clone()))
        .mount_with_children(mount(), move |_, children| {
            let contributions = Arc::clone(&callback_contributions);
            let transforms = Arc::clone(&callback_transforms);
            children.contribute_transformed(
                &declared,
                move |value| {
                    contributions.fetch_add(1, Ordering::SeqCst);
                    Owned(Cell::new(*value))
                },
                move |value| {
                    transforms.fetch_add(1, Ordering::SeqCst);
                    value
                },
            );
        });
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    let read = store.fold(&fold);
    assert!(
        IDENTITY_CLOSE_OWNER
            .with(|owner| owner.replace(Some(store)))
            .is_none()
    );
    let Err(error) = driver.finish(read) else {
        panic!("no contribution after subtree identity closes the store")
    };
    assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), TaskProgress::MayHaveRun);
    assert_eq!(IDENTITY_CLOSE_CALLS.with(Cell::get), 2);
    assert_eq!(contributions.load(Ordering::SeqCst), 0);
    assert_eq!(transforms.load(Ordering::SeqCst), 0);
}

#[test]
fn duplicate_channel_on_one_incarnation_fails_construction_and_drains_both_captures() {
    let driver = Driver::default();
    let fold = Fold::<State, Sum>::new();
    let drops = Arc::new(AtomicUsize::new(0));
    let captures = Arc::clone(&drops);
    let (builder, _) = Store::builder(State(Some(3)), Root, Runtime::new(driver.clone()))
        .mount_with_children(mount(), move |_, children| {
            for _ in 0..2 {
                let capture = Dropped(Arc::clone(&captures));
                children.contribute(&fold, move |_| {
                    let _keep = &capture;
                    panic!("duplicate construction evaluates no contributions")
                });
            }
        });
    let store = builder.build().unwrap();
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| driver.turn()))
        .expect_err("duplicate fold registration must fail during construction");
    assert_eq!(
        panic.downcast_ref::<&str>().copied(),
        Some("one fold declaration per channel and incarnation")
    );
    let error = driver.finish(store.ready()).unwrap_err();
    assert_eq!(error.reason(), TaskAccessReason::ActorFailed);
    assert_eq!(drops.load(Ordering::SeqCst), 2);
}
