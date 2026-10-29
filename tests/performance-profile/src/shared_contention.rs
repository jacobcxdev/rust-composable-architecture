//! Finite concurrent cache resolution, including forced publication losers.

use std::future::Future;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Barrier, Mutex};

use rca_core::{
    Shared, SharedKey, SharedKeyCache, SharedKeyIdentity, SharedLoadContext, SharedReaderKey,
    SharedSubscribeContext, SharedSubscriber,
};

use super::{
    Cause, Counts, Key, Probe, Subscription, Value, binding_counts, expect_counts, expect_expired,
    expect_live, poll_ready, provider_error, seed,
};
use crate::measurement::Measurement;

/// Fixed amplification for the hit cell, not a performance threshold.
const HIT_CALLS: u64 = 64;
/// A value outside the initial candidate range that proves a shared binding.
const MUTATED_TAG: u64 = 19;

#[derive(Clone, Copy)]
/// A validated cold race or live-hit race with at most eight callers.
pub(crate) struct Config {
    hit: bool,
    callers: usize,
}

impl Config {
    /// Accepts only the fixed concurrency matrix and rejects trailing arguments.
    pub(crate) fn parse(mut args: impl Iterator<Item = String>) -> Result<Self, &'static str> {
        let hit = match args.next().as_deref() {
            Some("shared-race-cold") => false,
            Some("shared-race-hit") => true,
            _ => return Err("unknown shared contention workload"),
        };
        let callers = match args.next().as_deref() {
            Some("1") => 1,
            Some("2") => 2,
            Some("4") => 4,
            Some("8") => 8,
            _ => return Err("shared contention CALLERS must be 1, 2, 4 or 8"),
        };
        if args.next().is_some() {
            return Err("expected shared contention WORKLOAD CALLERS");
        }
        Ok(Self { hit, callers })
    }
}

/// Holds every cold candidate at subscription before any can publish.
///
/// The provider releases its probe lock before waiting at the barrier. Live hits
/// use the same concrete key type without invoking this subscription path.
struct RacingKey {
    key: Key,
    subscribe_barrier: Option<Arc<Barrier>>,
}

impl SharedReaderKey for RacingKey {
    type Value = Value;
    type Error = Cause;
    type Subscription = Subscription;

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
    ) -> Result<Subscription, Cause> {
        let subscription = self.key.subscribe(context, subscriber)?;
        if let Some(barrier) = &self.subscribe_barrier {
            barrier.wait();
        }
        Ok(subscription)
    }
}

impl SharedKey for RacingKey {
    fn save(&self, value: &Value) -> impl Future<Output = Result<(), Cause>> + Send {
        self.key.save(value)
    }
}

/// Resolves one writable cohort and counts only a successful returned owner.
fn resolve(
    cache: &SharedKeyCache,
    probe: &Arc<Probe>,
    barrier: Option<Arc<Barrier>>,
    tag: u64,
) -> Result<Shared<Value>, &'static str> {
    let owner = poll_ready(cache.shared(
        RacingKey {
            key: Key::new(probe),
            subscribe_barrier: barrier,
        },
        || seed(probe, tag),
    ))?
    .inspect_err(|error| eprintln!("concurrent cache resolution failed: {error}"))
    .map_err(provider_error)?;
    probe
        .counts
        .successful_lookups
        .fetch_add(1, Ordering::Relaxed);
    probe
        .counts
        .cohort_constructions
        .fetch_add(1, Ordering::Relaxed);
    Ok(owner)
}

/// Counts serial validation reads separately from the concurrent lookup phase.
fn read(owner: &Shared<Value>, probe: &Probe) -> Result<u64, &'static str> {
    let tag = owner
        .read(|value| value.tag)
        .inspect_err(|error| eprintln!("serial concurrent-result read failed: {error}"))
        .map_err(|_| "concurrent read failed")?;
    probe.counts.read_calls.fetch_add(1, Ordering::Relaxed);
    Ok(tag)
}

