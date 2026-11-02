#![cfg(feature = "local")]
//! Local folds own non-Send values and share the ordered traversal contract.

use rca_core::local::{
    Feature, FeatureContext, Fold, OptionalMount, Runtime, RuntimeDriver, RuntimeFuture, Store,
};
use rca_core::{FoldKey, MountIdentity, TaskAccessReason, TaskProgress};
use std::cell::{Cell, RefCell};
use std::ops::ControlFlow;
use std::rc::{Rc, Weak};
use std::task::{Context, Poll, Waker};

/// Polls owner-local tasks only when a test requests a turn.
///
/// Time stays at zero. Elapsed waits complete, but future waits remain pending.
/// Yield requests one wake without automatically polling queued work.
/// Tests must stop their Stores and drain tasks before releasing the driver.
#[derive(Clone, Default)]
struct Driver(Rc<RefCell<Vec<RuntimeFuture>>>);

impl std::fmt::Debug for Driver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LocalFoldDriver")
    }
}

impl RuntimeDriver for Driver {
    fn spawn(&self, task: RuntimeFuture) {
        self.0.borrow_mut().push(task);
    }
    fn now(&self) -> rca_core::MonotonicInstant {
        rca_core::MonotonicInstant::ZERO
    }
    fn sleep_until(&self, deadline: rca_core::MonotonicInstant) -> RuntimeFuture {
        if deadline <= self.now() {
            Box::pin(std::future::ready(()))
        } else {
            Box::pin(std::future::pending())
        }
    }
    fn yield_now(&self) -> RuntimeFuture {
        let mut yielded = false;
        Box::pin(std::future::poll_fn(move |context| {
            if yielded {
                Poll::Ready(())
            } else {
                yielded = true;
                context.waker().wake_by_ref();
                Poll::Pending
            }
        }))
    }
}

impl Driver {
    /// Polls one task batch after releasing the queue borrow.
    ///
    /// Tasks spawned during this turn wait for the next turn.
    fn turn(&self) {
        let tasks = std::mem::take(&mut *self.0.borrow_mut());
        for mut task in tasks {
            if task
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
            {
                self.0.borrow_mut().push(task);
            }
        }
    }

    /// Polls an operation with at most 32 intervening driver turns.
    ///
    /// The turn limit detects stalled tests, not a runtime scheduling guarantee.
    ///
    /// # Panics
    ///
    /// Panics if the operation remains pending after the final poll.
    fn finish<F: Future>(&self, future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        for _ in 0..32 {
            if let Poll::Ready(value) = future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
            {
                return value;
            }
            self.turn();
        }
        panic!("bounded local fold progress")
    }
}

#[test]
fn driver_contract_sleep_completes_only_elapsed_deadlines() {
    use rca_core::MonotonicInstant;
    use std::time::Duration;

    let driver = Driver::default();
    let mut context = Context::from_waker(Waker::noop());
    let mut elapsed = driver.sleep_until(MonotonicInstant::ZERO);
    assert!(elapsed.as_mut().poll(&mut context).is_ready());

    for deadline in [
        MonotonicInstant::from_elapsed(Duration::from_nanos(1)),
        MonotonicInstant::MAX,
    ] {
        let mut future = driver.sleep_until(deadline);
        assert!(future.as_mut().poll(&mut context).is_pending());
        assert!(future.as_mut().poll(&mut context).is_pending());
    }
}

#[test]
fn driver_contract_yield_wakes_once_before_completion() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Wake;

    /// Counts scheduling requests without running another task.
    #[derive(Default)]
    struct WakeCount(AtomicUsize);

    impl Wake for WakeCount {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    let notifications = Arc::new(WakeCount::default());
    let waker = Waker::from(Arc::clone(&notifications));
    let mut context = Context::from_waker(&waker);
    let mut future = Driver::default().yield_now();

    assert_eq!(notifications.0.load(Ordering::SeqCst), 0);
    assert!(future.as_mut().poll(&mut context).is_pending());
    assert_eq!(notifications.0.load(Ordering::SeqCst), 1);
    assert!(future.as_mut().poll(&mut context).is_ready());
    assert_eq!(notifications.0.load(Ordering::SeqCst), 1);
}

/// Keeps fold values non-Send without requiring Clone on the wrapper.
struct Owned(Rc<Cell<usize>>);

/// Adds owned values while keeping the fold key itself non-Send.
struct Sum(std::marker::PhantomData<Rc<()>>);

