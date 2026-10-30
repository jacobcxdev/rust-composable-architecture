//! Borrowed direct-service lookups, distinct from Store/effect integration.
//!
//! Parent providers return 11 and scoped child providers return 29. Distinct
//! values expose accidental parent mutation or a lookup in the wrong environment.
//! Preparation, optional warmup, work, checking and teardown have separate boundaries.

use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use rca_core::{DependencyDefaultPolicy, DependencyDefaults, DependencyKey, DependencyValues};

use crate::measurement::Measurement;

/// Selects uncached, warmed, reset or temporarily overridden dependency lookups.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    /// Gives every work lookup its own prepared but uncached environment.
    Cold,
    /// Reuses providers created during the separate warmup phase.
    Cached,
    /// Drops the previous cached provider before every work lookup.
    Reset,
    /// Reads a temporary child override, then checks the unchanged parent value.
    Scope,
}

/// A validated point with at most 1,024 parent lookup operations.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Config {
    /// Chooses environment preparation, warmup and lookup behaviour.
    mode: Mode,
    /// Canonical CLI name emitted in the record.
    name: &'static str,
    /// Environments per round, except cold mode prebuilds every lookup separately.
    size: usize,
    /// Repeated rounds for warmed/reset/scoped modes, or cold fixture multiplicity.
    iterations: usize,
}

impl Config {
    /// Parses a known mode and bounded unsigned lookup dimensions.
    ///
    /// # Errors
    /// Returns an error for missing, non-decimal, overflowing or unsupported
    /// dimensions, unknown modes, or extra arguments.
    pub(crate) fn parse(mut args: impl Iterator<Item = String>) -> Result<Self, &'static str> {
        let (mode, name) = match args.next().as_deref() {
            Some("dependency-cold") => (Mode::Cold, "dependency-cold"),
            Some("dependency-cached") => (Mode::Cached, "dependency-cached"),
            Some("dependency-reset") => (Mode::Reset, "dependency-reset"),
            Some("dependency-scope") => (Mode::Scope, "dependency-scope"),
            _ => return Err("unknown dependency workload"),
        };
        let dimension = |text: Option<String>| {
            let text = text.ok_or("missing dependency dimension")?;
            if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err("invalid dependency dimension");
            }
            text.parse::<usize>()
                .map_err(|_| "dependency dimension overflow")
        };
        let size = dimension(args.next())?;
        let iterations = dimension(args.next())?;
        if args.next().is_some()
            || !matches!(size, 0 | 1 | 16)
            || !(1..=1024).contains(&iterations)
            || size.checked_mul(iterations).is_none_or(|q| q > 1024)
        {
            return Err("dependency workload requires size 0/1/16 and at most 1024 operations");
        }
        Ok(Self {
            mode,
            name,
            size,
            iterations,
        })
    }

    /// Counts parent lookups; a scoped operation additionally reads one child value.
    fn operations(self) -> usize {
        self.size * self.iterations
    }
}

/// Counts provider construction and destruction without cloning a borrowed provider.
#[derive(Default)]
struct Probe {
    /// Lazy default closures actually invoked, excluding explicit scoped values.
    factories: AtomicUsize,
    /// All provider values constructed, whether lazy defaults or explicit overrides.
    created: AtomicUsize,
    /// Provider destructors observed so far.
    dropped: AtomicUsize,
}

/// A snapshot of provider lifetime counters at one completed synchronous boundary.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Counts {
    /// Lazy default invocations up to this boundary.
    factories: usize,
    /// Provider constructions up to this boundary.
    created: usize,
    /// Provider destructions up to this boundary.
    dropped: usize,
}

impl Probe {
    /// Reads counters after synchronous work, without allocating or retaining providers.
    fn counts(&self) -> Counts {
        Counts {
            factories: self.factories.load(Ordering::Relaxed),
            created: self.created.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
        }
    }
}

/// A non-cloneable provider whose value is borrowed during dependency lookups.
///
/// Constructing a provider clones its probe owner. Borrowed lookups do not clone it.
struct Provider {
    /// Distinguishes parent value 11 from temporary child value 29.
    value: usize,
    /// Keeps lifetime counters available until this provider is destroyed.
    probe: Arc<Probe>,
}

