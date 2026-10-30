//! Exact-work root and mounted Feature transaction witnesses.
//!
//! Installation waits for initial task bodies before the warm work boundary.
//! Each transaction then checks causal completion without copying owned state or cache data.

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use rca_core::{
    Feature, FeatureContext, MountIdentity, MountSite, OptionalMount, RootMount, Store, Task,
    TaskContext,
};

use crate::host::Host;
use crate::measurement::{ExecutorCache, Measurement};

/// Bounds failure latency; elapsed time is not a performance metric.
const DEADLINE: Duration = Duration::from_secs(10);
/// Shared rejection text for incomplete work, invalid state or retained ownership.
const INVALID: &str = "Feature transaction work or ownership invalid";

/// Selects one root or mounted path at the two qualified iteration counts.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Config {
    name: &'static str,
    mounted: bool,
    iterations: u64,
}

impl Config {
    /// Requires one resident child and either one or 64 transactions.
    pub(crate) fn parse(mut args: impl Iterator<Item = String>) -> Result<Self, &'static str> {
        let (name, mounted) = match args.next().as_deref() {
            Some("feature-root-native") => ("feature-root-native", false),
            Some("feature-mounted-native") => ("feature-mounted-native", true),
            _ => return Err("unknown Feature workload"),
        };
        let width = crate::workloads::integer(args.next())?;
        let iterations = crate::workloads::integer(args.next())?;
        if width != 1 || !matches!(iterations, 1 | 64) || args.next().is_some() {
            return Err("Feature workloads require SIZE=1 and ITERATIONS=1 or 64");
        }
        Ok(Self {
            name,
            mounted,
            iterations,
        })
    }
}

/// Counts real callbacks and ownership; completion boundaries order the final reads.
#[derive(Default)]
struct Probes {
    /// State, root feature and mutable cache, in that order.
    created: [AtomicU64; 3],
    dropped: [AtomicU64; 3],
    owners: Mutex<Vec<Weak<()>>>,
    root_calls: AtomicU64,
    child_calls: AtomicU64,
    path_reads: AtomicU64,
    path_writes: AtomicU64,
    identities: AtomicU64,
    initial_selects: AtomicU64,
    initial_factories: AtomicU64,
    initial_bodies: AtomicU64,
}

/// Counts disposal and exposes a weak identity without retaining the measured owner.
struct Guard {
    probes: Arc<Probes>,
    _owner: Arc<()>,
    /// Index in the state/feature/cache counter arrays.
    kind: usize,
}
impl Guard {
    /// Registers one distinct lifetime before its measured owner becomes visible.
    fn new(probes: &Arc<Probes>, kind: usize) -> Self {
        let owner = Arc::new(());
        probes.owners.lock().unwrap().push(Arc::downgrade(&owner));
        probes.created[kind].fetch_add(1, Ordering::Relaxed);
        Self {
            probes: Arc::clone(probes),
            _owner: owner,
            kind,
        }
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        self.probes.dropped[self.kind].fetch_add(1, Ordering::Relaxed);
    }
}

/// Move-only child state; `Cell` deliberately prevents a `Sync` requirement.
struct ChildState {
    value: Cell<u64>,
    checksum: u64,
    cache_seen: u64,
    guard: Guard,
}
/// Keeps an unrelated literal payload beside the single resident child.
struct State {
    child: Option<ChildState>,
    unrelated: [u64; 16],
    probes: Arc<Probes>,
}
/// Private mutable feature history, separate from the state's observed value.
struct Cache {
    calls: Cell<u64>,
    guard: Guard,
}
impl Cache {
    /// Allocates the cache's independent ownership witness.
    fn new(probes: &Arc<Probes>) -> Self {
        Self {
            calls: Cell::new(0),
            guard: Guard::new(probes, 2),
        }
    }
    /// Updates state and private history together, without cloning either owner.
    fn increment(&self, state: &mut ChildState) {
        self.calls.set(self.calls.get() + 1);
        state.value.set(state.value.get() + 1);
        state.checksum += state.value.get();
        state.cache_seen = self.calls.get();
    }
}
enum RootAction {
    Increment,
}
enum ChildAction {
    Increment,
}
/// Owns the cache only in the root-dispatch control.
struct RootFeature {
    cache: Option<Cache>,
    guard: Guard,
}
impl Feature for RootFeature {
    type State = State;
    type Action = RootAction;
    fn update(
        &mut self,
        state: &mut State,
        _: &RootAction,
        _: &mut FeatureContext<'_, State, RootAction>,
    ) {
        self.guard.probes.root_calls.fetch_add(1, Ordering::Relaxed);
        self.cache
            .as_ref()
            .expect("root control owns its cache")
            .increment(state.child.as_mut().expect("resident child"));
    }
}
/// Owns the same cache behaviour behind a real mounted route.
struct ChildFeature(Cache);
impl Feature for ChildFeature {
    type State = ChildState;
    type Action = ChildAction;
    fn update(
        &mut self,
        state: &mut ChildState,
        _: &ChildAction,
        _: &mut FeatureContext<'_, ChildState, ChildAction>,
    ) {
        self.0
            .guard
            .probes
            .child_calls
            .fetch_add(1, Ordering::Relaxed);
        self.0.increment(state);
    }
}