/// Sums per-worker counters after the workers acknowledge their phase boundary.
fn counts(probes: &[Arc<Probe>]) -> Counts {
    let mut total = Counts::default();
    for probe in probes {
        let value = probe.counts.snapshot();
        macro_rules! sum {
            ($($field:ident),+ $(,)?) => { $(total.$field += value.$field;)+ };
        }
        sum!(
            seed_calls,
            initial_loads,
            user_loads,
            initial_subscribes,
            user_subscribes,
            save_calls,
            provider_returned_values,
            values_created,
            values_dropped,
            subscriptions_created,
            subscriptions_dropped,
            successful_lookups,
            read_calls,
            capture_operations,
            reader_captures,
            cohort_constructions
        );
    }
    total
}

/// Counts surviving candidate pairs and requires each producer to match its owners.
fn live_candidates(probes: &[Arc<Probe>]) -> Result<u64, &'static str> {
    let mut live = 0;
    for probe in probes {
        let counts = probe.counts.snapshot();
        if counts.values_created == counts.values_dropped {
            expect_expired(probe)?;
        } else {
            if counts.values_created != counts.values_dropped + 1
                || counts.subscriptions_created != counts.subscriptions_dropped + 1
            {
                return Err("concurrent candidate ownership mismatch");
            }
            expect_live(probe)?;
            if probe
                .slots
                .lock()
                .unwrap()
                .subscriber
                .as_ref()
                .is_none_or(|subscriber| subscriber.prepare_publication().is_none())
            {
                return Err("concurrent live producer closed early");
            }
            live += 1;
        }
    }
    Ok(live)
}

/// Retains every returned cohort in a buffer allocated before measured work.
fn worker(
    config: Config,
    index: usize,
    cache: &SharedKeyCache,
    probe: &Arc<Probe>,
    barrier: Option<&Arc<Barrier>>,
    mut owners: Vec<Shared<Value>>,
) -> Result<Vec<Shared<Value>>, &'static str> {
    let calls = if config.hit { HIT_CALLS } else { 1 };
    for _ in 0..calls {
        owners.push(resolve(cache, probe, barrier.cloned(), index as u64 + 1)?);
    }
    Ok(owners)
}

/// Checks one common winner, then mutates it to reject independent equal values.
///
/// The caller first validates nonempty, equally sized worker and probe batches.
fn validate_binding(
    owners: &[Vec<Shared<Value>>],
    probes: &[Arc<Probe>],
) -> Result<(u64, u64), &'static str> {
    let mut winner = None;
    let mut checksum = 0;
    for (batch, probe) in owners.iter().zip(probes) {
        for owner in batch {
            let tag = read(owner, probe)?;
            if *winner.get_or_insert(tag) != tag {
                return Err("concurrent callers observed different winners");
            }
            checksum += tag;
        }
    }
    owners[0][0]
        .try_update(
            |value| {
                value.tag = MUTATED_TAG;
                Ok::<_, ()>(())
            },
            || {},
        )
        .inspect_err(|error| eprintln!("serial concurrent-result mutation failed: {error}"))
        .map_err(|_| "concurrent mutation failed")?;
    for (batch, probe) in owners.iter().zip(probes) {
        for owner in batch {
            if read(owner, probe)? != MUTATED_TAG {
                return Err("concurrent callers retained split bindings");
            }
        }
    }
    Ok((winner.expect("validated nonempty worker batches"), checksum))
}

/// Requires every candidate's value, subscription and publication authority to expire.
fn released(probes: &[Arc<Probe>]) -> Result<(), &'static str> {
    for probe in probes {
        expect_expired(probe)?;
    }
    Ok(())
}

/// Separates completed thread setup, measured work, validation and final thread exit.
struct Gates {
    ready: Barrier,
    start: Barrier,
    done: Barrier,
    exit: Barrier,
}