impl Provider {
    /// Counts a real provider construction and retains its lifetime probe.
    fn new(value: usize, probe: &Arc<Probe>) -> Self {
        probe.created.fetch_add(1, Ordering::Relaxed);
        Self {
            value,
            probe: Arc::clone(probe),
        }
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        self.probe.dropped.fetch_add(1, Ordering::Relaxed);
    }
}

/// The fixture's single dependency slot, shared by parent and child environments.
struct Service;

impl DependencyKey for Service {
    type Value = Provider;
}

/// Owns prepared environments and their shared provider-lifetime probe.
struct Fixture {
    /// Independent parent environments prepared before the first phase boundary.
    values: Vec<DependencyValues>,
    /// Counts providers and detects owners retained beyond environment teardown.
    probe: Arc<Probe>,
}

/// Observed lookup count and separate parent/child value sums.
#[derive(Clone, Copy, Debug, Default)]
struct Reads {
    /// Successful parent plus child lookups performed during work.
    count: usize,
    /// Sum of borrowed parent values, independently expected as 11 per operation.
    parents: usize,
    /// Sum of child overrides, independently expected as 29 per scoped operation.
    children: usize,
}

impl Fixture {
    /// Prepares independent environments without performing lazy default lookups.
    ///
    /// # Errors
    /// Returns an error if a fresh environment rejects preparation or registration.
    fn new(config: Config) -> Result<Self, &'static str> {
        let count = if config.mode == Mode::Cold {
            config.operations()
        } else {
            config.size
        };
        let mut fixture = Self {
            values: Vec::with_capacity(count),
            probe: Arc::new(Probe::default()),
        };
        for _ in 0..count {
            let mut values = DependencyValues::for_policy(DependencyDefaultPolicy::Live);
            if config.mode == Mode::Scope {
                values
                    .prepare(|values| values.set::<Service>(Provider::new(11, &fixture.probe)))
                    .map_err(|_| "fresh dependency preparation rejected")?;
            } else {
                let probe = Arc::clone(&fixture.probe);
                values
                    .prepare(|values| {
                        values.register_defaults(DependencyDefaults::<Service>::new().with_live(
                            move || {
                                probe.factories.fetch_add(1, Ordering::Relaxed);
                                Provider::new(11, &probe)
                            },
                        ))
                    })
                    .map_err(|_| "fresh dependency preparation rejected")?
                    .map_err(|_| "fresh dependency registration rejected")?;
            }
            fixture.values.push(values);
        }
        Ok(fixture)
    }

    /// Resolves cached/reset providers before useful work, leaving other modes untouched.
    ///
    /// # Errors
    /// Returns an error if any warmed provider does not contain the parent value.
    fn warm(&mut self, config: Config) -> Result<(), &'static str> {
        if matches!(config.mode, Mode::Cached | Mode::Reset) {
            for values in &mut self.values {
                if black_box(values.get_or_insert::<Service>().value) != 11 {
                    return Err("dependency warm value mismatch");
                }
            }
        }
        Ok(())
    }

    /// Performs the selected borrowed lookups, including reset or scoped-override costs.
    #[inline(never)]
    fn work(&mut self, config: Config) -> Reads {
        let mut reads = Reads::default();
        let rounds = if config.mode == Mode::Cold {
            1
        } else {
            config.iterations
        };
        for _ in 0..rounds {
            for values in &mut self.values {
                if config.mode == Mode::Reset {
                    values.reset_cache();
                }
                if config.mode == Mode::Scope {
                    let value = values.scope(
                        |child| child.set::<Service>(Provider::new(29, &self.probe)),
                        |child| black_box(child.get_or_insert::<Service>().value),
                    );
                    reads.children += value;
                    reads.count += 1;
                }
                reads.parents += black_box(values.get_or_insert::<Service>().value);
                reads.count += 1;
            }
        }
        reads
    }

    /// Checks separate value sums, phase counts and unchanged parent/cache ownership.
    ///
    /// # Errors
    /// Returns an error for missing or incorrect reads, parent mutation, unexpected
    /// construction/destruction counts, or a default created during checking.
    fn validate(
        &mut self,
        config: Config,
        warm: Counts,
        reads: Reads,
    ) -> Result<Counts, &'static str> {
        let q = config.operations();
        let scoped = config.mode == Mode::Scope;
        if reads.count != q * if scoped { 2 } else { 1 }
            || reads.parents != 11 * q
            || reads.children != if scoped { 29 * q } else { 0 }
        {
            return Err("dependency work mismatch");
        }
        let observed = self.probe.counts();
        // Parent and retained-cache validation is outside the measured work.
        for values in &mut self.values {
            if values.get_or_insert::<Service>().value != 11 {
                return Err("dependency parent or cache value changed");
            }
        }
        let (expected_warm, expected) = match config.mode {
            Mode::Cold => (
                Counts::default(),
                Counts {
                    factories: q,
                    created: q,
                    dropped: 0,
                },
            ),
            Mode::Cached => {
                let counts = Counts {
                    factories: config.size,
                    created: config.size,
                    dropped: 0,
                };
                (counts, counts)
            }
            Mode::Reset => (
                Counts {
                    factories: config.size,
                    created: config.size,
                    dropped: 0,
                },
                Counts {
                    factories: config.size + q,
                    created: config.size + q,
                    dropped: q,
                },
            ),
            Mode::Scope => (
                Counts {
                    factories: 0,
                    created: config.size,
                    dropped: 0,
                },
                Counts {
                    factories: 0,
                    created: config.size + q,
                    dropped: q,
                },
            ),
        };
        if warm != expected_warm || observed != expected || self.probe.counts() != observed {
            return Err("dependency factory or lifetime mismatch");
        }
        Ok(observed)
    }

    /// Drops every environment before checking provider counts and remaining probe owners.
    ///
    /// # Errors
    /// Returns an error if a provider or another probe owner remains alive.
    fn teardown(self) -> Result<Counts, &'static str> {
        drop(self.values);
        let counts = self.probe.counts();
        let released = counts.created == counts.dropped && Arc::strong_count(&self.probe) == 1;
        drop(self.probe);
        if !released {
            return Err("dependency providers retained");
        }
        Ok(counts)
    }
}

