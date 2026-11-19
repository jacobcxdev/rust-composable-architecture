//! Checks when captured changes replace state and when caller-owned aliases remain visible.

use rca_core::{CaseStatePath, OptionalStatePath, StatePath};
use std::cell::Cell;

/// A borrowed, move-only payload avoids unrelated `Clone` and `Debug` requirements.
struct Payload<'a>(&'a str, usize);

/// Provides matching and nonmatching cases without a stored optional field.
enum Domain<'a> {
    Value(Payload<'a>),
    Other,
}

/// Borrows the matching payload without extending its source lifetime.
fn get<'a, 'b>(root: &'b Domain<'a>) -> Option<&'b Payload<'a>> {
    match root {
        Domain::Value(value) => Some(value),
        Domain::Other => None,
    }
}

/// Projects the same case mutably for captured replacement.
fn get_mut<'a, 'b>(root: &'b mut Domain<'a>) -> Option<&'b mut Payload<'a>> {
    match root {
        Domain::Value(value) => Some(value),
        Domain::Other => None,
    }
}

#[test]
fn owned_capture_commits_only_success_and_evaluates_once() {
    let label = String::from("local");
    let mut root = Domain::Value(Payload(&label, 1));
    let projections = Cell::new(0);
    let captures = Cell::new(0);
    let transforms = Cell::new(0);
    let path = CaseStatePath::new(get, |root| {
        projections.set(projections.get() + 1);
        get_mut(root)
    });
    let failed = path.try_modify_captured(
        &mut root,
        |value| {
            captures.set(captures.get() + 1);
            Payload(value.0, value.1)
        },
        |mut value| {
            transforms.set(transforms.get() + 1);
            value.1 = 9;
            Err::<Payload<'_>, _>((value, "failed"))
        },
    );
    let Err((rejected, message)) = failed else {
        panic!("failed transform must return its captured payload");
    };
    assert_eq!(message, "failed");
    assert_eq!(rejected.1, 9);
    assert_eq!(get(&root).unwrap().1, 1);
    assert_eq!(
        (projections.get(), captures.get(), transforms.get()),
        (1, 1, 1)
    );
    let success = path.try_modify_captured(
        &mut root,
        |value| {
            captures.set(captures.get() + 1);
            Payload(value.0, value.1)
        },
        |mut value| {
            transforms.set(transforms.get() + 1);
            value.1 = 4;
            Ok::<_, ()>(value)
        },
    );
    assert_eq!(success, Ok(true));
    assert_eq!(get(&root).unwrap().1, 4);
    assert_eq!(
        (projections.get(), captures.get(), transforms.get()),
        (2, 2, 2)
    );
}

#[test]
fn wrong_case_and_absent_optional_skip_callbacks() {
    let path = CaseStatePath::new(get, get_mut);
    let mut root = Domain::Other;
    assert_eq!(
        path.try_modify_captured(
            &mut root,
            |_| panic!("capture"),
            |_| -> Result<_, ()> { panic!("transform") }
        ),
        Ok(false)
    );
    assert!(matches!(root, Domain::Other));
    let optional = OptionalStatePath::new(Option::as_ref, Option::as_mut);
    let mut absent: Option<usize> = None;
    assert_eq!(
        optional.try_modify_captured(
            &mut absent,
            |_| panic!("capture"),
            |_| -> Result<_, ()> { panic!("transform") }
        ),
        Ok(false)
    );
    assert_eq!(absent, None);
}

#[test]
fn root_and_optional_support_non_clone_non_debug_values() {
    let label = String::from("borrowed");
    let mut root = Payload(&label, 2);
    let path = StatePath::new(|value: &Payload<'_>| value, |value: &mut Payload<'_>| value);
    assert_eq!(
        path.try_modify_captured(
            &mut root,
            |value| Payload(value.0, value.1),
            |value| Ok::<_, ()>(Payload(value.0, value.1 + 1))
        ),
        Ok(())
    );
    assert_eq!(root.1, 3);
    let optional = OptionalStatePath::new(Option::as_ref, Option::as_mut);
    let mut root = Some(root);
    assert_eq!(
        optional.try_modify_captured(
            &mut root,
            |value| Payload(value.0, value.1),
            |value| Ok::<_, ()>(Payload(value.0, 8))
        ),
        Ok(true)
    );
    assert_eq!(root.unwrap().1, 8);
}

