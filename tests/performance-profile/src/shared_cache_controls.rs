//! Correctness-only cache seam controls; none contribute to useful-work records.

use super::*;

/// Resolves a ready reader without adding the profiling wrapper's lookup counters.
fn reader(cache: &SharedKeyCache, probe: &Arc<Probe>, tag: u64) -> SharedReader<Value> {
    poll_ready(cache.reader(Key::new(probe), || seed(probe, tag)))
        .unwrap()
        .unwrap()
}

/// Configures the next provider response on this test's private probe.
fn key(probe: &Arc<Probe>, behavior: Behavior) -> Key {
    if let Behavior::Present(tag) = behavior {
        probe.provider_tag.store(tag, Ordering::Relaxed);
    }
    Key {
        behavior,
        ..Key::new(probe)
    }
}

/// Reads a captured binding without adding a profiling read operation.
fn tag(reader: &SharedReader<Value>) -> u64 {
    reader.read(|value| value.tag).unwrap()
}

// Fresh identities really invoke provider work; the live-hit oracle must reject it.
#[test]
fn fresh_identity_fault_is_rejected_by_hit_counter_oracle() {
    let probe = Arc::new(Probe::default());
    let cache = SharedKeyCache::new();
    let _base = reader(&cache, &probe, 7);
    let before = probe.counts.snapshot();
    let other = Key {
        address: "fresh-identity",
        ..Key::new(&probe)
    };
    let _fault = poll_ready(cache.reader(other, || seed(&probe, 7)))
        .unwrap()
        .unwrap();
    assert!(expect_counts(probe.counts.snapshot().since(before), Counts::default()).is_err());
}

// A real captured reader makes the next lookup a live hit, not an expired miss.
#[test]
fn retained_capture_fault_is_rejected_by_expiry_oracle() {
    let probe = Arc::new(Probe::default());
    let cache = SharedKeyCache::new();
    let base = reader(&cache, &probe, 7);
    let capture = base.clone();
    drop(base);
    let before = probe.counts.snapshot();
    let next = reader(&cache, &probe, 8);
    assert_eq!(tag(&next), 7);
    assert_eq!(probe.counts.snapshot(), before);
    drop(next);
    assert!(expect_expired(&probe).is_err());
    drop(capture);
    expect_expired(&probe).unwrap();
}

#[test]
fn live_hit_suppresses_seed_load_and_subscription() {
    let probe = Arc::new(Probe::default());
    let cache = SharedKeyCache::new();
    let base = reader(&cache, &probe, 7);
    let before = probe.counts.snapshot();
    let hit = poll_ready(cache.reader(Key::new(&probe), || panic!("live hit seeded")))
        .unwrap()
        .unwrap();
    assert_eq!(tag(&hit), 7);
    assert_eq!(probe.counts.snapshot(), before);
    drop((base, hit));
    expect_expired(&probe).unwrap();
}

#[test]
fn real_value_and_subscription_owners_fail_zero_owner_oracle() {
    let probe = Arc::new(Probe::default());
    let cache = SharedKeyCache::new();
    let owner = reader(&cache, &probe, 7);
    let publication = probe
        .slots
        .lock()
        .unwrap()
        .subscriber
        .as_ref()
        .unwrap()
        .prepare_publication()
        .unwrap();
    drop(owner);
    assert!(expect_expired(&probe).is_err());
    drop(publication);
    expect_expired(&probe).unwrap();
    let value = Value::new(&probe, 21);
    assert!(expect_expired(&probe).is_err());
    drop(value);
    expect_expired(&probe).unwrap();
}