/// Settles fixture ownership before propagating warmup, work-checking or teardown errors.
///
/// # Errors
/// Returns an error if warmup, work checking or final ownership checking fails.
fn execute(
    config: Config,
    measurement: &mut Measurement,
    mut fixture: Fixture,
) -> Result<(Reads, Counts, Counts, Counts), &'static str> {
    measurement.construction_finished();
    let warmed = fixture.warm(config);
    let warm = fixture.probe.counts();
    if matches!(config.mode, Mode::Cached | Mode::Reset) {
        measurement.warmup_finished();
    }
    let reads = fixture.work(config);
    measurement.work_finished();
    let valid = fixture.validate(config, warm, reads);
    measurement.validation_finished();
    let released = fixture.teardown();
    warmed?;
    Ok((reads, warm, valid?, released?))
}

/// Profiles direct dependency work and formats a record only after owner release.
///
/// # Errors
/// Returns an error for preparation, workload, ownership or allocation-check failures.
pub(crate) fn run(config: Config) -> Result<String, &'static str> {
    let mut measurement = Measurement::new(None);
    // execute settles its owned environments/probes on both success and error.
    let result =
        Fixture::new(config).and_then(|fixture| execute(config, &mut measurement, fixture));
    let allocation_result = measurement.finish();
    let (reads, warm, work, final_counts) = result?;
    let allocations = allocation_result?.map_or_else(|| "null".to_owned(), |record| record.json());
    let feature_profile = if cfg!(feature = "deterministic") {
        "deterministic-enabled"
    } else {
        "minimal-production"
    };
    Ok(format!(
        "{{\"schema\":\"rca-dependency-performance-profile-v1\",\"feature_profile\":\"{feature_profile}\",\"features\":{{\"deterministic\":{},\"allocations\":{}}},\"workload\":\"{}\",\"size\":{},\"iterations\":{},\"operations\":{},\"reads\":{},\"checksum\":{},\"parent_checksum\":{},\"child_checksum\":{},\"warm_factories\":{},\"work_factories\":{},\"work_drops\":{},\"providers_created\":{},\"providers_dropped\":{},\"providers_remaining\":0,\"domain_clones\":0,\"policy\":\"live\",\"driver_contract\":\"direct-synchronous-v1\",\"outcome\":\"validated\",\"allocations\":{}}}",
        cfg!(feature = "deterministic"),
        cfg!(feature = "allocations"),
        config.name,
        config.size,
        config.iterations,
        config.operations(),
        reads.count,
        reads.parents + reads.children,
        reads.parents,
        reads.children,
        warm.factories,
        work.factories - warm.factories,
        work.dropped - warm.dropped,
        final_counts.created,
        final_counts.dropped,
        allocations,
    ))
}

