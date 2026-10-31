//! Checks shared ownership, captured projections, access failures and explicit publication.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};

use rca_core::Shared;

#[cfg(not(miri))]
#[path = "../src/allocation_test/child.rs"]
mod subprocess;

#[test]
fn captured_reference_cleanup_panic_drops_read_and_mutation_results() {
    /// Panics only for the original value after its last captured owner is released.
    struct ValueDrop(bool);
    impl Drop for ValueDrop {
        fn drop(&mut self) {
            assert!(!self.0, "captured value cleanup");
        }
    }
    /// Counts result destruction even when releasing its source value panics.
    struct ResultDrop(Rc<Cell<usize>>);
    impl Drop for ResultDrop {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    let mut observed = Vec::new();
    for operation in 0..4 {
        let shared = Shared::new(ValueDrop(true));
        let replacement = Shared::new(ValueDrop(false));
        let drops = Rc::new(Cell::new(0));
        let output = || ResultDrop(Rc::clone(&drops));
        let rebind = || shared.rebind(&replacement).unwrap();
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match operation {
            0 => drop(shared.read(|_| {
                rebind();
                output()
            })),
            1 => drop(shared.update(|_| output(), rebind)),
            2 => drop(shared.try_update(|_| Ok::<_, ()>(output()), rebind)),
            3 => drop(shared.try_update(|_| Err::<(), _>(output()), rebind)),
            _ => unreachable!(),
        }))
        .expect_err("last captured value cleanup must propagate its panic");
        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"captured value cleanup")
        );
        assert!(!shared.read(|value| value.0).unwrap());
        observed.push(drops.get());
    }
    assert_eq!(
        observed,
        [1, 1, 1, 1],
        "read, update, try-update success and typed failure must all release their output"
    );
}

// A copied value or retargeted capture would lose one of these mutations.
#[test]
fn aliases_follow_cohort_rebind_but_readers_and_projections_capture_reference() {
    let shared = Shared::new((1, 2));
    let alias = shared.clone();
    let reader = shared.reader().unwrap();
    let projected = shared
        .project(|value| Some(&value.0), |value| Some(&mut value.0))
        .unwrap();
    let projected_reader = projected.reader();
    shared
        .try_update(
            |value| {
                value.0 = 3;
                Ok::<_, ()>(())
            },
            || {},
        )
        .unwrap();
    assert_eq!(alias.read(|value| value.0).unwrap(), 3);
    assert_eq!(reader.read(|value| value.0).unwrap(), 3);
    let replacement = Shared::new((10, 20));
    shared.rebind(&replacement).unwrap();
    assert_eq!(alias.read(|value| value.0).unwrap(), 10);
    projected
        .try_update(
            |value| {
                *value = 7;
                Ok::<_, ()>(())
            },
            || {},
        )
        .unwrap();
    assert_eq!(reader.read(|value| value.0).unwrap(), 7);
    assert_eq!(projected_reader.read(|value| *value).unwrap(), 7);
    alias
        .try_update(
            |value| {
                value.0 = 11;
                Ok::<_, ()>(())
            },
            || {},
        )
        .unwrap();
    assert_eq!(replacement.read(|value| value.0).unwrap(), 11);
    // Rebinding replacement's independently created cohort does not move alias.
    replacement.rebind(&Shared::new((30, 40))).unwrap();
    assert_eq!(alias.read(|value| value.0).unwrap(), 11);
}

#[test]
fn cloned_projection_keeps_its_original_nonclone_root_after_rebind() {
    /// Records when the last projection releases its move-only root.
    struct Root {
        value: u32,
        drops: Rc<Cell<usize>>,
    }
    impl Drop for Root {
        fn drop(&mut self) {
            self.drops.set(self.drops.get() + 1);
        }
    }

    let old_drops = Rc::new(Cell::new(0));
    let replacement_drops = Rc::new(Cell::new(0));
    let shared = Shared::new(Root {
        value: 1,
        drops: Rc::clone(&old_drops),
    });
    let projection = shared
        .project(|root| Some(&root.value), |root| Some(&mut root.value))
        .unwrap();
    let cloned = projection.clone();
    drop(projection);

    let replacement = Shared::new(Root {
        value: 10,
        drops: Rc::clone(&replacement_drops),
    });
    shared.rebind(&replacement).unwrap();
    assert_eq!(old_drops.get(), 0);

    let publications = Cell::new(0);
    cloned
        .try_update(
            |value| {
                *value = 7;
                Ok::<_, ()>(())
            },
            || publications.set(publications.get() + 1),
        )
        .unwrap();
    assert_eq!(cloned.read(|value| *value).unwrap(), 7);
    assert_eq!(publications.get(), 1);
    assert_eq!(shared.read(|root| root.value).unwrap(), 10);
    assert_eq!(replacement.read(|root| root.value).unwrap(), 10);
    assert_eq!(old_drops.get(), 0);

    drop(cloned);
    assert_eq!(old_drops.get(), 1);
    assert_eq!(replacement_drops.get(), 0);
}

