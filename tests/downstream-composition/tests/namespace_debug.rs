//! Checks generated namespaces through a renamed facade with non-Send, non-Debug payloads.

use architecture::{ActionPaths, StatePaths};
use std::marker::PhantomData;
use std::rc::Rc;

/// Deliberately lacks Debug so namespace bounds cannot hide behind payload traits.
struct Opaque;

/// Carries borrowed non-Send data and a const-sized value through generated paths.
#[derive(StatePaths)]
struct Borrowed<'a, const N: usize> {
    /// Borrows an existing resource without transferring its thread ownership.
    resource: &'a Rc<Opaque>,
    /// Exercises the namespace's const parameter.
    bytes: [u8; N],
}

/// Exercises borrowed and const-generic action payloads without domain Debug bounds.
#[derive(ActionPaths)]
enum Command<'a, const N: usize> {
    /// Borrows the original resource.
    Resource(#[doc = "The borrowed original resource."] &'a Rc<Opaque>),
    /// Owns a fixed-length payload.
    Bytes(#[doc = "The const-sized owned value."] [u8; N]),
}

/// Requires namespace thread-safety without requiring it from the domain.
fn assert_namespace<T: Send + Sync>() {}

/// Also requires namespace formatting when lifetime and const parameters suffice.
fn assert_debug_namespace<T: std::fmt::Debug + Send + Sync>() {}

/// Exposes a type parameter whose payload has no additional trait bounds.
#[derive(StatePaths)]
struct GenericState<T> {
    /// Holds the exact resource projected by the generated path.
    value: T,
}

/// Preserves a type-generic payload through embedding and extraction.
#[derive(ActionPaths)]
enum GenericAction<T> {
    /// Owns the original payload.
    Value(#[doc = "The owned value with no extra trait bounds."] T),
}

#[test]
fn packaged_type_generic_namespaces_need_no_domain_traits() {
    assert_namespace::<GenericStatePaths<Rc<Opaque>>>();
    assert_namespace::<GenericActionPaths<Rc<Opaque>>>();
    let resource = Rc::new(Opaque);
    let state = GenericState {
        value: Rc::clone(&resource),
    };
    assert!(Rc::ptr_eq(
        GenericStatePaths::value().get(&state),
        &resource
    ));
    let action = GenericActionPaths::value().embed(Rc::clone(&resource));
    assert!(Rc::ptr_eq(
        GenericActionPaths::value().extract(&action).unwrap(),
        &resource,
    ));
}

#[test]
fn packaged_generic_namespaces_format_without_domain_bounds() {
    assert_debug_namespace::<BorrowedPaths<'_, 2>>();
    assert_debug_namespace::<CommandPaths<'_, 2>>();
    let state_paths = BorrowedPaths::<'_, 2>(PhantomData);
    let action_paths = CommandPaths::<'_, 2>(PhantomData);
    assert!(format!("{state_paths:?}").starts_with("BorrowedPaths("));
    assert!(format!("{action_paths:?}").starts_with("CommandPaths("));
    let BorrowedPaths(_) = state_paths;
    let CommandPaths(_) = action_paths;

    let resource = Rc::new(Opaque);
    let state = Borrowed {
        resource: &resource,
        bytes: [1, 2],
    };
    assert!(Rc::ptr_eq(BorrowedPaths::resource().get(&state), &resource));
    assert_eq!(BorrowedPaths::bytes().get(&state), &[1, 2]);
    let command = CommandPaths::<2>::resource().embed(&resource);
    assert!(Rc::ptr_eq(
        CommandPaths::resource().extract(&command).unwrap(),
        &resource,
    ));
    let bytes = CommandPaths::<'_, 2>::bytes().embed([3, 4]);
    assert_eq!(CommandPaths::bytes().extract(&bytes), Some(&[3, 4]));
}
