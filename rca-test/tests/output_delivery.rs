//! Checks output ownership, assertion debt and handler eligibility across store lifetimes.

use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use futures_executor::block_on;
use futures_util::FutureExt;
use rca_core::{Feature, FeatureContext, MountIdentity, OptionalMount, Output, OutputFlow, Task};
use rca_test::TestStore;

/// Carries a non-cloneable allocation with an independent destruction count.
struct Owned {
    /// Allocation whose address and value identify the transferred payload.
    value: Box<Cell<usize>>,
    /// Counts destruction without retaining the payload itself.
    drops: Arc<AtomicUsize>,
}

impl Drop for Owned {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

/// Hosts an optional child and the payload consumed by its root handler.
struct State {
    /// Presence determines whether the child mount is live.
    child: Option<()>,
    /// Owned output retained until terminal state release.
    value: Option<Owned>,
}

/// Leaves state changes exclusively to output handlers.
struct Root;

impl Feature for Root {
    type State = State;
    type Action = ();
    fn reduce(&mut self, _: &mut State, (): &(), _: &mut FeatureContext<'_, State, ()>) {}
}

/// Supplies an output source without action-driven state changes.
struct Child;

impl Feature for Child {
    type State = ();
    type Action = u8;
    fn reduce(&mut self, (): &mut (), _: &u8, _: &mut FeatureContext<'_, (), u8>) {}
}

/// Mounts the optional child with a fixed identity for this store lifetime.
fn descriptor() -> OptionalMount<State, Child, ()> {
    OptionalMount::new(
        |state: &State| state.child.as_ref(),
        |state: &mut State| state.child.as_mut(),
        |state: &State| {
            state.child.as_ref().map(|()| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        |()| Child,
    )
}

/// Reads the payload value without taking ownership or cloning the allocation.
fn snapshot(state: &State) -> Option<usize> {
    state.value.as_ref().map(|value| value.value.get())
}

/// Adds an independently declared observation to output-boundary assertions.
struct CapturedValue;

impl rca_test::CaptureKey for CapturedValue {
    type Value = usize;
    type Error = ();
}

#[test]
fn typed_output_uses_the_same_pending_frontier_and_snapshot_assertion() {
    block_on(async {
        let drops = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let output_cell = Arc::new(OnceLock::<Output<Owned>>::new());
        let initial_output = Arc::clone(&output_cell);
        let mut owned = Some(Owned {
            value: Box::new(Cell::new(12)),
            drops: Arc::clone(&drops),
        });
        let mount = descriptor().on_initial(
            |()| (),
            move |_, _| {
                let owned = owned.take();
                let output = initial_output.get().unwrap().clone();
                Task::run(move |context| async move {
                    let Some(value) = owned else {
                        return;
                    };
                    context.output(&output, value).await.unwrap();
                })
            },
        );
        let (builder, site) = TestStore::projected_feature_builder(
            State {
                child: Some(()),
                value: None,
            },
            Root,
            snapshot,
        )
        .mount_with_site(mount);
        let observed = Arc::clone(&calls);
        let (builder, output) = builder.output::<Owned>(&site, |handlers| {
            handlers.at_root(move |state, value, _| {
                observed.fetch_add(1, Ordering::SeqCst);
                state.value = Some(value);
                OutputFlow::Consumed
            });
        });
        let (builder, wrong) = builder.output::<Owned>(&site, |_| {});
        output_cell.set(output.clone()).unwrap();
        let mut store = builder.build();
        let initial = store.receive_structural(|_| {}).await.unwrap();
        assert!(
            store
                .receive_output_terminal(&wrong, |_| panic!("wrong terminal declaration"))
                .await
                .unwrap_err()
                .is_no_received_action()
        );
        assert!(
            store
                .receive_output(
                    &wrong,
                    |_| panic!("wrong declaration"),
                    |_| panic!("wrong snapshot"),
                )
                .await
                .unwrap_err()
                .is_no_received_action()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        let _delivery = store
            .receive_output(
                &output,
                |value| value.value.get() == 12,
                |expected| *expected = Some(12),
            )
            .await
            .unwrap();
        initial.finish().await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        store.finish().await.unwrap();
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    });
}

/// Starts one owned output and stops before its root handler commits it.
///
/// The caller must receive or cancel the output and finish the initial task.
/// Intentional assertion failures retain debt until the caller checks teardown.
async fn pending_output_fixture(
    drops: Arc<AtomicUsize>,
    calls: Arc<AtomicUsize>,
) -> (
    TestStore<State, (), Option<usize>>,
    Output<Owned>,
    rca_test::TestActionTask,
) {
    let output_cell = Arc::new(OnceLock::<Output<Owned>>::new());
    let initial_output = Arc::clone(&output_cell);
    let mut owned = Some(Owned {
        value: Box::new(Cell::new(12)),
        drops,
    });
    let mount = descriptor().on_initial(
        |()| (),
        move |_, _| {
            let owned = owned.take();
            let output = initial_output.get().unwrap().clone();
            Task::run(move |context| async move {
                if let Some(value) = owned {
                    let _ = context.output(&output, value).await;
                }
            })
        },
    );
    let (builder, site) = TestStore::projected_feature_builder(
        State {
            child: Some(()),
            value: None,
        },
        Root,
        snapshot,
    )
    .mount_with_site(mount);
    let builder = builder
        .with_capture::<CapturedValue>(|state| Ok(snapshot(state).unwrap_or(0)))
        .unwrap();
    let (builder, output) = builder.output::<Owned>(&site, |handlers| {
        handlers.at_root(move |state, value, _| {
            calls.fetch_add(1, Ordering::SeqCst);
            state.value = Some(value);
            OutputFlow::Consumed
        });
    });
    output_cell.set(output.clone()).unwrap();
    let mut store = builder.build();
    let initial = store.receive_structural(|_| {}).await.unwrap();
    (store, output, initial)
}

#[test]
fn cancelling_after_output_mismatch_keeps_terminal_assertion_debt() {
    block_on(async {
        let drops = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let (mut store, output, initial) =
            pending_output_fixture(Arc::clone(&drops), Arc::clone(&calls)).await;
        let error = store
            .receive_output(
                &output,
                |value| value.value.get() == 99,
                |_| panic!("a failed matcher cannot prepare a snapshot expectation"),
            )
            .await
            .unwrap_err();
        assert!(error.is_action_mismatch());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(drops.load(Ordering::SeqCst), 0);

        initial.cancel().await.unwrap();
        initial.finish().await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(
            store.finish().await.unwrap_err().is_incomplete_assertion(),
            "cancelling an observed output cannot erase failed matcher and terminal debt"
        );
        assert!(
            store
                .receive_output_terminal(&output, |error| {
                    error.reason() == rca_core::TaskAccessReason::Cancelled
                        && error.progress() == rca_core::TaskProgress::NotStarted
                })
                .await
                .unwrap_err()
                .is_incomplete_assertion()
        );
        assert!(store.finish().await.unwrap_err().is_incomplete_assertion());
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(store)))
            .expect_err("Drop must retain failed output matching");
        assert!(panic.downcast_ref::<String>().unwrap().starts_with(
            "TestStore teardown failed: TestStore incomplete assertion for transaction boundary at "
        ));
    });
}

#[test]
fn cancelling_unreceived_output_keeps_terminal_component_without_snapshot_debt() {
    block_on(async {
        let drops = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let (mut store, output, initial) =
            pending_output_fixture(Arc::clone(&drops), Arc::clone(&calls)).await;
        let pending = store.finish().await.unwrap_err();
        assert!(pending.finish_diagnostics().unwrap().has_pending_action());

        initial.cancel().await.unwrap();
        initial.finish().await.unwrap();
        store
            .assert_state(|state| assert!(state.value.is_none()))
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(
            store.finish().await.unwrap_err().is_incomplete_assertion(),
            "an unreceived cancelled output still requires a terminal assertion"
        );
        store
            .receive_output_terminal(&output, |error| {
                error.reason() == rca_core::TaskAccessReason::Cancelled
                    && error.progress() == rca_core::TaskProgress::NotStarted
            })
            .await
            .unwrap();
        store.finish().await.unwrap();
    });
}

#[test]
fn cancellation_during_output_matcher_unwind_preserves_terminal_debt_after_borrow() {
    block_on(async {
        let drops = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let (mut store, output, initial) =
            pending_output_fixture(Arc::clone(&drops), Arc::clone(&calls)).await;
        let panic = std::panic::AssertUnwindSafe(store.receive_output(
            &output,
            |value| {
                initial.cancel().now_or_never().unwrap().unwrap();
                assert_eq!(value.value.get(), 12);
                assert_eq!(drops.load(Ordering::SeqCst), 0);
                assert_eq!(calls.load(Ordering::SeqCst), 0);
                panic!("matcher failed while cancellation awaited its borrow");
            },
            |_| panic!("a panicking matcher cannot prepare a snapshot expectation"),
        ))
        .catch_unwind()
        .await;
        assert_eq!(
            panic.as_ref().unwrap_err().downcast_ref::<&str>(),
            Some(&"matcher failed while cancellation awaited its borrow")
        );
        initial.finish().await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(
            store.finish().await.unwrap_err().is_incomplete_assertion(),
            "returning the borrowed output cannot acknowledge matcher or terminal debt"
        );
        assert!(
            store
                .receive_output_terminal(&output, |error| error.reason()
                    == rca_core::TaskAccessReason::Cancelled)
                .await
                .unwrap_err()
                .is_incomplete_assertion()
        );
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(store)))
            .expect_err("Drop must retain failed output matching after unwind");
        assert!(panic.downcast_ref::<String>().unwrap().starts_with(
            "TestStore teardown failed: TestStore incomplete assertion for transaction boundary at "
        ));
    });
}

#[test]
fn cancellation_before_output_release_admits_no_speculative_snapshot_debt() {
    block_on(async {
        let drops = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let (mut store, output, initial) =
            pending_output_fixture(Arc::clone(&drops), Arc::clone(&calls)).await;
        let error = store
            .receive_output_with_captures(
                &output,
                |_| true,
                |expected| {
                    *expected = Some(99);
                    initial
                        .cancel()
                        .now_or_never()
                        .expect("finite cancellation")
                        .unwrap();
                },
                |captures| captures.set::<CapturedValue>(99),
            )
            .await
            .unwrap_err();
        assert!(error.is_no_received_action());
        assert!(!error.is_incomplete_assertion());
        store
            .assert_state(|state| assert!(state.value.is_none()))
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        initial.finish().await.unwrap();
        assert!(store.finish().await.unwrap_err().is_incomplete_assertion());
        store
            .receive_output_terminal(&output, |error| {
                error.reason() == rca_core::TaskAccessReason::Cancelled
            })
            .await
            .unwrap();
        store.finish().await.unwrap();
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    });
}

#[test]
fn failed_output_matcher_stays_failed_after_successful_payload_cleanup() {
    for panics in [false, true] {
        block_on(async {
            let drops = Arc::new(AtomicUsize::new(0));
            let calls = Arc::new(AtomicUsize::new(0));
            let (mut store, output, initial) =
                pending_output_fixture(Arc::clone(&drops), Arc::clone(&calls)).await;
            let result = std::panic::AssertUnwindSafe(store.receive_output(
                &output,
                |_| {
                    assert!(!panics, "failed payload matcher");
                    false
                },
                |_| panic!("failed matching cannot construct an expectation"),
            ))
            .catch_unwind()
            .await;
            if panics {
                assert_eq!(
                    result.as_ref().unwrap_err().downcast_ref::<&str>(),
                    Some(&"failed payload matcher")
                );
            } else {
                assert!(result.unwrap().unwrap_err().is_action_mismatch());
            }
            let error = store
                .receive_output_with_captures(
                    &output,
                    |_| true,
                    |expected| *expected = Some(12),
                    |captures| captures.set::<CapturedValue>(12),
                )
                .await
                .unwrap_err();
            assert!(error.is_incomplete_assertion());
            initial.finish().await.unwrap();
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            store
                .assert_state(|state| assert_eq!(snapshot(state), Some(12)))
                .await
                .unwrap();
            assert!(store.finish().await.unwrap_err().is_incomplete_assertion());
            let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(store)))
                .expect_err("Drop must retain failed matching after successful payload cleanup");
            assert!(panic.downcast_ref::<String>().unwrap().starts_with(
                "TestStore teardown failed: TestStore incomplete assertion for transaction boundary at "
            ));
            assert_eq!(drops.load(Ordering::SeqCst), 1);
        });
    }
}

