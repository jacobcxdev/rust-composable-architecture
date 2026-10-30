//! Finite public Feature topology and ownership witnesses.
//!
//! Work includes real typed transactions and topology changes, not synthetic route lookups.
//! Retirement-only runs isolate one optional-child removal; stale-route checks follow the measured work.

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use rca_core::{
    EntryMount, Feature, FeatureContext, KeyedState, MountChildren, MountIdentity, MountSite,
    OptionalMount, RootMount, Store, TaskAccessReason, TaskContext, TaskProgress,
};

use crate::host::Host;
use crate::measurement::{ExecutorCache, Measurement};

/// Shared failure text for rejected useful-work and lifetime evidence.
const INVALID: &str = "topology work or ownership invalid";
/// Failure deadline, not a timing metric; no external work or timers are scheduled.
const DEADLINE: Duration = Duration::from_secs(30);
/// Caps ready client and actor progress for the largest supported topology.
const DRIVE_BUDGET: usize = 65_536;
/// Unrelated owned payload whose address and contents must survive topology changes.
const PAYLOAD_BYTES: usize = 1_048_576;
/// Fixed fill pattern checked before work, after work and after state extraction.
const PAYLOAD_BYTE: u8 = 0xa5;

/// Selects a finite topology and either mixed operations or retirement alone.
#[derive(Clone, Copy)]
struct Config {
    native: bool,
    retirement: bool,
    depth: usize,
    width: usize,
    operations: u64,
}

/// Rejects unavailable hosts and dimensions outside the literal oracle table.
fn parse(mut args: impl Iterator<Item = String>) -> Result<Config, &'static str> {
    let (native, retirement) = match args.next().as_deref() {
        Some("topology-native") => (true, false),
        Some("topology-deterministic") if cfg!(feature = "deterministic") => (false, false),
        Some("topology-retirement-native") => (true, true),
        Some("topology-retirement-deterministic") if cfg!(feature = "deterministic") => {
            (false, true)
        }
        _ => return Err("unknown or unavailable topology workload"),
    };
    let depth = args.next().and_then(|s| s.parse().ok()).ok_or(INVALID)?;
    let width = args.next().and_then(|s| s.parse().ok()).ok_or(INVALID)?;
    let operations = args.next().and_then(|s| s.parse().ok()).ok_or(INVALID)?;
    if retirement && (!matches!((depth, width), (5 | 20, 1)) || operations != 1) {
        return Err("retirement topology requires depth 5 or 20, width 1 and one removal");
    }
    if !matches!((depth, width), (1, 0 | 1 | 1000 | 10000) | (5 | 20, 1))
        || !matches!(operations, 1 | 64)
        || args.next().is_some()
    {
        return Err(
            "topology requires one of six finite depth/width points and 1 or 64 operations",
        );
    }
    Ok(Config {
        native,
        retirement,
        depth,
        width,
        operations,
    })
}

/// Separates real reductions, typed access and state/feature/cache lifetime counts.
#[derive(Default)]
struct Probes {
    /// State, feature and cache owners, in that order.
    created: [AtomicU64; 3],
    dropped: [AtomicU64; 3],
    owners: Mutex<Vec<Weak<()>>>,
    leaf_reductions: AtomicU64,
    ancestor_reductions: AtomicU64,
    root_structural_reductions: AtomicU64,
    root_empty_reductions: AtomicU64,
    typed_reads: AtomicU64,
    typed_updates: AtomicU64,
    #[cfg(test)]
    retain_kind: AtomicU64,
    #[cfg(test)]
    retained_feature: Mutex<Option<FeaturePayload>>,
    #[cfg(test)]
    retained_cache: Mutex<Option<Cache>>,
}

/// Attaches a distinct weak lifetime witness and a disposal count to one owner.
struct Guard {
    probes: Arc<Probes>,
    _owner: Arc<()>,
    /// Index in the state/feature/cache counter arrays.
    kind: usize,
}

impl Guard {
    /// Registers an owner without letting the retained probe prolong its lifetime.
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

/// Move-only chain state whose `Cell` permits `Send` without requiring `Sync`.
struct Node {
    child: Option<Box<Node>>,
    value: Cell<u64>,
    checksum: u64,
    cache_seen: u64,
    guard: Guard,
}

/// Constructs only a parser-admitted finite chain, with one state guard per node.
fn node(depth: usize, probes: &Arc<Probes>) -> Node {
    Node {
        child: (depth > 1).then(|| Box::new(node(depth - 1, probes))),
        value: Cell::new(0),
        checksum: 0,
        cache_seen: 0,
        guard: Guard::new(probes, 0),
    }
}

/// Owns the keyed chains and one unrelated payload that must never be reconstructed.
struct State {
    rows: KeyedState<usize, Node>,
    payload: Box<[u8]>,
    depth: usize,
    selected: usize,
    guard: Guard,
}

/// Private reduction history that distinguishes retained features from reconstructed ones.
struct Cache {
    calls: Cell<u64>,
    _guard: Guard,
}

impl Cache {
    /// Starts one fresh cache and its independent lifetime witness.
    fn new(probes: &Arc<Probes>) -> Self {
        Self {
            calls: Cell::new(0),
            _guard: Guard::new(probes, 2),
        }
    }
}

/// Public structural operations used by the two fixed workload schedules.
enum RootAction {
    Unchanged,
    Reverse,
    Remove,
    Reinsert,
    /// Leaves the keyed entry intact while removing its optional descendant chain.
    RetireChildren,
}
/// Counts root work separately so leaf dispatch cannot silently bubble upwards.
struct RootFeature {
    guard: Guard,
    _cache: Cache,
}

impl Feature for RootFeature {
    type State = State;
    type Action = RootAction;