#[cfg(test)]
pub(crate) mod tests {
    //! Mutations separate lookup correctness, factory timing and final owner release.

    use super::*;

    /// Uses one parent lookup so each mutation has a literal expected value and owner.
    fn config(name: &str) -> Config {
        Config::parse([name, "1", "1"].into_iter().map(str::to_owned)).unwrap()
    }

    #[test]
    fn missing_work_and_wrong_checksum_are_rejected() {
        let config = config("dependency-cold");
        let mut fixture = Fixture::new(config).unwrap();
        assert_eq!(
            fixture.validate(config, Counts::default(), Reads::default()),
            Err("dependency work mismatch")
        );
        let mut reads = fixture.work(config);
        reads.parents = 29;
        assert_eq!(
            fixture.validate(config, Counts::default(), reads),
            Err("dependency work mismatch")
        );
        fixture.teardown().unwrap();
    }

    #[test]
    fn missing_warm_factory_is_rejected_even_with_correct_reads() {
        let config = config("dependency-cached");
        let mut fixture = Fixture::new(config).unwrap();
        let warm = fixture.probe.counts();
        let reads = fixture.work(config);
        assert_eq!(reads.parents, 11);
        assert_eq!(
            fixture.validate(config, warm, reads),
            Err("dependency factory or lifetime mismatch")
        );
        fixture.teardown().unwrap();
    }

    #[test]
    fn mutation_of_parent_after_scope_is_rejected() {
        let config = config("dependency-scope");
        let mut fixture = Fixture::new(config).unwrap();
        let warm = fixture.probe.counts();
        let reads = fixture.work(config);
        fixture.values[0].set::<Service>(Provider::new(29, &fixture.probe));
        assert_eq!(
            fixture.validate(config, warm, reads),
            Err("dependency parent or cache value changed")
        );
        fixture.teardown().unwrap();
    }

    #[test]
    fn retained_environment_is_rejected_until_last_owner_drops() {
        let config = config("dependency-cold");
        let mut fixture = Fixture::new(config).unwrap();
        let reads = fixture.work(config);
        fixture.validate(config, Counts::default(), reads).unwrap();
        let retained = fixture.values[0].clone();
        let probe = Arc::clone(&fixture.probe);
        assert_eq!(fixture.teardown(), Err("dependency providers retained"));
        assert_eq!(probe.counts().dropped, 0);
        drop(retained);
        assert_eq!(probe.counts().dropped, 1);
    }

    /// Separates failed-work cleanup from a real cache owner retained past profiling.
    #[cfg(feature = "allocations")]
    #[cfg_attr(not(feature = "allocations"), test)]
    pub(crate) fn allocation_failure_cleanup_and_retention_controls() {
        if !crate::tests::in_child(
            "dependencies::tests::allocation_failure_cleanup_and_retention_controls",
        ) {
            return;
        }

        // Exercise production execute's failure cleanup, not just an oracle.
        let config = config("dependency-cached");
        let mut measurement = Measurement::new(None);
        let mut fixture = Fixture::new(config).unwrap();
        fixture.values.clear();
        let failed = execute(config, &mut measurement, fixture);
        let allocation_result = measurement.finish();
        assert!(matches!(failed, Err("dependency work mismatch")));
        assert!(
            allocation_result.is_ok(),
            "failed work must release all owned resources"
        );

        // A real retained cache owner must fail both lifetime and allocator checks.
        let mut measurement = Measurement::new(None);
        let mut fixture = Fixture::new(config).unwrap();
        fixture.warm(config).unwrap();
        let retained = fixture.values[0].clone();
        let failed = execute(config, &mut measurement, fixture);
        let allocation_result = measurement.finish();
        drop(retained);
        assert!(matches!(failed, Err("dependency providers retained")));
        assert!(
            allocation_result.is_err(),
            "retained cache must fail Measurement(None)"
        );
    }
}