// A callback inside the mutex would see Busy rather than the committed value.
#[test]
fn publication_runs_after_unlock_and_can_mutate_or_rebind() {
    let shared = Shared::new(1);
    let published = Cell::new(0);
    shared
        .try_update(
            |value| {
                *value = 2;
                Ok::<_, ()>(())
            },
            || {
                published.set(shared.read(|value| *value).unwrap());
                shared
                    .try_update(
                        |value| {
                            *value = 3;
                            Ok::<_, ()>(())
                        },
                        || {},
                    )
                    .unwrap();
                shared.rebind(&Shared::new(4)).unwrap();
            },
        )
        .unwrap();
    assert_eq!(published.get(), 2);
    assert_eq!(shared.read(|value| *value).unwrap(), 4);
}

// Swallowing Err or claiming rollback would lose the original move-only cause.
#[test]
fn mutation_error_retains_partial_commit_and_original_typed_cause() {
    /// Makes a returned cause's original local allocation observable.
    struct Failure(Rc<Cell<usize>>);
    let cause = Rc::new(Cell::new(17));
    let shared = Shared::new(1);
    let published = Cell::new(0);
    let error = shared
        .try_update(
            |value| {
                *value = 9;
                Err::<(), _>(Failure(Rc::clone(&cause)))
            },
            || published.set(shared.read(|value| *value).unwrap()),
        )
        .unwrap_err();
    assert!(error.is_committed());
    assert!(error.access_error().is_none());
    let failure = error.into_failure().unwrap();
    assert!(Rc::ptr_eq(&cause, &failure.0));
    assert_eq!(published.get(), 9);
}

// Blocking locks would deadlock; allowing reentry would expose partial state.
#[test]
fn overlapping_read_and_mutation_fail_without_publishing() {
    let shared = Shared::new(1);
    let called = Cell::new(false);
    shared
        .try_update(
            |value| {
                *value = 2;
                assert!(shared.read(|value| *value).unwrap_err().is_busy());
                let error = shared
                    .try_update(|_| Ok::<_, ()>(()), || called.set(true))
                    .unwrap_err();
                assert!(error.access_error().unwrap().is_busy());
                assert!(!error.is_committed());
                Ok::<_, ()>(())
            },
            || {},
        )
        .unwrap();
    assert!(!called.get());
}

// Lock failure takes precedence over an absent path and must skip both callbacks.
#[test]
fn projected_reads_skip_callbacks_until_access_is_available() {
    let shared = Shared::new(Some(String::from("original")));
    let paths = Cell::new(0);
    let reads = Cell::new(0);
    let projected = shared.reader().unwrap().project(|value| {
        paths.set(paths.get() + 1);
        value.as_ref()
    });
    let read = |value: &String| {
        reads.set(reads.get() + 1);
        value.len()
    };

    shared
        .read(|value| {
            assert_eq!(value.as_deref(), Some("original"));
            assert!(projected.read(read).unwrap_err().is_busy());
        })
        .unwrap();
    assert_eq!((paths.get(), reads.get()), (0, 0));
    assert_eq!(projected.read(read).unwrap(), 8);
    assert_eq!((paths.get(), reads.get()), (1, 1));

    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        shared.update(
            |value| {
                *value = None;
                panic!("interrupted mutation");
            },
            || panic!("a panicked mutation must not publish"),
        )
    }))
    .unwrap_err();
    assert_eq!(panic.downcast_ref::<&str>(), Some(&"interrupted mutation"));
    assert!(projected.read(read).unwrap_err().is_poisoned());
    assert_eq!((paths.get(), reads.get()), (1, 1));
    shared
        .rebind(&Shared::new(Some(String::from("replacement"))))
        .unwrap();
    assert!(
        shared
            .read(|value| value.as_deref() == Some("replacement"))
            .unwrap()
    );
    assert!(projected.read(read).unwrap_err().is_poisoned());
    assert_eq!((paths.get(), reads.get()), (1, 1));
}

