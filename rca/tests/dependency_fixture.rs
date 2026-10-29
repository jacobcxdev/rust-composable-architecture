//! Checks that scoped dependency work owns its context and mock access keeps identity.

use std::any::{Any, TypeId};
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use rca::{DependencyKey, DependencyMockMismatch, DependencyValues, try_dependency_mock};

/// Gives parent and child contexts independent, directly comparable values.
struct Number;
impl DependencyKey for Number {
    type Value = usize;
}

/// Exposes the original concrete value without a `Clone` or `Debug` requirement.
struct Mock(usize);

#[test]
fn owned_future_outlives_parent_and_checked_mock_keeps_opaque_domain() {
    let work = {
        let mut parent = DependencyValues::new();
        parent.set::<Number>(2);
        let work = parent.scope_async(
            |child| child.set::<Number>(7),
            |mut child| std::future::ready(Rc::new(*child.get_or_insert::<Number>())),
        );
        assert_eq!(*parent.get_or_insert::<Number>(), 2);
        work
    };
    let mut work = Box::pin(work);
    let mut cx = Context::from_waker(Waker::noop());
    match work.as_mut().poll(&mut cx) {
        Poll::Ready(value) => assert_eq!(*value, 7),
        Poll::Pending => panic!("ready fixture must complete"),
    }

    let mock = Mock(9);
    let interface: &dyn Any = &mock;
    let recovered = try_dependency_mock::<Mock>(interface).unwrap();
    assert!(std::ptr::eq(recovered, &raw const mock));
    assert_eq!(recovered.0, 9);
    let mismatch: DependencyMockMismatch = match try_dependency_mock::<usize>(interface) {
        Err(error) => error,
        Ok(_) => panic!("different concrete type must be rejected"),
    };
    assert_eq!(mismatch.expected_type_id(), TypeId::of::<usize>());
    assert_eq!(mismatch.actual_type_id(), TypeId::of::<Mock>());
    assert_eq!(mock.0, 9);
}