#[test]
fn provider_failures_preserve_move_owned_cause_and_retry_initial_work() {
    for (behavior, cause, attempts) in [
        (Behavior::LoadFailure, 42, 0),
        (Behavior::SubscribeFailure, 43, 1),
    ] {
        let probe = Arc::new(Probe::default());
        let cache = SharedKeyCache::new();
        let failure = poll_ready(cache.reader(key(&probe, behavior), || seed(&probe, 3)))
            .unwrap()
            .unwrap_err();
        assert_eq!(failure.into_failure().unwrap().0.get(), cause);
        assert_eq!(
            probe.counts.snapshot(),
            Counts {
                seed_calls: 1,
                initial_loads: 1,
                initial_subscribes: attempts,
                values_created: 1,
                values_dropped: 1,
                ..Counts::default()
            }
        );
        expect_expired(&probe).unwrap();
        let retry = reader(&cache, &probe, 7);
        assert_eq!(tag(&retry), 7);
        assert_eq!(
            probe.counts.snapshot(),
            Counts {
                seed_calls: 2,
                initial_loads: 2,
                initial_subscribes: attempts + 1,
                values_created: 2,
                values_dropped: 1,
                subscriptions_created: 1,
                ..Counts::default()
            }
        );
        drop(retry);
        expect_expired(&probe).unwrap();
        assert_eq!(probe.counts.snapshot().subscriptions_dropped, 1);
    }
}

#[test]
fn present_initial_load_evaluates_seed_then_drops_it_and_keeps_loaded_value() {
    let probe = Arc::new(Probe::default());
    let cache = SharedKeyCache::new();
    let owner = poll_ready(cache.reader(key(&probe, Behavior::Present(9)), || seed(&probe, 3)))
        .unwrap()
        .unwrap();
    assert_eq!(tag(&owner), 9);
    assert_eq!(
        probe.counts.snapshot(),
        Counts {
            seed_calls: 1,
            initial_loads: 1,
            initial_subscribes: 1,
            provider_returned_values: 1,
            values_created: 2,
            values_dropped: 1,
            subscriptions_created: 1,
            ..Counts::default()
        }
    );
    drop(owner);
    expect_expired(&probe).unwrap();
    assert_eq!(probe.counts.snapshot().values_dropped, 2);
}

#[test]
fn cache_hit_cohorts_rebind_independently_from_captured_references() {
    let probe = Arc::new(Probe::default());
    let local = Arc::new(Probe::default());
    let cache = SharedKeyCache::new();
    let first = poll_ready(cache.shared(Key::new(&probe), || seed(&probe, 7)))
        .unwrap()
        .unwrap();
    let second = poll_ready(cache.shared(Key::new(&probe), || panic!("hit seeded")))
        .unwrap()
        .unwrap();
    let clone = first.clone();
    let captured = first.reader().unwrap();
    first.rebind(&Shared::new(Value::new(&local, 20))).unwrap();
    assert_eq!(first.read(|value| value.tag).unwrap(), 20);
    assert_eq!(clone.read(|value| value.tag).unwrap(), 20);
    assert_eq!(second.read(|value| value.tag).unwrap(), 7);
    assert_eq!(tag(&captured), 7);
    drop((first, clone));
    expect_expired(&local).unwrap();
    expect_live(&probe).unwrap();
    drop((second, captured));
    expect_expired(&probe).unwrap();
    assert_eq!(probe.counts.snapshot().subscriptions_dropped, 1);
    assert_eq!(local.counts.snapshot().subscriptions_created, 0);
    assert_eq!(probe.counts.snapshot().save_calls, 0);
}

#[test]
fn final_capture_outlives_namespace_and_reset_keeps_old_binding() {
    let probe = Arc::new(Probe::default());
    let cache = SharedKeyCache::new();
    let base = reader(&cache, &probe, 7);
    let capture = base.clone();
    drop((base, cache));
    expect_live(&probe).unwrap();
    assert_eq!(tag(&capture), 7);
    drop(capture);
    expect_expired(&probe).unwrap();
    let probe = Arc::new(Probe::default());
    let cache = SharedKeyCache::new();
    let old = reader(&cache, &probe, 7);
    cache.reset().unwrap();
    let new = reader(&cache, &probe, 9);
    assert_eq!((tag(&old), tag(&new)), (7, 9));
    assert_eq!(probe.counts.snapshot().subscriptions_created, 2);
    drop(old);
    expect_live(&probe).unwrap();
    assert_eq!(probe.counts.snapshot().subscriptions_dropped, 1);
    drop(new);
    expect_expired(&probe).unwrap();
    assert_eq!(probe.counts.snapshot().subscriptions_dropped, 2);
}

