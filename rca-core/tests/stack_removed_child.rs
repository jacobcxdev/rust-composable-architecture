//! Owning stack removal preserves boxes and refuses stale or interrupted parent authority.
//!
//! Explicit driver turns release the test's held parent tasks. These schedules
//! check ownership and actor ordering, not executor wake fairness.

use rca_core::{EntryMount, Feature, FeatureContext, RootMount, Runtime, StackState};
use std::{
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, Waker},
};

/// Manually polls a frozen-clock actor; yielding still suspends for one turn.
#[derive(Clone, Default)]
struct Driver(Arc<Mutex<Vec<rca_core::RuntimeFuture>>>);
impl std::fmt::Debug for Driver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Driver")
    }
}
impl rca_core::RuntimeDriver for Driver {
    fn spawn(&self, task: rca_core::RuntimeFuture) {
        self.0.lock().unwrap().push(task);
    }
    fn now(&self) -> rca_core::MonotonicInstant {
        rca_core::MonotonicInstant::ZERO
    }
    fn sleep_until(&self, deadline: rca_core::MonotonicInstant) -> rca_core::RuntimeFuture {
        if deadline <= self.now() {
            Box::pin(std::future::ready(()))
        } else {
            Box::pin(std::future::pending())
        }
    }
    fn yield_now(&self) -> rca_core::RuntimeFuture {
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
#[test]
fn manual_driver_clock_waits_respect_the_fixed_origin() {
    let driver = Driver::default();
    assert_eq!(
        rca_core::RuntimeDriver::now(&driver),
        rca_core::MonotonicInstant::ZERO
    );
    let mut elapsed =
        rca_core::RuntimeDriver::sleep_until(&driver, rca_core::MonotonicInstant::ZERO);
    let mut context = Context::from_waker(Waker::noop());
    assert!(elapsed.as_mut().poll(&mut context).is_ready());

    for deadline in [
        rca_core::MonotonicInstant::from_elapsed(std::time::Duration::from_nanos(1)),
        rca_core::MonotonicInstant::MAX,
    ] {
        let mut future = rca_core::RuntimeDriver::sleep_until(&driver, deadline);
        assert!(future.as_mut().poll(&mut context).is_pending());
    }
    assert!(driver.0.lock().unwrap().is_empty());
}

#[test]
fn manual_driver_yield_suspends_and_wakes_before_completion() {
    /// Counts scheduling requests without driving the actor from the waker.
    struct WakeCount(std::sync::atomic::AtomicUsize);
    impl std::task::Wake for WakeCount {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    let driver = Driver::default();
    let notifications = Arc::new(WakeCount(std::sync::atomic::AtomicUsize::new(0)));
    let waker = Waker::from(Arc::clone(&notifications));
    let mut context = Context::from_waker(&waker);
    let mut future = rca_core::RuntimeDriver::yield_now(&driver);
    assert_eq!(notifications.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(future.as_mut().poll(&mut context).is_pending());
    assert_eq!(notifications.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(future.as_mut().poll(&mut context).is_ready());
    assert_eq!(notifications.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(driver.0.lock().unwrap().is_empty());
}

impl Driver {
    /// Polls the captured queue once, outside its lock; new work waits for a later turn.
    fn turn(&self) {
        let futures = std::mem::take(&mut *self.0.lock().unwrap());
        for mut future in futures {
            if future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
            {
                self.0.lock().unwrap().push(future);
            }
        }
    }
    /// Bounds progress for a finite operation rather than waiting on wall-clock time.
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
struct Root;
impl Feature for Root {
    type State = StackState<Box<usize>>;
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
    type State = Box<usize>;
    type Action = ();
    fn update(&mut self, _: &mut Box<usize>, (): &(), _: &mut FeatureContext<'_, Box<usize>, ()>) {}
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One nested stack schedule checks parent-owned removal and both pop-last outcomes"
)]
#[expect(
    clippy::borrowed_box,
    reason = "The nested recipe borrows the existing boxed child state"
)]
fn nested_stack_recipe_removes_only_last_child_through_parent_context() {
    use rca_core::{MountIdentity, OptionalMount, Store, TaskAccessReason, TaskProgress};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct Outer;
    impl Feature for Outer {
        type State = Option<StackState<Box<usize>>>;
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
    let first = stack.push(Box::new(10));
    let last_value = Box::new(20);
    let last_pointer = (&raw const *last_value) as usize;
    let last = stack.push(last_value);
    let parent_context = Arc::new(Mutex::new(None));
    let export_parent = Arc::clone(&parent_context);
    let release = Arc::new(AtomicBool::new(false));
    let release_task = Arc::clone(&release);
    let descriptor = OptionalMount::new(
        Option::as_ref,
        Option::as_mut,
        |state: &Option<StackState<Box<usize>>>| {
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
            let export = Arc::clone(&export_parent);
            let release = Arc::clone(&release_task);
            rca_core::Task::run(move |context| async move {
                *export.lock().unwrap() = Some(context);
                std::future::poll_fn(move |_| {
                    if release.load(Ordering::SeqCst) {
                        Poll::Ready(())
                    } else {
                        Poll::Pending
                    }
                })
                .await;
            })
        },
    );
    let entries = Arc::new(Mutex::new(Vec::new()));
    let export = Arc::clone(&entries);
    let (builder, parent_site) = Store::builder(Some(stack), Outer, Runtime::new(driver.clone()))
        .mount_with_children(descriptor, move |_, children| {
            let export = Arc::clone(&export);
            children.stack_removable(
                |state| state,
                |state| state,
                move |id, _| {
                    let export = Arc::clone(&export);
                    let id = id.clone();
                    (
                        EntryMount::new(Leaf),
                        move |_: &Box<usize>,
                              children: &mut rca_core::MountChildren<
                            '_,
                            Option<StackState<Box<usize>>>,
                            (),
                            Box<usize>,
                            (),
                        >,
                              removal| {
                            export.lock().unwrap().push((
                                id.clone(),
                                children.site().clone(),
                                removal,
                            ));
                        },
                    )
                },
            );
        });
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    driver.turn();
    let parent = parent_context.lock().unwrap().take().unwrap();
    let entries = entries.lock().unwrap().clone();
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

    let entered = Arc::new(AtomicUsize::new(0));
    let before = Arc::clone(&entered);
    let after = Arc::clone(&entered);
    let refused = driver
        .finish(parent.remove_child(
            &entries[0].2,
            &first_child,
            move |_| {
                before.fetch_add(1, Ordering::SeqCst);
            },
            move |_, _, _| {
                after.fetch_add(1, Ordering::SeqCst);
            },
        ))
        .unwrap_err();
    assert_eq!(refused.reason(), TaskAccessReason::InvalidTarget);
    assert_eq!(refused.progress(), TaskProgress::MayHaveRun);
    assert_eq!(entered.load(Ordering::SeqCst), 0);
    assert_eq!(driver.finish(parent.read(StackState::len)).unwrap(), 2);

    let removed = Arc::new(AtomicUsize::new(0));
    let export_removed = Arc::clone(&removed);
    let task = driver
        .finish(parent.remove_child(
            &entries[1].2,
            &last_child,
            |_| {},
            move |stack, removed_child, _| {
                assert_eq!(stack.len(), 1);
                let value = removed_child.into_inner();
                assert_eq!((&raw const *value) as usize, last_pointer);
                export_removed.store(*value, Ordering::SeqCst);
            },
        ))
        .unwrap();
    assert_eq!(removed.load(Ordering::SeqCst), 20);
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
        driver.finish(first_child.read(|value| **value)).unwrap(),
        10
    );
    release.store(true, Ordering::SeqCst);
    driver.finish(task.finish());
    driver.finish(store.shutdown()).unwrap();
}

#[test]
#[expect(
    clippy::borrowed_box,
    reason = "The mount callback borrows the boxed Feature State whose allocation identity is under test"
)]
fn native_stack_declares_real_entry_sites() {
    let driver = Driver::default();
    let mut stack = StackState::new();
    let id = stack.push(Box::new(7));
    let sites = Arc::new(Mutex::new(Vec::new()));
    let export = Arc::clone(&sites);
    let store = RootMount::new(Root)
        .builder(stack, Runtime::new(driver.clone()))
        .stack_removable(
            |state| state,
            |state| state,
            move |id, _| {
                let export = Arc::clone(&export);
                let id = id.clone();
                (
                    EntryMount::new(Leaf),
                    move |_: &Box<usize>,
                          children: &mut rca_core::MountChildren<
                        '_,
                        StackState<Box<usize>>,
                        (),
                        Box<usize>,
                        (),
                    >,
                          removal| {
                        export
                            .lock()
                            .unwrap()
                            .push((id.clone(), children.site().clone(), removal));
                    },
                )
            },
        )
        .build()
        .unwrap();
    driver.finish(store.ready()).unwrap();
    let site = sites.lock().unwrap()[0].1.clone();
    assert_eq!(sites.lock().unwrap()[0].0, id);
    let child = driver.finish(store.acquire(&site)).unwrap().unwrap();
    assert_eq!(driver.finish(child.read(|value| **value)).unwrap(), 7);
    driver.finish(store.shutdown()).unwrap();
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One stack removal sequence preserves pop, suffix ownership and live-parent authority"
)]
#[expect(
    clippy::borrowed_box,
    reason = "The mount callback borrows the boxed Feature State whose owned removal is under test"
)]
fn checked_pop_last_and_owned_suffix_use_live_parent_authority() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let driver = Driver::default();
    let mut stack = StackState::new();
    let first = stack.push(Box::new(10));
    let middle = stack.push(Box::new(20));
    let old = Box::new(30);
    let old_pointer = (&raw const *old) as usize;
    let last = stack.push(old);
    assert!(stack.try_push(last.clone(), Box::new(99)).is_err());
    let sites = Arc::new(Mutex::new(Vec::new()));
    let export = Arc::clone(&sites);
    let parent = Arc::new(Mutex::new(None));
    let export_parent = Arc::clone(&parent);
    let release = Arc::new(AtomicBool::new(false));
    let release_task = Arc::clone(&release);
    let builder = RootMount::new(Root)
        .on_initial(
            |_| (),
            move |_, _| {
                let export = Arc::clone(&export_parent);
                let release = Arc::clone(&release_task);
                rca_core::Task::run(move |context| async move {
                    *export.lock().unwrap() = Some(context);
                    std::future::poll_fn(move |_| {
                        if release.load(Ordering::SeqCst) {
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
                let export = Arc::clone(&export);
                let id = id.clone();
                (
                    EntryMount::new(Leaf),
                    move |_: &Box<usize>,
                          children: &mut rca_core::MountChildren<
                        '_,
                        StackState<Box<usize>>,
                        (),
                        Box<usize>,
                        (),
                    >,
                          removal| {
                        export
                            .lock()
                            .unwrap()
                            .push((id.clone(), children.site().clone(), removal));
                    },
                )
            },
        );
    let (builder, output) = builder.root_output::<Box<usize>>(|_| {});
    let (builder, suffix_output) =
        builder.root_output::<Vec<(rca_core::StackId, Box<usize>)>>(|_| {});
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    driver.turn();
    let parent = parent.lock().unwrap().take().unwrap();
    let entries = sites.lock().unwrap().clone();
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
    let callbacks = Arc::new(AtomicUsize::new(0));
    let before = Arc::clone(&callbacks);
    let after = Arc::clone(&callbacks);
    let refused = driver
        .finish(parent.remove_child(
            &entries[0].2,
            &first_context,
            move |_| {
                before.fetch_add(1, Ordering::SeqCst);
            },
            move |_, _, _| {
                after.fetch_add(1, Ordering::SeqCst);
            },
        ))
        .unwrap_err();
    assert_eq!(refused.reason(), rca_core::TaskAccessReason::InvalidTarget);
    assert_eq!(refused.progress(), rca_core::TaskProgress::MayHaveRun);
    assert_eq!(callbacks.load(Ordering::SeqCst), 0);
    assert_eq!(
        driver.finish(first_context.read(|value| **value)).unwrap(),
        10
    );
    assert_eq!(driver.finish(store.read(StackState::len)).unwrap(), 3);

    driver
        .finish(last_context.update(|value| **value += 2))
        .unwrap();
    assert_eq!(
        driver.finish(last_context.read(|value| **value)).unwrap(),
        32
    );
    assert_eq!(
        driver.finish(first_context.read(|value| **value)).unwrap(),
        10
    );
    assert_eq!(
        driver.finish(middle_context.read(|value| **value)).unwrap(),
        20
    );

    let delivered = Arc::new(AtomicUsize::new(0));
    let receive = Arc::clone(&delivered);
    let task = driver
        .finish(parent.remove_child(
            &entries[2].2,
            &last_context,
            |value| **value += 1,
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
                            assert_eq!((&raw const *value) as usize, old_pointer);
                            assert_eq!(*value, 33);
                            receive.fetch_add(1, Ordering::SeqCst);
                        },
                    )
                    .unwrap();
            },
        ))
        .unwrap();
    for _ in 0..5 {
        driver.turn();
    }
    assert_eq!(delivered.load(Ordering::SeqCst), 1);
    let replacement = last.clone();
    driver
        .finish(parent.update(move |stack| stack.try_push(replacement, Box::new(300)).unwrap()))
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
        suffix.iter().map(|(_, value)| **value).collect::<Vec<_>>(),
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
    let popped = Arc::new(AtomicUsize::new(0));
    let export_popped = Arc::clone(&popped);
    let final_task = driver
        .finish(parent.remove_child(
            &entries[0].2,
            &first_context,
            |_| {},
            move |_, removed, _| {
                export_popped.store(*removed.into_inner(), Ordering::SeqCst);
            },
        ))
        .unwrap();
    assert_eq!(
        popped.load(Ordering::SeqCst),
        10,
        "same formerly non-last live target succeeds once last"
    );
    assert!(driver.finish(store.read(StackState::is_empty)).unwrap());
    release.store(true, Ordering::SeqCst);
    driver.finish(task.finish());
    driver.finish(final_task.finish());
    driver.finish(store.shutdown()).unwrap();
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "Four callback boundaries share one real stack fixture and preserve their complete ownership timelines"
)]
fn owning_stack_removal_rechecks_selector_mutation_and_callback_closure() {
    use rca_core::{Store, TaskAccessReason, TaskProgress};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct DropCount(Arc<AtomicUsize>);
    impl Drop for DropCount {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    /// Records original-box identity and disposal separately from replacement state.
    struct Value {
        value: Box<usize>,
        _drop: DropCount,
    }
    /// Runs one actual selector mutation after the child has been acquired.
    type WriteHook = Box<dyn FnOnce(&mut StackState<Value>) + Send>;
    /// Captures surviving IDs, box addresses and values before their final disposal.
    type DisposalSnapshot = Vec<(rca_core::StackId, usize, usize)>;
    /// Observes terminal contents without keeping removed payloads alive.
    struct State {
        stack: StackState<Value>,
        hook: Arc<Mutex<Option<WriteHook>>>,
        disposed: Arc<Mutex<Option<DisposalSnapshot>>>,
    }
    impl Drop for State {
        fn drop(&mut self) {
            *self.disposed.lock().unwrap() = Some(
                self.stack
                    .iter()
                    .map(|(id, value)| {
                        (id.clone(), (&raw const *value.value) as usize, *value.value)
                    })
                    .collect(),
            );
        }
    }
    struct Parent(Arc<AtomicUsize>);
    impl Feature for Parent {
        type State = State;
        type Action = ();
        fn update(&mut self, _: &mut State, (): &(), _: &mut FeatureContext<'_, State, ()>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    struct Child(Arc<AtomicUsize>);
    impl Feature for Child {
        type State = Value;
        type Action = ();
        fn update(&mut self, _: &mut Value, (): &(), _: &mut FeatureContext<'_, Value, ()>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    /// Chooses the user-code boundary where removal loses its original authority.
    #[derive(Clone, Copy)]
    enum Interruption {
        SelectorClose,
        SelectorRemove,
        SelectorReplace,
        BeforeClose,
    }

    for interruption in [
        Interruption::SelectorClose,
        Interruption::SelectorRemove,
        Interruption::SelectorReplace,
        Interruption::BeforeClose,
    ] {
        let driver = Driver::default();
        let original_drops = Arc::new(AtomicUsize::new(0));
        let original = Value {
            value: Box::new(7),
            _drop: DropCount(Arc::clone(&original_drops)),
        };
        let original_pointer = (&raw const *original.value) as usize;
        let mut stack = StackState::new();
        let id = stack.push(original);
        let hook = Arc::new(Mutex::new(None::<WriteHook>));
        let disposed = Arc::new(Mutex::new(None));
        let reductions = Arc::new(AtomicUsize::new(0));
        let child_reductions = Arc::clone(&reductions);
        let parent = Arc::new(Mutex::new(None));
        let export_parent = Arc::clone(&parent);
        let release = Arc::new(AtomicBool::new(false));
        let release_task = Arc::clone(&release);
        let entries = Arc::new(Mutex::new(Vec::new()));
        let export_entries = Arc::clone(&entries);
        let store = RootMount::new(Parent(Arc::clone(&reductions)))
            .on_initial(
                |_| (),
                move |_, _| {
                    let export = Arc::clone(&export_parent);
                    let release = Arc::clone(&release_task);
                    rca_core::Task::run(move |context| async move {
                        *export.lock().unwrap() = Some(context);
                        std::future::poll_fn(move |_| {
                            if release.load(Ordering::SeqCst) {
                                Poll::Ready(())
                            } else {
                                Poll::Pending
                            }
                        })
                        .await;
                    })
                },
            )
            .builder(
                State {
                    stack,
                    hook: Arc::clone(&hook),
                    disposed: Arc::clone(&disposed),
                },
                Runtime::new(driver.clone()),
            )
            .stack_removable(
                |state| &state.stack,
                |state| {
                    let hook = state.hook.lock().unwrap().take();
                    if let Some(hook) = hook {
                        hook(&mut state.stack);
                    }
                    &mut state.stack
                },
                move |id, _| {
                    let export = Arc::clone(&export_entries);
                    let id = id.clone();
                    (
                        EntryMount::new(Child(Arc::clone(&child_reductions))),
                        move |_: &Value,
                              children: &mut rca_core::MountChildren<'_, State, (), Value, ()>,
                              removal| {
                            export.lock().unwrap().push((
                                id.clone(),
                                children.site().clone(),
                                removal,
                            ));
                        },
                    )
                },
            )
            .build()
            .unwrap();
        driver.finish(store.ready()).unwrap();
        driver.turn();
        let parent = parent.lock().unwrap().take().unwrap();
        let entries = entries.lock().unwrap().clone();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, id);
        let child = driver
            .finish(store.acquire(&entries[0].1))
            .unwrap()
            .unwrap();
        let owner = Arc::new(Mutex::new(Some(store)));
        let removed = Arc::new(Mutex::new(None::<Value>));
        let replacement_drops = Arc::new(AtomicUsize::new(0));
        let selector_calls = Arc::new(AtomicUsize::new(0));
        let selector_count = Arc::clone(&selector_calls);
        let selector_owner = Arc::clone(&owner);
        let selector_removed = Arc::clone(&removed);
        let replacement_count = Arc::clone(&replacement_drops);
        // Arm only after installation and acquisition: the mutation occurs
        // inside the actual owning-field write selector, not a queued edit.
        *hook.lock().unwrap() = Some(Box::new(move |stack| {
            selector_count.fetch_add(1, Ordering::SeqCst);
            match interruption {
                Interruption::SelectorClose => drop(selector_owner.lock().unwrap().take()),
                Interruption::SelectorRemove | Interruption::SelectorReplace => {
                    let (removed_id, value) = stack.pop().unwrap();
                    assert_eq!(removed_id, id);
                    *selector_removed.lock().unwrap() = Some(value);
                    if matches!(interruption, Interruption::SelectorReplace) {
                        stack
                            .try_push(
                                removed_id,
                                Value {
                                    value: Box::new(90),
                                    _drop: DropCount(replacement_count),
                                },
                            )
                            .unwrap_or_else(|_| panic!("removed logical ID can be reinserted"));
                    }
                }
                Interruption::BeforeClose => {}
            }
        }));
        let before_calls = Arc::new(AtomicUsize::new(0));
        let before_count = Arc::clone(&before_calls);
        let after_calls = Arc::new(AtomicUsize::new(0));
        let after_count = Arc::clone(&after_calls);
        let callback_drops = Arc::new(AtomicUsize::new(0));
        let before_capture = DropCount(Arc::clone(&callback_drops));
        let after_capture = DropCount(Arc::clone(&callback_drops));
        let before_owner = Arc::clone(&owner);
        let error = driver
            .finish(parent.remove_child(
                &entries[0].2,
                &child,
                move |value| {
                    let _ = &before_capture;
                    before_count.fetch_add(1, Ordering::SeqCst);
                    assert_eq!((&raw const *value.value) as usize, original_pointer);
                    assert_eq!(*value.value, 7);
                    if matches!(interruption, Interruption::BeforeClose) {
                        drop(before_owner.lock().unwrap().take());
                    }
                },
                move |_, _, _| {
                    let _ = &after_capture;
                    after_count.fetch_add(1, Ordering::SeqCst);
                },
            ))
            .unwrap_err();
        let closing = matches!(
            interruption,
            Interruption::SelectorClose | Interruption::BeforeClose
        );
        assert_eq!(
            error.reason(),
            if closing {
                TaskAccessReason::StoreClosed
            } else {
                TaskAccessReason::ScopeEnded
            }
        );
        assert_eq!(error.progress(), TaskProgress::MayHaveRun);
        assert_eq!(selector_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            before_calls.load(Ordering::SeqCst),
            usize::from(matches!(interruption, Interruption::BeforeClose))
        );
        assert_eq!(after_calls.load(Ordering::SeqCst), 0);
        assert_eq!(callback_drops.load(Ordering::SeqCst), 2);
        assert_eq!(reductions.load(Ordering::SeqCst), 0);

        if !closing {
            let old = removed.lock().unwrap().take().unwrap();
            assert_eq!((&raw const *old.value) as usize, original_pointer);
            assert_eq!(*old.value, 7);
            assert_eq!(original_drops.load(Ordering::SeqCst), 0);
            drop(old);
            assert_eq!(original_drops.load(Ordering::SeqCst), 1);
            let store: Store<State, ()> = owner.lock().unwrap().take().unwrap();
            let read = store.read(|state| {
                state
                    .stack
                    .iter()
                    .map(|(id, value)| (id.clone(), *value.value))
                    .collect::<Vec<_>>()
            });
            let current = driver.finish(read).unwrap();
            match interruption {
                Interruption::SelectorRemove => assert!(current.is_empty()),
                Interruption::SelectorReplace => {
                    assert_eq!(current, vec![(entries[0].0.clone(), 90)]);
                }
                _ => panic!("only selector mutation retains the Store"),
            }
            release.store(true, Ordering::SeqCst);
            driver.finish(store.shutdown()).unwrap();
        }
        for _ in 0..4 {
            driver.turn();
        }
        if closing {
            assert_eq!(
                disposed.lock().unwrap().as_ref().unwrap(),
                &vec![(entries[0].0.clone(), original_pointer, 7)],
                "closure must leave the original entry owned by actor state"
            );
        }
        assert!(owner.lock().unwrap().is_none());
        assert_eq!(original_drops.load(Ordering::SeqCst), 1);
        assert_eq!(
            replacement_drops.load(Ordering::SeqCst),
            usize::from(matches!(interruption, Interruption::SelectorReplace))
        );
        assert!(driver.0.lock().unwrap().is_empty());
    }
}