impl FoldKey for Sum {
    type Value = Owned;
    fn identity() -> Owned {
        Owned(Rc::new(Cell::new(0)))
    }
    fn reduce(accumulator: Owned, next: Owned) -> ControlFlow<Owned, Owned> {
        accumulator.0.set(accumulator.0.get() + next.0.get());
        ControlFlow::Continue(accumulator)
    }
}

struct Root {
    parent: Option<Parent>,
}

/// Separates leaf replacement identity from the retained parent recipe.
struct Parent {
    leaf: Option<Rc<Cell<usize>>>,
    replacement: u64,
}

enum Replace {
    Leaf(Rc<usize>),
    RemoveParent,
}

struct RootFeature;

impl Feature for RootFeature {
    type State = Root;
    type Action = Replace;
    fn update(
        &mut self,
        state: &mut Root,
        action: &Replace,
        _: &mut FeatureContext<'_, Root, Replace>,
    ) {
        match action {
            Replace::Leaf(value) => {
                let parent = state.parent.as_mut().unwrap();
                parent.leaf = Some(Rc::new(Cell::new(**value)));
                parent.replacement += 1;
            }
            Replace::RemoveParent => state.parent = None,
        }
    }
}

struct ParentFeature;

impl Feature for ParentFeature {
    type State = Parent;
    type Action = ();
    fn update(&mut self, _: &mut Parent, (): &(), _: &mut FeatureContext<'_, Parent, ()>) {}
}

struct LeafFeature;

impl Feature for LeafFeature {
    type State = Rc<Cell<usize>>;
    type Action = Rc<()>;
    fn update(
        &mut self,
        _: &mut Self::State,
        _: &Self::Action,
        _: &mut FeatureContext<'_, Self::State, Self::Action>,
    ) {
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One Local fold sequence preserves owned subtree values and Rc callback replacement lifetimes"
)]
fn local_owned_subtree_and_replacement_keep_rc_callbacks_nominally_local() {
    let driver = Driver::default();
    let fold = Fold::<Root, Sum>::default();
    let fold_debug = format!("{fold:?}");
    assert!(fold_debug.contains("Fold"));
    assert!(!fold_debug.contains("Owned") && !fold_debug.contains("Rc"));
    let declared = fold.clone();
    let captures = Rc::new(RefCell::new(Vec::<Weak<()>>::new()));
    let declared_captures = std::rc::Rc::clone(&captures);
    let sites = Rc::new(RefCell::new(Vec::new()));
    let declared_sites = std::rc::Rc::clone(&sites);
    let parent_recipes = Rc::new(Cell::new(0));
    let recipes = std::rc::Rc::clone(&parent_recipes);
    let mount = OptionalMount::new(
        |root: &Root| root.parent.as_ref(),
        |root: &mut Root| root.parent.as_mut(),
        |root: &Root| {
            root.parent.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        |_| ParentFeature,
    );
    let (builder, parent_site) = Store::builder(
        Root {
            parent: Some(Parent {
                leaf: Some(Rc::new(Cell::new(2))),
                replacement: 0,
            }),
        },
        RootFeature,
        Runtime::new(driver.clone()),
    )
    .mount_with_children(mount, move |_, children| {
        recipes.set(recipes.get() + 1);
        children.contribute(&declared, |_| Owned(Rc::new(Cell::new(1))));
        let old_captures = std::rc::Rc::clone(&declared_captures);
        let leaf = OptionalMount::new(
            |parent: &Parent| parent.leaf.as_ref(),
            |parent: &mut Parent| parent.leaf.as_mut(),
            |parent: &Parent| {
                parent.leaf.as_ref().map(|_| MountIdentity {
                    key: (),
                    replacement: parent.replacement,
                    configuration: 0,
                })
            },
            move |_| {
                assert!(
                    old_captures
                        .borrow()
                        .iter()
                        .all(|capture| capture.upgrade().is_none())
                );
                LeafFeature
            },
        );
        let declared = declared.clone();
        let captures = std::rc::Rc::clone(&declared_captures);
        let site = children.optional_with_children(leaf, move |_, children| {
            let capture = Rc::new(());
            captures.borrow_mut().push(Rc::downgrade(&capture));
            children.contribute_transformed(
                &declared,
                move |state| {
                    let _ = &capture;
                    Owned(Rc::new(Cell::new(state.get())))
                },
                |value| {
                    value.0.set(value.0.get() * 10);
                    value
                },
            );
        });
        declared_sites.borrow_mut().push(site);
    });
    let other = Store::new(
        Root {
            parent: Some(Parent {
                leaf: Some(Rc::new(Cell::new(5))),
                replacement: 0,
            }),
        },
        RootFeature,
        Runtime::new(driver.clone()),
    );
    driver.finish(other.ready()).unwrap();
    #[cfg(feature = "test-util")]
    let validation_error = rca_core::testing::local::TransactionGate::<Root, Replace>::new()
        .validate_acquisition(&other, &parent_site)
        .expect_err("an unbuilt Local declaration has no actor binding");
    #[cfg(feature = "test-util")]
    assert_eq!(validation_error.reason(), TaskAccessReason::StoreClosed);
    #[cfg(feature = "test-util")]
    assert_eq!(validation_error.progress(), TaskProgress::NotStarted);
    let Err(error) = driver.finish(other.acquire_fold(&fold, &parent_site)) else {
        panic!("an unbuilt Local declaration has no root authority");
    };
    assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
    assert_eq!(error.progress(), TaskProgress::NotStarted);
    assert_eq!(parent_recipes.get(), 0);
    assert!(captures.borrow().is_empty());

    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    assert_eq!(driver.finish(store.fold(&fold)).unwrap().0.get(), 21);
    let site = sites.borrow()[0].clone();
    let old = driver
        .finish(store.acquire_fold(&fold, &site))
        .unwrap()
        .unwrap();
    let view_debug = format!("{old:?}");
    assert!(view_debug.contains("FoldView"));
    assert!(!view_debug.contains("Owned") && !view_debug.contains("Rc"));
    assert_eq!(driver.finish(old.read()).unwrap().0.get(), 20);
    let Err(error) = driver.finish(other.acquire_fold(&fold, &site)) else {
        panic!("a foreign Local Store cannot acquire the original mounted fold");
    };
    assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
    assert_eq!(error.progress(), TaskProgress::NotStarted);
    assert_eq!(driver.finish(other.fold(&fold)).unwrap().0.get(), 0);
    drop(
        driver
            .finish(store.send(Replace::Leaf(Rc::new(8))))
            .unwrap(),
    );
    assert_eq!(driver.finish(store.fold(&fold)).unwrap().0.get(), 81);
    assert_eq!(parent_recipes.get(), 1);
    let Err(error) = driver.finish(old.read()) else {
        panic!("retired local subtree")
    };
    assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
    assert_eq!(error.progress(), TaskProgress::NotStarted);
    let fresh = driver
        .finish(store.acquire_fold(&fold, &site))
        .unwrap()
        .unwrap();
    assert_eq!(driver.finish(fresh.read()).unwrap().0.get(), 80);
    let parent = driver.finish(store.acquire(&parent_site)).unwrap().unwrap();
    driver
        .finish(parent.update(|parent| parent.leaf = None))
        .unwrap();
    assert!(
        driver
            .finish(store.acquire_fold(&fold, &site))
            .unwrap()
            .is_none()
    );
    assert_eq!(driver.finish(store.fold(&fold)).unwrap().0.get(), 1);
    assert!(
        captures
            .borrow()
            .iter()
            .all(|capture| capture.upgrade().is_none())
    );
    drop(driver.finish(store.send(Replace::RemoveParent)).unwrap());
    assert!(
        driver
            .finish(store.read(|root| root.parent.is_none()))
            .unwrap()
    );
    let Err(error) = driver.finish(fresh.read()) else {
        panic!("a retained Local fold cannot outlive its removed ancestor");
    };
    assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
    assert_eq!(error.progress(), TaskProgress::NotStarted);
    let Err(error) = driver.finish(store.acquire_fold(&fold, &site)) else {
        panic!("a removed Local declaration cannot be reacquired by its live root");
    };
    assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
    assert_eq!(error.progress(), TaskProgress::NotStarted);
    assert_eq!(driver.finish(store.fold(&fold)).unwrap().0.get(), 0);
    assert_eq!(parent_recipes.get(), 1);
    assert!(
        captures
            .borrow()
            .iter()
            .all(|capture| capture.upgrade().is_none())
    );
    drop(store);
    driver.turn();
    assert!(
        captures
            .borrow()
            .iter()
            .all(|capture| capture.upgrade().is_none())
    );
    let Err(error) = driver.finish(fresh.read()) else {
        panic!("closed local root")
    };
    assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
    let Err(error) = driver.finish(other.acquire_fold(&fold, &site)) else {
        panic!("an expired declaration cannot be acquired by a live foreign Store");
    };
    assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
    assert_eq!(error.progress(), TaskProgress::NotStarted);
    driver.finish(other.shutdown()).unwrap();
}

#[test]
fn local_transform_only_group_preserves_its_first_descendant() {
    /// Stops reduction after the first real contribution, not a synthetic identity.
    struct First;

    impl FoldKey for First {
        type Value = Owned;
        fn identity() -> Owned {
            Owned(Rc::new(Cell::new(0)))
        }
        fn reduce(_: Owned, next: Owned) -> ControlFlow<Owned, Owned> {
            ControlFlow::Break(next)
        }
    }
    let driver = Driver::default();
    let fold = Fold::<Root, First>::new();
    let declared = fold.clone();
    let calls = Rc::new(Cell::new(0));
    let selected = Rc::clone(&calls);
    let mount = OptionalMount::new(
        |root: &Root| root.parent.as_ref(),
        |root: &mut Root| root.parent.as_mut(),
        |root: &Root| {
            root.parent.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        |_| ParentFeature,
    );
    let (builder, site) = Store::builder(
        Root {
            parent: Some(Parent {
                leaf: Some(Rc::new(Cell::new(2))),
                replacement: 0,
            }),
        },
        RootFeature,
        Runtime::new(driver.clone()),
    )
    .mount_with_children(mount, move |_, children| {
        let selected = Rc::clone(&selected);
        children.transform_fold(&declared, move |value| {
            selected.set(selected.get() + 1);
            value.0.set(value.0.get() * 10);
            value
        });
        let leaf = OptionalMount::new(
            |parent: &Parent| parent.leaf.as_ref(),
            |parent: &mut Parent| parent.leaf.as_mut(),
            |parent: &Parent| {
                parent.leaf.as_ref().map(|_| MountIdentity {
                    key: (),
                    replacement: parent.replacement,
                    configuration: 0,
                })
            },
            |_| LeafFeature,
        );
        let declared = declared.clone();
        children.optional_with_children(leaf, move |_, children| {
            children.contribute(&declared, |value| Owned(Rc::new(Cell::new(value.get()))));
        });
    });
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    assert_eq!(calls.get(), 0);
    assert_eq!(driver.finish(store.fold(&fold)).unwrap().0.get(), 20);
    let view = driver
        .finish(store.acquire_fold(&fold, &site))
        .unwrap()
        .unwrap();
    assert_eq!(calls.get(), 1, "acquisition evaluates no transforms");
    assert_eq!(driver.finish(view.read()).unwrap().0.get(), 20);
    assert_eq!(calls.get(), 2);
    driver.finish(store.shutdown()).unwrap();
    assert_eq!(Rc::strong_count(&calls), 1);
}

#[test]
fn duplicate_local_channel_drains_rc_captures_without_evaluating_them() {
    /// Counts release of each rejected or installed contribution capture.
    struct Capture(Rc<Cell<usize>>);

    impl Drop for Capture {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }
    let driver = Driver::default();
    let fold = Fold::<Root, Sum>::new();
    let drops = Rc::new(Cell::new(0));
    let captures = Rc::clone(&drops);
    let mount = OptionalMount::new(
        |root: &Root| root.parent.as_ref(),
        |root: &mut Root| root.parent.as_mut(),
        |root: &Root| {
            root.parent.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        |_| ParentFeature,
    );
    let (builder, _) = Store::builder(
        Root {
            parent: Some(Parent {
                leaf: None,
                replacement: 0,
            }),
        },
        RootFeature,
        Runtime::new(driver.clone()),
    )
    .mount_with_children(mount, move |_, children| {
        for _ in 0..2 {
            let capture = Capture(Rc::clone(&captures));
            children.contribute(&fold, move |_| {
                let _ = &capture;
                panic!("duplicate local construction evaluates no contributions")
            });
        }
    });
    let store = builder.build().unwrap();
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| driver.turn()))
        .expect_err("duplicate channel installation must panic");
    assert_eq!(
        panic.downcast_ref::<&str>(),
        Some(&"one fold declaration per channel and incarnation")
    );
    let error = driver.finish(store.ready()).unwrap_err();
    assert_eq!(error.reason(), TaskAccessReason::ActorFailed);
    assert_eq!(drops.get(), 2);
}
