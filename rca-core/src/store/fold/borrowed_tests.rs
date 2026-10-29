//! Checkpoints after non-escaping read metadata destruction.

/// Runs the same ownership checks with Shared and Local callback storage.
macro_rules! cases {
    () => {
        use super::{CallbackLease as _, FoldKey, PureEntry, TypedEntry};
        use crate::store::lifecycle::{CloseState, Lifecycle};
        use crate::store::scope::{ScopeRegistry, ScopeToken};
        #[cfg(feature = "test-util")]
        use crate::store::task_context::TaskAccessError;
        use crate::store::task_context::{TaskAccessReason, TaskProgress};
        use std::ops::ControlFlow;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        /// Adds scalar contributions without introducing capture storage.
        struct Sum;
        impl FoldKey for Sum {
            type Value = usize;
            fn identity() -> usize {
                0
            }
            fn reduce(a: usize, b: usize) -> ControlFlow<usize, usize> {
                ControlFlow::Continue(a + b)
            }
        }
        /// Releases its self-held owner during projection, then closes on final drop.
        struct Registration {
            /// Keeps the weak-only fixture alive until `read_committed` releases it.
            held: Storage<Option<Strong<Self>>>,
            /// Records closure from registration or temporary metadata destruction.
            lifecycle: Arc<Lifecycle>,
            /// Also closes through temporary metadata before the registration is released.
            close_in_read: bool,
        }
        impl Drop for Registration {
            fn drop(&mut self) {
                self.lifecycle.close(CloseState::Abortive);
            }
        }
        impl OutputRegistration for Registration {
            fn installed(&self) -> Option<(Arc<()>, ScopeToken)> {
                None
            }
        }
        /// Closes the Store when temporary projection metadata is destroyed.
        struct ReadMetadata(Arc<Lifecycle>);
        impl Drop for ReadMetadata {
            fn drop(&mut self) {
                self.0.close(CloseState::Abortive);
            }
        }
        impl HandlerRegistration<usize, usize, (), ()> for Registration {
            fn freeze_handler_route(&self, _: &ScopeRegistry) -> Option<Route> {
                None
            }
        }
        impl CommandRegistration<usize, (), ()> for Registration {
            fn freeze_dispatch(&self, _: &ScopeRegistry) -> CommandRoute {
                panic!("fold metadata test must not acquire a command")
            }
        }
        impl Acquisition<usize, usize, (), ()> for Registration {
            fn root_identity(&self) -> Option<Arc<()>> {
                Some(self.lifecycle.root_identity())
            }
            fn read_committed<'a>(
                &self,
                root: &'a usize,
                _: &dyn Fn() -> bool,
            ) -> Option<&'a usize> {
                if self.close_in_read {
                    let metadata = ReadMetadata(Arc::clone(&self.lifecycle));
                    drop(metadata);
                }
                release(self);
                Some(root)
            }
            #[cfg(feature = "test-util")]
            fn validate(&self, _: &Arc<()>) -> Result<(), TaskAccessError> {
                panic!("fold metadata test must not validate acquisition")
            }
            fn acquire(
                self: Strong<Self>,
                _: Arc<()>,
                #[cfg(feature = "test-util")] _: Option<Gate>,
            ) -> AcquireFuture {
                panic!("fold metadata test must not acquire a child")
            }
        }
        #[test]
        fn metadata_and_registration_drop_close_before_contribution() {
            for close_in_read in [false, true] {
                let (lifecycle, _) = ScopeRegistry::for_test();
                let calls = Arc::new(AtomicUsize::new(0));
                let registration = Strong::new(Registration {
                    held: Storage::new(None),
                    lifecycle: Arc::clone(&lifecycle),
                    close_in_read,
                });
                retain(&registration);
                let erased: Strong<dyn Acquisition<usize, usize, (), ()>> = registration;
                let weak = Strong::downgrade(&erased);
                drop(erased);
                let selected = Arc::clone(&calls);
                let entry = TypedEntry::<_, _, _, _, Sum> {
                    registration: weak,
                    contribution: Some(Box::new(move |value| {
                        selected.fetch_add(1, Ordering::SeqCst);
                        *value
                    })),
                    transform: None,
                };
                let error = entry.contribution(&3, &lifecycle).unwrap_err();
                assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
                assert_eq!(error.progress(), TaskProgress::MayHaveRun);
                assert_eq!(calls.load(Ordering::SeqCst), 0);
            }
        }

        #[test]
        #[expect(
            clippy::too_many_lines,
            reason = "one callback identity crosses retirement, unwind, reacquisition and final capture disposal"
        )]
        fn retirement_and_unwound_leases_release_guards_before_capture_drop() {
            /// Checks retired authority and released guards when the capture is destroyed.
            struct InspectDrop<F: Fn()>(F);

            impl<F: Fn()> Drop for InspectDrop<F> {
                fn drop(&mut self) {
                    (self.0)();
                }
            }

            // Compare idle retirement, lease unwinding and retirement during a live lease.
            for mode in 0..3 {
                let (lifecycle, _) = ScopeRegistry::for_test();
                let registration: Strong<dyn Acquisition<usize, usize, (), ()>> =
                    Strong::new(Registration {
                        held: Storage::new(None),
                        lifecycle: Arc::clone(&lifecycle),
                        close_in_read: false,
                    });
                let identity = Strong::as_ptr(&registration).cast::<()>() as usize;
                let fold = super::Fold::<usize, Sum>::new();
                let drops = Arc::new(AtomicUsize::new(0));
                let cell = Strong::<super::EntryCell<usize, Sum>>::new_cyclic(|cell| {
                    let retired_cell = cell.clone();
                    let retired_index = Strong::downgrade(&fold.index);
                    let counted = Arc::clone(&drops);
                    let capture = InspectDrop(move || {
                        let index = retired_index.upgrade().expect("retained fold channel");
                        inspect_storage(&index, |index| {
                            assert!(
                                !index.contains_key(&identity),
                                "retired authority is absent"
                            );
                        });
                        if let Some(cell) = retired_cell.upgrade() {
                            inspect_storage(&cell.payload, |payload| assert!(payload.is_none()));
                        }
                        counted.fetch_add(1, Ordering::SeqCst);
                    });
                    let query_cell = cell.clone();
                    let query_index = Strong::downgrade(&fold.index);
                    super::EntryCell {
                        payload: Storage::new(Some(Box::new(TypedEntry::<_, _, _, _, Sum> {
                            registration: Strong::downgrade(&registration),
                            contribution: Some(Box::new(move |value| {
                                std::hint::black_box(&capture);
                                let index = query_index.upgrade().unwrap();
                                inspect_storage(&index, |index| {
                                    assert!(index.contains_key(&identity));
                                });
                                let cell = query_cell.upgrade().unwrap();
                                inspect_storage(&cell.payload, |payload| {
                                    assert!(payload.is_none())
                                });
                                *value
                            })),
                            transform: None,
                        }))),
                    }
                });
                inspect_storage(&fold.index, |index| {
                    index.insert(
                        identity,
                        super::Dispatch {
                            _source: Strong::<dyn Acquisition<usize, usize, (), ()>>::downgrade(
                                &registration,
                            ),
                            cell: Strong::downgrade(&cell),
                        },
                    );
                });
                let owner = super::EntryOwner {
                    cell,
                    index: Strong::downgrade(&fold.index),
                    identity,
                };

                if mode == 0 {
                    drop(owner);
                } else {
                    let lease = fold.acquire(identity).expect("installed callback");
                    assert_eq!(lease.entry().contribution(&7, &lifecycle).unwrap(), Some(7));
                    if mode == 1 {
                        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            let _lease = lease;
                            panic!("unwind the active lease");
                        }));
                        let panic = panic.expect_err("the active lease must unwind");
                        assert_eq!(
                            panic.downcast_ref::<&str>().copied(),
                            Some("unwind the active lease")
                        );
                        drop(
                            fold.acquire(identity)
                                .expect("unwind restored the callback"),
                        );
                        drop(owner);
                    } else {
                        drop(owner);
                        assert_eq!(drops.load(Ordering::SeqCst), 0);
                        assert!(fold.acquire(identity).is_none());
                        drop(lease);
                    }
                }

                assert_eq!(drops.load(Ordering::SeqCst), 1);
                assert!(fold.acquire(identity).is_none());
            }
        }
    };
}
#[cfg(feature = "local")]
pub(super) use cases;