/// Counts structural reads separately from mutable transaction routing.
fn read_child(state: &State) -> Option<&ChildState> {
    state.probes.path_reads.fetch_add(1, Ordering::Relaxed);
    state.child.as_ref()
}
/// Counts actual mutable path traversal for the mounted transaction.
fn write_child(state: &mut State) -> Option<&mut ChildState> {
    state.probes.path_writes.fetch_add(1, Ordering::Relaxed);
    state.child.as_mut()
}

/// Cumulative callback counts captured at installation and work boundaries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Counts {
    root: u64,
    child: u64,
    reads: u64,
    writes: u64,
    identities: u64,
    initial_selects: u64,
    initial_factories: u64,
    initial_bodies: u64,
}
impl Counts {
    /// Reads counters only after the corresponding host completion boundary.
    fn read(probes: &Probes) -> Self {
        Self {
            root: probes.root_calls.load(Ordering::Relaxed),
            child: probes.child_calls.load(Ordering::Relaxed),
            reads: probes.path_reads.load(Ordering::Relaxed),
            writes: probes.path_writes.load(Ordering::Relaxed),
            identities: probes.identities.load(Ordering::Relaxed),
            initial_selects: probes.initial_selects.load(Ordering::Relaxed),
            initial_factories: probes.initial_factories.load(Ordering::Relaxed),
            initial_bodies: probes.initial_bodies.load(Ordering::Relaxed),
        }
    }
}

/// Copies scalar validation facts without extending state or cache ownership.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Projection {
    value: u64,
    checksum: u64,
    cache_seen: u64,
    unrelated: u64,
}
/// Reads the resident child and the untouched unrelated checksum.
fn project(state: &State) -> Projection {
    let child = state
        .child
        .as_ref()
        .expect("fixture retains its only child");
    Projection {
        value: child.value.get(),
        checksum: child.checksum,
        cache_seen: child.cache_seen,
        unrelated: state.unrelated.iter().sum(),
    }
}