    fn update(
        &mut self,
        state: &mut State,
        action: &RootAction,
        _: &mut FeatureContext<'_, State, RootAction>,
    ) {
        match action {
            RootAction::Unchanged => {
                self.guard
                    .probes
                    .root_empty_reductions
                    .fetch_add(1, Ordering::Relaxed);
            }
            RootAction::Reverse => {
                self.guard
                    .probes
                    .root_structural_reductions
                    .fetch_add(1, Ordering::Relaxed);
                state.rows.reverse();
            }
            RootAction::Remove => {
                self.guard
                    .probes
                    .root_structural_reductions
                    .fetch_add(1, Ordering::Relaxed);
                drop(
                    state
                        .rows
                        .remove(&state.selected)
                        .expect("selected entry exists"),
                );
            }
            RootAction::Reinsert => {
                self.guard
                    .probes
                    .root_structural_reductions
                    .fetch_add(1, Ordering::Relaxed);
                state
                    .rows
                    .insert(state.selected, node(state.depth, &state.guard.probes));
            }
            RootAction::RetireChildren => {
                self.guard
                    .probes
                    .root_structural_reductions
                    .fetch_add(1, Ordering::Relaxed);
                // Preserve the keyed entry's stamp: its optional child, not the
                // collection, must retire the non-empty descendant DFS prefix.
                drop(
                    state
                        .rows
                        .get_mut(&state.selected)
                        .expect("selected entry survives")
                        .child
                        .take(),
                );
            }
        }
    }
}

/// Moves a feature guard and its cache together in the retention control.
struct FeaturePayload {
    guard: Guard,
    cache: Cache,
}

/// Holds real feature data that the test-only destructor can deliberately retain.
struct NodeFeature {
    payload: Option<FeaturePayload>,
}

impl NodeFeature {
    /// Creates one feature/cache pair for a newly installed node.
    fn new(probes: &Arc<Probes>) -> Self {
        Self {
            payload: Some(FeaturePayload {
                guard: Guard::new(probes, 1),
                cache: Cache::new(probes),
            }),
        }
    }
}

// The first NodeFeature destroyed is in the selected old subtree. A control
// moves its complete owned data, not merely a weak-lifetime marker.
#[cfg(test)]
impl Drop for NodeFeature {
    fn drop(&mut self) {
        let payload = self
            .payload
            .take()
            .expect("Feature owns its payload until drop");
        let probes = Arc::clone(&payload.guard.probes);
        match probes.retain_kind.swap(0, Ordering::Relaxed) {
            1 => *probes.retained_feature.lock().unwrap() = Some(payload),
            2 => *probes.retained_cache.lock().unwrap() = Some(payload.cache),
            _ => {}
        }
    }
}

#[cfg(test)]
/// Disarms retention and breaks deliberate probe/payload cycles on every exit path.
struct RetentionCleanup(Arc<Probes>);

#[cfg(test)]
impl Drop for RetentionCleanup {
    fn drop(&mut self) {
        // Disarm before taking payloads so an early-error fixture drop cannot
        // install another retained owner. Drop outside locks to break cycles.
        self.0.retain_kind.store(0, Ordering::Relaxed);
        let feature = self.0.retained_feature.lock().unwrap().take();
        let cache = self.0.retained_cache.lock().unwrap().take();
        drop(feature);
        drop(cache);
    }
}

impl Feature for NodeFeature {
    type State = Node;
    type Action = ();

