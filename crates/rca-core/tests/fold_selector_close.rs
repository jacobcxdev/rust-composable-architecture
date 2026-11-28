//! Structural selector closure stops before the next selector or contribution.
use rca_core::{FoldKey, MountIdentity, TaskAccessReason, TaskProgress};
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
#[cfg(feature = "local")]
#[path = "fold/local_driver.rs"]
mod local_driver;
#[path = "fold/support.rs"]
mod support;

/// Counts wake requests without scheduling or polling another task.
#[derive(Default)]
struct WakeCount(AtomicUsize);

impl std::task::Wake for WakeCount {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// Runs the same selector and projection schedules through Shared and Local APIs.
macro_rules! selector_cases {
    () => {
        use super::{
            Arc, AtomicBool, AtomicUsize, ControlFlow, FoldKey, MountIdentity, Mutex, Ordering,
            TaskAccessReason, TaskProgress,
        };
        use domain::{Feature, FeatureContext, Fold, OptionalMount, Runtime, Store};

        #[test]
        fn driver_clock_respects_elapsed_and_future_deadlines() {
            use domain::RuntimeDriver as _;
            use rca_core::MonotonicInstant;
            use std::task::{Context, Waker};
            use std::time::Duration;

            let driver = Driver::default();
            let notifications = Arc::new(super::WakeCount::default());
            let waker = Waker::from(Arc::clone(&notifications));
            let mut context = Context::from_waker(&waker);
            assert_eq!(driver.now(), MonotonicInstant::ZERO);

            let mut elapsed = driver.sleep_until(MonotonicInstant::ZERO);
            assert!(
                elapsed.as_mut().poll(&mut context).is_ready(),
                "elapsed deadline must be ready on its first poll"
            );

            for deadline in [
                MonotonicInstant::from_elapsed(Duration::from_nanos(1)),
                MonotonicInstant::MAX,
            ] {
                let mut sleep = driver.sleep_until(deadline);
                assert!(sleep.as_mut().poll(&mut context).is_pending());
                assert!(sleep.as_mut().poll(&mut context).is_pending());
                assert_eq!(driver.now(), MonotonicInstant::ZERO);
            }
            assert_eq!(notifications.0.load(Ordering::SeqCst), 0);
        }

        #[test]
        fn driver_yield_suspends_and_wakes_once_before_completion() {
            use domain::RuntimeDriver as _;
            use std::task::{Context, Waker};

            let driver = Driver::default();
            let notifications = Arc::new(super::WakeCount::default());
            let waker = Waker::from(Arc::clone(&notifications));
            let mut context = Context::from_waker(&waker);
            let mut yielded = driver.yield_now();
            assert_eq!(notifications.0.load(Ordering::SeqCst), 0);
            assert!(
                yielded.as_mut().poll(&mut context).is_pending(),
                "yield must offer a scheduling turn"
            );
            assert_eq!(notifications.0.load(Ordering::SeqCst), 1);
            assert!(yielded.as_mut().poll(&mut context).is_ready());
            assert_eq!(notifications.0.load(Ordering::SeqCst), 1);
            assert_eq!(driver.now(), rca_core::MonotonicInstant::ZERO);
        }

        /// Adds child values so callback counts can detect forbidden traversal.
        struct Sum;
        impl FoldKey for Sum {
            type Value = usize;
            fn identity() -> usize {
                0
            }
            fn reduce(value: usize, next: usize) -> ControlFlow<usize, usize> {
                ControlFlow::Continue(value + next)
            }
        }
        /// Lets a selector drop the last Store owner outside the mutex guard.
        type Owner = Arc<Mutex<Option<Store<State, ()>>>>;
        /// Enables closure only after construction has finished selecting children.
        struct State {
            panel: Option<PanelState>,
            owner: Owner,
            enabled: Arc<AtomicBool>,
            close_parent: bool,
        }
        /// Counts nested selections separately from the root selector.
        struct PanelState {
            leaf: Option<usize>,
            owner: Owner,
            enabled: Arc<AtomicBool>,
            close_parent: bool,
            selectors: Arc<AtomicUsize>,
        }
        /// Leaves root data unchanged while selectors supply the terminal event.
        struct Root;
        impl Feature for Root {
            type State = State;
            type Action = ();
            fn update(&mut self, _: &mut State, _: &(), _: &mut FeatureContext<'_, State, ()>) {}
        }
        /// Keeps the intermediate owner present without reducer side effects.
        struct Panel;
        impl Feature for Panel {
            type State = PanelState;
            type Action = ();
            fn update(
                &mut self,
                _: &mut PanelState,
                _: &(),
                _: &mut FeatureContext<'_, PanelState, ()>,
            ) {
            }
        }
        /// Supplies the final child whose contribution must be suppressed after closure.
        struct Leaf;
        impl Feature for Leaf {
            type State = usize;
            type Action = ();
            fn update(&mut self, _: &mut usize, _: &(), _: &mut FeatureContext<'_, usize, ()>) {}
        }
        /// Removes the owner under the lock, then drops it after unlocking.
        fn close(owner: &Owner) {
            let store = owner.lock().unwrap().take();
            drop(store);
        }
        /// Checks the selected closure point without letting setup counts satisfy assertions.
        #[expect(
            clippy::too_many_lines,
            reason = "one shutdown schedule keeps selector ordering and callback suppression assertions together"
        )]
        fn selected(close_parent: bool, transformed: bool) {
            let driver = Driver::default();
            let owner = Arc::new(Mutex::new(None));
            let enabled = Arc::new(AtomicBool::new(false));
            let selectors = Arc::new(AtomicUsize::new(0));
            let calls = Arc::new(AtomicUsize::new(0));
            let transforms = Arc::new(AtomicUsize::new(0));
            let fold = Fold::<State, Sum>::new();
            let declared = fold.clone();
            let selected_calls = Arc::clone(&calls);
            let selected_transforms = Arc::clone(&transforms);
            let state = State {
                panel: Some(PanelState {
                    leaf: Some(3),
                    owner: Arc::clone(&owner),
                    enabled: Arc::clone(&enabled),
                    close_parent,
                    selectors: Arc::clone(&selectors),
                }),
                owner: Arc::clone(&owner),
                enabled: Arc::clone(&enabled),
                close_parent,
            };
            let parent = OptionalMount::new(
                |state: &State| {
                    if state.close_parent && state.enabled.load(Ordering::SeqCst) {
                        close(&state.owner);
                    }
                    state.panel.as_ref()
                },
                |state: &mut State| state.panel.as_mut(),
                |_: &State| {
                    Some(MountIdentity {
                        key: (),
                        replacement: 0,
                        configuration: 0,
                    })
                },
                |_| Panel,
            );
            let (builder, _) = Store::builder(state, Root, Runtime::new(driver.clone()))
                .mount_with_children(parent, move |_, children| {
                    let fold = declared.clone();
                    let calls = Arc::clone(&selected_calls);
                    let transforms = Arc::clone(&selected_transforms);
                    children.optional_with_children(
                        OptionalMount::new(
                            |panel: &PanelState| {
                                panel.selectors.fetch_add(1, Ordering::SeqCst);
                                if !panel.close_parent && panel.enabled.load(Ordering::SeqCst) {
                                    close(&panel.owner);
                                }
                                panel.leaf.as_ref()
                            },
                            |panel: &mut PanelState| panel.leaf.as_mut(),
                            |_: &PanelState| {
                                Some(MountIdentity {
                                    key: (),
                                    replacement: 0,
                                    configuration: 0,
                                })
                            },
                            |_| Leaf,
                        ),
                        move |_, children| {
                            let calls = Arc::clone(&calls);
                            let contribute = move |value: &usize| {
                                calls.fetch_add(1, Ordering::SeqCst);
                                *value
                            };
                            if transformed {
                                let transforms = Arc::clone(&transforms);
                                children.contribute_transformed(&fold, contribute, move |value| {
                                    transforms.fetch_add(1, Ordering::SeqCst);
                                    value
                                });
                            } else {
                                children.contribute(&fold, contribute);
                            }
                        },
                    );
                });
            let store = builder.build().unwrap();
            driver.finish(store.ready()).unwrap();
            selectors.store(0, Ordering::SeqCst);
            enabled.store(true, Ordering::SeqCst);
            let read = store.fold(&fold);
            *owner.lock().unwrap() = Some(store);
            let error = driver.finish(read).unwrap_err();
            assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
            assert_eq!(error.progress(), TaskProgress::MayHaveRun);
            assert_eq!(
                selectors.load(Ordering::SeqCst),
                usize::from(!close_parent),
                "closed parent suppresses the nested selector"
            );
            assert_eq!(
                calls.load(Ordering::SeqCst),
                0,
                "closed selector suppresses contribution"
            );
            assert_eq!(
                transforms.load(Ordering::SeqCst),
                0,
                "closed selector suppresses transform"
            );
        }
        #[test]
        fn parent_selector_close_stops_nested_selector() {
            selected(true, false);
        }
        #[test]
        fn leaf_selector_close_stops_contribution() {
            selected(false, false);
        }
        #[test]
        fn transformed_leaf_selector_close_stops_contribution_and_transform() {
            selected(false, true);
        }
        #[test]
        fn live_owner_missing_projection_rejects_then_restores_fold_callbacks() {
            /// Hides a projection without retiring its still-live mounted owner.
            struct ProjectionState {
                leaf: Option<usize>,
                visible: Arc<AtomicBool>,
            }
            /// Preserves the owner while the test changes projection visibility externally.
            struct ProjectionRoot;
            impl Feature for ProjectionRoot {
                type State = ProjectionState;
                type Action = ();
                fn update(
                    &mut self,
                    _: &mut ProjectionState,
                    _: &(),
                    _: &mut FeatureContext<'_, ProjectionState, ()>,
                ) {
                }
            }
            let driver = Driver::default();
            let visible = Arc::new(AtomicBool::new(true));
            let calls = Arc::new(AtomicUsize::new(0));
            let transforms = Arc::new(AtomicUsize::new(0));
            let fold = Fold::<ProjectionState, Sum>::new();
            let declared = fold.clone();
            let contributed = Arc::clone(&calls);
            let transformed = Arc::clone(&transforms);
            let mount = OptionalMount::new(
                |state: &ProjectionState| {
                    if state.visible.load(Ordering::SeqCst) {
                        state.leaf.as_ref()
                    } else {
                        None
                    }
                },
                |state: &mut ProjectionState| state.leaf.as_mut(),
                |state: &ProjectionState| {
                    state.leaf.map(|_| MountIdentity {
                        key: (),
                        replacement: 0,
                        configuration: 0,
                    })
                },
                |_| Leaf,
            );
            let (builder, _) = Store::builder(
                ProjectionState {
                    leaf: Some(7),
                    visible: Arc::clone(&visible),
                },
                ProjectionRoot,
                Runtime::new(driver.clone()),
            )
            .mount_with_children(mount, move |_, children| {
                let contributed = Arc::clone(&contributed);
                let transformed = Arc::clone(&transformed);
                children.contribute_transformed(
                    &declared,
                    move |value| {
                        contributed.fetch_add(1, Ordering::SeqCst);
                        *value
                    },
                    move |value| {
                        transformed.fetch_add(1, Ordering::SeqCst);
                        value * 2
                    },
                );
            });
            let store = builder.build().unwrap();
            driver.finish(store.ready()).unwrap();
            assert_eq!(driver.finish(store.fold(&fold)).unwrap(), 14);
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert_eq!(transforms.load(Ordering::SeqCst), 1);

            visible.store(false, Ordering::SeqCst);
            let Err(error) = driver.finish(store.fold(&fold)) else {
                panic!("a live owner with no projected child must reject the fold");
            };
            assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
            assert_eq!(error.progress(), TaskProgress::MayHaveRun);
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert_eq!(transforms.load(Ordering::SeqCst), 1);

            visible.store(true, Ordering::SeqCst);
            assert_eq!(driver.finish(store.fold(&fold)).unwrap(), 14);
            assert_eq!(calls.load(Ordering::SeqCst), 2);
            assert_eq!(transforms.load(Ordering::SeqCst), 2);
            driver.finish(store.shutdown()).unwrap();
        }
    };
}
mod shared {
    use super::support::Driver;
    use rca_core as domain;
    selector_cases!();
}
#[cfg(feature = "local")]
mod local {
    use super::local_driver::Driver;
    use rca_core::local as domain;
    selector_cases!();
}