/// Retains the real host, optional child route and initial-work completion channel.
struct Fixture {
    host: Host,
    host_owner: Weak<()>,
    store: Store<State, RootAction>,
    site: Option<MountSite<State, ChildState, ChildAction, RootAction>>,
    child: Option<TaskContext<ChildState, ChildAction>>,
    initial: async_channel::Receiver<()>,
    probes: Arc<Probes>,
    /// Snapshot after installation and all initial task bodies complete.
    cold: Counts,
    ready_completed: bool,
    commits: u64,
    causal_completed: u64,
}
impl Fixture {
    /// Builds the selected topology within the construction allocation phase.
    fn new(config: Config) -> Result<Self, &'static str> {
        let (host, runtime, host_owner) = Host::new(true, 16_384)?;
        let probes = Arc::new(Probes::default());
        let state = State {
            child: Some(ChildState {
                value: Cell::new(0),
                checksum: 0,
                cache_seen: 0,
                guard: Guard::new(&probes, 0),
            }),
            unrelated: [7; 16],
            probes: Arc::clone(&probes),
        };
        let feature = RootFeature {
            cache: (!config.mounted).then(|| Cache::new(&probes)),
            guard: Guard::new(&probes, 1),
        };
        let (completed, initial) = async_channel::bounded(2);
        let root_probes = Arc::clone(&probes);
        let root_completed = completed.clone();
        let root = RootMount::new(feature).on_initial(
            |state: &State| {
                state.probes.initial_selects.fetch_add(1, Ordering::Relaxed);
                project(state).unrelated
            },
            move |_, _| {
                root_probes
                    .initial_factories
                    .fetch_add(1, Ordering::Relaxed);
                let probes = Arc::clone(&root_probes);
                let completed = root_completed.clone();
                Task::run(move |_| async move {
                    probes.initial_bodies.fetch_add(1, Ordering::Relaxed);
                    completed
                        .try_send(())
                        .expect("bounded initial completion slot");
                })
            },
        );
        let builder = root.builder(state, runtime);
        let (store, site) = if config.mounted {
            let make_probes = Arc::clone(&probes);
            let initial_probes = Arc::clone(&probes);
            let mount = OptionalMount::new(
                read_child,
                write_child,
                |state: &State| {
                    state.probes.identities.fetch_add(1, Ordering::Relaxed);
                    state.child.as_ref().map(|_| MountIdentity {
                        key: (),
                        replacement: 0,
                        configuration: 0,
                    })
                },
                move |_| ChildFeature(Cache::new(&make_probes)),
            )
            .on_initial(
                |state: &ChildState| {
                    state
                        .guard
                        .probes
                        .initial_selects
                        .fetch_add(1, Ordering::Relaxed);
                    state.value.get()
                },
                move |_, _| {
                    initial_probes
                        .initial_factories
                        .fetch_add(1, Ordering::Relaxed);
                    let probes = Arc::clone(&initial_probes);
                    let completed = completed.clone();
                    Task::run(move |_| async move {
                        probes.initial_bodies.fetch_add(1, Ordering::Relaxed);
                        completed
                            .try_send(())
                            .expect("bounded initial completion slot");
                    })
                },
            );
            let (builder, site) = builder.mount_with_site(mount);
            (builder.build().map_err(|_| INVALID)?, Some(site))
        } else {
            drop(completed);
            (builder.build().map_err(|_| INVALID)?, None)
        };
        Ok(Self {
            host,
            host_owner,
            store,
            site,
            child: None,
            initial,
            cold: Counts::read(&probes),
            probes,
            ready_completed: false,
            commits: 0,
            causal_completed: 0,
        })
    }

    /// Waits for readiness and initial work before acquiring the optional child route.
    fn install(&mut self, config: Config) -> Result<(), &'static str> {
        self.child = self.host.run_bounded(
            async {
                self.store.ready().await.map_err(|_| INVALID)?;
                for _ in 0..=u64::from(config.mounted) {
                    self.initial.recv().await.map_err(|_| INVALID)?;
                }
                if let Some(site) = &self.site {
                    self.store
                        .acquire(site)
                        .await
                        .map_err(|_| INVALID)?
                        .ok_or(INVALID)
                        .map(Some)
                } else {
                    Ok(None)
                }
            },
            DEADLINE,
        )??;
        self.ready_completed = true;
        self.cold = Counts::read(&self.probes);
        Ok(())
    }
}

/// Measures only repeated commits and their causal completion on the installed route.
#[inline(never)]
fn profile_feature_transactions(
    fixture: &mut Fixture,
    iterations: u64,
) -> Result<(), &'static str> {
    let Fixture {
        host,
        store,
        child,
        commits,
        causal_completed,
        ..
    } = fixture;
    host.run_bounded(
        async {
            for _ in 0..iterations {
                let task = if let Some(child) = child {
                    child
                        .send(ChildAction::Increment)
                        .await
                        .map_err(|_| INVALID)?
                } else {
                    store
                        .send(RootAction::Increment)
                        .await
                        .map_err(|_| INVALID)?
                };
                *commits += 1;
                task.finish().await;
                if !task.is_finished() || task.is_cancelled() {
                    return Err(INVALID);
                }
                *causal_completed += 1;
            }
            Ok(())
        },
        DEADLINE,
    )?
}

#[cfg(test)]
/// Real skipped work or retained ownership, rather than forged final counters.
#[derive(Clone, Copy)]
enum Fault {
    SkipLast,
    RetainOwner,
}

