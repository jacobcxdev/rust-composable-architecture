#![cfg(feature = "local")]
//! Owner-local removal preserves payload identity and rejects retired child authority.

use rca_core::StackState;
use rca_core::local::{EntryMount, Feature, FeatureContext, RootMount, Runtime};
use std::{
    cell::{Cell, RefCell},
    pin::Pin,
    rc::Rc,
    task::{Context, Poll, Waker},
};

/// Polls owner-local tasks without advancing its fixed zero clock.
///
/// Elapsed waits complete. Future waits remain pending, and yield requests one
/// wake. Tests must stop their Stores and drain tasks before releasing the driver.
#[derive(Clone, Default)]
struct Driver(Rc<RefCell<Vec<rca_core::local::RuntimeFuture>>>);

impl std::fmt::Debug for Driver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Driver")
    }
}

impl rca_core::local::RuntimeDriver for Driver {
    fn spawn(&self, task: rca_core::local::RuntimeFuture) {
        self.0.borrow_mut().push(task);
    }
    fn now(&self) -> rca_core::MonotonicInstant {
        rca_core::MonotonicInstant::ZERO
    }
    fn sleep_until(&self, deadline: rca_core::MonotonicInstant) -> rca_core::local::RuntimeFuture {
        if deadline <= self.now() {
            Box::pin(std::future::ready(()))
        } else {
            Box::pin(std::future::pending())
        }
    }
    fn yield_now(&self) -> rca_core::local::RuntimeFuture {
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
    /// Polls one queued batch without holding a `RefCell` borrow.
    ///
    /// New tasks wait until the next explicit turn.
    fn turn(&self) {
        let futures = std::mem::take(&mut *self.0.borrow_mut());
        for mut future in futures {
            if future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
            {
                self.0.borrow_mut().push(future);
            }
        }
    }

    /// Polls an operation with at most 64 intervening driver turns.
    ///
    /// The limit bounds a stalled test, not a runtime scheduling guarantee.
    ///
    /// # Panics
    ///
    /// Panics if the operation remains pending after the final poll.
    fn finish<F: Future>(&self, future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        for _ in 0..64 {
            if let Poll::Ready(value) =
                Pin::as_mut(&mut future).poll(&mut Context::from_waker(Waker::noop()))
            {
                return value;
            }
            self.turn();
        }
        panic!("bounded driver did not finish")
    }
}

#[test]
fn driver_contract_sleep_completes_only_elapsed_deadlines() {
    use rca_core::MonotonicInstant;
    use rca_core::local::RuntimeDriver as _;
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
    use rca_core::local::RuntimeDriver as _;
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

/// Keeps removed values non-Send and exposes their original allocation identity.
struct Owned(Rc<Cell<usize>>);

impl Owned {
    /// Allocates an independently owned cell for a child value.
    fn new(value: usize) -> Self {
        Self(Rc::new(Cell::new(value)))
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One keyed removal sequence preserves other entries and replacement non-retargeting"
)]
fn keyed_owned_removal_preserves_other_entries_and_never_retargets_replacement() {
    type Rows = rca_core::KeyedState<u32, Owned>;
    struct RowsFeature;
    impl Feature for RowsFeature {
        type State = Rows;
        type Action = ();
        fn update(&mut self, _: &mut Rows, (): &(), _: &mut FeatureContext<'_, Rows, ()>) {}
    }
    let driver = Driver::default();
    let old = Rc::new(Cell::new(4));
    let identity = Rc::as_ptr(&old);
    let rows: Rows = [(1, Owned(old)), (2, Owned::new(20))].into_iter().collect();
    let contexts = Rc::new(RefCell::new(None));
    let export_context = std::rc::Rc::clone(&contexts);
    let release = Rc::new(Cell::new(false));
    let release_task = std::rc::Rc::clone(&release);
    let sites = Rc::new(RefCell::new(Vec::new()));
    let export_sites = std::rc::Rc::clone(&sites);
    let child_outputs = Rc::new(RefCell::new(Vec::new()));
    let export_outputs = Rc::clone(&child_outputs);
    let builder = RootMount::new(RowsFeature)
        .on_initial(
            |_| (),
            move |_, _| {
                let export = std::rc::Rc::clone(&export_context);
                let release = std::rc::Rc::clone(&release_task);
                rca_core::local::Task::run(move |context| async move {
                    *export.borrow_mut() = Some(context);
                    std::future::poll_fn(move |_| {
                        if release.get() {
                            Poll::Ready(())
                        } else {
                            Poll::Pending
                        }
                    })
                    .await;
                })
            },
        )
        .builder(rows, Runtime::new(driver.clone()))
        .keyed_removable(
            |rows| rows,
            |rows| rows,
            move |key, _| {
                let export = std::rc::Rc::clone(&export_sites);
                let outputs = Rc::clone(&export_outputs);
                let key = *key;
                (
                    rca_core::local::EntryMount::new(Leaf),
                    move |_: &Owned,
                          children: &mut rca_core::local::MountChildren<
                        '_,
                        Rows,
                        (),
                        Owned,
                        (),
                    >,
                          removal| {
                        let site = children.site().clone();
                        let child_output = children.output::<Rc<Cell<usize>>>(&site, |_| {});
                        outputs.borrow_mut().push((key, child_output));
                        export
                            .borrow_mut()
                            .push((key, children.site().clone(), removal));
                    },
                )
            },
        );
    let (builder, output) = builder.root_output::<Rc<Cell<usize>>>(|_| {});
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    driver.turn();
    let parent = contexts.borrow_mut().take().unwrap();
    let (site, removal) = {
        let sites = sites.borrow_mut();
        let (_, site, removal) = sites.iter().find(|(key, _, _)| *key == 1).unwrap();
        (site.clone(), removal.clone())
    };
    let child = driver.finish(store.acquire(&site)).unwrap().unwrap();
    drop(driver.finish(child.update(|child| child.0.set(6))).unwrap());
    assert_eq!(
        driver
            .finish(
                store.read(|rows| (rows.get(&1).unwrap().0.get(), rows.get(&2).unwrap().0.get()))
            )
            .unwrap(),
        (6, 20)
    );
    let child_output = child_outputs
        .borrow_mut()
        .iter()
        .find(|(key, _)| *key == 1)
        .unwrap()
        .1
        .clone();
    let received = Rc::new(Cell::new(0));
    let receive = std::rc::Rc::clone(&received);
    let task = driver
        .finish(parent.remove_child(
            &removal,
            &child,
            |child| {
                assert_eq!(child.0.get(), 6);
                child.0.set(7);
            },
            move |rows, removed, context| {
                rows.insert(1, Owned::new(100));
                let error = removed
                    .into_output(
                        context,
                        &child_output,
                        |_| panic!("retired child-owned declaration must reject before conversion"),
                        |_| panic!("rejected child output cannot receive"),
                    )
                    .unwrap_err();
                let (removed, error) = error.into_parts();
                assert_eq!(error.reason(), rca_core::TaskAccessReason::ScopeEnded);
                removed
                    .into_output(
                        context,
                        &output,
                        |child| child.0,
                        move |result| {
                            let rca_core::OutputDelivery::Unhandled { value, .. } = result.unwrap()
                            else {
                                panic!("root output")
                            };
                            assert_eq!(Rc::as_ptr(&value), identity);
                            assert_eq!(value.get(), 7);
                            receive.set(receive.get() + 1);
                        },
                    )
                    .unwrap();
            },
        ))
        .unwrap();
    let before = Rc::new(Cell::new(0));
    let entered = std::rc::Rc::clone(&before);
    let error = driver
        .finish(parent.remove_child(
            &removal,
            &child,
            move |_| {
                entered.set(entered.get() + 1);
            },
            |_, _, _| panic!("stale entry"),
        ))
        .unwrap_err();
    assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
    assert_eq!(before.get(), 0);
    release.set(true);
    driver.finish(task.finish());
    assert_eq!(received.get(), 1);
    assert_eq!(
        driver
            .finish(
                store.read(|rows| (rows.get(&1).unwrap().0.get(), rows.get(&2).unwrap().0.get()))
            )
            .unwrap(),
        (100, 20)
    );
    assert!(
        driver.finish(store.acquire(&site)).is_err(),
        "retired entry declaration cannot retarget"
    );
    let replacement_site = sites.borrow_mut().last().unwrap().1.clone();
    assert_eq!(
        driver
            .finish(
                driver
                    .finish(store.acquire(&replacement_site))
                    .unwrap()
                    .unwrap()
                    .read(|child| child.0.get())
            )
            .unwrap(),
        100
    );

    driver.finish(store.shutdown()).unwrap();
    drop(store);
    let remaining = std::mem::take(&mut *driver.0.borrow_mut());
    let remaining_count = remaining.len();
    drop(remaining);
    assert_eq!(remaining_count, 0, "the test must dispose of its actor");
    let queue = Rc::downgrade(&driver.0);
    drop(driver);
    assert!(
        queue.upgrade().is_none(),
        "no driver handle retains the queue"
    );
}

struct Root;

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One cancelled-child sequence preserves independent parent-owned removal authority"
)]
fn cancelled_child_producer_does_not_cancel_parent_owned_removal() {
    struct Parent;
    impl Feature for Parent {
        type State = Option<Owned>;
        type Action = ();
        fn update(
            &mut self,
            _: &mut Self::State,
            (): &(),
            _: &mut FeatureContext<'_, Self::State, ()>,
        ) {
        }
    }
    /// Exports task authority separately from the parent that can remove it.
    struct Worker(Rc<RefCell<Option<rca_core::local::TaskContext<Owned, ()>>>>);

    impl Feature for Worker {
        type State = Owned;
        type Action = ();
        fn update(&mut self, _: &mut Owned, (): &(), context: &mut FeatureContext<'_, Owned, ()>) {
            let export = std::rc::Rc::clone(&self.0);
            context.task(rca_core::local::Task::run(move |task| async move {
                *export.borrow_mut() = Some(task);
                std::future::pending::<()>().await;
            }));
        }
    }
    let driver = Driver::default();
    let parents = Rc::new(RefCell::new(None));
    let parent_export = std::rc::Rc::clone(&parents);
    let child_export = Rc::new(RefCell::new(None));
    let children = std::rc::Rc::clone(&child_export);
    let release = Rc::new(Cell::new(false));
    let release_task = std::rc::Rc::clone(&release);
    let (builder, site, removal) = RootMount::new(Parent)
        .on_initial(
            |_| (),
            move |_, _| {
                let export = std::rc::Rc::clone(&parent_export);
                let release = std::rc::Rc::clone(&release_task);
                rca_core::local::Task::run(move |task| async move {
                    *export.borrow_mut() = Some(task);
                    std::future::poll_fn(move |_| {
                        if release.get() {
                            Poll::Ready(())
                        } else {
                            Poll::Pending
                        }
                    })
                    .await;
                })
            },
        )
        .builder(Some(Owned::new(7)), Runtime::new(driver.clone()))
        .mount_removable(
            |state| state,
            |state| state,
            |state| {
                state.as_ref().map(|_| rca_core::MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            move |_| Worker(std::rc::Rc::clone(&children)),
        );
    let (builder, output) = builder.root_output::<Owned>(|_| {});
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    let mounted = driver.finish(store.acquire(&site)).unwrap().unwrap();
    let child_task = driver.finish(mounted.send(())).unwrap();
    driver.turn();
    let child = child_export.borrow_mut().take().unwrap();
    child_task.cancel();
    assert_eq!(
        driver.finish(child.read(|_| ())).unwrap_err().reason(),
        rca_core::TaskAccessReason::Cancelled
    );
    let parent = parents.borrow_mut().take().unwrap();
    let received = Rc::new(Cell::new(0));
    let count = std::rc::Rc::clone(&received);
    let task = driver
        .finish(parent.remove_child(
            &removal,
            &child,
            |value| value.0.set(9),
            move |_, removed, context| {
                removed
                    .into_output(
                        context,
                        &output,
                        |value| value,
                        move |result| {
                            let rca_core::OutputDelivery::Unhandled { value, .. } = result.unwrap()
                            else {
                                panic!("root output");
                            };
                            assert_eq!(value.0.get(), 9);
                            count.set(count.get() + 1);
                        },
                    )
                    .unwrap();
            },
        ))
        .unwrap();
    release.set(true);
    driver.finish(task.finish());
    driver.finish(child_task.finish());
    assert_eq!(received.get(), 1);
    assert!(driver.finish(store.read(Option::is_none)).unwrap());

    driver.finish(store.shutdown()).unwrap();
    drop(store);
    let remaining = std::mem::take(&mut *driver.0.borrow_mut());
    let remaining_count = remaining.len();
    drop(remaining);
    assert_eq!(remaining_count, 0, "the test must dispose of its actor");
    let queue = Rc::downgrade(&driver.0);
    drop(driver);
    assert!(
        queue.upgrade().is_none(),
        "no driver handle retains the queue"
    );
}

impl Feature for Root {
    type State = StackState<Owned>;
    type Action = ();
    fn update(
        &mut self,
        _: &mut Self::State,
        (): &(),
        _: &mut FeatureContext<'_, Self::State, ()>,
    ) {
    }
}

struct Leaf;

impl Feature for Leaf {
    type State = Owned;
    type Action = ();
    fn update(&mut self, _: &mut Owned, (): &(), _: &mut FeatureContext<'_, Owned, ()>) {}
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One nested Local stack schedule checks parent-owned removal and both pop-last outcomes"
)]
fn nested_stack_recipe_removes_only_last_child_through_parent_context() {
    use rca_core::local::{OptionalMount, Store};
    use rca_core::{MountIdentity, TaskAccessReason, TaskProgress};

    struct Outer;
    impl Feature for Outer {
        type State = Option<StackState<Owned>>;
        type Action = ();
        fn update(
            &mut self,
            _: &mut Self::State,
            (): &(),
            _: &mut FeatureContext<'_, Self::State, ()>,
        ) {
        }
    }

    let driver = Driver::default();
    let mut stack = StackState::new();
    let first = stack.push(Owned::new(10));
    let last_value = Owned::new(20);
    let last_pointer = Rc::as_ptr(&last_value.0);
    let last = stack.push(last_value);
    let parent_context = Rc::new(RefCell::new(None));
    let export_parent = Rc::clone(&parent_context);
    let release = Rc::new(Cell::new(false));
    let release_task = Rc::clone(&release);
    let descriptor = OptionalMount::new(
        Option::as_ref,
        Option::as_mut,
        |state: &Option<StackState<Owned>>| {
            state.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        |_| Root,
    )
    .on_initial(
        |_| (),
        move |_, _| {
            let export = Rc::clone(&export_parent);
            let release = Rc::clone(&release_task);
            rca_core::local::Task::run(move |context| async move {
                *export.borrow_mut() = Some(context);
                std::future::poll_fn(move |_| {
                    if release.get() {
                        Poll::Ready(())
                    } else {
                        Poll::Pending
                    }
                })
                .await;
            })
        },
    );
    let entries = Rc::new(RefCell::new(Vec::new()));
    let export = Rc::clone(&entries);
    let (builder, parent_site) = Store::builder(Some(stack), Outer, Runtime::new(driver.clone()))
        .mount_with_children(descriptor, move |_, children| {
            let export = Rc::clone(&export);
            let parent_source = children.site().clone();
            children.stack_removable(
                |state| state,
                |state| state,
                move |id, _| {
                    let export = Rc::clone(&export);
                    let parent_source = parent_source.clone();
                    let id = id.clone();
                    (
                        EntryMount::new(Leaf),
                        move |_: &Owned,
                              children: &mut rca_core::local::MountChildren<
                            '_,
                            Option<StackState<Owned>>,
                            (),
                            Owned,
                            (),
                        >,
                              removal| {
                            let output = children.output::<Rc<Cell<usize>>>(&parent_source, |_| {});
                            export.borrow_mut().push((
                                id.clone(),
                                children.site().clone(),
                                removal,
                                output,
                            ));
                        },
                    )
                },
            );
        });
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    driver.turn();
    let parent = parent_context.borrow_mut().take().unwrap();
    let entries = entries.borrow().clone();
    assert_eq!(
        entries.iter().map(|entry| &entry.0).collect::<Vec<_>>(),
        vec![&first, &last]
    );
    let mounted_parent = driver.finish(store.acquire(&parent_site)).unwrap().unwrap();
    assert_eq!(
        driver.finish(mounted_parent.read(StackState::len)).unwrap(),
        2
    );
    let first_child = driver
        .finish(store.acquire(&entries[0].1))
        .unwrap()
        .unwrap();
    let last_child = driver
        .finish(store.acquire(&entries[1].1))
        .unwrap()
        .unwrap();

    let entered = Rc::new(Cell::new(0));
    let before = Rc::clone(&entered);
    let after = Rc::clone(&entered);
    let refused = driver
        .finish(parent.remove_child(
            &entries[0].2,
            &first_child,
            move |_| before.set(before.get() + 1),
            move |_, _, _| after.set(after.get() + 1),
        ))
        .unwrap_err();
    assert_eq!(refused.reason(), TaskAccessReason::InvalidTarget);
    assert_eq!(refused.progress(), TaskProgress::MayHaveRun);
    assert_eq!(entered.get(), 0);
    assert_eq!(driver.finish(parent.read(StackState::len)).unwrap(), 2);

    let removed = Rc::new(Cell::new(0));
    let export_removed = Rc::clone(&removed);
    let retired_output = entries[1].3.clone();
    let task = driver
        .finish(parent.remove_child(
            &entries[1].2,
            &last_child,
            |_| {},
            move |stack, removed_child, context| {
                assert_eq!(stack.len(), 1);
                let (removed_child, error) = removed_child
                    .into_output(
                        context,
                        &retired_output,
                        |_| panic!("retired output owner cannot convert the child"),
                        |_| panic!("rejected output cannot receive"),
                    )
                    .unwrap_err()
                    .into_parts();
                assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
                assert_eq!(error.progress(), TaskProgress::NotStarted);
                let value = removed_child.into_inner();
                assert_eq!(Rc::as_ptr(&value.0), last_pointer);
                export_removed.set(value.0.get());
            },
        ))
        .unwrap();
    assert_eq!(removed.get(), 20);
    assert_eq!(
        driver.finish(last_child.read(|_| ())).unwrap_err().reason(),
        TaskAccessReason::ScopeEnded
    );
    assert_eq!(driver.finish(parent.read(StackState::len)).unwrap(), 1);
    assert_eq!(
        driver
            .finish(store.read(|state| state.as_ref().unwrap().len()))
            .unwrap(),
        1
    );
    assert_eq!(
        driver
            .finish(first_child.read(|value| value.0.get()))
            .unwrap(),
        10
    );
    release.set(true);
    driver.finish(task.finish());
    driver.finish(store.shutdown()).unwrap();
}

#[test]
fn native_stack_declares_real_entry_sites() {
    let driver = Driver::default();
    let mut stack = StackState::new();
    let id = stack.push(Owned::new(7));
    let sites = Rc::new(RefCell::new(Vec::new()));
    let export = Rc::clone(&sites);
    let store = RootMount::new(Root)
        .builder(stack, Runtime::new(driver.clone()))
        .stack_removable(
            |state| state,
            |state| state,
            move |id, _| {
                let export = Rc::clone(&export);
                let id = id.clone();
                (
                    EntryMount::new(Leaf),
                    move |_: &Owned,
                          children: &mut rca_core::local::MountChildren<
                        '_,
                        StackState<Owned>,
                        (),
                        Owned,
                        (),
                    >,
                          removal| {
                        export
                            .borrow_mut()
                            .push((id.clone(), children.site().clone(), removal));
                    },
                )
            },
        )
        .build()
        .unwrap();
    driver.finish(store.ready()).unwrap();
    let site = sites.borrow_mut()[0].1.clone();
    assert_eq!(sites.borrow_mut()[0].0, id);
    let child = driver.finish(store.acquire(&site)).unwrap().unwrap();
    assert_eq!(driver.finish(child.read(|value| value.0.get())).unwrap(), 7);
    drop(driver.finish(child.update(|value| value.0.set(8))).unwrap());
    assert_eq!(driver.finish(child.read(|value| value.0.get())).unwrap(), 8);
    assert_eq!(
        driver
            .finish(store.read(move |stack| stack.get(&id).unwrap().0.get()))
            .unwrap(),
        8
    );

    driver.finish(store.shutdown()).unwrap();
    drop(store);
    let remaining = std::mem::take(&mut *driver.0.borrow_mut());
    let remaining_count = remaining.len();
    drop(remaining);
    assert_eq!(remaining_count, 0, "the test must dispose of its actor");
    let queue = Rc::downgrade(&driver.0);
    drop(driver);
    assert!(
        queue.upgrade().is_none(),
        "no driver handle retains the queue"
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One Local stack removal sequence preserves pop, suffix ownership and live-parent authority"
)]
fn checked_pop_last_and_owned_suffix_use_live_parent_authority() {
    let driver = Driver::default();
    let mut stack = StackState::new();
    let first = stack.push(Owned::new(10));
    let middle = stack.push(Owned::new(20));
    let old = Owned::new(30);
    let old_pointer = Rc::as_ptr(&old.0);
    let last = stack.push(old);
    assert!(stack.try_push(last.clone(), Owned::new(99)).is_err());
    let sites = Rc::new(RefCell::new(Vec::new()));
    let export = Rc::clone(&sites);
    let parent = Rc::new(RefCell::new(None));
    let export_parent = Rc::clone(&parent);
    let release = Rc::new(Cell::new(false));
    let release_task = Rc::clone(&release);
    let builder = RootMount::new(Root)
        .on_initial(
            |_| (),
            move |_, _| {
                let export = Rc::clone(&export_parent);
                let release = Rc::clone(&release_task);
                rca_core::local::Task::run(move |context| async move {
                    *export.borrow_mut() = Some(context);
                    std::future::poll_fn(move |_| {
                        if release.get() {
                            Poll::Ready(())
                        } else {
                            Poll::Pending
                        }
                    })
                    .await;
                })
            },
        )
        .builder(stack, Runtime::new(driver.clone()))
        .stack_removable(
            |state| state,
            |state| state,
            move |id, _| {
                let export = Rc::clone(&export);
                let id = id.clone();
                (
                    EntryMount::new(Leaf),
                    move |_: &Owned,
                          children: &mut rca_core::local::MountChildren<
                        '_,
                        StackState<Owned>,
                        (),
                        Owned,
                        (),
                    >,
                          removal| {
                        export
                            .borrow_mut()
                            .push((id.clone(), children.site().clone(), removal));
                    },
                )
            },
        );
    let (builder, output) = builder.root_output::<Owned>(|_| {});
    let (builder, suffix_output) = builder.root_output::<Vec<(rca_core::StackId, Owned)>>(|_| {});
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    driver.turn();
    let parent = parent.borrow_mut().take().unwrap();
    let entries = sites.borrow_mut().clone();
    let first_context = driver
        .finish(store.acquire(&entries[0].1))
        .unwrap()
        .unwrap();
    let middle_context = driver
        .finish(store.acquire(&entries[1].1))
        .unwrap()
        .unwrap();
    let last_context = driver
        .finish(store.acquire(&entries[2].1))
        .unwrap()
        .unwrap();
    let callbacks = Rc::new(Cell::new(0));
    let before = Rc::clone(&callbacks);
    let after = Rc::clone(&callbacks);
    let refused = driver
        .finish(parent.remove_child(
            &entries[0].2,
            &first_context,
            move |_| {
                before.set(before.get() + 1);
            },
            move |_, _, _| {
                after.set(after.get() + 1);
            },
        ))
        .unwrap_err();
    assert_eq!(refused.reason(), rca_core::TaskAccessReason::InvalidTarget);
    assert_eq!(refused.progress(), rca_core::TaskProgress::MayHaveRun);
    assert_eq!(callbacks.get(), 0);
    assert_eq!(
        driver
            .finish(first_context.read(|value| value.0.get()))
            .unwrap(),
        10
    );
    assert_eq!(driver.finish(store.read(StackState::len)).unwrap(), 3);

    let delivered = Rc::new(Cell::new(0));
    let receive = Rc::clone(&delivered);
    let task = driver
        .finish(parent.remove_child(
            &entries[2].2,
            &last_context,
            |value| value.0.set(value.0.get() + 1),
            move |_, removed, context| {
                removed
                    .into_output(
                        context,
                        &output,
                        |value| value,
                        move |result| {
                            let rca_core::OutputDelivery::Unhandled { value, .. } = result.unwrap()
                            else {
                                panic!("root output has no ancestor")
                            };
                            assert_eq!(Rc::as_ptr(&value.0), old_pointer);
                            assert_eq!(value.0.get(), 31);
                            receive.set(receive.get() + 1);
                        },
                    )
                    .unwrap();
            },
        ))
        .unwrap();
    for _ in 0..5 {
        driver.turn();
    }
    assert_eq!(delivered.get(), 1);
    let replacement = last.clone();
    driver
        .finish(parent.update(move |stack| {
            stack
                .try_push(replacement, Owned::new(300))
                .unwrap_or_else(|_| panic!("retired ID is available"));
        }))
        .unwrap();
    assert_eq!(
        driver
            .finish(last_context.read(|_| ()))
            .unwrap_err()
            .reason(),
        rca_core::TaskAccessReason::ScopeEnded
    );
    let suffix_start = first.clone();
    let (suffix, _, _) = driver
        .finish(parent.update(move |stack| stack.try_pop_to(&suffix_start).unwrap()))
        .unwrap()
        .into_parts();
    assert_eq!(
        suffix.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>(),
        vec![middle, last]
    );
    assert_eq!(
        suffix
            .iter()
            .map(|(_, value)| value.0.get())
            .collect::<Vec<_>>(),
        vec![20, 300]
    );
    assert_eq!(
        driver
            .finish(middle_context.read(|_| ()))
            .unwrap_err()
            .reason(),
        rca_core::TaskAccessReason::ScopeEnded
    );
    let rca_core::OutputDelivery::Unhandled { value: suffix, .. } = driver
        .finish(parent.output(&suffix_output, suffix))
        .unwrap()
    else {
        panic!("root suffix output")
    };
    assert_eq!(suffix.len(), 2);
    let popped = Rc::new(Cell::new(0));
    let export_popped = Rc::clone(&popped);
    let final_task = driver
        .finish(parent.remove_child(
            &entries[0].2,
            &first_context,
            |_| {},
            move |_, removed, _| {
                export_popped.set(removed.into_inner().0.get());
            },
        ))
        .unwrap();
    assert_eq!(
        popped.get(),
        10,
        "same formerly non-last live target succeeds once last"
    );
    assert!(driver.finish(store.read(StackState::is_empty)).unwrap());
    release.set(true);
    driver.finish(task.finish());
    driver.finish(final_task.finish());

    driver.finish(store.shutdown()).unwrap();
    drop(store);
    let remaining = std::mem::take(&mut *driver.0.borrow_mut());
    let remaining_count = remaining.len();
    drop(remaining);
    assert_eq!(remaining_count, 0, "the test must dispose of its actor");
    let queue = Rc::downgrade(&driver.0);
    drop(driver);
    assert!(
        queue.upgrade().is_none(),
        "no driver handle retains the queue"
    );
}
