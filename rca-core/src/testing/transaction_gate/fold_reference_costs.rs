//! Counts actual extra gate ownership retained by diagnostic Fold endpoints.

use std::sync::Arc;

/// Runs the same reference-count workload with shared or local storage.
macro_rules! endpoint_reference_case {
    ($borrow:ident) => {
        use std::task::{Context, Poll, Waker};

        /// Queues manual test turns on a clock fixed at zero.
        ///
        /// Future sleeps stay pending. A yield wakes once before its next poll.
        #[derive(Clone, Default)]
        struct Driver(Ownership<Storage<Vec<domain::RuntimeFuture>>>);

        impl std::fmt::Debug for Driver {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("FoldReferenceDriver")
            }
        }

        impl domain::RuntimeDriver for Driver {
            fn spawn(&self, task: domain::RuntimeFuture) {
                self.0.$borrow().push(task);
            }

            fn now(&self) -> crate::MonotonicInstant {
                crate::MonotonicInstant::ZERO
            }

            fn sleep_until(&self, deadline: crate::MonotonicInstant) -> domain::RuntimeFuture {
                if deadline <= self.now() {
                    Box::pin(std::future::ready(()))
                } else {
                    Box::pin(std::future::pending())
                }
            }

            fn yield_now(&self) -> domain::RuntimeFuture {
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
            /// Polls queued tasks once, outside the queue's lock or mutable borrow.
            ///
            /// New tasks wait until a later turn. Manual polling does not prove
            /// that a real executor receives or acts on wake requests.
            fn turn(&self) {
                let tasks = std::mem::take(&mut *self.0.$borrow());
                for mut task in tasks {
                    if task
                        .as_mut()
                        .poll(&mut Context::from_waker(Waker::noop()))
                        .is_pending()
                    {
                        self.0.$borrow().push(task);
                    }
                }
            }

            /// Drives a finite request with at most 32 manual turns.
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
                panic!("bounded reference-count fixture");
            }
        }

        /// Leaves state unchanged while the workload measures gate ownership.
        struct Noop<S>(std::marker::PhantomData<fn() -> S>);

        impl<S: 'static> domain::Feature for Noop<S> {
            type State = S;
            type Action = ();

            fn reduce(&mut self, _: &mut S, _: &(), _: &mut domain::FeatureContext<'_, S, ()>) {}
        }

        /// Adds child values without requiring the folded value to be `Sync`.
        struct Sum;

        impl crate::FoldKey for Sum {
            type Value = std::cell::Cell<usize>;

            fn identity() -> Self::Value {
                std::cell::Cell::new(0)
            }

            fn reduce(
                a: Self::Value,
                b: Self::Value,
            ) -> std::ops::ControlFlow<Self::Value, Self::Value> {
                a.set(a.get() + b.get());
                std::ops::ControlFlow::Continue(a)
            }
        }

        /// An elapsed deadline is ready without a driver turn.
        #[test]
        fn driver_elapsed_sleep_is_ready_on_first_poll() {
            let driver = Driver::default();
            let mut sleep =
                domain::RuntimeDriver::sleep_until(&driver, crate::MonotonicInstant::ZERO);
            let mut context = Context::from_waker(Waker::noop());

            assert_eq!(sleep.as_mut().poll(&mut context), Poll::Ready(()));
        }

        /// Future deadlines stay pending because this fixture's clock never advances.
        #[test]
        fn driver_future_sleeps_stay_pending() {
            let driver = Driver::default();
            let mut context = Context::from_waker(Waker::noop());

            for deadline in [
                crate::MonotonicInstant::from_elapsed(std::time::Duration::from_nanos(1)),
                crate::MonotonicInstant::MAX,
            ] {
                let mut sleep = domain::RuntimeDriver::sleep_until(&driver, deadline);
                assert_eq!(sleep.as_mut().poll(&mut context), Poll::Pending);
                driver.turn();
                assert_eq!(sleep.as_mut().poll(&mut context), Poll::Pending);
            }
        }

        /// A yield requests another poll before it completes.
        #[test]
        fn driver_yield_wakes_once_before_completing() {
            use std::sync::atomic::{AtomicUsize, Ordering};
            use std::task::Wake;

            /// Counts wake requests from the directly polled yield future.
            #[derive(Default)]
            struct WakeCount(AtomicUsize);

            impl Wake for WakeCount {
                fn wake(self: Arc<Self>) {
                    self.0.fetch_add(1, Ordering::SeqCst);
                }
            }

            let driver = Driver::default();
            let notifications = Arc::new(WakeCount::default());
            let waker = Waker::from(Arc::clone(&notifications));
            let mut context = Context::from_waker(&waker);
            let mut yielded = domain::RuntimeDriver::yield_now(&driver);

            assert_eq!(notifications.0.load(Ordering::SeqCst), 0);
            assert_eq!(yielded.as_mut().poll(&mut context), Poll::Pending);
            assert_eq!(notifications.0.load(Ordering::SeqCst), 1);
            assert_eq!(yielded.as_mut().poll(&mut context), Poll::Ready(()));
            assert_eq!(notifications.0.load(Ordering::SeqCst), 1);
        }

        /// Measures gate references at each endpoint ownership boundary.
        #[test]
        fn diagnostic_endpoint_gate_reference_deltas_are_explicit() {
            let driver = Driver::default();
            let gate = Gate::new();
            let fold = domain::Fold::<Option<usize>, Sum>::new();
            let declared = fold.clone();
            let mount = domain::OptionalMount::new(
                Option::as_ref,
                Option::as_mut,
                |state: &Option<usize>| {
                    state.map(|_| crate::MountIdentity {
                        key: (),
                        replacement: 0,
                        configuration: 0,
                    })
                },
                |_| Noop::<usize>(std::marker::PhantomData),
            );
            let (builder, site) = domain::Store::feature_builder(
                Some(2),
                Noop::<Option<usize>>(std::marker::PhantomData),
                domain::Runtime::new(driver.clone()),
            )
            .transaction_gate(gate.clone())
            .mount_with_children(mount, move |_, children| {
                children.contribute(&declared, |value| std::cell::Cell::new(*value));
            });
            let store = builder.build().unwrap();
            driver.turn();
            let boundary = gate.boundary().unwrap();
            drop(
                driver
                    .finish(gate.release_pending_with_receipt().unwrap().finish())
                    .unwrap(),
            );
            assert!(gate.acknowledge(boundary));
            driver.finish(store.ready()).unwrap();
            driver.turn();

            let baseline = Arc::strong_count(&gate.core.shared);
            let ordinary = driver
                .finish(store.acquire_fold(&fold, &site))
                .unwrap()
                .unwrap();
            assert_eq!(Arc::strong_count(&gate.core.shared), baseline);

            let unpolled = gate.acquire_fold(&store, &fold, &site);
            assert_eq!(Arc::strong_count(&gate.core.shared), baseline + 2);
            drop(unpolled);
            assert_eq!(Arc::strong_count(&gate.core.shared), baseline);

            let view = driver
                .finish(gate.acquire_fold(&store, &fold, &site))
                .unwrap()
                .unwrap();
            assert_eq!(Arc::strong_count(&gate.core.shared), baseline + 1);
            let clone = view.clone();
            assert_eq!(
                Arc::strong_count(&gate.core.shared),
                baseline + 1,
                "view clone shares its endpoint"
            );

            let read = gate.fold_view(&store, &view);
            assert_eq!(Arc::strong_count(&gate.core.shared), baseline + 2);
            drop(read);
            assert_eq!(Arc::strong_count(&gate.core.shared), baseline + 1);

            drop(store);
            driver.turn();
            assert_eq!(
                Arc::strong_count(&gate.core.shared),
                2,
                "only facade and shared endpoint remain"
            );
            drop(view);
            drop(clone);
            drop(ordinary);
            assert_eq!(Arc::strong_count(&gate.core.shared), 1);
            eprintln!(
                "Fold diagnostic gate refs: baseline={baseline}; unpolled acquire +2; retained endpoint +1; view clone +0; unpolled read +1; after Store drop 2; after views drop 1; optional gate field bytes={}",
                std::mem::size_of::<Option<Gate>>()
            );
        }
    };
}

#[cfg(feature = "local")]
pub(super) use endpoint_reference_case;

/// Uses shared ownership and locking for the reference-count workload.
mod shared {
    use super::Arc;
    use crate as domain;
    use parking_lot::Mutex as Storage;
    use std::sync::Arc as Ownership;

    /// Checks a root with one optional child and no actions.
    type Gate = super::super::TransactionGate<Option<usize>, ()>;

    endpoint_reference_case!(lock);
}