// Removing a payload must neither return a cache nor run the mutation callback.
#[test]
fn absent_projection_has_no_cache_and_never_resurrects_optional_payload() {
    let shared = Shared::new(Some(3));
    let projected = shared.project(Option::as_ref, Option::as_mut).unwrap();
    assert_eq!(projected.read(|value| *value).unwrap(), 3);
    shared
        .try_update(
            |value| {
                *value = None;
                Ok::<_, ()>(())
            },
            || {},
        )
        .unwrap();
    assert!(projected.read(|value| *value).unwrap_err().is_absent());
    let called = Cell::new(false);
    let error = projected.try_update(
        |_| -> Result<(), ()> { panic!("absent mutation ran") },
        || called.set(true),
    );
    let error: rca_core::SharedMutationError<()> = error.unwrap_err();
    assert!(error.access_error().unwrap().is_absent());
    assert!(!error.is_committed());
    assert!(!called.get());
    assert_eq!(shared.read(|value| *value).unwrap(), None);
    shared
        .try_update(
            |value| {
                *value = Some(5);
                Ok::<_, ()>(())
            },
            || {},
        )
        .unwrap();
    assert_eq!(projected.read(|value| *value).unwrap(), 5);
}

// A panic during mutation poisons access; callback panic must not poison it.
#[test]
fn mutation_poison_is_explicit_and_publication_panic_is_after_commit() {
    let shared = Shared::new(0);
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _: Result<(), rca_core::SharedMutationError<()>> = shared.try_update(
            |value| {
                *value = 1;
                panic!("mutation panic");
            },
            || panic!("must not publish a panicked mutation"),
        );
    }));
    let panic = panicked.expect_err("the mutation panic must propagate");
    assert_eq!(panic.downcast_ref::<&str>(), Some(&"mutation panic"));
    assert!(shared.read(|value| *value).unwrap_err().is_poisoned());
    let error = shared.try_update(|_| Ok::<_, ()>(()), || {}).unwrap_err();
    assert!(error.access_error().unwrap().is_poisoned());
    assert!(!error.is_committed());
    let shared = Shared::new(0);
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        shared
            .try_update(
                |value| {
                    *value = 2;
                    Ok::<_, ()>(())
                },
                || panic!("publication panic"),
            )
            .unwrap();
    }))
    .expect_err("the publication panic must propagate");
    assert_eq!(panic.downcast_ref::<&str>(), Some(&"publication panic"));
    assert_eq!(shared.read(|value| *value).unwrap(), 2);
}

// Blanket Clone/Debug/Send bounds would reject this real local payload.
#[test]
fn local_move_only_value_and_borrowed_projection_capture_are_supported() {
    /// Deliberately omits `Clone`, `Debug` and `Send` to expose accidental API bounds.
    struct Local(Rc<Cell<u8>>);
    let shared = Shared::new(Local(Rc::new(Cell::new(1))));
    let enabled = Cell::new(true);
    let projected = shared
        .project(
            |value| enabled.get().then_some(&value.0),
            |value| enabled.get().then_some(&mut value.0),
        )
        .unwrap();
    let reader = projected.reader();
    projected
        .try_update(
            |value| {
                value.set(8);
                Ok::<_, ()>(())
            },
            || {},
        )
        .unwrap();
    assert_eq!(reader.read(|value| value.get()).unwrap(), 8);
    enabled.set(false);
    assert!(reader.read(|value| value.get()).unwrap_err().is_absent());
}