/// Combines callback, scalar-state and lifetime facts after teardown.
struct Outcome {
    projection: Projection,
    cold: Counts,
    work: Counts,
    ready_completed: bool,
    commits: u64,
    causal_completed: u64,
    created: [u64; 3],
    dropped: [u64; 3],
    owners_released: bool,
    host_stopped: bool,
}
impl Outcome {
    /// Checks hand-derived literals and exact path counts for each supported shape.
    fn validate(&self, config: Config) -> Result<(), &'static str> {
        let mounted = u64::from(config.mounted);
        let initial = 1 + mounted;
        let cold = Counts {
            root: 0,
            child: 0,
            reads: 2 * mounted,
            writes: 0,
            identities: mounted,
            initial_selects: initial,
            initial_factories: initial,
            initial_bodies: initial,
        };
        let work = Counts {
            root: config.iterations * (1 - mounted),
            child: config.iterations * mounted,
            writes: config.iterations * mounted,
            identities: (config.iterations + 1) * mounted,
            ..cold
        };
        // Hand-derived sequence sums keep the oracle independent of reduction.
        let checksum = if config.iterations == 1 { 1 } else { 2080 };
        if self.cold != cold
            || self.work != work
            || !self.ready_completed
            || self.commits != config.iterations
            || self.causal_completed != config.iterations
            || self.projection
                != (Projection {
                    value: config.iterations,
                    checksum,
                    cache_seen: config.iterations,
                    unrelated: 112,
                })
            || self.created != [1; 3]
            || self.dropped != [1; 3]
            || !self.owners_released
            || !self.host_stopped
        {
            return Err(INVALID);
        }
        Ok(())
    }

    /// Formats the checked result after allocation measurement ends.
    fn json(&self, config: Config, allocations: &str) -> String {
        format!(
            concat!(
                "{{\"schema\":\"rca-feature-transactions-profile-v1\",\"workload\":\"{}\",\"size\":1,\"depth\":1,\"iterations\":{},",
                "\"feature_profile\":\"{}\",\"features\":{{\"deterministic\":{},\"allocations\":{}}},",
                "\"ready_completed\":{},\"commits\":{},\"commit_basis\":\"successful-send-receipts\",\"causal_completed\":{},",
                "\"final_value\":{},\"checksum\":{},\"cache_seen\":{},\"unrelated_checksum\":{},",
                "\"root_reductions\":{},\"child_reductions\":{},\"cold_path_reads\":{},\"work_path_reads\":{},\"work_path_writes\":{},\"work_identity_checks\":{},",
                "\"initial_selectors\":{},\"initial_factories\":{},\"initial_bodies\":{},",
                "\"owners_created\":{:?},\"owners_dropped\":{:?},\"owners_released\":{},\"host_stopped\":{},\"allocations\":{}}}"
            ),
            config.name,
            config.iterations,
            if cfg!(feature = "deterministic") {
                "deterministic-enabled"
            } else {
                "minimal-production"
            },
            cfg!(feature = "deterministic"),
            cfg!(feature = "allocations"),
            self.ready_completed,
            self.commits,
            self.causal_completed,
            self.projection.value,
            self.projection.checksum,
            self.projection.cache_seen,
            self.projection.unrelated,
            self.work.root,
            self.work.child,
            self.cold.reads,
            self.work.reads - self.cold.reads,
            self.work.writes - self.cold.writes,
            self.work.identities - self.cold.identities,
            self.work.initial_selects,
            self.work.initial_factories,
            self.work.initial_bodies,
            self.created,
            self.dropped,
            self.owners_released,
            self.host_stopped,
            allocations
        )
    }
}

/// Runs a qualified native workload and emits a record only after validation.
pub(crate) fn run(config: Config) -> Result<String, &'static str> {
    run_inner(
        config,
        #[cfg(test)]
        None,
    )
}