    fn update(&mut self, state: &mut Node, (): &(), _: &mut FeatureContext<'_, Node, ()>) {
        let payload = self
            .payload
            .as_ref()
            .expect("live Feature owns its payload");
        if state.child.is_some() {
            payload
                .guard
                .probes
                .ancestor_reductions
                .fetch_add(1, Ordering::Relaxed);
        } else {
            payload
                .guard
                .probes
                .leaf_reductions
                .fetch_add(1, Ordering::Relaxed);
        }
        payload.cache.calls.set(payload.cache.calls.get() + 1);
        state.value.set(state.value.get() + 1);
        state.checksum += state.value.get();
        state.cache_seen = payload.cache.calls.get();
    }
}

/// Resolves a node through the real root's typed mount path.
type LeafSite = MountSite<State, Node, (), RootAction>;
/// Retains authored sites, not acquired contexts or feature payloads.
type Sites = Arc<Mutex<Vec<LeafSite>>>;

/// Declares real optional edges for the three permitted finite depths.
/// This private recipe is not a general recursive feature abstraction.
fn declare_chain(
    state: &Node,
    children: &mut MountChildren<'_, State, RootAction, Node, ()>,
    selected: bool,
    sites: &Sites,
) {
    if state.child.is_none() {
        if selected {
            sites.lock().unwrap().push(children.site().clone());
        }
        return;
    }
    let probes = Arc::clone(&state.guard.probes);
    let sites = Arc::clone(sites);
    children.optional_with_children(
        OptionalMount::new(
            |state: &Node| state.child.as_deref(),
            |state: &mut Node| state.child.as_deref_mut(),
            |state: &Node| {
                state.child.as_ref().map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            move |_| NodeFeature::new(&probes),
        ),
        move |state, children| declare_chain(state, children, selected, &sites),
    );
}

/// Scalar endpoint facts that do not retain the projected state or cache.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Projection {
    value: u64,
    checksum: u64,
    cache: u64,
}

/// Reads values without claiming that a typed child route was traversed.
fn project(state: &Node) -> Projection {
    Projection {
        value: state.value.get(),
        checksum: state.checksum,
        cache: state.cache_seen,
    }
}

/// Counts successful typed child reads; rejected stale access must not call it.
fn typed_project(state: &Node) -> Projection {
    state
        .guard
        .probes
        .typed_reads
        .fetch_add(1, Ordering::Relaxed);
    project(state)
}

/// Copies the three lifetime counters after the relevant completion boundary.
fn owners(probes: &[AtomicU64; 3]) -> [u64; 3] {
    std::array::from_fn(|i| probes[i].load(Ordering::Relaxed))
}

/// Holds the installed topology and independent probes through final state extraction.
struct Fixture {
    host: Host,
    host_owner: Weak<()>,
    store: Store<State, RootAction>,
    sites: Sites,
    leaf: Option<TaskContext<Node, ()>>,
    /// Retained keyed-entry route used only by retirement-only validation.
    parent: Option<TaskContext<Node, ()>>,
    probes: Arc<Probes>,
    /// Identity token for comparison only; never converted back or dereferenced.
    payload_address: usize,
}

impl Fixture {
    /// Constructs the host and full owned topology inside the construction phase.
    fn new(config: Config) -> Result<Self, &'static str> {
        let (host, runtime, host_owner) = Host::new(config.native, DRIVE_BUDGET)?;
        let probes = Arc::new(Probes::default());
        let sites = Sites::default();
        let selected = config.width / 2;
        let state = State {
            rows: (0..config.width)
                .map(|id| (id, node(config.depth, &probes)))
                .collect(),
            payload: vec![PAYLOAD_BYTE; PAYLOAD_BYTES].into_boxed_slice(),
            depth: config.depth,
            selected,
            guard: Guard::new(&probes, 0),
        };
        let payload_address = state.payload.as_ptr() as usize;
        let root = RootFeature {
            guard: Guard::new(&probes, 1),
            _cache: Cache::new(&probes),
        };
        let make_probes = Arc::clone(&probes);
        let make_sites = Arc::clone(&sites);
        let store = RootMount::new(root)
            .builder(state, runtime)
            .keyed_with_children(
                |state| &state.rows,
                |state| &mut state.rows,
                move |id, _| {
                    let selected = *id == selected;
                    let sites = Arc::clone(&make_sites);
                    (
                        EntryMount::new(NodeFeature::new(&make_probes)),
                        move |state: &Node,
                              children: &mut MountChildren<'_, State, RootAction, Node, ()>| {
                            if config.retirement && selected {
                                sites.lock().unwrap().push(children.site().clone());
                            }
                            declare_chain(state, children, selected, &sites);
                        },
                    )
                },
            )
            .build()
            .map_err(|_| INVALID)?;
        Ok(Self {
            host,
            host_owner,
            store,
            sites,
            leaf: None,
            parent: None,
            probes,
            payload_address,
        })
    }

    /// Completes installation and acquires routes before useful-work measurement starts.
    fn install(&mut self, config: Config) -> Result<(), &'static str> {
        self.host
            .run_ready_bounded(self.store.ready(), DEADLINE, DRIVE_BUDGET)?
            .map_err(|_| INVALID)?;
        if config.width != 0 {
            let site = self.sites.lock().unwrap().last().cloned().ok_or(INVALID)?;
            self.leaf = Some(
                self.host
                    .run_ready_bounded(self.store.acquire(&site), DEADLINE, DRIVE_BUDGET)?
                    .map_err(|_| INVALID)?
                    .ok_or(INVALID)?,
            );
            if config.retirement {
                let site = self.sites.lock().unwrap().first().cloned().ok_or(INVALID)?;
                self.parent = Some(
                    self.host
                        .run_ready_bounded(self.store.acquire(&site), DEADLINE, DRIVE_BUDGET)?
                        .map_err(|_| INVALID)?
                        .ok_or(INVALID)?,
                );
            }
        }
        Ok(())
    }

    /// Checks the unrelated payload through a root read outside the work symbol.
    fn payload_unchanged(&self) -> Result<bool, &'static str> {
        let address = self.payload_address;
        self.host
            .run_ready_bounded(
                self.store
                    .read(move |state| payload_matches(state, address)),
                DEADLINE,
                DRIVE_BUDGET,
            )?
            .map_err(|_| INVALID)
    }
}

/// Requires both original allocation identity and the complete expected byte pattern.
fn payload_matches(state: &State, address: usize) -> bool {
    state.payload.as_ptr() as usize == address
        && state.payload.len() == PAYLOAD_BYTES
        && state.payload.iter().all(|byte| *byte == PAYLOAD_BYTE)
}

/// Captures actual schedule progress without deriving success from final state alone.
#[derive(Default)]
struct Work {
    steady: Projection,
    fresh: Projection,
    leaf_sends: u64,
    causal_completed: u64,
    typed_updates: u64,
    typed_update_causal_completed: u64,
    structural_commits: u64,
    structural_causal_completed: u64,
    empty_root_commits: u64,
    empty_root_causal_completed: u64,
    stale_rejections: u64,
    steady_created: [u64; 3],
    reverse_created: [u64; 3],
    removed_dropped: [u64; 3],
    order_checks: u64,
    preserved_after_reverse: bool,
    fresh_started_zero: bool,
    retained_entry_unchanged: bool,
}

/// Requires causal completion and rejects cancellation after every successful commit.
async fn finished(task: &rca_core::ActionTask) -> Result<(), &'static str> {
    task.finish().await;
    if task.is_finished() && !task.is_cancelled() {
        Ok(())
    } else {
        Err(INVALID)
    }
}

