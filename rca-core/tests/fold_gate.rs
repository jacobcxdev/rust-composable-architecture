#![cfg(feature = "test-util")]
//! Public Fold diagnostics preserve the exact held transaction and its ownership.
#[cfg(feature = "local")]
#[path = "fold/local_driver.rs"]
mod local_driver;
#[path = "fold/support.rs"]
mod support;

/// Runs the same held-transaction schedules through Shared and Local diagnostic APIs.
macro_rules! gate_cases {
    () => {
        use super::support::poll;
        use domain::{Feature, FeatureContext, Fold, OptionalMount, Runtime, Store};
        use rca_core::{FoldKey, MountIdentity, TaskAccessReason, TaskProgress};
        use std::cell::Cell;
        use std::ops::ControlFlow;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Weak};

        /// Keeps fold values owned and non-`Sync`, including through diagnostic replies.
        struct Owned(Cell<usize>);
        /// Adds contributions without changing the transaction under inspection.
        struct Sum;
        impl FoldKey for Sum {
            type Value = Owned;
            fn identity() -> Owned {
                Owned(Cell::new(0))
            }
            fn reduce(acc: Owned, next: Owned) -> ControlFlow<Owned, Owned> {
                acc.0.set(acc.0.get() + next.0.get());
                ControlFlow::Continue(acc)
            }
        }
        /// Separates callback selection from value and configuration changes.
        struct State {
            child: Option<usize>,
            configuration: u64,
            selections: Arc<AtomicUsize>,
        }
        /// Carries an independently observed payload through the held transaction.
        struct Action {
            payload: Arc<()>,
            refresh: bool,
        }
        /// Changes the child value, optionally invalidating captured fold authority.
        struct Root;
        impl Feature for Root {
            type State = State;
            type Action = Action;
            fn reduce(
                &mut self,
                state: &mut State,
                action: &Action,
                _: &mut FeatureContext<'_, State, Action>,
            ) {
                let _owned_payload = &action.payload;
                if action.refresh {
                    state.configuration += 1;
                }
                *state.child.as_mut().unwrap() += 1;
            }
        }
        /// Owns a weakly observed allocation until the mounted feature is destroyed.
        struct Child(Arc<()>);
        impl Feature for Child {
            type State = usize;
            type Action = ();
            fn reduce(&mut self, _: &mut usize, _: &(), _: &mut FeatureContext<'_, usize, ()>) {
                let _owned_feature = &self.0;
            }
        }
        /// Groups the gate and Store with independent selection, capture and feature probes.
        struct Fixture {
            driver: Driver,
            gate: TransactionGate<State, Action>,
            store: Store<State, Action>,
            fold: Fold<State, Sum>,
            site: domain::MountSite<State, usize, (), Action>,
            calls: Arc<AtomicUsize>,
            selections: Arc<AtomicUsize>,
            feature: Weak<()>,
            callback: Weak<()>,
        }
        /// Completes and acknowledges startup before testing ordinary transaction admission.
        fn fixture() -> Fixture {
            let driver = Driver::default();
            let gate = TransactionGate::new();
            let fold = Fold::<State, Sum>::new();
            let declared = fold.clone();
            let calls = Arc::new(AtomicUsize::new(0));
            let selected = Arc::clone(&calls);
            let selections = Arc::new(AtomicUsize::new(0));
            let feature = Arc::new(());
            let feature_weak = Arc::downgrade(&feature);
            let callback = Arc::new(());
            let callback_weak = Arc::downgrade(&callback);
            let mount = OptionalMount::new(
                |state: &State| {
                    state.selections.fetch_add(1, Ordering::SeqCst);
                    state.child.as_ref()
                },
                |state: &mut State| state.child.as_mut(),
                |state: &State| {
                    state.child.map(|_| MountIdentity {
                        key: (),
                        replacement: 0,
                        configuration: state.configuration,
                    })
                },
                move |_| Child(Arc::clone(&feature)),
            );
            let (builder, site) = Store::feature_builder(
                State {
                    child: Some(2),
                    configuration: 0,
                    selections: Arc::clone(&selections),
                },
                Root,
                Runtime::new(driver.clone()),
            )
            .transaction_gate(gate.clone())
            .mount_with_children(mount, move |_, children| {
                let callback = Arc::clone(&callback);
                let calls = Arc::clone(&selected);
                children.contribute(&declared, move |value| {
                    let _owned_capture = &callback;
                    calls.fetch_add(1, Ordering::SeqCst);
                    Owned(Cell::new(*value))
                });
            });
            let store = builder.build().unwrap();
            driver.turn();
            let boundary = gate.boundary().unwrap();
            let receipt = gate.release_pending_with_receipt().unwrap();
            drop(driver.finish(receipt.finish()).unwrap());
            assert!(gate.acknowledge(boundary));
            driver.finish(store.ready()).unwrap();
            Fixture {
                driver,
                gate,
                store,
                fold,
                site,
                calls,
                selections,
                feature: feature_weak,
                callback: callback_weak,
            }
        }

        #[test]
        fn admitted_fold_is_closed_without_entering_contributions_or_publishing_debt() {
            for acquire in [false, true] {
                let f = fixture();
                let selected = f.selections.load(Ordering::SeqCst);
                let read = if acquire {
                    futures_util::future::Either::Left(f.store.acquire_fold(&f.fold, &f.site))
                } else {
                    futures_util::future::Either::Right(futures_util::FutureExt::map(
                        f.store.fold(&f.fold),
                        |result| result.map(|_| None),
                    ))
                };
                let mut read = Box::pin(read);
                assert!(poll(read.as_mut()).is_pending());
                assert_eq!(f.calls.load(Ordering::SeqCst), 0);
                assert_eq!(f.selections.load(Ordering::SeqCst), selected);
                assert_eq!(f.gate.boundary(), None);
                assert!(!f.gate.has_pending());

                drop(f.store);
                // A transport waiter would reject on closure now. This reply still
                // belongs to the admitted command until the actor drains it.
                assert!(poll(read.as_mut()).is_pending());
                f.driver.turn();
                let std::task::Poll::Ready(Err(error)) = poll(read.as_mut()) else {
                    panic!("shutdown must reject the admitted, unentered fold");
                };
                assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
                assert_eq!(error.progress(), TaskProgress::NotStarted);
                assert_eq!(f.calls.load(Ordering::SeqCst), 0);
                assert_eq!(f.selections.load(Ordering::SeqCst), selected);
                assert_eq!(f.gate.boundary(), None);
                assert!(!f.gate.has_pending());
                assert!(f.feature.upgrade().is_none());
                assert!(f.callback.upgrade().is_none());
                drop(read);
                assert_eq!(Arc::strong_count(&f.calls), 1);
                assert_eq!(Arc::strong_count(&f.selections), 1);
            }
        }

        #[test]
        fn full_mailbox_fold_rejects_shutdown_without_an_actor_turn() {
            for acquire in [false, true] {
                let f = fixture();
                let selected = f.selections.load(Ordering::SeqCst);
                let payload = Arc::new(());
                let receipts: Vec<_> = (0..rca_core::DEFAULT_MAILBOX_CAPACITY)
                    .map(|_| {
                        f.store
                            .try_send(Action {
                                payload: Arc::clone(&payload),
                                refresh: false,
                            })
                            .unwrap_or_else(|_| panic!("fill the bounded ordinary mailbox"))
                    })
                    .collect();
                assert!(
                    f.store
                        .try_send(Action {
                            payload: Arc::clone(&payload),
                            refresh: false,
                        })
                        .unwrap_err()
                        .is_full()
                );
                let read = if acquire {
                    futures_util::future::Either::Left(f.store.acquire_fold(&f.fold, &f.site))
                } else {
                    futures_util::future::Either::Right(futures_util::FutureExt::map(
                        f.store.fold(&f.fold),
                        |result| result.map(|_| None),
                    ))
                };
                let mut read = Box::pin(read);
                assert!(poll(read.as_mut()).is_pending());
                assert_eq!(f.calls.load(Ordering::SeqCst), 0);
                assert_eq!(f.selections.load(Ordering::SeqCst), selected);
                assert_eq!(f.gate.boundary(), None);
                assert!(!f.gate.has_pending());

                drop(f.store);
                let std::task::Poll::Ready(Err(error)) = poll(read.as_mut()) else {
                    panic!("closed transport rejects a waiting fold without actor progress");
                };
                assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
                assert_eq!(error.progress(), TaskProgress::NotStarted);
                assert_eq!(f.calls.load(Ordering::SeqCst), 0);
                assert_eq!(f.selections.load(Ordering::SeqCst), selected);
                assert_eq!(f.gate.boundary(), None);
                assert!(!f.gate.has_pending());
                f.driver.turn();
                for receipt in receipts {
                    assert!(f.driver.finish(receipt.finish()).is_err());
                }
                assert!(f.feature.upgrade().is_none());
                assert!(f.callback.upgrade().is_none());
                assert_eq!(Arc::strong_count(&payload), 1);
                drop(read);
                assert_eq!(Arc::strong_count(&f.calls), 1);
                assert_eq!(Arc::strong_count(&f.selections), 1);
            }
        }

        #[test]
        fn queued_fold_rejects_owner_loss_between_diagnostic_queries() {
            for acquire in [false, true] {
                let f = fixture();
                let receipt = f
                    .store
                    .try_send(Action {
                        payload: Arc::new(()),
                        refresh: false,
                    })
                    .unwrap_or_else(|_| panic!("admitted action"));
                f.driver.turn();
                assert!(f.gate.has_pending());
                let boundary = f.gate.boundary().unwrap();
                let selected = f.selections.load(Ordering::SeqCst);
                let queued = Arc::new(std::sync::Mutex::new(None));
                let returned = Arc::clone(&queued);
                let gate = f.gate.clone();
                let store = f.store;
                let fold = f.fold;
                let site = f.site;
                let mut inspect = Box::pin(
                    f.gate
                        .assert_state(move |state| {
                            assert_eq!(state.child, Some(2));
                            let read = if acquire {
                                futures_util::future::Either::Left(
                                    gate.acquire_fold(&store, &fold, &site),
                                )
                            } else {
                                futures_util::future::Either::Right(futures_util::FutureExt::map(
                                    gate.fold(&store, &fold),
                                    |result| result.map(|_| None),
                                ))
                            };
                            let mut read = Box::pin(read);
                            assert!(poll(read.as_mut()).is_pending());
                            *returned.lock().unwrap() = Some(read);
                            // The diagnostic slot is free, but its successor must not
                            // enter callbacks after this final owner is destroyed.
                            drop(store);
                        })
                        .finish(),
                );
                assert!(poll(inspect.as_mut()).is_pending());
                f.driver.turn();
                f.driver.finish(inspect).unwrap();
                let read = queued.lock().unwrap().take().unwrap();
                let Err(error) = f.driver.finish(read) else {
                    panic!("queued fold must reject closure before entry");
                };
                assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
                assert_eq!(error.progress(), TaskProgress::NotStarted);
                assert_eq!(f.calls.load(Ordering::SeqCst), 0);
                assert_eq!(f.selections.load(Ordering::SeqCst), selected);
                assert!(f.driver.finish(receipt.finish()).is_err());
                assert!(f.feature.upgrade().is_none());
                assert!(f.callback.upgrade().is_none());
                assert_eq!(
                    f.gate.boundary(),
                    Some(boundary),
                    "closure cannot acknowledge debt"
                );
                assert!(!f.gate.has_pending());
            }
        }

        #[test]
        fn foreign_gate_rejects_fold_and_acquisition_without_touching_either_root() {
            let f = fixture();
            let foreign = fixture();
            let receipt = f
                .store
                .try_send(Action {
                    payload: Arc::new(()),
                    refresh: false,
                })
                .unwrap_or_else(|_| panic!("admitted action"));
            f.driver.turn();
            let boundary = f.gate.boundary().unwrap();
            let before = f.gate.pending_diagnostics().unwrap();
            let selected = f.selections.load(Ordering::SeqCst);
            let foreign_selected = foreign.selections.load(Ordering::SeqCst);
            let Err(error) = f.driver.finish(f.gate.fold(&foreign.store, &foreign.fold)) else {
                panic!("foreign gate must reject the fold root");
            };
            assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
            assert_eq!(error.progress(), TaskProgress::NotStarted);
            let Err(error) = f.driver.finish(f.gate.acquire_fold(
                &foreign.store,
                &foreign.fold,
                &foreign.site,
            )) else {
                panic!("foreign gate must reject the acquisition root");
            };
            assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
            assert_eq!(error.progress(), TaskProgress::NotStarted);
            assert_eq!(f.calls.load(Ordering::SeqCst), 0);
            assert_eq!(foreign.calls.load(Ordering::SeqCst), 0);
            assert_eq!(f.selections.load(Ordering::SeqCst), selected);
            assert_eq!(foreign.selections.load(Ordering::SeqCst), foreign_selected);
            assert_eq!(f.gate.boundary(), Some(boundary));
            assert_eq!(f.gate.pending_diagnostics(), Some(before));
            assert!(f.gate.has_pending());
            let release = f.gate.release_pending_with_receipt().unwrap();
            drop(f.driver.finish(release.finish()).unwrap());
            drop(f.driver.finish(receipt.finish()).unwrap());
            assert!(f.gate.acknowledge(boundary));
            drop(f.store);
            f.driver.turn();
            drop(foreign.store);
            foreign.driver.turn();
            assert!(f.feature.upgrade().is_none());
            assert!(foreign.feature.upgrade().is_none());
            assert!(f.callback.upgrade().is_none());
            assert!(foreign.callback.upgrade().is_none());
        }

        #[test]
        fn foreign_gate_rejects_acquisition_after_submitting_owner_expires() {
            let f = fixture();
            let foreign = fixture();
            let receipt = f
                .store
                .try_send(Action {
                    payload: Arc::new(()),
                    refresh: false,
                })
                .unwrap_or_else(|_| panic!("admitted action"));
            f.driver.turn();
            let boundary = f.gate.boundary().unwrap();
            let before = f.gate.pending_diagnostics().unwrap();
            let selected = f.selections.load(Ordering::SeqCst);
            let foreign_selected = foreign.selections.load(Ordering::SeqCst);
            let mut acquire = Box::pin(f.gate.acquire_fold(
                &foreign.store,
                &foreign.fold,
                &foreign.site,
            ));
            assert!(poll(acquire.as_mut()).is_pending());

            // Admission belongs to the still-live foreign owner. Its actor is
            // fully destroyed before the receiving gate executes the query.
            drop(foreign.store);
            foreign.driver.turn();
            assert!(foreign.feature.upgrade().is_none());
            assert!(foreign.callback.upgrade().is_none());
            let Err(error) = f.driver.finish(acquire) else {
                panic!("an expired submitting owner must reject acquisition");
            };
            assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
            assert_eq!(error.progress(), TaskProgress::NotStarted);
            assert_eq!(f.calls.load(Ordering::SeqCst), 0);
            assert_eq!(foreign.calls.load(Ordering::SeqCst), 0);
            assert_eq!(f.selections.load(Ordering::SeqCst), selected);
            assert_eq!(foreign.selections.load(Ordering::SeqCst), foreign_selected);
            f.driver
                .finish(
                    f.gate
                        .assert_state(|state| {
                            assert_eq!(state.child, Some(2));
                            assert_eq!(state.configuration, 0);
                        })
                        .finish(),
                )
                .unwrap();
            assert_eq!(f.gate.boundary(), Some(boundary));
            assert_eq!(f.gate.pending_diagnostics(), Some(before));
            assert!(f.gate.has_pending());

            let release = f.gate.release_pending_with_receipt().unwrap();
            drop(f.driver.finish(release.finish()).unwrap());
            drop(f.driver.finish(receipt.finish()).unwrap());
            assert!(f.gate.acknowledge(boundary));
            drop(f.store);
            f.driver.turn();
            assert!(f.feature.upgrade().is_none());
            assert!(f.callback.upgrade().is_none());
        }

        #[test]
        fn held_fold_preserves_revision_publication_tasks_pending_action_and_assertion_debt() {
            let f = fixture();
            let rendered = format!("{:?}", f.fold);
            assert!(rendered.contains("Fold"));
            assert!(!rendered.contains("Owned") && !rendered.contains("0x"));
            let ordinary = f
                .driver
                .finish(f.store.acquire_fold(&f.fold, &f.site))
                .unwrap()
                .unwrap();
            let view = f
                .driver
                .finish(f.gate.acquire_fold(&f.store, &f.fold, &f.site))
                .unwrap()
                .unwrap();
            let mut observations = f
                .driver
                .finish(f.store.observe(|state| state.child))
                .unwrap();
            assert_eq!(f.driver.finish(observations.next()), Some(Some(2)));
            let mut send = Box::pin(f.store.send(Action {
                payload: Arc::new(()),
                refresh: true,
            }));
            assert!(poll(send.as_mut()).is_pending());
            f.driver.turn();
            let boundary = f.gate.boundary().unwrap();
            let before = f.gate.pending_diagnostics().unwrap();
            let latest = f.gate.latest_diagnostics();
            drop(f.gate.fold(&f.store, &f.fold));
            drop(f.gate.fold_view(&f.store, &view));
            assert_eq!(f.calls.load(Ordering::SeqCst), 0);
            assert_eq!(
                f.driver
                    .finish(f.gate.fold(&f.store, &f.fold))
                    .unwrap()
                    .0
                    .get(),
                2
            );
            assert_eq!(
                f.driver
                    .finish(f.gate.fold_view(&f.store, &view))
                    .unwrap()
                    .0
                    .get(),
                2
            );
            let Err(error) = f.driver.finish(f.gate.fold_view(&f.store, &ordinary)) else {
                panic!("ordinary endpoint must not hide an ingress wait")
            };
            assert_eq!(error.reason(), TaskAccessReason::PhaseDenied);
            assert_eq!(error.progress(), TaskProgress::NotStarted);
            assert_eq!(f.gate.boundary(), Some(boundary));
            assert!(f.gate.has_pending());
            assert_eq!(f.gate.pending_diagnostics(), Some(before));
            assert_eq!(f.gate.latest_diagnostics(), latest);
            let mut publication = Box::pin(observations.next());
            assert!(poll(publication.as_mut()).is_pending());
            assert!(poll(send.as_mut()).is_pending());
            let receipt = f.gate.release_pending_with_receipt().unwrap();
            drop(f.driver.finish(receipt.finish()).unwrap());
            drop(f.driver.finish(send).unwrap());
            assert_eq!(f.gate.boundary(), Some(boundary));
            assert!(!f.gate.has_pending());
            let committed = f.gate.latest_diagnostics();
            let selected = f.selections.load(Ordering::SeqCst);
            let Err(error) = f.driver.finish(f.gate.fold_view(&f.store, &view)) else {
                panic!("configuration invalidates captured authority")
            };
            assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
            assert_eq!(f.selections.load(Ordering::SeqCst), selected);
            assert_eq!(
                f.driver
                    .finish(f.gate.fold(&f.store, &f.fold))
                    .unwrap()
                    .0
                    .get(),
                3
            );
            assert_eq!(
                f.gate.boundary(),
                Some(boundary),
                "query cannot acknowledge assertion debt"
            );
            assert_eq!(f.gate.latest_diagnostics(), committed);
            assert!(f.gate.acknowledge(boundary));
            // Taking a published value is an ordinary actor command, so consume it
            // only after explicitly acknowledging the diagnostic frontier.
            assert_eq!(f.driver.finish(publication), Some(Some(3)));

            f.driver.finish(f.store.shutdown()).unwrap();
            let feature = Weak::clone(&f.feature);
            let callback = Weak::clone(&f.callback);
            drop(f);
            assert!(
                feature.upgrade().is_none(),
                "fixture releases its child feature"
            );
            assert!(
                callback.upgrade().is_none(),
                "fixture releases its fold callback"
            );
        }

        #[test]
        fn retained_diagnostic_view_does_not_keep_pending_payload_feature_or_callback_alive() {
            let f = fixture();
            let view = f
                .driver
                .finish(f.gate.acquire_fold(&f.store, &f.fold, &f.site))
                .unwrap()
                .unwrap();
            let payload = Arc::new(());
            let weak = Arc::downgrade(&payload);
            let receipt = f
                .store
                .try_send(Action {
                    payload,
                    refresh: false,
                })
                .unwrap_or_else(|_| panic!("admitted action"));
            let mut send = Box::pin(receipt.finish());
            assert!(poll(send.as_mut()).is_pending());
            f.driver.turn();
            assert!(f.gate.has_pending());
            assert!(weak.upgrade().is_some());
            let closed_view = f.gate.fold_view(&f.store, &view);
            let expired_view = f.gate.fold_view(&f.store, &view);
            let closed_fold = f.store.fold(&f.fold);
            drop(f.store);
            let Err(error) = f.driver.finish(closed_view) else {
                panic!("diagnostic read must reject a closed owner before actor teardown");
            };
            assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
            assert_eq!(error.progress(), TaskProgress::NotStarted);
            let Err(error) = f.driver.finish(closed_fold) else {
                panic!("ordinary fold must reject a closed owner before actor teardown");
            };
            assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
            assert_eq!(error.progress(), TaskProgress::NotStarted);
            f.driver.turn();
            assert!(weak.upgrade().is_none());
            assert!(f.feature.upgrade().is_none());
            assert!(f.callback.upgrade().is_none());
            assert!(f.driver.finish(send).is_err());
            let Err(error) = f.driver.finish(expired_view) else {
                panic!("diagnostic read must reject an expired owner after actor teardown");
            };
            assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
            assert_eq!(error.progress(), TaskProgress::NotStarted);
            let Err(error) = f.driver.finish(view.read()) else {
                panic!("retained metadata cannot retain the store")
            };
            assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
        }

        #[test]
        fn first_polled_waiter_cannot_retarget_after_its_boundary_is_acknowledged() {
            let f = fixture();
            let view = f
                .driver
                .finish(f.gate.acquire_fold(&f.store, &f.fold, &f.site))
                .unwrap()
                .unwrap();
            let mut send = Box::pin(f.store.send(Action {
                payload: Arc::new(()),
                refresh: false,
            }));
            assert!(poll(send.as_mut()).is_pending());
            f.driver.turn();
            let boundary = f.gate.boundary().unwrap();
            let mut occupy = Box::pin(f.gate.assert_state(|_| {}).finish());
            assert!(poll(occupy.as_mut()).is_pending());
            let mut waiting = Box::pin(f.gate.fold_view(&f.store, &view));
            assert!(poll(waiting.as_mut()).is_pending());
            let receipt = f.gate.release_pending_with_receipt().unwrap();
            drop(f.driver.finish(receipt.finish()).unwrap());
            f.driver.finish(occupy).unwrap();
            drop(f.driver.finish(send).unwrap());
            assert!(f.gate.acknowledge(boundary));
            let receipt = f
                .store
                .try_send(Action {
                    payload: Arc::new(()),
                    refresh: false,
                })
                .unwrap_or_else(|_| panic!("admitted successor"));
            let mut successor = Box::pin(receipt.finish());
            assert!(poll(successor.as_mut()).is_pending());
            f.driver.turn();
            assert_ne!(f.gate.boundary(), Some(boundary));
            let Err(error) = f.driver.finish(waiting) else {
                panic!("old boundary cannot retarget")
            };
            assert_eq!(error.reason(), TaskAccessReason::PhaseDenied);
            assert_eq!(error.progress(), TaskProgress::NotStarted);
            assert_eq!(f.calls.load(Ordering::SeqCst), 0);
            drop(f.store);
            f.driver.turn();
            assert!(f.driver.finish(successor).is_err());
        }
    };
}

mod shared {
    use super::support::Driver;
    use rca_core as domain;
    use rca_core::testing::TransactionGate;
    gate_cases!();
}

#[cfg(feature = "local")]
mod local {
    use super::local_driver::Driver;
    use rca_core::local as domain;
    use rca_core::testing::local::TransactionGate;
    gate_cases!();
}