#[test]
fn weak_producer_cannot_revive_owner_and_drops_rejected_value() {
    let probe = Arc::new(Probe::default());
    let cache = SharedKeyCache::new();
    let owner = reader(&cache, &probe, 7);
    let subscriber = probe
        .slots
        .lock()
        .unwrap()
        .subscriber
        .as_ref()
        .unwrap()
        .clone();
    drop(owner);
    assert!(subscriber.prepare_publication().is_none());
    assert!(!subscriber.yield_value(Value::new(&probe, 99)).unwrap());
    expect_expired(&probe).unwrap();
    assert_eq!(probe.counts.snapshot().values_created, 2);
    assert_eq!(probe.counts.snapshot().values_dropped, 2);
}

#[test]
fn pending_provider_is_rejected_after_one_poll_and_drops_seed() {
    let probe = Arc::new(Probe::default());
    let cache = SharedKeyCache::new();
    let result = poll_ready(cache.reader(key(&probe, Behavior::Pending), || seed(&probe, 3)));
    assert!(matches!(
        result,
        Err("direct cache fixture unexpectedly pending")
    ));
    expect_expired(&probe).unwrap();
    assert_eq!(
        probe.counts.snapshot(),
        Counts {
            seed_calls: 1,
            initial_loads: 1,
            values_created: 1,
            values_dropped: 1,
            ..Counts::default()
        }
    );
}

#[test]
fn require_loads_before_live_lookup_and_discards_present_provider_value() {
    let probe = Arc::new(Probe::default());
    let cache = SharedKeyCache::new();
    let base = reader(&cache, &probe, 7);
    let before = probe.counts.snapshot();
    let absent = poll_ready(cache.require_reader(Key::new(&probe)))
        .unwrap()
        .unwrap_err();
    assert!(absent.is_absent());
    assert_eq!(
        probe.counts.snapshot().since(before),
        Counts {
            user_loads: 1,
            ..Counts::default()
        }
    );
    let before = probe.counts.snapshot();
    let present = poll_ready(cache.require_reader(key(&probe, Behavior::Present(9))))
        .unwrap()
        .unwrap();
    assert_eq!(tag(&present), 7);
    assert_eq!(
        probe.counts.snapshot().since(before),
        Counts {
            user_loads: 1,
            provider_returned_values: 1,
            values_created: 1,
            values_dropped: 1,
            ..Counts::default()
        }
    );
    drop((base, present));
    expect_expired(&probe).unwrap();
}