/// Measures one real removal and its causal completion, excluding stale-route validation.
#[inline(never)]
fn profile_retirement(fixture: &Fixture, skip_removal: bool) -> Result<Work, &'static str> {
    fixture.host.run_ready_bounded(
        async {
            let mut work = Work::default();
            if !skip_removal {
                let task = fixture
                    .store
                    .send(RootAction::RetireChildren)
                    .await
                    .map_err(|_| INVALID)?;
                work.structural_commits = 1;
                finished(&task).await?;
                work.structural_causal_completed = 1;
            }
            work.removed_dropped = owners(&fixture.probes.dropped);
            work.steady_created = owners(&fixture.probes.created);
            Ok(work)
        },
        DEADLINE,
        DRIVE_BUDGET,
    )?
}

/// Requires stale descendant rejection and an unchanged, still-accessible keyed entry.
fn validate_retirement(fixture: &Fixture, work: &mut Work) -> Result<(), &'static str> {
    fixture.host.run_ready_bounded(
        async {
            let leaf = fixture.leaf.as_ref().ok_or(INVALID)?;
            if let Err(error) = leaf.read(typed_project).await
                && error.reason() == TaskAccessReason::ScopeEnded
                && error.progress() == TaskProgress::NotStarted
            {
                work.stale_rejections = 1;
            }
            let parent = fixture.parent.as_ref().ok_or(INVALID)?;
            let parent_unchanged = parent
                .read(|state| state.child.is_none() && project(state) == Projection::default())
                .await
                .map_err(|_| INVALID)?;
            work.retained_entry_unchanged = parent_unchanged
                && fixture
                    .store
                    .read(|state| state.rows.len() == 1 && state.rows.contains_id(&state.selected))
                    .await
                    .map_err(|_| INVALID)?;
            Ok(())
        },
        DEADLINE,
        DRIVE_BUDGET,
    )?
}

/// Measures the complete steady, reverse, remove, reinsert and fresh-send schedule.
#[inline(never)]
#[expect(
    clippy::too_many_lines,
    reason = "Keep the existing measured steady, reverse, removal and reinsertion schedule in one profiling symbol."
)]
fn profile_topology(
    fixture: &Fixture,
    config: Config,
    skip_send: bool,
) -> Result<Work, &'static str> {
    fixture.host.run_ready_bounded(
        async {
            let mut work = Work::default();
            let Some(leaf) = &fixture.leaf else {
                for _ in 0..config.operations {
                    let task = fixture
                        .store
                        .send(RootAction::Unchanged)
                        .await
                        .map_err(|_| INVALID)?;
                    work.empty_root_commits += 1;
                    finished(&task).await?;
                    work.empty_root_causal_completed += 1;
                }
                work.steady_created = owners(&fixture.probes.created);
                return Ok(work);
            };
            for operation in 0..config.operations {
                let before = leaf.read(typed_project).await.map_err(|_| INVALID)?;
                if before.value != 2 * operation || before.cache != operation {
                    return Err(INVALID);
                }
                let update = leaf
                    .update(|state| {
                        state
                            .guard
                            .probes
                            .typed_updates
                            .fetch_add(1, Ordering::Relaxed);
                        state.value.set(state.value.get() + 1);
                    })
                    .await
                    .map_err(|_| INVALID)?;
                work.typed_updates += 1;
                finished(update.task()).await?;
                work.typed_update_causal_completed += 1;
                if !(skip_send && operation + 1 == config.operations) {
                    let task = leaf.send(()).await.map_err(|_| INVALID)?;
                    work.leaf_sends += 1;
                    finished(&task).await?;
                    work.causal_completed += 1;
                }
            }
            // Root read records the steady endpoint without fabricating a leaf read.
            work.steady = fixture
                .store
                .read(|state| {
                    let mut node = state
                        .rows
                        .get(&state.selected)
                        .expect("selected chain exists");
                    while let Some(child) = &node.child {
                        node = child;
                    }
                    project(node)
                })
                .await
                .map_err(|_| INVALID)?;
            work.steady_created = owners(&fixture.probes.created);
            let task = fixture
                .store
                .send(RootAction::Reverse)
                .await
                .map_err(|_| INVALID)?;
            work.structural_commits += 1;
            finished(&task).await?;
            work.structural_causal_completed += 1;
            let width = config.width;
            let selected = width / 2;
            if !fixture
                .store
                .read(move |state| state.rows.iter().map(|(key, _)| *key).eq((0..width).rev()))
                .await
                .map_err(|_| INVALID)?
            {
                return Err(INVALID);
            }
            work.order_checks += 1;
            work.preserved_after_reverse =
                leaf.read(typed_project).await.map_err(|_| INVALID)? == work.steady;
            work.reverse_created = owners(&fixture.probes.created);
            let task = fixture
                .store
                .send(RootAction::Remove)
                .await
                .map_err(|_| INVALID)?;
            work.structural_commits += 1;
            finished(&task).await?;
            work.structural_causal_completed += 1;
            work.removed_dropped = owners(&fixture.probes.dropped);
            if !fixture
                .store
                .read(move |state| {
                    state
                        .rows
                        .iter()
                        .map(|(key, _)| *key)
                        .eq((0..width).rev().filter(|key| *key != selected))
                })
                .await
                .map_err(|_| INVALID)?
            {
                return Err(INVALID);
            }
            work.order_checks += 1;
            let task = fixture
                .store
                .send(RootAction::Reinsert)
                .await
                .map_err(|_| INVALID)?;
            work.structural_commits += 1;
            finished(&task).await?;
            work.structural_causal_completed += 1;
            if !fixture
                .store
                .read(move |state| {
                    state.rows.iter().map(|(key, _)| *key).eq((0..width)
                        .rev()
                        .filter(|key| *key != selected)
                        .chain(std::iter::once(selected)))
                })
                .await
                .map_err(|_| INVALID)?
            {
                return Err(INVALID);
            }
            work.order_checks += 1;
            // Equal keys must not revive the removed incarnation's authority.
            match leaf.read(typed_project).await {
                Err(error) if error.reason() == TaskAccessReason::ScopeEnded => {
                    work.stale_rejections += 1;
                }
                _ => return Err(INVALID),
            }
            let site = fixture
                .sites
                .lock()
                .unwrap()
                .last()
                .cloned()
                .ok_or(INVALID)?;
            let fresh = fixture
                .store
                .acquire(&site)
                .await
                .map_err(|_| INVALID)?
                .ok_or(INVALID)?;
            work.fresh_started_zero =
                fresh.read(typed_project).await.map_err(|_| INVALID)? == Projection::default();
            let task = fresh.send(()).await.map_err(|_| INVALID)?;
            work.leaf_sends += 1;
            finished(&task).await?;
            work.causal_completed += 1;
            work.fresh = fresh.read(typed_project).await.map_err(|_| INVALID)?;
            Ok(work)
        },
        DEADLINE,
        DRIVE_BUDGET,
    )?
}

