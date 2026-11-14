//! Measures allocation costs without the ordinary test harness.

use super::{ErasedValue, register_erased_observation};
use crate::store::observation::PublishPolicy;
use crate::test_support as support;
use crate::{Effect, Reduce, Store};
use futures_util::FutureExt as _;
use std::task::{Context, Poll};

/// Measures changed and unchanged scoped projection publication.
pub(crate) fn distinct_projection_publication_costs() {
    if crate::allocation_test::run_in_child(
        "store::scoped::tests::distinct_projection_publication_costs",
    ) {
        return;
    }
    let runtime = support::ManualRuntime::new();
    let store = Store::new(
        0,
        Reduce::new(|state: &mut usize, action: &usize, _| {
            *state += action;
            Effect::none()
        }),
        runtime.runtime(),
    );
    let child = store.scope(crate::identity_state_path(), crate::identity_action_path());
    let mut actor = runtime.take_only_queued_task();
    let mut context = Context::from_waker(std::task::Waker::noop());
    let registration = child.observe_distinct(|number| *number);
    let mut registration = std::pin::pin!(registration);
    assert!(registration.as_mut().poll(&mut context).is_pending());
    assert!(actor.as_mut().poll(&mut context).is_pending());
    let Poll::Ready(Ok(mut values)) = registration.as_mut().poll(&mut context) else {
        panic!("the actor must acknowledge the distinct observation");
    };
    assert_eq!(values.next().now_or_never(), Some(Some(0)));

    let mut samples = [(0, 0); 3];
    let profiler = dhat::Profiler::builder().testing().build();
    for ((amount, expected), sample) in [(1, Some(1)), (1, Some(2)), (0, None)]
        .into_iter()
        .zip(&mut samples)
    {
        let receipt = store.try_send(amount).unwrap();
        let before = dhat::HeapStats::get();
        assert!(actor.as_mut().poll(&mut context).is_pending());
        let after = dhat::HeapStats::get();
        *sample = (
            after.total_blocks - before.total_blocks,
            after.total_bytes - before.total_bytes,
        );
        drop(receipt.finish().now_or_never().unwrap().unwrap());
        let next = values.next();
        let mut next = std::pin::pin!(next);
        assert!(next.as_mut().poll(&mut context).is_pending());
        assert!(actor.as_mut().poll(&mut context).is_pending());
        match expected {
            Some(expected) => assert_eq!(
                next.as_mut().poll(&mut context),
                Poll::Ready(Some(expected)),
            ),
            None => assert!(next.as_mut().poll(&mut context).is_pending()),
        }
    }
    drop(profiler);
    println!("scoped distinct publication (changed, changed, unchanged): {samples:?}");
}

/// Measures scoped observation storage and verifies complete release.
pub(crate) fn scoped_observation_adapter_storage_costs() {
    if crate::allocation_test::run_in_child(
        "store::scoped::tests::scoped_observation_adapter_storage_costs",
    ) {
        return;
    }
    let runtime = support::ManualRuntime::new();
    let store = Store::new(
        7,
        Reduce::new(|_: &mut usize, (): &(), _| Effect::none()),
        runtime.runtime(),
    );
    let mut actor = runtime.take_only_queued_task();
    let mut context = Context::from_waker(std::task::Waker::noop());
    assert!(actor.as_mut().poll(&mut context).is_pending());

    let profiler = dhat::Profiler::builder().testing().build();
    let before = dhat::HeapStats::get();
    let mut registration = register_erased_observation(
        store.clone(),
        |state: &usize| Box::new(*state) as ErasedValue,
        PublishPolicy::always(),
        None,
    );
    assert!(registration.as_mut().poll(&mut context).is_pending());
    assert!(actor.as_mut().poll(&mut context).is_pending());
    let Poll::Ready(Ok(registered)) = registration.as_mut().poll(&mut context) else {
        panic!("the actor must acknowledge real registration");
    };
    drop(registration);
    let mut values = registered.into_typed::<usize>();
    let registered = dhat::HeapStats::get();
    assert_eq!(values.next().now_or_never(), Some(Some(7)));
    drop(actor);
    assert_eq!(values.next().now_or_never(), Some(None));
    drop(values);
    drop(store);
    let released = dhat::HeapStats::get();
    drop(profiler);
    println!(
        "scoped observation registration: blocks={} bytes={} live_bytes={}; released_blocks={} released_bytes={}",
        registered.total_blocks - before.total_blocks,
        registered.total_bytes - before.total_bytes,
        registered.curr_bytes - before.curr_bytes,
        released.curr_blocks,
        released.curr_bytes,
    );
    assert_eq!(
        released.curr_blocks, 0,
        "registration must release its storage",
    );
}