use crate::store::mount::{Acquisition, commands::Registration as CommandRegistration};
use crate::store::output::{HandlerRegistration, OutputRegistration};
use parking_lot::Mutex as Storage;
use std::sync::Arc as Strong;

/// Carries a frozen handler route without its concrete child type.
type Route = Box<dyn crate::store::output::HandlerRoute<usize, (), usize, ()> + Send>;
/// Keeps command dispatch authority separate from the fold callback.
type CommandRoute = Strong<dyn crate::store::mount::commands::Dispatch<usize, (), ()>>;
/// Matches the Shared acquisition signature without starting acquisition in this fixture.
type AcquireFuture = std::pin::Pin<
    Box<
        dyn std::future::Future<
                Output = Result<Option<crate::TaskContext<usize, ()>>, crate::TaskAccessError>,
            > + Send,
    >,
>;
#[cfg(feature = "test-util")]
/// Names the diagnostic gate argument that the acquisition sentinel rejects.
type Gate = crate::testing::TransactionGateCore<usize, ()>;

/// Keeps one registration owner until the projection explicitly releases it.
fn retain(registration: &Strong<Registration>) {
    *registration.held.lock() = Some(Strong::clone(registration));
}
/// Removes the self-held owner while the caller's upgraded registration remains alive.
fn release(registration: &Registration) {
    drop(registration.held.lock().take());
}
/// Requires the metadata lock to be free before examining its contents.
fn inspect_storage<T, R>(storage: &Storage<T>, inspect: impl FnOnce(&mut T) -> R) -> R {
    let mut guard = storage.try_lock().expect("metadata lock released");
    inspect(&mut guard)
}

cases!();