/// Retains measured progress and teardown facts for independent literal validation.
struct Outcome {
    work: Work,
    cold_created: [u64; 3],
    created: [u64; 3],
    dropped: [u64; 3],
    leaf_reductions: u64,
    ancestor_reductions: u64,
    root_structural_reductions: u64,
    root_empty_reductions: u64,
    typed_reads: u64,
    typed_updates: u64,
    sites_created: usize,
    payload_unchanged: bool,
    owners_released: bool,
    host_stopped: bool,
}

impl Outcome {
    /// Checks exact owner totals, useful work, untouched payload and released host.
    fn validate(&self, config: Config) -> Result<(), &'static str> {
        let (cold, total) = match (config.retirement, config.depth, config.width) {
            (true, 5, 1) => (6, 6),
            (true, 20, 1) => (21, 21),
            (false, 1, 0) => (1, 1),
            (false, 1, 1) => (2, 3),
            (false, 5, 1) => (6, 11),
            (false, 20, 1) => (21, 41),
            (false, 1, 1000) => (1001, 1002),
            (false, 1, 10000) => (10001, 10002),
            _ => return Err(INVALID),
        };
        let work = &self.work;
        if self.cold_created != [cold; 3]
            || work.steady_created != [cold; 3]
            || self.created != [total; 3]
            || self.dropped != [total; 3]
            || self.ancestor_reductions != 0
            || !self.payload_unchanged
            || !self.owners_released
            || !self.host_stopped
        {
            return Err(INVALID);
        }
        if config.retirement {
            return self.validate_retirement(config);
        }
        if config.width == 0 {
            if work.empty_root_commits != config.operations
                || work.empty_root_causal_completed != config.operations
                || self.root_empty_reductions != config.operations
                || self.sites_created != 0
                || work.leaf_sends != 0
                || work.causal_completed != 0
                || self.leaf_reductions != 0
                || work.typed_updates != 0
                || self.typed_updates != 0
                || self.typed_reads != 0
                || work.structural_commits != 0
                || self.root_structural_reductions != 0
            {
                return Err(INVALID);
            }
        } else {
            let expected = match config.operations {
                1 => Projection {
                    value: 2,
                    checksum: 2,
                    cache: 1,
                },
                64 => Projection {
                    value: 128,
                    checksum: 4160,
                    cache: 64,
                },
                _ => return Err(INVALID),
            };
            if work.steady != expected
                || work.fresh
                    != (Projection {
                        value: 1,
                        checksum: 1,
                        cache: 1,
                    })
                || work.leaf_sends != config.operations + 1
                || self.leaf_reductions != config.operations + 1
                || work.causal_completed != config.operations + 1
                || work.typed_updates != config.operations
                || self.typed_updates != config.operations
                || work.typed_update_causal_completed != config.operations
                || self.typed_reads != config.operations + 3
                || work.structural_commits != 3
                || work.structural_causal_completed != 3
                || self.root_structural_reductions != 3
                || self.root_empty_reductions != 0
                || work.empty_root_commits != 0
                || work.stale_rejections != 1
                || work.order_checks != 3
                || self.sites_created != 2
                || work.reverse_created != [cold; 3]
                || work.removed_dropped != [config.depth as u64; 3]
                || !work.preserved_after_reverse
                || !work.fresh_started_zero
            {
                return Err(INVALID);
            }
        }
        Ok(())
    }

    /// Rejects skipped retirement, unrelated reductions or surviving descendant owners.
    fn validate_retirement(&self, config: Config) -> Result<(), &'static str> {
        let work = &self.work;
        if config.operations != 1
            || work.structural_commits != 1
            || work.structural_causal_completed != 1
            || self.root_structural_reductions != 1
            || work.removed_dropped != [config.depth as u64 - 1; 3]
            || work.stale_rejections != 1
            || !work.retained_entry_unchanged
            || self.sites_created != 2
            || work.leaf_sends != 0
            || self.leaf_reductions != 0
            || self.typed_reads != 0
            || self.typed_updates != 0
            || self.root_empty_reductions != 0
        {
            return Err(INVALID);
        }
        Ok(())
    }

    /// Formats validated facts after measurement, keeping authored counts explicitly labelled.
    fn json(&self, config: Config, allocations: &str) -> String {
        let work = &self.work;
        let backend = if config.native {
            "native-tokio"
        } else {
            "deterministic"
        };
        if config.retirement {
            return format!(
                concat!(
                    "{{\"schema\":\"rca-feature-retirement-profile-v1\",\"workload\":\"topology-retirement-{}\",\"domain\":\"Shared\",\"backend\":\"{}\",",
                    "\"depth\":{},\"width\":1,\"iterations\":1,\"payload_bytes\":1048576,\"measured_work\":\"one-optional-child-removal-and-causal-completion\",",
                    "\"retirement_commits\":{},\"retirement_causal_completed\":{},\"retired_features\":{},\"authored_retired_descendant_declarations\":{},\"useful_owner_drops\":{:?},",
                    "\"root_structural_reductions\":{},\"leaf_sends\":{},\"leaf_reductions\":{},\"ancestor_reductions\":{},\"typed_reads\":{},\"typed_updates\":{},",
                    "\"stale_rejections\":{},\"retained_entry_unchanged\":{},\"cold_owners_created\":{:?},\"owners_created\":{:?},\"owners_dropped\":{:?},",
                    "\"owners_released\":{},\"host_stopped\":{},\"payload_unchanged\":{},\"allocations\":{}}}"
                ),
                if config.native {
                    "native"
                } else {
                    "deterministic"
                },
                backend,
                config.depth,
                work.structural_commits,
                work.structural_causal_completed,
                work.removed_dropped[1],
                // Authored topology, not a new runtime instrumentation counter.
                config.depth - 2,
                work.removed_dropped,
                self.root_structural_reductions,
                work.leaf_sends,
                self.leaf_reductions,
                self.ancestor_reductions,
                self.typed_reads,
                self.typed_updates,
                work.stale_rejections,
                work.retained_entry_unchanged,
                self.cold_created,
                self.created,
                self.dropped,
                self.owners_released,
                self.host_stopped,
                self.payload_unchanged,
                allocations,
            );
        }
        let denominator = if config.width == 0 {
            "null".to_owned()
        } else {
            work.leaf_sends.to_string()
        };
        format!(
            concat!(
                "{{\"schema\":\"rca-feature-topology-profile-v1\",\"workload\":\"topology-{}\",\"domain\":\"Shared\",\"backend\":\"{}\",",
                "\"depth\":{},\"width\":{},\"iterations\":{},\"payload_bytes\":1048576,\"measured_work\":\"steady-cycles-then-reverse-ordered-remove-reinsert-fresh-send\",",
                "\"steady_value\":{},\"steady_checksum\":{},\"steady_cache\":{},\"fresh_value\":{},\"fresh_cache\":{},",
                "\"leaf_sends\":{},\"leaf_send_denominator\":{},\"causal_completed\":{},\"typed_reads\":{},\"typed_updates\":{},\"typed_update_causal_completed\":{},",
                "\"structural_commits\":{},\"structural_causal_completed\":{},\"root_structural_reductions\":{},\"ancestor_reductions\":{},\"stale_rejections\":{},",
                "\"empty_root_commits\":{},\"empty_root_causal_completed\":{},\"cold_owners_created\":{:?},\"owners_created\":{:?},\"owners_dropped\":{:?},",
                "\"owners_released\":{},\"host_stopped\":{},\"payload_unchanged\":{},\"allocations\":{}}}"
            ),
            if config.native {
                "native"
            } else {
                "deterministic"
            },
            backend,
            config.depth,
            config.width,
            config.operations,
            work.steady.value,
            work.steady.checksum,
            work.steady.cache,
            work.fresh.value,
            work.fresh.cache,
            work.leaf_sends,
            denominator,
            work.causal_completed,
            self.typed_reads,
            self.typed_updates,
            work.typed_update_causal_completed,
            work.structural_commits,
            work.structural_causal_completed,
            self.root_structural_reductions,
            self.ancestor_reductions,
            work.stale_rejections,
            work.empty_root_commits,
            work.empty_root_causal_completed,
            self.cold_created,
            self.created,
            self.dropped,
            self.owners_released,
            self.host_stopped,
            self.payload_unchanged,
            allocations
        )
    }
}