#[test]
fn output_component_checks_independent_declared_capture_on_the_same_boundary() {
    block_on(async {
        let drops = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let (mut store, output, initial) =
            pending_output_fixture(Arc::clone(&drops), Arc::clone(&calls)).await;
        let _delivery = store
            .receive_output_with_captures(
                &output,
                |value| value.value.get() == 12,
                |expected| *expected = Some(12),
                |captures| captures.set::<CapturedValue>(12),
            )
            .await
            .unwrap();
        initial.finish().await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        store.finish().await.unwrap();
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    });
}

#[test]
fn committed_output_mismatch_keeps_the_first_assertion_debt() {
    block_on(async {
        let drops = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let (mut store, output, initial) =
            pending_output_fixture(Arc::clone(&drops), Arc::clone(&calls)).await;
        let panic = std::panic::AssertUnwindSafe(store.receive_output_with_captures(
            &output,
            |_| true,
            |expected| *expected = Some(99),
            |captures| captures.set::<CapturedValue>(12),
        ))
        .catch_unwind()
        .await;
        let message = panic
            .as_ref()
            .unwrap_err()
            .downcast_ref::<String>()
            .unwrap();
        assert!(message.starts_with("TestStore state mismatch at "));
        assert!(message.contains(" during receive_output: projected coverage"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        store
            .assert_state(|state| assert_eq!(snapshot(state), Some(12)))
            .await
            .unwrap();
        let error = store
            .receive_output(&output, |_| panic!("unacknowledged debt"), |_| {})
            .await
            .unwrap_err();
        assert!(error.is_incomplete_assertion());
        assert!(store.finish().await.unwrap_err().is_incomplete_assertion());
        drop(initial);
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(store)))
            .expect_err("Drop must retain the first output snapshot mismatch");
        assert!(panic.downcast_ref::<String>().unwrap().starts_with(
            "TestStore teardown failed: TestStore incomplete assertion for receive_output at "
        ));
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    });
}

