//! Checks update results, publication order and cleanup across shared access failures.

use std::cell::Cell;
use std::rc::Rc;

use rca_core::{Shared, SharedAccessError};

#[test]
fn assignment_and_owned_result_publish_once_after_unlock_even_unchanged() {
    /// Proves that an update can return an owned, non-`Send` result.
    struct Output(Rc<Cell<u8>>);
    let shared = Shared::new(1);
    let mutations = Cell::new(0);
    let publications = Cell::new(0);
    let result: Result<Output, SharedAccessError> = shared.update(
        |value| {
            *value = 3;
            mutations.set(mutations.get() + 1);
            Output(Rc::new(Cell::new(*value)))
        },
        || {
            assert_eq!(shared.read(|value| *value).unwrap(), 3);
            publications.set(publications.get() + 1);
        },
    );
    assert_eq!(result.unwrap().0.get(), 3);
    shared
        .update(|_| (), || publications.set(publications.get() + 1))
        .unwrap();
    assert_eq!(mutations.get(), 1);
    assert_eq!(publications.get(), 2);
}

#[test]
fn borrowed_move_only_projection_and_callbacks_stay_local() {
    /// Keeps the payload local and move-only rather than imposing transport bounds.
    struct Local(Rc<Cell<u8>>);
    /// Moves a local owner into the update callback.
    struct Capture(Rc<Cell<u8>>);
    let shared = Shared::new(Some(Local(Rc::new(Cell::new(1)))));
    let present = Cell::new(true);
    let projection = shared
        .project(
            |root| present.get().then_some(root.as_ref()).flatten(),
            |root| present.get().then_some(root.as_mut()).flatten(),
        )
        .unwrap();
    let capture = Capture(Rc::new(Cell::new(7)));
    let published = Cell::new(0);
    let result = projection
        .update(
            move |value| {
                value.0 = capture.0;
                Local(Rc::clone(&value.0))
            },
            || published.set(projection.read(|value| value.0.get()).unwrap()),
        )
        .unwrap();
    assert_eq!(result.0.get(), 7);
    assert_eq!(published.get(), 7);
    projection.update(|_| (), || published.set(8)).unwrap();
    assert_eq!(published.get(), 8);
    present.set(false);
    assert!(
        projection
            .update(
                |_| panic!("absent mutation"),
                || panic!("absent publication")
            )
            .unwrap_err()
            .is_absent()
    );
}

#[test]
fn unavailable_access_skips_both_callbacks_and_checks_lock_before_presence() {
    let shared = Shared::new(Some(1));
    let projection = shared.project(Option::as_ref, Option::as_mut).unwrap();
    shared
        .update(
            |value| {
                *value = None;
                assert!(
                    shared
                        .update(|_| panic!("busy mutation"), || panic!("busy publication"))
                        .unwrap_err()
                        .is_busy()
                );
                assert!(
                    projection
                        .update(
                            |_| panic!("busy projection mutation"),
                            || panic!("busy projection publication")
                        )
                        .unwrap_err()
                        .is_busy()
                );
            },
            || {},
        )
        .unwrap();
    assert!(
        projection
            .update(
                |_| panic!("absent mutation"),
                || panic!("absent publication")
            )
            .unwrap_err()
            .is_absent()
    );
    assert_eq!(shared.read(|value| *value).unwrap(), None);
    shared.update(|value| *value = Some(2), || {}).unwrap();
    projection.update(|value| *value = 5, || {}).unwrap();
    assert_eq!(shared.read(|value| *value).unwrap(), Some(5));
    shared.update(|value| *value = None, || {}).unwrap();
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        shared.update(|_| panic!("poison"), || panic!("poison publication"))
    }))
    .expect_err("the mutation must poison the shared value");
    assert_eq!(panic.downcast_ref::<&str>(), Some(&"poison"));
    assert!(
        shared
            .update(
                |_| panic!("poisoned mutation"),
                || panic!("poisoned publication")
            )
            .unwrap_err()
            .is_poisoned()
    );
    assert!(
        projection
            .update(
                |_| panic!("poisoned projection mutation"),
                || panic!("poisoned projection publication")
            )
            .unwrap_err()
            .is_poisoned()
    );
}