/// Parses one finite workload and emits a record only after all ownership checks pass.
pub(crate) fn run(args: impl Iterator<Item = String>) -> Result<String, &'static str> {
    run_inner(parse(args)?, false, None)
}

/// Preserves the allocation phases and releases deliberate retained owners after recording failure.
fn run_inner(
    config: Config,
    skip_send: bool,
    retain_kind: Option<usize>,
) -> Result<String, &'static str> {
    let cache = if config.native {
        ExecutorCache::Native
    } else {
        ExecutorCache::Deterministic
    };
    let mut measurement = Measurement::new(Some(cache));
    let mut fixture = Fixture::new(config)?;
    measurement.construction_finished();
    fixture.install(config)?;
    let cold_created = owners(&fixture.probes.created);
    let payload_before = fixture.payload_unchanged()?;
    measurement.cold_finished();
    #[cfg(test)]
    let retention_cleanup = RetentionCleanup(Arc::clone(&fixture.probes));
    #[cfg(test)]
    fixture
        .probes
        .retain_kind
        .store(retain_kind.unwrap_or(0) as u64, Ordering::Relaxed);
    let mut work = if config.retirement {
        profile_retirement(&fixture, skip_send)?
    } else {
        profile_topology(&fixture, config, skip_send)?
    };
    measurement.work_finished();
    if config.retirement {
        validate_retirement(&fixture, &mut work)?;
    }
    let payload_unchanged = payload_before && fixture.payload_unchanged()?;
    measurement.validation_finished();
    let Fixture {
        host,
        host_owner,
        store,
        sites,
        leaf,
        parent,
        probes,
        payload_address,
    } = fixture;
    let sites_created = sites.lock().unwrap().len();
    drop(leaf);
    drop(parent);
    drop(sites);
    let state = host
        .run_ready_bounded(store.shutdown_into_state(), DEADLINE, DRIVE_BUDGET)?
        .map_err(|_| INVALID)?;
    let payload_unchanged = payload_unchanged && payload_matches(&state, payload_address);
    let retained_state = if retain_kind == Some(0) {
        Some(state)
    } else {
        drop(state);
        None
    };
    drop(host);
    let outcome = Outcome {
        work,
        cold_created,
        created: owners(&probes.created),
        dropped: owners(&probes.dropped),
        leaf_reductions: probes.leaf_reductions.load(Ordering::Relaxed),
        ancestor_reductions: probes.ancestor_reductions.load(Ordering::Relaxed),
        root_structural_reductions: probes.root_structural_reductions.load(Ordering::Relaxed),
        root_empty_reductions: probes.root_empty_reductions.load(Ordering::Relaxed),
        typed_reads: probes.typed_reads.load(Ordering::Relaxed),
        typed_updates: probes.typed_updates.load(Ordering::Relaxed),
        sites_created,
        payload_unchanged,
        owners_released: probes
            .owners
            .lock()
            .unwrap()
            .iter()
            .all(|owner| owner.upgrade().is_none()),
        host_stopped: host_owner.upgrade().is_none(),
    };
    // Preserve rejection evidence before releasing the deliberate test owner.
    let validation = outcome.validate(config);
    drop(retained_state);
    #[cfg(test)]
    drop(retention_cleanup);
    drop(probes);
    drop(host_owner);
    let allocations = measurement
        .finish()?
        .map_or_else(|| "null".to_owned(), |record| record.json());
    validation?;
    Ok(outcome.json(config, &allocations))
}