/// Runs the real publication race and releases its owners before reporting success.
///
/// Gates and result handoff are included in work; spawning and joining are not.
/// Barrier failures require the caller's process deadline: these native joins do
/// not provide an internal timeout or a proof of every possible interleaving.
#[expect(
    clippy::too_many_lines,
    reason = "keeps concurrent phase and unconditional cleanup ordering visible"
)]
pub(crate) fn run(config: Config) -> Result<String, &'static str> {
    let mut measurement = Measurement::new(None);
    let cache = SharedKeyCache::new();
    let probes: Vec<_> = (0..config.callers)
        .map(|_| Arc::new(Probe::default()))
        .collect();
    let gates = Arc::new(Gates {
        ready: Barrier::new(config.callers + 1),
        start: Barrier::new(config.callers + 1),
        done: Barrier::new(config.callers + 1),
        exit: Barrier::new(config.callers + 1),
    });
    let barrier = (!config.hit).then(|| Arc::new(Barrier::new(config.callers)));
    let slots: Vec<_> = (0..config.callers)
        .map(|_| Arc::new(Mutex::new(None)))
        .collect();
    let mut threads = Vec::with_capacity(config.callers);
    let mut owners = Vec::with_capacity(config.callers);
    for (index, (probe, slot)) in probes.iter().zip(&slots).enumerate() {
        let (probe, slot, gates, cache, barrier) = (
            Arc::clone(probe),
            Arc::clone(slot),
            Arc::clone(&gates),
            cache.clone(),
            barrier.clone(),
        );
        // All returned cohorts stay live for later serial validation. Allocation
        // is construction work; the fixed worker loop never grows this buffer.
        let results = Vec::with_capacity(if config.hit {
            usize::try_from(HIT_CALLS).expect("fixed batch fits usize")
        } else {
            1
        });
        threads.push(std::thread::spawn(move || {
            gates.ready.wait();
            gates.start.wait();
            *slot.lock().unwrap() = Some(worker(
                config,
                index,
                &cache,
                &probe,
                barrier.as_ref(),
                results,
            ));
            gates.done.wait();
            gates.exit.wait();
        }));
    }
    gates.ready.wait();
    measurement.construction_finished();
    // Same concrete RacingKey type as the workers: no compatibility shortcut.
    let base = if config.hit {
        Some(resolve(&cache, &probes[0], None, 7)?)
    } else {
        None
    };
    let cold = counts(&probes);
    measurement.cold_finished();
    gates.start.wait();
    gates.done.wait();
    let after_work = counts(&probes);
    let work = after_work.since(cold);
    measurement.work_finished();
    let mut failure = None;
    for slot in &slots {
        match slot
            .lock()
            .unwrap()
            .take()
            .expect("done gate follows worker result")
        {
            Ok(owner) => owners.push(owner),
            Err(error) => {
                failure.get_or_insert(error);
            }
        }
    }
    let validation = (|| {
        if let Some(error) = failure {
            return Err(error);
        }
        let n = config.callers as u64;
        let expected_cold = if config.hit {
            binding_counts(1, true)
        } else {
            Counts::default()
        };
        expect_counts(cold, expected_cold)?;
        let expected_work = if config.hit {
            Counts {
                successful_lookups: HIT_CALLS * n,
                cohort_constructions: HIT_CALLS * n,
                ..Counts::default()
            }
        } else {
            let mut value = binding_counts(n, true);
            value.values_dropped = n - 1;
            value.subscriptions_dropped = n - 1;
            value
        };
        expect_counts(work, expected_work)?;
        if live_candidates(&probes)? != 1 {
            return Err("concurrent cache did not retain exactly one candidate");
        }
        let calls = if config.hit { HIT_CALLS } else { 1 };
        if owners.len() != config.callers || owners.iter().any(|batch| batch.len() as u64 != calls)
        {
            return Err("concurrent worker result count mismatch");
        }
        let (winner, checksum) = validate_binding(&owners, &probes)?;
        if config.hit && winner != 7 || !config.hit && !(1..=n).contains(&winner) {
            return Err("concurrent winner was not a candidate");
        }
        if checksum != winner * calls * n {
            return Err("concurrent checksum mismatch");
        }
        Ok((winner, checksum))
    })();
    let after_validation = counts(&probes);
    measurement.validation_finished();
    // Retain one returned owner after every sibling and the cold owner is dropped.
    let last = owners.last_mut().and_then(Vec::pop);
    owners.clear();
    drop(base);
    let retained = (|| {
        if validation.is_err() {
            return Ok(());
        }
        if live_candidates(&probes)? != 1 {
            return Err("concurrent last owner released early");
        }
        if read(last.as_ref().expect("validated all workers"), &probes[0])? != MUTATED_TAG {
            return Err("concurrent last owner lost mutation");
        }
        Ok(())
    })();
    drop(last);
    gates.exit.wait();
    for thread in threads {
        thread.join().map_err(|_| "concurrent worker panicked")?;
    }
    let final_release = released(&probes);
    let totals = counts(&probes);
    let validation_counts = after_validation.since(after_work);
    let teardown = totals.since(after_validation);
    drop((owners, slots, barrier, gates, cache, probes));
    // Always finish allocation accounting, but preserve the first semantic
    // failure if cleanup also fails. The cleanup diagnostic remains retained.
    let allocations = measurement
        .finish()
        .inspect_err(|error| eprintln!("concurrent allocation cleanup failed: {error}"));
    let (winner, checksum) = validation?;
    retained?;
    final_release?;
    let allocations = allocations?;
    expect_counts(
        validation_counts,
        Counts {
            read_calls: 2 * config.callers as u64 * if config.hit { HIT_CALLS } else { 1 },
            ..Counts::default()
        },
    )?;
    expect_counts(
        teardown,
        Counts {
            read_calls: 1,
            values_dropped: 1,
            subscriptions_dropped: 1,
            ..Counts::default()
        },
    )?;
    Ok(format!(
        concat!(
            "{{\"schema\":\"rca-shared-contention-profile-v1\",\"execution\":\"native-threads-direct-ready\",",
            "\"workload\":\"{}\",\"callers\":{},\"calls_per_worker\":{},\"winner_tag\":{},\"checksum\":{},",
            "\"returned_cohorts_retained_after_work\":{},\"cold_cohorts_retained_after_work\":{},",
            "\"read_phase\":\"serial-validation\",",
            "\"features\":{{\"allocations\":{},\"deterministic\":{}}},",
            "\"cache_namespaces\":1,\"live_candidates_after_work\":1,\"mutation_visible\":true,",
            "\"owners_released\":true,\"all_candidate_values_expired\":true,",
            "\"all_candidate_subscriptions_expired\":true,\"all_producers_closed\":true,",
            "\"counters\":{{\"cold\":{},\"work\":{},\"validation\":{},\"teardown\":{}}},",
            "\"totals\":{},\"allocations\":{}}}"
        ),
        if config.hit {
            "shared-race-hit"
        } else {
            "shared-race-cold"
        },
        config.callers,
        if config.hit { HIT_CALLS } else { 1 },
        winner,
        checksum,
        config.callers as u64 * if config.hit { HIT_CALLS } else { 1 },
        u64::from(config.hit),
        cfg!(feature = "allocations"),
        cfg!(feature = "deterministic"),
        cold.json(),
        work.json(),
        validation_counts.json(),
        teardown.json(),
        totals.json(),
        allocations.map_or_else(|| "null".to_owned(), |record| record.json())
    ))
}