#[test]
fn deferred_output_builder_owns_captures_not_retained_weak_handle() {
    /// Keeps the complete handler capture alive until the builder releases it.
    struct Capture(Arc<()>);

    let resource = Arc::new(());
    let weak = Arc::downgrade(&resource);
    let capture = Capture(resource);
    let (builder, site) = TestStore::projected_feature_builder(
        State {
            child: Some(()),
            value: None,
        },
        Root,
        snapshot,
    )
    .mount_with_site(descriptor());
    let (builder, output) = builder.output::<Owned>(&site, |handlers| {
        handlers.at_root(move |_, value, _| {
            let _ = &capture;
            let _ = &capture.0;
            OutputFlow::Continue(value)
        });
    });
    assert!(weak.upgrade().is_some());
    drop(builder);
    assert!(weak.upgrade().is_none());
    drop((output, site));
}

#[test]
fn unbound_and_absent_handlers_cannot_intercept_live_owned_output() {
    block_on(async {
        let host = rca_test::DeterministicRuntime::new();
        let (unbuilt, unbound) = rca_core::Store::feature_builder(
            State {
                child: Some(()),
                value: None,
            },
            Root,
            host.runtime(),
        )
        .mount_with_site(descriptor());
        let (builder, source) = rca_core::Store::feature_builder(
            State {
                child: Some(()),
                value: None,
            },
            Root,
            host.runtime(),
        )
        .mount_with_site(descriptor());
        let absent = OptionalMount::new(
            |_: &State| None::<&()>,
            |_: &mut State| None::<&mut ()>,
            |_: &State| None::<MountIdentity<()>>,
            |()| Child,
        );
        let (builder, absent) = builder.mount_with_site(absent);
        let calls = Arc::new(AtomicUsize::new(0));
        let root_calls = Arc::clone(&calls);
        let (builder, output) = builder.output::<Owned>(&source, |handlers| {
            handlers.at(&unbound, |(), _, _| {
                panic!("unbuilt declaration is not a handler")
            });
            handlers.at(&absent, |(), _, _| panic!("absent child is not a handler"));
            handlers.at_root(move |state, value, _| {
                root_calls.fetch_add(1, Ordering::SeqCst);
                assert!(state.value.is_none());
                state.value = Some(value);
                OutputFlow::Consumed
            });
        });
        let store = builder.build().unwrap();
        host.run(store.ready()).await.unwrap().unwrap();
        assert!(
            host.run(store.acquire(&absent))
                .await
                .unwrap()
                .unwrap()
                .is_none()
        );
        let child = host
            .run(store.acquire(&source))
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let drops = Arc::new(AtomicUsize::new(0));
        let value = Owned {
            value: Box::new(Cell::new(31)),
            drops: Arc::clone(&drops),
        };
        let identity = (&raw const *value.value).addr();
        let rca_core::OutputDelivery::Consumed { task } = host
            .run(child.output(&output, value))
            .await
            .unwrap()
            .unwrap()
        else {
            panic!("the live root handler must consume the output");
        };
        host.run(task.finish()).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        let state = host
            .run(store.shutdown_into_state())
            .await
            .unwrap()
            .unwrap();
        let value = state.value.as_ref().unwrap();
        assert_eq!((&raw const *value.value).addr(), identity);
        assert_eq!(value.value.get(), 31);
        drop(state);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        drop(unbuilt);
        drop((child, output, unbound, absent));
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    });
}