#[cfg(test)]
/// Runs a real depth-five schedule with one omitted send or retained owner.
fn run_control(fault: &str) -> Result<String, &'static str> {
    let retain_kind = match fault {
        "skip-send" => None,
        "retain-state" => Some(0),
        "retain-feature" => Some(1),
        "retain-cache" => Some(2),
        _ => return Err("unknown topology control"),
    };
    run_inner(
        Config {
            native: true,
            retirement: false,
            depth: 5,
            width: 1,
            operations: 1,
        },
        fault == "skip-send",
        retain_kind,
    )
}

#[cfg(test)]
pub(crate) mod tests {
    // Skipped optional-child removal, incidental leaf work or early teardown
    // cannot satisfy these independently counted retirement-only outcomes.
    #[cfg_attr(not(feature = "allocations"), test)]
    pub(crate) fn retirement_only_removes_descendants_and_preserves_the_keyed_entry() {
        #[cfg(feature = "allocations")]
        if !crate::tests::in_child(
            "topology::tests::retirement_only_removes_descendants_and_preserves_the_keyed_entry",
        ) {
            return;
        }
        for backend in ["native", "deterministic"] {
            if backend == "deterministic" && !cfg!(feature = "deterministic") {
                continue;
            }
            for (depth, removed, descendants, total) in [(5, 4, 3, 6), (20, 19, 18, 21)] {
                let record = crate::dispatch(
                    [
                        format!("topology-retirement-{backend}"),
                        depth.to_string(),
                        "1".to_owned(),
                        "1".to_owned(),
                    ]
                    .into_iter(),
                )
                .expect("retirement dispatch must execute a real optional-child removal");
                let value: serde_json::Value = serde_json::from_str(&record).unwrap();
                assert_eq!(value["workload"], format!("topology-retirement-{backend}"));
                assert_eq!(value["retirement_commits"], 1);
                assert_eq!(value["retirement_causal_completed"], 1);
                assert_eq!(value["retired_features"], removed);
                assert_eq!(
                    value["authored_retired_descendant_declarations"],
                    descendants
                );
                assert_eq!(
                    value["useful_owner_drops"],
                    serde_json::json!([removed, removed, removed])
                );
                assert_eq!(
                    value["cold_owners_created"],
                    serde_json::json!([total, total, total])
                );
                assert_eq!(
                    value["owners_created"],
                    serde_json::json!([total, total, total])
                );
                assert_eq!(
                    value["owners_dropped"],
                    serde_json::json!([total, total, total])
                );
                assert_eq!(value["root_structural_reductions"], 1);
                assert_eq!(value["leaf_sends"], 0);
                assert_eq!(value["leaf_reductions"], 0);
                assert_eq!(value["ancestor_reductions"], 0);
                assert_eq!(value["typed_reads"], 0);
                assert_eq!(value["typed_updates"], 0);
                assert_eq!(value["stale_rejections"], 1);
                assert_eq!(value["retained_entry_unchanged"], true);
                assert_eq!(value["payload_unchanged"], true);
                assert_eq!(value["owners_released"], true);
                assert_eq!(value["host_stopped"], true);
            }
        }
    }

    #[cfg_attr(not(feature = "allocations"), test)]
    pub(crate) fn retirement_only_rejects_skipped_removal_and_each_retained_owner() {
        #[cfg(feature = "allocations")]
        if !crate::tests::in_child(
            "topology::tests::retirement_only_rejects_skipped_removal_and_each_retained_owner",
        ) {
            return;
        }
        let config = super::parse(
            ["topology-retirement-native", "5", "1", "1"]
                .into_iter()
                .map(str::to_owned),
        )
        .expect("retirement controls must reach real execution");
        for (skip_removal, retained) in [
            (true, None),
            (false, Some(0)),
            (false, Some(1)),
            (false, Some(2)),
        ] {
            assert_eq!(
                super::run_inner(config, skip_removal, retained).err(),
                Some(super::INVALID)
            );
        }
    }