#[cfg(test)]
mod controls {
    use super::*;

    // Equal values alone cannot detect accidentally independent cache bindings.
    #[test]
    fn independent_bindings_are_rejected_by_mutation_oracle() {
        let probes = [Arc::new(Probe::default()), Arc::new(Probe::default())];
        let caches = [SharedKeyCache::new(), SharedKeyCache::new()];
        let owners: Vec<_> = caches
            .iter()
            .zip(&probes)
            .map(|(cache, probe)| vec![resolve(cache, probe, None, 7).unwrap()])
            .collect();
        assert_eq!(
            validate_binding(&owners, &probes),
            Err("concurrent callers retained split bindings")
        );
        drop(owners);
        released(&probes).unwrap();
    }

    // An actual captured reference must defeat the final zero-owner predicate.
    #[test]
    fn retained_reader_is_rejected_until_its_real_release() {
        let probes = [Arc::new(Probe::default())];
        let cache = SharedKeyCache::new();
        let owner = resolve(&cache, &probes[0], None, 7).unwrap();
        let retained = owner.reader().unwrap();
        drop(owner);
        assert_eq!(live_candidates(&probes).unwrap(), 1);
        assert!(released(&probes).is_err());
        assert_eq!(retained.read(|value| value.tag).unwrap(), 7);
        drop(retained);
        released(&probes).unwrap();
    }
}