#[cfg(feature = "local")]
mod local_handler_registration {
    //! Repeats handler-lifetime checks with payloads that cannot leave their thread.

    use super::{Cell, MountIdentity, Ordering, OutputFlow, block_on};
    use rca_core::local::{Feature, FeatureContext, OptionalMount, Store};
    use std::rc::Rc;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    /// Carries an `Rc` allocation and records when the payload wrapper drops.
    struct Owned {
        /// Thread-local allocation checked through pointer and weak-reference witnesses.
        value: Rc<Cell<usize>>,
        /// Counts wrapper destruction independently of the allocation lifetime.
        drops: Arc<AtomicUsize>,
    }

    impl Drop for Owned {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Hosts the local child and any payload consumed by the root.
    struct State {
        /// Presence determines whether the local child mount is live.
        child: Option<()>,
        /// Output retained until terminal state release.
        value: Option<Owned>,
    }

    /// Leaves local state changes exclusively to output handlers.
    struct Root;

    impl Feature for Root {
        type State = State;
        type Action = ();
        fn reduce(&mut self, _: &mut State, (): &(), _: &mut FeatureContext<'_, State, ()>) {}
    }

    /// Supplies the local output source without action-driven state changes.
    struct Child;

    impl Feature for Child {
        type State = ();
        type Action = u8;
        fn reduce(&mut self, (): &mut (), _: &u8, _: &mut FeatureContext<'_, (), u8>) {}
    }