/// Keeps construction, installation, work, validation and teardown allocation phases distinct.
fn run_inner(config: Config, #[cfg(test)] fault: Option<Fault>) -> Result<String, &'static str> {
    let mut measurement = Measurement::new(Some(ExecutorCache::Native));
    let mut fixture = Fixture::new(config)?;
    measurement.construction_finished();
    fixture.install(config)?;
    measurement.cold_finished();
    #[cfg(test)]
    let retained = fault
        .filter(|f| matches!(f, Fault::RetainOwner))
        .and_then(|_| {
            fixture
                .probes
                .owners
                .lock()
                .unwrap()
                .last()
                .unwrap()
                .upgrade()
        });
    let iterations = config.iterations;
    #[cfg(test)]
    let iterations = iterations - u64::from(matches!(fault, Some(Fault::SkipLast)));
    profile_feature_transactions(&mut fixture, iterations)?;
    measurement.work_finished();
    let work = Counts::read(&fixture.probes);
    let projection = fixture
        .host
        .run_bounded(fixture.store.read(project), DEADLINE)?
        .map_err(|_| INVALID)?;
    measurement.validation_finished();
    let Fixture {
        host,
        host_owner,
        store,
        site,
        child,
        initial,
        probes,
        cold,
        ready_completed,
        commits,
        causal_completed,
    } = fixture;
    drop(child);
    drop(site);
    drop(initial);
    let state = host
        .run_bounded(store.shutdown_into_state(), DEADLINE)?
        .map_err(|_| INVALID)?;
    if project(&state) != projection {
        return Err(INVALID);
    }
    drop(state);
    drop(host);
    let outcome = Outcome {
        projection,
        cold,
        work,
        ready_completed,
        commits,
        causal_completed,
        created: std::array::from_fn(|i| probes.created[i].load(Ordering::Relaxed)),
        dropped: std::array::from_fn(|i| probes.dropped[i].load(Ordering::Relaxed)),
        owners_released: probes
            .owners
            .lock()
            .unwrap()
            .iter()
            .all(|owner| owner.upgrade().is_none()),
        host_stopped: host_owner.upgrade().is_none(),
    };
    drop(probes);
    drop(host_owner);
    #[cfg(test)]
    drop(retained);
    let allocations = measurement
        .finish()?
        .map_or_else(|| "null".to_owned(), |record| record.json());
    outcome.validate(config)?;
    Ok(outcome.json(config, &allocations))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{Config, Fault, run_inner};

    #[cfg_attr(not(feature = "allocations"), test)]
    pub(crate) fn rejects_skipped_work_and_retained_owner() {
        #[cfg(feature = "allocations")]
        if !crate::tests::in_child(
            "feature_transactions::tests::rejects_skipped_work_and_retained_owner",
        ) {
            return;
        }
        let mut errors = Vec::new();
        for name in ["feature-root-native", "feature-mounted-native"] {
            for fault in [Fault::SkipLast, Fault::RetainOwner] {
                let config =
                    Config::parse([name, "1", "1"].into_iter().map(str::to_owned)).unwrap();
                errors.push(run_inner(config, Some(fault)).err());
            }
        }
        assert_eq!(errors, [Some(super::INVALID); 4]);
    }

    // Missing dispatch or silently skipped domain work must fail these literal oracles.
    #[cfg_attr(not(feature = "allocations"), test)]
    pub(crate) fn root_and_mounted_dispatch_complete_one_and_sixty_four_increments() {
        #[cfg(feature = "allocations")]
        if !crate::tests::in_child(
            "feature_transactions::tests::root_and_mounted_dispatch_complete_one_and_sixty_four_increments",
        ) {
            return;
        }
        for name in ["feature-root-native", "feature-mounted-native"] {
            for (iterations, checksum) in [(1, 1), (64, 2080)] {
                let record = crate::dispatch(
                    [name.to_owned(), "1".to_owned(), iterations.to_string()].into_iter(),
                )
                .expect("Feature workload must dispatch and validate real work");
                let value: serde_json::Value = serde_json::from_str(&record).unwrap();
                assert_eq!(value["iterations"], iterations);
                assert_eq!(value["commits"], iterations);
                assert_eq!(value["causal_completed"], iterations);
                assert_eq!(value["final_value"], iterations);
                assert_eq!(value["checksum"], checksum);
                assert_eq!(value["unrelated_checksum"], 112);
                assert_eq!(value["owners_released"], true);
                assert_eq!(value["ready_completed"], true);
                assert_eq!(value["cache_seen"], iterations);
                assert_eq!(value["host_stopped"], true);
                assert_eq!(value["owners_created"], serde_json::json!([1, 1, 1]));
                assert_eq!(value["owners_dropped"], serde_json::json!([1, 1, 1]));
                let mounted = name == "feature-mounted-native";
                assert_eq!(
                    value["root_reductions"],
                    if mounted { 0 } else { iterations }
                );
                assert_eq!(
                    value["child_reductions"],
                    if mounted { iterations } else { 0 }
                );
                assert_eq!(value["work_path_reads"], 0);
                assert_eq!(
                    value["work_path_writes"],
                    if mounted { iterations } else { 0 }
                );
                assert_eq!(
                    value["work_identity_checks"],
                    if mounted { iterations } else { 0 }
                );
            }
        }
    }

    #[test]
    fn rejects_dimensions_outside_the_qualified_workload() {
        for args in [
            vec![],
            vec!["feature-root-native", "1"],
            vec!["feature-root-native", "0", "1"],
            vec!["feature-root-native", "2", "1"],
            vec!["feature-mounted-native", "1", "0"],
            vec!["feature-mounted-native", "1", "2"],
            vec!["feature-mounted-native", "1", "65"],
            vec!["feature-mounted-native", "1", "-1"],
            vec!["feature-mounted-native", "1", "1", "extra"],
            vec!["feature-mounted-deterministic", "1", "1"],
        ] {
            assert!(Config::parse(args.into_iter().map(str::to_owned)).is_err());
        }
    }
}
