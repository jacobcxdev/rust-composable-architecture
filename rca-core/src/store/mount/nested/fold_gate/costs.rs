//! Paired finite assay using the existing allocator process and callback routes.
//!
//! Literal and traversal cases use the same route acquisition and callbacks.
//! They compare traversal overhead, not an independent implementation of routing.
//! Literal values and callback traces remain separate correctness checks.
use super::{
    Arc, CallbackLease, Contribution, Digits, First, Fixture, Mutex, Stats, Subtree, Sum, feed,
    query,
};
use crate::store::mount::allocation_tests::measure;

/// Calls a prebuilt identity list without traversal frames or roster searching.
fn literal<K: Contribution>(fixture: &Fixture<K>, identities: &[usize]) -> (K::Value, Stats)
where
    K::Value: Send,
{
    let mut accumulator = Some(K::identity());
    let mut stats = Stats::default();
    for identity in identities {
        let entry = fixture
            .index
            .acquire(*identity, &fixture.scopes, &mut stats)
            .unwrap();
        let value = entry.entry().contribution(&fixture.state).unwrap();
        stats.callbacks += 1;
        if feed::<K>(&mut accumulator, value) {
            break;
        }
    }
    (accumulator.unwrap(), stats)
}

/// Compares finite direct, cold, warm, transformed and early-stop queries.
pub(super) fn run() {
    let trace = Arc::new(Mutex::new(Vec::with_capacity(3)));
    let (fixture, construction) = measure(|| {
        Fixture::<Digits>::new(
            (None, None),
            std::sync::Arc::clone(&trace),
            Arc::new(Mutex::new(Vec::new())),
            |value| value,
        )
    });
    let (subtree, capture) = measure(|| Subtree::capture(&fixture.site, &fixture.scopes));
    // Fixture-only literal metadata roster. No contribution/result value scratch.
    let identities: Vec<_> = fixture
        .mounts
        .iter()
        .filter_map(|owner| owner.registration_identity())
        .collect();
    assert_eq!(identities.len(), 3);
    let ((value, direct_stats), direct) = measure(|| literal(&fixture, &identities));
    assert_eq!(value, 123);
    assert_eq!(*trace.lock(), [1, 2, 3]);
    trace.lock().clear();
    let ((value, cold_stats), cold) = measure(|| {
        query(
            &fixture.state,
            &fixture.scopes,
            &fixture.mounts,
            &fixture.index,
            &subtree,
        )
        .unwrap()
    });
    assert_eq!(value, 123);
    assert_eq!(*trace.lock(), [1, 2, 3]);
    trace.lock().clear();
    let ((value, warm_stats), warm) = measure(|| {
        query(
            &fixture.state,
            &fixture.scopes,
            &fixture.mounts,
            &fixture.index,
            &subtree,
        )
        .unwrap()
    });
    assert_eq!(value, 123);
    assert_eq!(warm_stats.max_frames, 0);
    assert_eq!(warm_stats.frame_capacity, 0);
    eprintln!(
        "private fold Shared blocks/bytes: fixture construction={construction:?}; captured subtree={capture:?}; literal same-route callbacks={direct:?}; query cold={cold:?}; query warm={warm:?}"
    );
    eprintln!(
        "private fold Shared counters: literal={direct_stats:?}; cold={cold_stats:?}; warm={warm_stats:?}"
    );

    let transformed = Fixture::<Sum>::new(
        (Some(|value| value * 10), Some(|value| value * 10)),
        Arc::new(Mutex::new(Vec::with_capacity(3))),
        Arc::new(Mutex::new(Vec::new())),
        |value| value,
    );
    let subtree = Subtree::capture(&transformed.site, &transformed.scopes);
    let ((value, stats), cost) = measure(|| {
        query(
            &transformed.state,
            &transformed.scopes,
            &transformed.mounts,
            &transformed.index,
            &subtree,
        )
        .unwrap()
    });
    assert_eq!(value, 240);
    assert_eq!(stats.max_frames, 2);
    eprintln!(
        "private fold Shared two explicit transforms blocks/bytes={cost:?}; counters={stats:?}"
    );

    let first = Fixture::<First>::new(
        (None, Some(|_| panic!("Break suppresses transform"))),
        Arc::new(Mutex::new(Vec::with_capacity(1))),
        Arc::new(Mutex::new(Vec::new())),
        |value| value,
    );
    let subtree = Subtree::capture(&first.site, &first.scopes);
    let ((value, stats), cost) = measure(|| {
        query(
            &first.state,
            &first.scopes,
            &first.mounts,
            &first.index,
            &subtree,
        )
        .unwrap()
    });
    assert_eq!(value, 1);
    assert_eq!(stats.callbacks, 1);
    assert_eq!(stats.frozen_routes, 1);
    eprintln!("private fold Shared early Break blocks/bytes={cost:?}; counters={stats:?}");
}