#[test]
fn required_and_optional_errors_do_not_commit() {
    let path = StatePath::new(|value: &usize| value, |value: &mut usize| value);
    let mut root = 1;
    assert_eq!(
        path.try_modify_captured(&mut root, |value| *value, |_| Err::<usize, _>(7)),
        Err(7)
    );
    assert_eq!(root, 1);
    let path = OptionalStatePath::new(Option::as_ref, Option::as_mut);
    let mut root = Some(1);
    assert_eq!(
        path.try_modify_captured(&mut root, |value| *value, |_| Err::<usize, _>(7)),
        Err(7)
    );
    assert_eq!(root, Some(1));
}

#[test]
fn keyed_projection_uses_current_child_and_absence_has_no_authority() {
    let mut root = rca_core::KeyedState::new();
    root.insert(3, 10);
    let path = OptionalStatePath::new(
        |root: &rca_core::KeyedState<i32, usize>| root.get(&3),
        |root: &mut rca_core::KeyedState<i32, usize>| root.get_mut(&3),
    );
    assert_eq!(
        path.try_modify_captured(&mut root, |value| *value, |value| Ok::<_, ()>(value + 1)),
        Ok(true)
    );
    assert_eq!(root.get(&3), Some(&11));
    root.remove(&3);
    assert_eq!(
        path.try_modify_captured(
            &mut root,
            |_| panic!("capture"),
            |_| -> Result<_, ()> { panic!("transform") }
        ),
        Ok(false)
    );
    root.insert(3, 20);
    assert_eq!(
        path.try_modify_captured(&mut root, |value| *value, |value| Ok::<_, ()>(value + 1)),
        Ok(true)
    );
    assert_eq!(root.get(&3), Some(&21));
}

#[test]
fn success_drops_original_payload_once() {
    /// Counts both the replaced original and the replacement when their owners end.
    struct DropValue<'a>(&'a Cell<usize>, usize);

    impl Drop for DropValue<'_> {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }
    let drops = Cell::new(0);
    let mut root = Some(DropValue(&drops, 1));
    let path = OptionalStatePath::new(Option::as_ref, Option::as_mut);
    assert_eq!(
        path.try_modify_captured(
            &mut root,
            |value| DropValue(value.0, value.1),
            |mut value| {
                value.1 = 2;
                Ok::<_, ()>(value)
            }
        ),
        Ok(true)
    );
    assert_eq!(drops.get(), 1);
    assert_eq!(root.as_ref().unwrap().1, 2);
    drop(root);
    assert_eq!(drops.get(), 2);
}

#[test]
fn borrowed_error_and_shared_capture_are_not_rollback_operations() {
    use std::rc::Rc;
    let path = CaseStatePath::new(get, get_mut);
    let mut root = Domain::Value(Payload("value", 1));
    let operation = |value: &mut Payload<'_>| {
        value.1 = 2;
        Err::<(), _>(7)
    };
    assert_eq!(operation(path.get_mut(&mut root).unwrap()), Err(7));
    assert_eq!(get(&root).unwrap().1, 2);

    let mut shared = Some(Rc::new(Cell::new(1)));
    let path = OptionalStatePath::new(Option::as_ref, Option::as_mut);
    assert_eq!(
        path.try_modify_captured(&mut shared, Rc::clone, |value| {
            value.set(9);
            Err::<Rc<Cell<i32>>, _>(7)
        }),
        Err(7)
    );
    assert_eq!(shared.as_ref().unwrap().get(), 9);
}