#[test]
fn reader_load_miss_hit_and_failure_keep_distinct_source_captures() {
    let source = Arc::new(Probe::default());
    let target = Arc::new(Probe::default());
    let cache = SharedKeyCache::new();
    let mut receiving = Shared::new(Value::new(&source, 5)).reader().unwrap();
    let old = receiving.clone();
    poll_ready(cache.load_reader(
        &mut receiving,
        key(&target, Behavior::Present(23)),
        |value| {
            assert_eq!(value.tag, 5);
            seed(&target, 17)
        },
    ))
    .unwrap()
    .unwrap();
    assert_eq!((tag(&receiving), tag(&old)), (23, 5));
    assert_eq!(
        target.counts.snapshot(),
        Counts {
            seed_calls: 1,
            user_loads: 1,
            user_subscribes: 1,
            provider_returned_values: 1,
            values_created: 2,
            values_dropped: 1,
            subscriptions_created: 1,
            ..Counts::default()
        }
    );
    let target_capture = receiving.clone();
    let mut hit = old.clone();
    let before = target.counts.snapshot();
    poll_ready(
        cache.load_reader(&mut hit, key(&target, Behavior::Present(29)), |_| {
            panic!("load hit seeded")
        }),
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        (tag(&receiving), tag(&target_capture), tag(&hit), tag(&old)),
        (29, 29, 29, 5)
    );
    assert_eq!(
        target.counts.snapshot().since(before),
        Counts {
            user_loads: 1,
            provider_returned_values: 1,
            values_created: 1,
            values_dropped: 1,
            ..Counts::default()
        }
    );
    drop((receiving, target_capture, hit, old));
    expect_expired(&source).unwrap();
    expect_expired(&target).unwrap();

    let source = Arc::new(Probe::default());
    let target = Arc::new(Probe::default());
    let mut receiving = Shared::new(Value::new(&source, 5)).reader().unwrap();
    let old = receiving.clone();
    let failure = poll_ready(cache.load_reader(
        &mut receiving,
        key(&target, Behavior::LoadFailure),
        |_| seed(&target, 17),
    ))
    .unwrap()
    .unwrap_err();
    assert_eq!(failure.into_failure().unwrap().0.get(), 42);
    assert_eq!((tag(&receiving), tag(&old)), (17, 5));
    assert_eq!(
        target.counts.snapshot(),
        Counts {
            seed_calls: 1,
            user_loads: 1,
            user_subscribes: 1,
            values_created: 1,
            subscriptions_created: 1,
            ..Counts::default()
        }
    );
    drop((receiving, old));
    expect_expired(&source).unwrap();
    expect_expired(&target).unwrap();
}

#[test]
fn writable_load_failure_rebinds_its_cohort_before_failure() {
    let source = Arc::new(Probe::default());
    let target = Arc::new(Probe::default());
    let cache = SharedKeyCache::new();
    let receiving = Shared::new(Value::new(&source, 5));
    let clone = receiving.clone();
    let old = receiving.reader().unwrap();
    let failure =
        poll_ready(
            cache.load(&receiving, key(&target, Behavior::LoadFailure), |value| {
                assert_eq!(value.tag, 5);
                seed(&target, 17)
            }),
        )
        .unwrap()
        .unwrap_err();
    assert_eq!(failure.into_failure().unwrap().0.get(), 42);
    assert_eq!(receiving.read(|value| value.tag).unwrap(), 17);
    assert_eq!(clone.read(|value| value.tag).unwrap(), 17);
    assert_eq!(tag(&old), 5);
    assert_eq!(
        target.counts.snapshot(),
        Counts {
            seed_calls: 1,
            user_loads: 1,
            user_subscribes: 1,
            values_created: 1,
            subscriptions_created: 1,
            ..Counts::default()
        }
    );
    drop((receiving, clone, old));
    expect_expired(&source).unwrap();
    expect_expired(&target).unwrap();
}

#[test]
fn incompatible_live_codec_fails_before_seed_or_provider_work() {
    let probe = Arc::new(Probe::default());
    let cache = SharedKeyCache::new();
    let _base = reader(&cache, &probe, 7);
    let before = probe.counts.snapshot();
    let incompatible = Key {
        other_codec: true,
        ..Key::new(&probe)
    };
    assert!(
        poll_ready(cache.reader(incompatible, || panic!("collision seeded")))
            .unwrap()
            .unwrap_err()
            .is_collision()
    );
    assert_eq!(probe.counts.snapshot(), before);
}

/// Attaches a destructor that resolves another address in the same namespace.
struct ReentrantKey {
    key: Key,
    cache: SharedKeyCache,
    extra: Arc<Probe>,
    reenter: bool,
}
/// Keeps the ordinary subscription alive while its destructor re-enters the cache.
struct ReentrantSubscription {
    _subscription: Subscription,
    cache: SharedKeyCache,
    extra: Arc<Probe>,
    reenter: bool,
}