#[test]
fn mutation_panic_skips_publication_but_publication_panic_cannot_poison() {
    let shared = Shared::new(Some(0));
    let projection = shared.project(Option::as_ref, Option::as_mut).unwrap();
    let published = Cell::new(false);
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        projection.update(
            |value| {
                *value = 1;
                panic!("mutation");
            },
            || published.set(true),
        )
    }));
    let panic = panic.expect_err("the mutation panic must propagate");
    assert_eq!(panic.downcast_ref::<&str>(), Some(&"mutation"));
    assert!(!published.get());
    assert!(shared.read(|value| *value).unwrap_err().is_poisoned());

    let shared = Shared::new(Some(0));
    let projection = shared.project(Option::as_ref, Option::as_mut).unwrap();
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        projection.update(
            |value| *value = 2,
            || {
                assert_eq!(projection.read(|value| *value).unwrap(), 2);
                panic!("publication");
            },
        )
    }));
    let panic = panic.expect_err("the publication panic must propagate");
    assert_eq!(panic.downcast_ref::<&str>(), Some(&"publication"));
    assert_eq!(shared.read(|value| *value).unwrap(), Some(2));
    shared.update(|value| *value = Some(3), || {}).unwrap();
}

#[test]
fn publication_can_reenter_and_rebind_without_retargeting_retained_projection() {
    let shared = Shared::new(Some(1));
    let alias = shared.clone();
    let retained = shared.reader().unwrap();
    let projection = shared.project(Option::as_ref, Option::as_mut).unwrap();
    let replacement = Shared::new(Some(11));
    shared
        .update(
            |value| {
                *value = Some(7);
                shared.rebind(&replacement).unwrap();
                *value = Some(13);
            },
            || {
                assert_eq!(retained.read(|value| *value).unwrap(), Some(13));
                alias.update(|value| *value = Some(17), || {}).unwrap();
                projection.update(|value| *value = 19, || {}).unwrap();
            },
        )
        .unwrap();
    assert_eq!(retained.read(|value| *value).unwrap(), Some(19));
    assert_eq!(projection.read(|value| *value).unwrap(), 19);
    assert_eq!(alias.read(|value| *value).unwrap(), Some(17));
    assert_eq!(replacement.read(|value| *value).unwrap(), Some(17));
}

#[test]
fn fallible_projection_preserves_original_opaque_partial_commit() {
    /// Retains cause identity without requiring formatting or cloning traits.
    struct Failure(Rc<Cell<u8>>);
    let shared = Shared::new(Some(1));
    let projection = shared.project(Option::as_ref, Option::as_mut).unwrap();
    let cause = Rc::new(Cell::new(7));
    let published = Cell::new(0);
    let error = projection
        .try_update(
            |value| {
                *value = 5;
                Err::<(), _>(Failure(Rc::clone(&cause)))
            },
            || published.set(projection.read(|value| *value).unwrap()),
        )
        .unwrap_err();
    assert!(error.is_committed());
    assert!(error.access_error().is_none());
    assert!(Rc::ptr_eq(&error.into_failure().unwrap().0, &cause));
    assert_eq!(published.get(), 5);
}

#[test]
fn absent_mutation_drops_unused_callbacks_after_unlock() {
    /// Reenters shared access when an unused callback releases its capture.
    struct Probe<'a>(&'a Shared<Option<u32>>, &'a Cell<usize>);
    impl Drop for Probe<'_> {
        fn drop(&mut self) {
            assert_eq!(self.0.read(|value| *value).unwrap(), None);
            self.1.set(self.1.get() + 1);
        }
    }
    let shared = Shared::new(None);
    let projection = shared.project(Option::as_ref, Option::as_mut).unwrap();
    let dropped = Cell::new(0);
    let mutation = Probe(&shared, &dropped);
    let publication = Probe(&shared, &dropped);
    assert!(
        projection
            .update(
                move |_| {
                    drop(mutation);
                    panic!("absent mutation");
                },
                move || {
                    drop(publication);
                    panic!("absent publication");
                },
            )
            .unwrap_err()
            .is_absent()
    );
    assert_eq!(dropped.get(), 2);
}

#[test]
fn panicked_publication_drops_owned_result_after_unlock() {
    /// Requires the owned result to drop only after shared access is unlocked.
    struct Probe<'a>(&'a Shared<u32>, &'a Cell<bool>);
    impl Drop for Probe<'_> {
        fn drop(&mut self) {
            assert_eq!(self.0.read(|value| *value).unwrap(), 5);
            self.1.set(true);
        }
    }
    let shared = Shared::new(0);
    let dropped = Cell::new(false);
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        shared.update(
            |value| {
                *value = 5;
                Probe(&shared, &dropped)
            },
            || panic!("publication"),
        )
    }));
    let Err(panic) = panic else {
        panic!("the publication panic must propagate");
    };
    assert_eq!(panic.downcast_ref::<&str>(), Some(&"publication"));
    assert!(dropped.get());
    assert_eq!(shared.read(|value| *value).unwrap(), 5);
}