// The blocked worker holds a real mutation, not a sleep-based race.
#[test]
fn transported_values_report_contention_without_blocking() {
    /// Is `Send` but not `Sync`, and records its final release after all joins.
    struct Value {
        value: Cell<u8>,
        drops: Arc<AtomicUsize>,
    }
    impl Drop for Value {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    // Bound barriers and joins as well as the existing 15-second progress check.
    // Miri runs this same schedule directly under the surrounding job's limit.
    #[cfg(not(miri))]
    if subprocess::run_in_child("transported_values_report_contention_without_blocking") {
        return;
    }

    for contenders in [1, 2, 4, 8] {
        let drops = Arc::new(AtomicUsize::new(0));
        let shared = Shared::new(Value {
            value: Cell::new(0),
            drops: Arc::clone(&drops),
        });
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let worker = shared.clone();
        let worker_entered = Arc::clone(&entered);
        let worker_release = Arc::clone(&release);
        let writer = std::thread::spawn(move || {
            worker.try_update(
                |value| {
                    value.value.set(7);
                    worker_entered.wait();
                    worker_release.wait();
                    Ok::<_, ()>(())
                },
                || {},
            )
        });
        entered.wait();
        let start = Arc::new(Barrier::new(contenders + 1));
        let (complete, completed) = std::sync::mpsc::channel();
        let threads: Vec<_> = (0..contenders)
            .map(|_| {
                let shared = shared.clone();
                let reader = shared.reader().unwrap();
                let start = Arc::clone(&start);
                let complete = complete.clone();
                std::thread::spawn(move || {
                    start.wait();
                    let mut read_called = false;
                    let read = reader.read(|value| {
                        read_called = true;
                        value.value.get()
                    });
                    let mut mutation_called = false;
                    let mut published = false;
                    let mutation = shared.try_update(
                        |value| {
                            mutation_called = true;
                            value.value.set(99);
                            Ok::<_, ()>(())
                        },
                        || published = true,
                    );
                    complete.send(()).unwrap();
                    (read, mutation, read_called, mutation_called, published)
                })
            })
            .collect();
        drop(complete);
        start.wait();
        // Release the writer even if access regresses to blocking, then join.
        // The watchdog detects stalled progress, not a performance threshold.
        let progress = (0..contenders)
            .try_for_each(|_| completed.recv_timeout(std::time::Duration::from_secs(15)));
        release.wait();
        writer.join().unwrap().unwrap();
        let results: Vec<_> = threads
            .into_iter()
            .map(std::thread::JoinHandle::join)
            .collect();
        assert!(
            progress.is_ok(),
            "contended access must not wait: {progress:?}"
        );
        assert_eq!(results.len(), contenders);
        for result in results {
            let (read, mutation, read_called, mutation_called, published) = result.unwrap();
            assert!(read.unwrap_err().is_busy());
            let error = mutation.unwrap_err();
            assert!(error.access_error().unwrap().is_busy());
            assert!(!error.is_committed());
            assert!(!read_called && !mutation_called && !published);
        }
        assert_eq!(shared.read(|value| value.value.get()).unwrap(), 7);
        shared.update(|value| value.value.set(11), || {}).unwrap();
        assert_eq!(shared.read(|value| value.value.get()).unwrap(), 11);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(shared);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(Arc::strong_count(&drops), 1);
    }
}

// Case/key absence must follow the selected path, not default a missing child.
#[test]
fn case_and_keyed_projections_reject_absence_and_follow_current_membership() {
    /// Switches a projection's membership without replacing its shared reference.
    enum Screen {
        Off,
        On(u32),
    }
    let screen = Shared::new(Screen::On(7));
    let case = screen
        .project(
            |value| match value {
                Screen::On(value) => Some(value),
                Screen::Off => None,
            },
            |value| match value {
                Screen::On(value) => Some(value),
                Screen::Off => None,
            },
        )
        .unwrap();
    screen
        .try_update(
            |value| {
                *value = Screen::Off;
                Ok::<_, ()>(())
            },
            || {},
        )
        .unwrap();
    assert!(case.read(|value| *value).unwrap_err().is_absent());
    assert!(
        case.try_update(|_| Ok::<_, ()>(()), || {})
            .unwrap_err()
            .access_error()
            .unwrap()
            .is_absent()
    );
    let rows = Shared::new(
        [(1, 11)]
            .into_iter()
            .collect::<rca_core::KeyedState<u8, u32>>(),
    );
    let row = rows
        .project(|rows| rows.get(&1), |rows| rows.get_mut(&1))
        .unwrap();
    rows.try_update(
        |rows| {
            rows.remove(&1);
            Ok::<_, ()>(())
        },
        || {},
    )
    .unwrap();
    assert!(row.read(|value| *value).unwrap_err().is_absent());
    assert!(
        row.try_update(|_| Ok::<_, ()>(()), || {})
            .unwrap_err()
            .access_error()
            .unwrap()
            .is_absent()
    );
    assert_eq!(rows.read(rca_core::KeyedState::len).unwrap(), 0);
    rows.try_update(
        |rows| {
            rows.insert(1, 13);
            Ok::<_, ()>(())
        },
        || {},
    )
    .unwrap();
    assert_eq!(row.read(|value| *value).unwrap(), 13);
}

// Dropping a displaced reference inside the cohort mutex would reject reentry.
#[test]
fn displaced_value_destructor_can_read_rebound_cohort() {
    /// Runs one callback when rebinding releases the displaced value.
    struct Value {
        number: u32,
        /// Taken before invocation so the callback cannot run twice.
        on_drop: Option<Box<dyn FnOnce()>>,
    }
    impl Drop for Value {
        fn drop(&mut self) {
            if let Some(callback) = self.on_drop.take() {
                callback();
            }
        }
    }
    let shared = Shared::new(Value {
        number: 1,
        on_drop: None,
    });
    let observed = Rc::new(Cell::new(0));
    let destination = Rc::clone(&observed);
    let alias = shared.clone();
    shared
        .try_update(
            |value| {
                value.on_drop = Some(Box::new(move || {
                    destination.set(alias.read(|value| value.number).unwrap());
                }));
                Ok::<_, ()>(())
            },
            || {},
        )
        .unwrap();
    shared
        .rebind(&Shared::new(Value {
            number: 2,
            on_drop: None,
        }))
        .unwrap();
    assert_eq!(observed.get(), 2);
}

// The existing explicit source receives only post-unlock committed snapshots.
#[test]
fn explicit_publication_feeds_owned_observation_without_implicit_registration() {
    use futures_executor::block_on;
    use std::cell::RefCell;
    let shared = Shared::new(1);
    let producer = Rc::new(RefCell::new(None));
    let registered = Rc::clone(&producer);
    let reader = shared.reader().unwrap();
    let mut observation = rca_core::ExternalObservation::<u32, ()>::new(move |source| {
        *registered.borrow_mut() = Some(source);
        (reader.read(|value| *value).unwrap(), || {})
    });
    assert_eq!(block_on(observation.next()).unwrap(), Some(1));
    shared
        .try_update(
            |value| {
                *value = 2;
                Ok::<_, ()>(())
            },
            || {
                producer
                    .borrow()
                    .as_ref()
                    .unwrap()
                    .publish(shared.read(|value| *value).unwrap())
                    .unwrap();
            },
        )
        .unwrap();
    assert_eq!(block_on(observation.next()).unwrap(), Some(2));
    assert!(producer.borrow().as_ref().unwrap().finish());
    assert_eq!(block_on(observation.next()).unwrap(), None);
}

// An unused read callback must be discarded only after releasing the value lock.
#[test]
fn absent_projection_drops_unused_callback_outside_lock() {
    /// Reenters shared access when the rejected read closure is destroyed.
    struct OnDrop(Box<dyn Fn()>);
    impl Drop for OnDrop {
        fn drop(&mut self) {
            (self.0)();
        }
    }
    let shared = Shared::<Option<u32>>::new(None);
    let projection = shared.reader().unwrap().project(Option::as_ref);
    let alias = shared.clone();
    let dropped = Rc::new(Cell::new(false));
    let observed = Rc::clone(&dropped);
    let probe = OnDrop(Box::new(move || {
        assert_eq!(alias.read(|value| *value).unwrap(), None);
        observed.set(true);
    }));
    assert!(
        projection
            .read(move |value| {
                drop(probe);
                *value
            })
            .unwrap_err()
            .is_absent()
    );
    assert!(dropped.get());
}

// Rebinding after capture must not redirect an in-progress mutation's writes.
#[test]
fn rebind_during_mutation_changes_cohort_without_retargeting_active_reference() {
    let shared = Shared::new(1);
    let captured = shared.reader().unwrap();
    let replacement = Shared::new(11);
    shared
        .try_update(
            |value| {
                *value = 7;
                shared.rebind(&replacement).unwrap();
                assert_eq!(shared.read(|value| *value).unwrap(), 11);
                *value = 13;
                Ok::<_, ()>(())
            },
            || {
                assert_eq!(captured.read(|value| *value).unwrap(), 13);
                assert_eq!(shared.read(|value| *value).unwrap(), 11);
            },
        )
        .unwrap();
}

#[test]
fn committed_mutation_error_keeps_typed_cause_but_redacts_diagnostics() {
    /// Exposes a secret directly so the enclosing error must actively redact it.
    #[derive(Debug)]
    struct Failure(&'static str, Rc<()>);

    impl std::fmt::Display for Failure {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.0)
        }
    }

    impl std::error::Error for Failure {}

    let secret = "private mutation detail";
    let identity = Rc::new(());
    let failure = Failure(secret, Rc::clone(&identity));
    assert!(format!("{failure:?}").contains(secret));
    let shared = Shared::new(1);
    let publications = Cell::new(0);
    let error = shared
        .try_update(
            |value| {
                *value = 9;
                Err::<(), _>(failure)
            },
            || {
                assert_eq!(shared.read(|value| *value).unwrap(), 9);
                publications.set(publications.get() + 1);
            },
        )
        .unwrap_err();

    assert!(error.is_committed());
    assert!(error.access_error().is_none());
    assert_eq!(publications.get(), 1);
    let source = std::error::Error::source(&error)
        .and_then(|source| source.downcast_ref::<Failure>())
        .expect("the original typed mutation cause remains available");
    assert!(Rc::ptr_eq(&source.1, &identity));
    assert_eq!(source.to_string(), secret);
    assert!(!format!("{error:?} {error}").contains(secret));
    let recovered = error.into_failure().unwrap();
    assert!(Rc::ptr_eq(&recovered.1, &identity));
    assert_eq!(recovered.0, secret);
}