impl Drop for ReentrantSubscription {
    fn drop(&mut self) {
        if self.reenter {
            let extra = Key {
                address: "destructor-extra",
                ..Key::new(&self.extra)
            };
            let owner = poll_ready(self.cache.reader(extra, || seed(&self.extra, 11)))
                .unwrap()
                .unwrap();
            assert_eq!(tag(&owner), 11);
            drop(owner);
        }
    }
}

impl SharedReaderKey for ReentrantKey {
    type Value = Value;
    type Error = Cause;
    type Subscription = ReentrantSubscription;
    fn identity(&self) -> SharedKeyIdentity {
        self.key.identity()
    }
    fn load(
        &self,
        context: SharedLoadContext<'_, Value>,
    ) -> impl Future<Output = Result<Option<Value>, Cause>> + Send {
        self.key.load(context)
    }
    fn subscribe(
        &self,
        context: SharedSubscribeContext,
        subscriber: SharedSubscriber<Value>,
    ) -> Result<Self::Subscription, Cause> {
        Ok(ReentrantSubscription {
            _subscription: self.key.subscribe(context, subscriber)?,
            cache: self.cache.clone(),
            extra: Arc::clone(&self.extra),
            reenter: self.reenter,
        })
    }
}

#[test]
fn reentry_child_rejects_empty_selection() {
    let result = std::panic::catch_unwind(|| {
        crate::tests::in_child("shared_cache::controls::missing_reentry_child")
    });
    let panic = result.expect_err("missing child selection must be rejected");
    let message = panic
        .downcast_ref::<String>()
        .expect("formatted child-selection diagnostic");
    assert!(message.starts_with(
        "isolated allocation test shared_cache::controls::missing_reentry_child did not pass exactly once:"
    ));
}

#[test]
/// Runs lock re-entry in a direct child so a deadlock cannot strand this test process.
fn losing_candidate_destructor_reenters_outside_lock_with_bounded_child() {
    const NAME: &str = "shared_cache::controls::losing_candidate_destructor_reenters_outside_lock_with_bounded_child";
    if !crate::tests::in_child(NAME) {
        return;
    }
    let probe = Arc::new(Probe::default());
    let extra = Arc::new(Probe::default());
    let cache = SharedKeyCache::new();
    let make = |reenter| ReentrantKey {
        key: Key::new(&probe),
        cache: cache.clone(),
        extra: Arc::clone(&extra),
        reenter,
    };
    let mut winner = None;
    let returned = poll_ready(cache.reader(make(true), || {
        probe.counts.seed_calls.fetch_add(1, Ordering::Relaxed);
        winner = Some(
            poll_ready(cache.reader(make(false), || seed(&probe, 7)))
                .unwrap()
                .unwrap(),
        );
        Value::new(&probe, 3)
    }))
    .unwrap()
    .unwrap();
    assert_eq!(tag(&returned), 7);
    assert_eq!(
        probe.counts.snapshot(),
        Counts {
            seed_calls: 2,
            initial_loads: 2,
            initial_subscribes: 2,
            values_created: 2,
            values_dropped: 1,
            subscriptions_created: 2,
            subscriptions_dropped: 1,
            ..Counts::default()
        }
    );
    assert_eq!(
        extra.counts.snapshot(),
        Counts {
            seed_calls: 1,
            initial_loads: 1,
            initial_subscribes: 1,
            values_created: 1,
            values_dropped: 1,
            subscriptions_created: 1,
            subscriptions_dropped: 1,
            ..Counts::default()
        }
    );
    expect_expired(&extra).unwrap();
    drop(returned);
    assert_eq!(probe.counts.snapshot().subscriptions_dropped, 1);
    assert_eq!(tag(winner.as_ref().unwrap()), 7);
    drop(winner);
    expect_expired(&probe).unwrap();
    assert_eq!(probe.counts.snapshot().subscriptions_dropped, 2);
}