    /// Mounts the local child with a fixed identity for this store lifetime.
    fn descriptor() -> OptionalMount<State, Child, ()> {
        OptionalMount::new(
            |state: &State| state.child.as_ref(),
            |state: &mut State| state.child.as_mut(),
            |state: &State| {
                state.child.map(|()| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            |()| Child,
        )
    }

    #[test]
    fn unbound_and_absent_handlers_cannot_intercept_live_rc_output() {
        block_on(async {
            let host = rca_test::local::DeterministicRuntime::new();
            let (unbuilt, unbound) = Store::feature_builder(
                State {
                    child: Some(()),
                    value: None,
                },
                Root,
                host.runtime(),
            )
            .mount_with_site(descriptor());
            let (builder, source) = Store::feature_builder(
                State {
                    child: Some(()),
                    value: None,
                },
                Root,
                host.runtime(),
            )
            .mount_with_site(descriptor());
            let absent = OptionalMount::new(
                |_: &State| None::<&()>,
                |_: &mut State| None::<&mut ()>,
                |_: &State| None::<MountIdentity<()>>,
                |()| Child,
            );
            let (builder, absent) = builder.mount_with_site(absent);
            let calls = Rc::new(Cell::new(0));
            let root_calls = Rc::clone(&calls);
            let (builder, output) = builder.output::<Owned>(&source, |handlers| {
                handlers.at(&unbound, |(), _, _| {
                    panic!("unbuilt declaration is not a handler")
                });
                handlers.at(&absent, |(), _, _| panic!("absent child is not a handler"));
                handlers.at_root(move |state, value, _| {
                    root_calls.set(root_calls.get() + 1);
                    assert!(state.value.is_none());
                    state.value = Some(value);
                    OutputFlow::Consumed
                });
            });
            let store = builder.build().unwrap();
            host.run(store.ready()).await.unwrap().unwrap();
            assert!(
                host.run(store.acquire(&absent))
                    .await
                    .unwrap()
                    .unwrap()
                    .is_none()
            );
            let child = host
                .run(store.acquire(&source))
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let drops = Arc::new(AtomicUsize::new(0));
            let value = Rc::new(Cell::new(31));
            let identity = Rc::as_ptr(&value);
            let weak = Rc::downgrade(&value);
            let rca_core::OutputDelivery::Consumed { task } = host
                .run(child.output(
                    &output,
                    Owned {
                        value,
                        drops: Arc::clone(&drops),
                    },
                ))
                .await
                .unwrap()
                .unwrap()
            else {
                panic!("the live root handler must consume the output");
            };
            host.run(task.finish()).await.unwrap();
            assert_eq!(calls.get(), 1);
            assert_eq!(drops.load(Ordering::SeqCst), 0);
            let state = host
                .run(store.shutdown_into_state())
                .await
                .unwrap()
                .unwrap();
            let value = state.value.as_ref().unwrap();
            assert_eq!(Rc::as_ptr(&value.value), identity);
            assert_eq!(value.value.get(), 31);
            drop(state);
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert!(weak.upgrade().is_none());
            drop(unbuilt);
            drop((child, output, unbound, absent));
            assert_eq!(drops.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "One owner-local cross-runtime destructor schedule preserves the real foreign owner and exact Rc lifetime"
    )]
    fn draining_foreign_handler_cannot_intercept_live_rc_output() {
        use futures_util::FutureExt;
        use std::cell::RefCell;

        /// Runs one callback while the foreign child is being destroyed.
        struct DropChild(Option<Box<dyn FnOnce()>>);

        impl Feature for DropChild {
            type State = ();
            type Action = u8;
            fn reduce(&mut self, (): &mut (), _: &u8, _: &mut FeatureContext<'_, (), u8>) {}
        }
        impl Drop for DropChild {
            fn drop(&mut self) {
                self.0.take().unwrap()();
            }
        }

        block_on(async {
            let host = rca_test::local::DeterministicRuntime::new();
            let foreign_host = rca_test::local::DeterministicRuntime::new();
            let callback = Rc::new(RefCell::new(None::<Box<dyn FnOnce()>>));
            let constructor = Rc::clone(&callback);
            let foreign_mount = OptionalMount::new(
                |state: &State| state.child.as_ref(),
                |state: &mut State| state.child.as_mut(),
                |state: &State| {
                    state.child.map(|()| MountIdentity {
                        key: (),
                        replacement: 0,
                        configuration: 0,
                    })
                },
                move |()| DropChild(constructor.borrow_mut().take()),
            );
            let (foreign_builder, foreign_site) = Store::feature_builder(
                State {
                    child: Some(()),
                    value: None,
                },
                Root,
                foreign_host.runtime(),
            )
            .mount_with_site(foreign_mount);
            let (builder, source) = Store::feature_builder(
                State {
                    child: Some(()),
                    value: None,
                },
                Root,
                host.runtime(),
            )
            .mount_with_site(descriptor());
            let phase = Rc::new(Cell::new(0));
            let calls = Rc::new(Cell::new(0));
            let root_phase = Rc::clone(&phase);
            let root_calls = Rc::clone(&calls);
            let (builder, output) = builder.output::<Owned>(&source, |handlers| {
                handlers.at(&foreign_site, |(), _, _| {
                    panic!("drained foreign handler must not enter")
                });
                handlers.at_root(move |state, value, _| {
                    assert_eq!(root_phase.get(), 1);
                    root_calls.set(root_calls.get() + 1);
                    assert!(state.value.is_none());
                    state.value = Some(value);
                    OutputFlow::Consumed
                });
            });
            let store = builder.build().unwrap();
            host.run(store.ready()).await.unwrap().unwrap();
            let child = host
                .run(store.acquire(&source))
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let drops = Arc::new(AtomicUsize::new(0));
            let value = Rc::new(Cell::new(53));
            let identity = Rc::as_ptr(&value);
            let weak = Rc::downgrade(&value);
            let value = Owned {
                value,
                drops: Arc::clone(&drops),
            };
            let callback_host = host.clone();
            let callback_phase = Rc::clone(&phase);
            *callback.borrow_mut() = Some(Box::new(move || {
                assert_eq!(callback_phase.replace(1), 0);
                let mut delivery = std::pin::pin!(child.output(&output, value));
                assert!(delivery.as_mut().now_or_never().is_none());
                callback_host.run_until_stalled().unwrap();
                let rca_core::OutputDelivery::Consumed { task } = delivery
                    .as_mut()
                    .now_or_never()
                    .expect("different live actor answers output")
                    .unwrap()
                else {
                    panic!("root must consume during foreign Feature::drop");
                };
                task.finish()
                    .now_or_never()
                    .expect("handler launches no work");
                callback_phase.set(2);
            }));
            let foreign = foreign_builder.build().unwrap();
            foreign_host.run(foreign.ready()).await.unwrap().unwrap();
            assert!(callback.borrow().is_none());
            assert_eq!(phase.get(), 0);
            foreign_host
                .run(foreign.shutdown_into_state())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(phase.get(), 2);
            assert_eq!(calls.get(), 1);
            assert_eq!(drops.load(Ordering::SeqCst), 0);
            let state = host
                .run(store.shutdown_into_state())
                .await
                .unwrap()
                .unwrap();
            let value = state.value.as_ref().unwrap();
            assert_eq!(Rc::as_ptr(&value.value), identity);
            assert_eq!(value.value.get(), 53);
            drop(state);
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert!(weak.upgrade().is_none());
        });
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One cross-runtime destructor schedule preserves the real foreign owner and exact output lifetime"
)]
fn draining_foreign_handler_cannot_intercept_live_owned_output() {
    /// Runs one callback while the foreign child is being destroyed.
    struct DropChild(Option<Box<dyn FnOnce() + Send>>);

    impl Feature for DropChild {
        type State = ();
        type Action = u8;
        fn reduce(&mut self, (): &mut (), _: &u8, _: &mut FeatureContext<'_, (), u8>) {}
    }
    impl Drop for DropChild {
        fn drop(&mut self) {
            self.0.take().unwrap()();
        }
    }

    block_on(async {
        let host = rca_test::DeterministicRuntime::new();
        let foreign_host = rca_test::DeterministicRuntime::new();
        let callback = Arc::new(std::sync::Mutex::new(None::<Box<dyn FnOnce() + Send>>));
        let constructor = Arc::clone(&callback);
        let foreign_mount = OptionalMount::new(
            |state: &State| state.child.as_ref(),
            |state: &mut State| state.child.as_mut(),
            |state: &State| {
                state.child.map(|()| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            move |()| DropChild(constructor.lock().unwrap().take()),
        );
        let (foreign_builder, foreign_site) = rca_core::Store::feature_builder(
            State {
                child: Some(()),
                value: None,
            },
            Root,
            foreign_host.runtime(),
        )
        .mount_with_site(foreign_mount);
        let (builder, source) = rca_core::Store::feature_builder(
            State {
                child: Some(()),
                value: None,
            },
            Root,
            host.runtime(),
        )
        .mount_with_site(descriptor());
        let phase = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let root_phase = Arc::clone(&phase);
        let root_calls = Arc::clone(&calls);
        let (builder, output) = builder.output::<Owned>(&source, |handlers| {
            handlers.at(&foreign_site, |(), _, _| {
                panic!("drained foreign handler must not enter")
            });
            handlers.at_root(move |state, value, _| {
                assert_eq!(root_phase.load(Ordering::SeqCst), 1);
                root_calls.fetch_add(1, Ordering::SeqCst);
                assert!(state.value.is_none());
                state.value = Some(value);
                OutputFlow::Consumed
            });
        });
        let store = builder.build().unwrap();
        host.run(store.ready()).await.unwrap().unwrap();
        let child = host
            .run(store.acquire(&source))
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let drops = Arc::new(AtomicUsize::new(0));
        let value = Owned {
            value: Box::new(Cell::new(53)),
            drops: Arc::clone(&drops),
        };
        let identity = (&raw const *value.value).addr();
        let callback_host = host.clone();
        let callback_phase = Arc::clone(&phase);
        *callback.lock().unwrap() = Some(Box::new(move || {
            assert_eq!(callback_phase.swap(1, Ordering::SeqCst), 0);
            let mut delivery = std::pin::pin!(child.output(&output, value));
            assert!(delivery.as_mut().now_or_never().is_none());
            callback_host.run_until_stalled().unwrap();
            let rca_core::OutputDelivery::Consumed { task } = delivery
                .as_mut()
                .now_or_never()
                .expect("different live actor answers output")
                .unwrap()
            else {
                panic!("root must consume during foreign Feature::drop");
            };
            task.finish()
                .now_or_never()
                .expect("handler launches no work");
            callback_phase.store(2, Ordering::SeqCst);
        }));
        let foreign = foreign_builder.build().unwrap();
        foreign_host.run(foreign.ready()).await.unwrap().unwrap();
        assert!(callback.lock().unwrap().is_none());
        assert_eq!(phase.load(Ordering::SeqCst), 0);
        foreign_host
            .run(foreign.shutdown_into_state())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(phase.load(Ordering::SeqCst), 2);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        let state = host
            .run(store.shutdown_into_state())
            .await
            .unwrap()
            .unwrap();
        let value = state.value.as_ref().unwrap();
        assert_eq!((&raw const *value.value).addr(), identity);
        assert_eq!(value.value.get(), 53);
        drop(state);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    });
}