    #[test]
    fn retirement_only_rejects_empty_shallow_wide_or_repeated_work() {
        for arguments in [
            vec!["0", "1", "1"],
            vec!["1", "1", "1"],
            vec!["5", "0", "1"],
            vec!["5", "2", "1"],
            vec!["5", "1", "0"],
            vec!["20", "1", "64"],
            vec!["5", "1"],
            vec!["5", "1", "1", "extra"],
            vec!["invalid", "1", "1"],
        ] {
            assert!(
                super::parse(
                    std::iter::once("topology-retirement-native")
                        .chain(arguments)
                        .map(str::to_owned)
                )
                .is_err()
            );
        }
    }

    #[cfg_attr(not(feature = "allocations"), test)]
    pub(crate) fn topology_controls_reject_skipped_work_and_each_retained_owner() {
        #[cfg(feature = "allocations")]
        if !crate::tests::in_child(
            "topology::tests::topology_controls_reject_skipped_work_and_each_retained_owner",
        ) {
            return;
        }
        for fault in [
            "skip-send",
            "retain-state",
            "retain-feature",
            "retain-cache",
        ] {
            assert_eq!(
                super::run_control(fault).err(),
                Some("topology work or ownership invalid")
            );
        }
    }

    #[cfg(feature = "deterministic")]
    #[cfg_attr(not(feature = "allocations"), test)]
    pub(crate) fn deterministic_topology_uses_the_same_exact_work_contract() {
        #[cfg(feature = "allocations")]
        if !crate::tests::in_child(
            "topology::tests::deterministic_topology_uses_the_same_exact_work_contract",
        ) {
            return;
        }
        let record = crate::dispatch(
            ["topology-deterministic", "5", "1", "64"]
                .into_iter()
                .map(str::to_owned),
        )
        .expect("deterministic topology must execute real Feature work");
        let value: serde_json::Value = serde_json::from_str(&record).unwrap();
        assert_eq!(value["backend"], "deterministic");
        assert_eq!(value["domain"], "Shared");
        assert_eq!(value["leaf_sends"], 65);
        assert_eq!(value["typed_updates"], 64);
        assert_eq!(value["steady_checksum"], 4160);
        assert_eq!(value["owners_dropped"], serde_json::json!([11, 11, 11]));
    }

    // These literals reject missing dispatch, skipped leaf transactions, fabricated
    // empty work, reconstructed caches, ancestor reduction, and leaked ownership.
    #[cfg_attr(not(feature = "allocations"), test)]
    pub(crate) fn six_topologies_complete_exact_leaf_work_and_release_owners() {
        #[cfg(feature = "allocations")]
        if !crate::tests::in_child(
            "topology::tests::six_topologies_complete_exact_leaf_work_and_release_owners",
        ) {
            return;
        }
        for (depth, width, owners) in [
            (1, 0, 1),
            (1, 1, 3),
            (5, 1, 11),
            (20, 1, 41),
            (1, 1000, 1002),
            (1, 10000, 10002),
        ] {
            for (operations, old_value, old_checksum) in [(1, 2, 2), (64, 128, 4160)] {
                let record = crate::dispatch(
                    [
                        "topology-native".to_owned(),
                        depth.to_string(),
                        width.to_string(),
                        operations.to_string(),
                    ]
                    .into_iter(),
                )
                .expect("topology dispatch must execute and validate real Feature work");
                let value: serde_json::Value = serde_json::from_str(&record).unwrap();
                assert_eq!(value["depth"], depth);
                assert_eq!(value["width"], width);
                assert_eq!(value["ancestor_reductions"], 0);
                assert_eq!(
                    value["owners_created"],
                    serde_json::json!([owners, owners, owners])
                );
                assert_eq!(
                    value["owners_dropped"],
                    serde_json::json!([owners, owners, owners])
                );
                assert_eq!(value["owners_released"], true);
                assert_eq!(value["host_stopped"], true);
                assert_eq!(value["payload_unchanged"], true);
                if width == 0 {
                    assert_eq!(value["empty_root_commits"], operations);
                    assert_eq!(value["empty_root_causal_completed"], operations);
                    assert_eq!(value["leaf_sends"], 0);
                    assert_eq!(value["typed_updates"], 0);
                    assert_eq!(value["typed_reads"], 0);
                    assert_eq!(value["causal_completed"], 0);
                    assert_eq!(value["leaf_send_denominator"], serde_json::Value::Null);
                } else {
                    assert_eq!(value["steady_value"], old_value);
                    assert_eq!(value["steady_checksum"], old_checksum);
                    assert_eq!(value["steady_cache"], operations);
                    assert_eq!(value["leaf_sends"], operations + 1);
                    assert_eq!(value["typed_updates"], operations);
                    assert_eq!(value["typed_reads"], operations + 3);
                    assert_eq!(value["causal_completed"], operations + 1);
                    assert_eq!(value["typed_update_causal_completed"], operations);
                    assert_eq!(value["structural_commits"], 3);
                    assert_eq!(value["structural_causal_completed"], 3);
                    assert_eq!(value["root_structural_reductions"], 3);
                    assert_eq!(value["stale_rejections"], 1);
                    assert_eq!(value["fresh_value"], 1);
                    assert_eq!(value["fresh_cache"], 1);
                }
            }
        }
    }
}