#[test]
fn access_mutation_errors_preserve_source_without_claiming_a_commit() {
    let shared = Shared::new(Some(1));
    let projection = shared.project(Option::as_ref, Option::as_mut).unwrap();
    let mutations = Cell::new(0);
    let publications = Cell::new(0);
    let update = |value: &mut u32| {
        *value += 1;
        mutations.set(mutations.get() + 1);
        Ok::<(), std::io::Error>(())
    };
    let publish = || publications.set(publications.get() + 1);

    let busy = shared
        .read(|_| projection.try_update(update, publish).unwrap_err())
        .unwrap();
    // Positive control proves the same callbacks mutate and publish when access succeeds.
    projection.try_update(update, publish).unwrap();
    assert_eq!(shared.read(|value| *value).unwrap(), Some(2));
    shared.update(|value| *value = None, || {}).unwrap();
    let absent = projection.try_update(update, publish).unwrap_err();
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        shared.read::<()>(|_| panic!("read callback poisons the value"))
    }));
    let panic = panic.expect_err("read callback must poison the value");
    assert_eq!(
        panic.downcast_ref::<&str>(),
        Some(&"read callback poisons the value")
    );
    let poisoned = projection.try_update(update, publish).unwrap_err();

    for (error, expected) in [
        (busy, (true, false, false)),
        (absent, (false, false, true)),
        (poisoned, (false, true, false)),
    ] {
        assert!(!error.is_committed());
        let access = error.access_error().unwrap();
        assert_eq!(
            (access.is_busy(), access.is_poisoned(), access.is_absent()),
            expected
        );
        let source = std::error::Error::source(&error)
            .and_then(|source| source.downcast_ref::<rca_core::SharedAccessError>())
            .expect("access rejection keeps its typed source");
        assert!(std::ptr::eq(access, source));
        assert_eq!(error.to_string(), access.to_string());
        assert!(error.into_failure().is_none());
    }
    assert_eq!(mutations.get(), 1);
    assert_eq!(publications.get(), 1);
}

#[test]
fn shared_diagnostics_hide_non_debug_values_without_evaluating_projections() {
    /// Deliberately has no `Debug` implementation despite containing readable text.
    struct Secret(&'static str);

    let secret = "private shared payload";
    let shared = Shared::new(Some(Secret(secret)));
    let reader = shared.reader().unwrap();
    let path_calls = Cell::new(0);
    let projection = shared
        .project(
            |root| {
                path_calls.set(path_calls.get() + 1);
                root.as_ref()
            },
            Option::as_mut,
        )
        .unwrap();
    let projected_reader = projection.reader();
    let diagnostics = shared
        .read(|value| {
            assert_eq!(value.as_ref().unwrap().0, secret);
            [
                format!("{shared:?}"),
                format!("{reader:?}"),
                format!("{projection:?}"),
                format!("{projected_reader:?}"),
            ]
        })
        .unwrap();
    assert_eq!(path_calls.get(), 0);
    for diagnostic in diagnostics {
        assert!(!diagnostic.contains(secret));
    }
    // The actual projection can read the secret once the value lock is released.
    assert_eq!(projected_reader.read(|value| value.0).unwrap(), secret);
    assert_eq!(path_calls.get(), 1);
}
