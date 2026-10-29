//! Profiles resident keyed effects across steady work and child removal.
//!
//! Construction and initial effect admission precede useful work. The work phase
//! includes routing, retirement waits and fixture counters; it is not reducer-only
//! cost. Final projection, ownership checks and JSON formatting have separate phases.

use std::collections::VecDeque;
use std::fmt::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use async_channel::{Receiver, Sender};
use rca_core::{
    ActionTask, Effect, KeyedAction, KeyedActionPath, KeyedState, KeyedStatePath, ReduceContext,
    Reducer, Store, for_each,
};

use crate::host::Host;
use crate::measurement::{ExecutorCache, Measurement};

/// Rejects a record whose trace or final ownership checks did not pass.
const INVALID: &str = "keyed scaling trace or ownership invalid";
/// A deadlock watchdog, not a performance target.
const DEADLINE: Duration = Duration::from_secs(10);
/// Indices shared by the creation and destruction counters.
const STATE: usize = 0;
/// Counts the parent and child reducer instances separately from their state.
const REDUCER: usize = 1;
/// Counts each move-only effect capture, including cancelled operations.
const CAPTURE: usize = 2;
/// Largest qualified width; fixes projection storage without retaining row owners.
const MAX_SIZE: usize = 1024;

/// Chooses whether each cycle preserves, replaces one or replaces every child.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Steady,
    RemoveOne,
    RemoveAll,
}

/// Validated workload dimensions and executor choice.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Config {
    name: &'static str,
    size: u64,
    iterations: u64,
    mode: Mode,
    native: bool,
    /// Bounds both scheduler work and outer polling, independently of wall time.
    budget: usize,
}

impl Config {
    /// Rejects unsupported widths, zero work and overflowing count or poll budgets.
    pub(crate) fn parse(mut args: impl Iterator<Item = String>) -> Result<Self, &'static str> {
        let name = args.next().ok_or("expected WORKLOAD SIZE ITERATIONS")?;
        let (name, mode, native) = match name.as_str() {
            "keyed-scale-steady-native" => ("keyed-scale-steady-native", Mode::Steady, true),
            "keyed-scale-steady-deterministic" => {
                ("keyed-scale-steady-deterministic", Mode::Steady, false)
            }
            "keyed-scale-remove-one-native" => {
                ("keyed-scale-remove-one-native", Mode::RemoveOne, true)
            }
            "keyed-scale-remove-one-deterministic" => (
                "keyed-scale-remove-one-deterministic",
                Mode::RemoveOne,
                false,
            ),
            "keyed-scale-remove-all-native" => {
                ("keyed-scale-remove-all-native", Mode::RemoveAll, true)
            }
            "keyed-scale-remove-all-deterministic" => (
                "keyed-scale-remove-all-deterministic",
                Mode::RemoveAll,
                false,
            ),
            _ => return Err("unknown workload"),
        };
        if !native && !cfg!(feature = "deterministic") {
            return Err("deterministic workloads require the deterministic feature");
        }
        let size = crate::workloads::integer(args.next())?;
        let iterations = crate::workloads::integer(args.next())?;
        if !matches!(size, 0 | 1 | 16 | 256 | 1024) || iterations == 0 || args.next().is_some() {
            return Err("keyed scaling requires SIZE=0/1/16/256/1024 and positive ITERATIONS");
        }
        // Covers every root reduction and useful scheduler step, including remove-all.
        let work = iterations
            .checked_mul(
                size.checked_mul(2)
                    .and_then(|n| n.checked_add(4))
                    .ok_or(INVALID)?,
            )
            .and_then(|n| n.checked_add(size))
            .ok_or("keyed scaling work overflow")?;
        let budget = usize::try_from(work)
            .ok()
            .and_then(|n| n.checked_mul(128))
            .and_then(|n| n.checked_add(2048))
            .ok_or("keyed scaling poll-budget overflow")?;
        Ok(Self {
            name,
            size,
            iterations,
            mode,
            native,
            budget,
        })
    }
}

/// Records actual lifetimes independently of the reported workload totals.
#[derive(Default)]
struct Probes {
    created: [AtomicU64; 3],
    dropped: [AtomicU64; 3],
    /// Weak identities detect retention even when destruction counters balance.
    owners: Mutex<Vec<(usize, Weak<()>)>>,
    child_calls: AtomicU64,
    effects_started: AtomicU64,
}

impl Probes {
    /// Returns creation and destruction totals for one ownership category.
    fn count(&self, kind: usize) -> (u64, u64) {
        (
            self.created[kind].load(Ordering::Relaxed),
            self.dropped[kind].load(Ordering::Relaxed),
        )
    }

    /// Requires every identity to expire without retaining any strong witness.
    fn released(&self) -> bool {
        self.owners
            .lock()
            .unwrap()
            .iter()
            .all(|(_, owner)| owner.upgrade().is_none())
    }
}

/// Witnesses one state, reducer or effect lifetime without making it cloneable.
struct Guard {
    probes: Arc<Probes>,
    _owner: Arc<()>,
    kind: usize,
}

impl Guard {
    /// Registers a distinct weak identity and increments its creation count.
    fn new(probes: &Arc<Probes>, kind: usize) -> Self {
        let owner = Arc::new(());
        probes
            .owners
            .lock()
            .unwrap()
            .push((kind, Arc::downgrade(&owner)));
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

/// Move-only state prevents a projection from accidentally retaining a row.
struct LeafState {
    incarnation: u64,
    value: u64,
    _guard: Guard,
}

impl LeafState {
    /// Starts one row generation with no routed updates.
    fn new(probes: &Arc<Probes>, incarnation: u64) -> Self {
        Self {
            incarnation,
            value: 0,
            _guard: Guard::new(probes, STATE),
        }
    }
}

/// Updates a row or starts an effect that remains suspended until retirement.
enum LeafAction {
    Update,
    Start {
        started: Sender<()>,
        gate: Receiver<()>,
    },
}

/// Edits ordered membership or routes one action to a resident child.
enum Action {
    Touch,
    RemoveOne,
    RemoveAll,
    ReinsertOne { id: Option<u64>, incarnation: u64 },
    ReinsertAll { size: u64, incarnation: u64 },
    Child(KeyedAction<u64, LeafAction>),
}

/// Holds ordered child state and the independent parent reduction count.
struct State {
    rows: KeyedState<u64, LeafState>,
    parent_calls: u64,
}

/// Applies membership edits; empty clears deliberately exercise the empty path.
struct Parent {
    guard: Guard,
    #[cfg(test)]
    fault: Option<Fault>,
}

/// Counts child reductions and owns each suspended effect's lifetime witness.
struct Leaf {
    guard: Guard,
}

impl Reducer for Parent {
    type State = State;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut State,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        state.parent_calls += 1;
        match action {
            Action::Touch => {
                if state.rows.is_empty() {
                    state.rows.clear();
                }
            }
            Action::Child(_) => {}
            Action::RemoveOne => {
                #[cfg(test)]
                if self.fault == Some(Fault::UncancelledRetired) {
                    return Effect::none();
                }
                let first = state.rows.ids().next().copied();
                if let Some(id) = first {
                    #[cfg(test)]
                    if self.fault == Some(Fault::WrongOrder) {
                        drop(state.rows.swap_remove(&id).expect("first child exists"));
                        return Effect::none();
                    }
                    drop(state.rows.remove(&id).expect("first child exists"));
                } else {
                    state.rows.clear();
                }
            }
            Action::RemoveAll => state.rows.clear(),
            Action::ReinsertOne { id, incarnation } => {
                if let Some(id) = id {
                    state
                        .rows
                        .insert(*id, LeafState::new(&self.guard.probes, *incarnation));
                } else {
                    state.rows.clear();
                }
            }
            Action::ReinsertAll { size, incarnation } => {
                if *size == 0 {
                    state.rows.clear();
                }
                for id in 0..*size {
                    state
                        .rows
                        .insert(id, LeafState::new(&self.guard.probes, *incarnation));
                }
            }
        }
        Effect::none()
    }
}

impl Reducer for Leaf {
    type State = LeafState;
    type Action = LeafAction;

    fn reduce(
        &mut self,
        state: &mut LeafState,
        action: &LeafAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<LeafAction> {
        self.guard
            .probes
            .child_calls
            .fetch_add(1, Ordering::Relaxed);
        match action {
            LeafAction::Update => {
                state.value += 1;
                Effect::none()
            }
            LeafAction::Start { started, gate } => {
                let started = started.clone();
                let gate = gate.clone();
                let capture = Guard::new(&self.guard.probes, CAPTURE);
                Effect::run(move |_sender| async move {
                    capture
                        .probes
                        .effects_started
                        .fetch_add(1, Ordering::Relaxed);
                    let _ = started.send(()).await;
                    let _ = gate.recv().await;
                    drop(capture);
                })
            }
        }
    }
}

/// Copies only the row identity, generation and committed update count.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Row {
    id: u64,
    incarnation: u64,
    value: u64,
}

/// Keeps validation data inline, without heap allocation or retained row owners.
#[derive(Debug, PartialEq, Eq)]
struct Projection {
    rows: [Row; MAX_SIZE],
    len: usize,
    parent_calls: u64,
}

/// Copies every ordered row after useful work, using the qualified width bound.
#[expect(
    clippy::large_stack_arrays,
    reason = "the bounded 24 KiB projection avoids heap allocations and cannot retain row owners"
)]
fn project(state: &State) -> Projection {
    let mut projection = Projection {
        rows: [Row::default(); MAX_SIZE],
        len: 0,
        parent_calls: state.parent_calls,
    };
    for (id, leaf) in state.rows.iter() {
        projection.rows[projection.len] = Row {
            id: *id,
            incarnation: leaf.incarnation,
            value: leaf.value,
        };
        projection.len += 1;
    }
    projection
}

/// Counts initial starts plus each cycle's membership edits, starts and updates.
fn expected_parent(config: Config) -> u64 {
    let per_cycle = match config.mode {
        Mode::Steady => 1 + config.size,
        Mode::RemoveOne => 2 + u64::from(config.size != 0) + config.size,
        Mode::RemoveAll => 2 + config.size * 2,
    };
    config.size + config.iterations * per_cycle
}

/// Counts initial residents and all replacement generations.
fn expected_created(config: Config) -> u64 {
    config.size
        + config.iterations
            * match config.mode {
                Mode::Steady => 0,
                Mode::RemoveOne => u64::from(config.size != 0),
                Mode::RemoveAll => config.size,
            }
}

/// Excludes the final resident cohort from replacement lifetime totals.
fn expected_removed(config: Config) -> u64 {
    expected_created(config) - config.size
}

/// Checks every row against the closed-form order, generation and update totals.
fn validate_projection(config: Config, projection: &Projection) -> Result<(), &'static str> {
    if projection.len as u64 != config.size || projection.parent_calls != expected_parent(config) {
        return Err(INVALID);
    }
    for (position, actual) in projection.rows[..projection.len].iter().enumerate() {
        let position = u64::try_from(position).map_err(|_| INVALID)?;
        let expected = match config.mode {
            Mode::Steady => Row {
                id: position,
                incarnation: 0,
                value: config.iterations,
            },
            Mode::RemoveAll => Row {
                id: position,
                incarnation: config.iterations,
                value: 1,
            },
            Mode::RemoveOne => {
                // Each cycle moves the first row to the back. Its most recent
                // replacement is the last matching ID in that round-robin trace.
                let id = (position + config.iterations % config.size) % config.size;
                let incarnation = if config.iterations > id {
                    config.iterations - (config.iterations - 1 - id) % config.size
                } else {
                    0
                };
                Row {
                    id,
                    incarnation,
                    value: if incarnation == 0 {
                        config.iterations
                    } else {
                        config.iterations - incarnation + 1
                    },
                }
            }
        };
        if *actual != expected {
            return Err(INVALID);
        }
    }
    Ok(())
}

/// Counts client operations independently of reducer-side probes.
#[derive(Default)]
struct Counts {
    root_sends: u64,
    immediate_finished: u64,
    routed_updates: u64,
    removed_joined: u64,
}

/// Keeps one resident task suspended until removal or store shutdown.
struct Started {
    id: u64,
    task: ActionTask,
    _gate: Sender<()>,
}

/// Owns the real store, its host and the ordered resident task cohort.
struct Fixture {
    /// Temporarily taken while the host drives a mutable borrow of this fixture.
    host: Option<Host>,
    store: Store<State, Action>,
    host_owner: Weak<()>,
    probes: Arc<Probes>,
    /// Mirrors resident order; front removal and back insertion avoid shifting handles.
    active: VecDeque<Started>,
    counts: Counts,
}

impl Fixture {
    /// Builds a one-slot store before any measured admission or useful work.
    fn new(config: Config, #[cfg(test)] fault: Option<Fault>) -> Result<Self, &'static str> {
        let probes = Arc::new(Probes::default());
        let rows = (0..config.size)
            .map(|id| (id, LeafState::new(&probes, 0)))
            .collect();
        let parent = Parent {
            guard: Guard::new(&probes, REDUCER),
            #[cfg(test)]
            fault,
        };
        let leaf = Leaf {
            guard: Guard::new(&probes, REDUCER),
        };
        let (host, runtime, host_owner) = Host::new(config.native, config.budget)?;
        let reducer = for_each(
            parent,
            KeyedStatePath::new(
                |state: &State| &state.rows,
                |state: &mut State| &mut state.rows,
            ),
            KeyedActionPath::new(
                |action: &Action| match action {
                    Action::Child(child) => Some(child),
                    _ => None,
                },
                Action::Child,
            ),
            leaf,
        );
        let store = Store::builder(
            State {
                rows,
                parent_calls: 0,
            },
            reducer,
            runtime,
        )
        .mailbox_capacity(1)
        .build()
        .map_err(|_| "keyed scaling Store construction failed")?;
        Ok(Self {
            host: Some(host),
            store,
            host_owner,
            probes,
            active: VecDeque::new(),
            counts: Counts::default(),
        })
    }

    /// Sends an action with finite causal work and waits for that work to finish.
    async fn immediate(&mut self, action: Action) -> Result<(), &'static str> {
        self.counts.root_sends += 1;
        let task = self.store.send(action).await.map_err(|_| INVALID)?;
        task.finish().await;
        if !task.is_finished() {
            return Err(INVALID);
        }
        self.counts.immediate_finished += 1;
        Ok(())
    }

    /// Observes an effect's actual start before retaining its task and closed gate.
    async fn start(&mut self, id: u64) -> Result<(), &'static str> {
        let (started, received) = async_channel::bounded(1);
        let (gate, wait) = async_channel::bounded(1);
        self.counts.root_sends += 1;
        let task = self
            .store
            .send(Action::Child(KeyedAction::new(
                id,
                LeafAction::Start {
                    started,
                    gate: wait,
                },
            )))
            .await
            .map_err(|_| INVALID)?;
        received.recv().await.map_err(|_| INVALID)?;
        if task.is_finished() || task.is_cancelled() {
            return Err(INVALID);
        }
        self.active.push_back(Started {
            id,
            task,
            _gate: gate,
        });
        Ok(())
    }

    /// Commits one routed update and counts it only after causal completion.
    async fn update(&mut self, id: u64) -> Result<(), &'static str> {
        self.immediate(Action::Child(KeyedAction::new(id, LeafAction::Update)))
            .await?;
        self.counts.routed_updates += 1;
        Ok(())
    }

    /// Starts every initial resident before the useful-work measurement boundary.
    fn warmup(&mut self, config: Config) -> Result<(), &'static str> {
        let host = self.host.take().expect("host held by fixture");
        let result = host.run_ready_bounded(
            async {
                for id in 0..config.size {
                    self.start(id).await?;
                }
                Ok::<_, &'static str>(())
            },
            DEADLINE,
            config.budget,
        );
        self.host = Some(host);
        result?
    }

    /// Performs one membership change, verifies retirement and updates all survivors.
    async fn cycle(
        &mut self,
        config: Config,
        iteration: u64,
        #[cfg(test)] fault: Option<Fault>,
    ) -> Result<(), &'static str> {
        match config.mode {
            Mode::Steady => self.immediate(Action::Touch).await?,
            Mode::RemoveOne => {
                self.immediate(Action::RemoveOne).await?;
                let id = if let Some(old) = self.active.front() {
                    old.task.finish().await;
                    if !old.task.is_finished() {
                        return Err(INVALID);
                    }
                    let old = self.active.pop_front().expect("front was present");
                    self.counts.removed_joined += 1;
                    let id = old.id;
                    drop(old);
                    Some(id)
                } else {
                    None
                };
                if self.probes.count(CAPTURE).1 != self.counts.removed_joined {
                    return Err(INVALID);
                }
                #[cfg(test)]
                if fault == Some(Fault::SiblingCancelled) {
                    self.active.front().ok_or(INVALID)?.task.cancel();
                }
                for sibling in &self.active {
                    if sibling.task.is_finished() || sibling.task.is_cancelled() {
                        return Err(INVALID);
                    }
                }
                let incarnation = {
                    #[cfg(test)]
                    if fault == Some(Fault::WrongIncarnation) {
                        iteration - 1
                    } else {
                        iteration
                    }
                    #[cfg(not(test))]
                    iteration
                };
                self.immediate(Action::ReinsertOne { id, incarnation })
                    .await?;
                if let Some(id) = id {
                    self.start(id).await?;
                }
            }
            Mode::RemoveAll => {
                self.immediate(Action::RemoveAll).await?;
                for old in &self.active {
                    old.task.finish().await;
                    if !old.task.is_finished() {
                        return Err(INVALID);
                    }
                    self.counts.removed_joined += 1;
                }
                self.active.clear();
                if self.probes.count(CAPTURE).1 != self.counts.removed_joined {
                    return Err(INVALID);
                }
                self.immediate(Action::ReinsertAll {
                    size: config.size,
                    incarnation: iteration,
                })
                .await?;
                for id in 0..config.size {
                    self.start(id).await?;
                }
            }
        }
        for index in 0..self.active.len() {
            #[cfg(test)]
            if fault == Some(Fault::MissingUpdate) && index + 1 == self.active.len() {
                continue;
            }
            self.update(self.active[index].id).await?;
        }
        for resident in &self.active {
            if resident.task.is_finished() || resident.task.is_cancelled() {
                return Err(INVALID);
            }
        }
        Ok(())
    }

    /// Checks the final ordered state, exact work totals and still-live residents.
    fn validate(&self, config: Config) -> Result<Projection, &'static str> {
        let projection = self
            .host
            .as_ref()
            .expect("host restored after work")
            .run_ready_bounded(self.store.read(project), DEADLINE, config.budget)?
            .map_err(|_| INVALID)?;
        validate_projection(config, &projection)?;
        let created = expected_created(config);
        let removed = expected_removed(config);
        let child_calls = created + config.iterations * config.size;
        if self.counts.root_sends != expected_parent(config)
            || self.counts.immediate_finished != expected_parent(config) - created
            || self.counts.routed_updates != config.iterations * config.size
            || self.counts.removed_joined != removed
            || self.probes.child_calls.load(Ordering::Relaxed) != child_calls
            || self.probes.effects_started.load(Ordering::Relaxed) != created
            || self.probes.count(STATE) != (created, removed)
            || self.probes.count(CAPTURE) != (created, removed)
            || self.probes.count(REDUCER) != (2, 0)
            || self.active.len() as u64 != config.size
        {
            return Err(INVALID);
        }
        Ok(projection)
    }

    /// Extracts unchanged state and joins residents before checking every owner expired.
    fn teardown(self, checked: &Projection, config: Config) -> Result<Outcome, &'static str> {
        let Self {
            host,
            store,
            host_owner,
            probes,
            active,
            counts,
        } = self;
        let host = host.expect("host restored after work");
        let state = host
            .run_ready_bounded(store.shutdown_into_state(), DEADLINE, config.budget)?
            .map_err(|_| INVALID)?;
        let projection = project(&state);
        if &projection != checked {
            return Err(INVALID);
        }
        let resident_joined = host.run_ready_bounded(
            async {
                for resident in active {
                    resident.task.finish().await;
                    if !resident.task.is_finished() {
                        return Err(INVALID);
                    }
                }
                Ok::<_, &'static str>(config.size)
            },
            DEADLINE,
            config.budget,
        )??;
        drop(state);
        drop(host);
        let created = expected_created(config);
        if !probes.released()
            || host_owner.upgrade().is_some()
            || probes.count(STATE) != (created, created)
            || probes.count(CAPTURE) != (created, created)
            || probes.count(REDUCER) != (2, 2)
            || counts.removed_joined + resident_joined != created
        {
            return Err(INVALID);
        }
        Ok(Outcome {
            projection,
            counts,
            child_calls: probes.child_calls.load(Ordering::Relaxed),
            created,
            resident_joined,
        })
    }

    #[cfg(all(test, feature = "deterministic"))]
    /// Shuts down a failed trace without assuming its expected workload counts.
    fn cleanup_failed_trace(self, config: Config) -> Result<(), &'static str> {
        let Self {
            host,
            store,
            host_owner,
            probes,
            active,
            ..
        } = self;
        let host = host.expect("host restored after work");
        let state = host
            .run_ready_bounded(store.shutdown_into_state(), DEADLINE, config.budget)?
            .map_err(|_| INVALID)?;
        host.run_ready_bounded(
            async {
                for resident in active {
                    resident.task.finish().await;
                    if !resident.task.is_finished() {
                        return Err(INVALID);
                    }
                }
                Ok::<_, &'static str>(())
            },
            DEADLINE,
            config.budget,
        )??;
        drop(state);
        drop(host);
        for kind in [STATE, REDUCER, CAPTURE] {
            let (created, dropped) = probes.count(kind);
            if created != dropped {
                return Err(INVALID);
            }
        }
        if !probes.released() || host_owner.upgrade().is_some() {
            return Err(INVALID);
        }
        Ok(())
    }
}

/// Drives the finite real Store trace after construction and start observation.
#[inline(never)]
fn profile_keyed_scaling_work(
    fixture: &mut Fixture,
    config: Config,
    #[cfg(test)] fault: Option<Fault>,
) -> Result<(), &'static str> {
    let host = fixture.host.take().expect("host held by fixture");
    let result = host.run_ready_bounded(
        async {
            for iteration in 1..=config.iterations {
                fixture
                    .cycle(
                        config,
                        iteration,
                        #[cfg(test)]
                        fault,
                    )
                    .await?;
            }
            Ok::<_, &'static str>(())
        },
        DEADLINE,
        config.budget,
    );
    fixture.host = Some(host);
    result?
}

/// Contains copied results only after trace and teardown validation succeed.
struct Outcome {
    projection: Projection,
    counts: Counts,
    child_calls: u64,
    created: u64,
    resident_joined: u64,
}

impl Outcome {
    /// Formats a validated record after all allocation snapshots have finished.
    fn json(&self, config: Config, allocations: &str) -> String {
        let mut rows = String::from("[");
        for (index, row) in self.projection.rows[..self.projection.len]
            .iter()
            .enumerate()
        {
            if index != 0 {
                rows.push(',');
            }
            write!(rows, "[{},{},{}]", row.id, row.incarnation, row.value).unwrap();
        }
        rows.push(']');
        format!(
            "{{\"schema\":\"rca-keyed-scaling-performance-profile-v1\",\"workload\":\"{}\",\"size\":{},\"iterations\":{},\"features\":{{\"deterministic\":{},\"allocations\":{}}},\"feature_profile\":\"{}\",\"parent_reductions\":{},\"child_reductions\":{},\"root_sends\":{},\"immediate_causal_completed\":{},\"routed_updates\":{},\"tasks_started\":{},\"tasks_cancelled\":{},\"removed_tasks_cancelled\":{},\"shutdown_tasks_cancelled\":{},\"removed_tasks_joined\":{},\"resident_tasks_joined\":{},\"tasks_completed\":{},\"states_created\":{},\"states_dropped\":{},\"captures_created\":{},\"captures_dropped\":{},\"reducers_created\":2,\"reducers_dropped\":2,\"final_projection\":{},\"state_extracted\":true,\"host_task_dropped\":true,\"owners_released\":true,\"host_stopped\":true,\"allocations\":{}}}",
            config.name,
            config.size,
            config.iterations,
            cfg!(feature = "deterministic"),
            cfg!(feature = "allocations"),
            if cfg!(feature = "deterministic") {
                "deterministic-enabled"
            } else {
                "minimal-production"
            },
            self.projection.parent_calls,
            self.child_calls,
            self.counts.root_sends,
            self.counts.immediate_finished,
            self.counts.routed_updates,
            self.created,
            self.created,
            self.counts.removed_joined,
            self.resident_joined,
            self.counts.removed_joined,
            self.resident_joined,
            self.created,
            self.created,
            self.created,
            self.created,
            self.created,
            rows,
            allocations,
        )
    }
}

/// Measures separate lifecycle phases and emits a record only after full validation.
pub(crate) fn run(config: Config) -> Result<String, &'static str> {
    // Ready-only deterministic driving has no retained executor thread-local cache.
    let mut measurement = Measurement::new(config.native.then_some(ExecutorCache::Native));
    let mut fixture = Fixture::new(
        config,
        #[cfg(test)]
        None,
    )?;
    measurement.construction_finished();
    fixture.warmup(config)?;
    measurement.warmup_finished();
    profile_keyed_scaling_work(
        &mut fixture,
        config,
        #[cfg(test)]
        None,
    )?;
    measurement.work_finished();
    let projection = fixture.validate(config)?;
    measurement.validation_finished();
    let outcome = fixture.teardown(&projection, config)?;
    let allocation = measurement
        .finish()?
        .map_or_else(|| "null".to_owned(), |record| record.json());
    Ok(outcome.json(config, &allocation))
}

#[cfg(test)]
/// Breaks real workload operations to challenge trace and lifetime validation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
    MissingUpdate,
    WrongOrder,
    WrongIncarnation,
    SiblingCancelled,
    RetainedCapture,
    UncancelledRetired,
}

#[cfg(test)]
/// Runs a faulty trace, including a real retained capture with balanced drop counts.
fn control(fault: Fault) -> Result<(), &'static str> {
    let name = if fault == Fault::RetainedCapture {
        "keyed-scale-steady-native"
    } else {
        "keyed-scale-remove-one-native"
    };
    let config = Config::parse([name, "16", "1"].into_iter().map(str::to_owned))?;
    let mut fixture = Fixture::new(config, Some(fault))?;
    fixture.warmup(config)?;
    profile_keyed_scaling_work(&mut fixture, config, Some(fault))?;
    let projection = fixture.validate(config)?;
    // Keep the identity of a real resident effect alive past shutdown. Counts
    // alone remain balanced, so the weak-owner check must reject this fault.
    let retained = if fault == Fault::RetainedCapture {
        fixture
            .probes
            .owners
            .lock()
            .unwrap()
            .iter()
            .find(|(kind, _)| *kind == CAPTURE)
            .and_then(|(_, owner)| owner.upgrade())
    } else {
        None
    };
    let result = fixture.teardown(&projection, config).map(|_| ());
    drop(retained);
    result
}

#[cfg(all(test, feature = "deterministic"))]
/// Requires uncancelled retirement to fail promptly, then proves cleanup still succeeds.
fn uncancelled_retirement_control() -> Result<(), &'static str> {
    let config = Config::parse(
        ["keyed-scale-remove-one-deterministic", "16", "1"]
            .into_iter()
            .map(str::to_owned),
    )?;
    let mut fixture = Fixture::new(config, Some(Fault::UncancelledRetired))?;
    fixture.warmup(config)?;
    let result = profile_keyed_scaling_work(&mut fixture, config, Some(Fault::UncancelledRetired));
    if result != Err("deterministic ready drive quiescent pending")
        || fixture.active.len() != 16
        || fixture
            .active
            .front()
            .is_none_or(|old| old.task.is_finished() || old.task.is_cancelled())
        || fixture.probes.count(CAPTURE) != (16, 0)
    {
        return Err(INVALID);
    }
    fixture.cleanup_failed_trace(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Keeps literal workload cases concise without bypassing argument validation.
    fn config(name: &str, size: &str, iterations: &str) -> Config {
        Config::parse([name, size, iterations].into_iter().map(str::to_owned)).unwrap()
    }

    #[test]
    fn keyed_scaling_accepts_only_finite_input_contract() {
        for name in [
            "keyed-scale-steady-native",
            "keyed-scale-remove-one-native",
            "keyed-scale-remove-all-native",
            #[cfg(feature = "deterministic")]
            "keyed-scale-steady-deterministic",
            #[cfg(feature = "deterministic")]
            "keyed-scale-remove-one-deterministic",
            #[cfg(feature = "deterministic")]
            "keyed-scale-remove-all-deterministic",
        ] {
            for size in ["0", "1", "16", "256", "1024"] {
                config(name, size, "1");
            }
        }
        for args in [
            vec!["keyed-scale-steady-native", "2", "1"],
            vec!["keyed-scale-steady-native", "0", "0"],
            vec!["keyed-scale-steady-native", "-1", "1"],
            vec!["keyed-scale-steady-native", "0", "18446744073709551615"],
            vec!["keyed-scale-steady-native", "18446744073709551616", "1"],
            vec!["keyed-scale-steady-native", "0", "oops"],
            vec!["keyed-scale-steady-native", "0", "1", "extra"],
            vec!["keyed-scale-unknown-native", "0", "1"],
        ] {
            assert!(Config::parse(args.into_iter().map(str::to_owned)).is_err());
        }
        #[cfg(not(feature = "deterministic"))]
        for name in [
            "keyed-scale-steady-deterministic",
            "keyed-scale-remove-one-deterministic",
            "keyed-scale-remove-all-deterministic",
        ] {
            assert!(Config::parse([name, "0", "1"].into_iter().map(str::to_owned)).is_err());
        }
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "literal ordered projections and lifetime counts keep the trace oracle independent"
    )]
    fn keyed_scaling_real_trace_preserves_order_and_exact_lifetimes() {
        #[cfg(feature = "allocations")]
        if !crate::tests::in_child(
            "keyed_scaling::tests::keyed_scaling_real_trace_preserves_order_and_exact_lifetimes",
        ) {
            return;
        }
        for (name, size, rows, parent, child, created, cancelled) in [
            (
                "keyed-scale-steady-native",
                "0",
                serde_json::json!([]),
                2,
                0,
                0,
                0,
            ),
            (
                "keyed-scale-remove-one-native",
                "0",
                serde_json::json!([]),
                4,
                0,
                0,
                0,
            ),
            (
                "keyed-scale-remove-all-native",
                "0",
                serde_json::json!([]),
                4,
                0,
                0,
                0,
            ),
            (
                "keyed-scale-steady-native",
                "1",
                serde_json::json!([[0, 0, 2]]),
                5,
                3,
                1,
                0,
            ),
            (
                "keyed-scale-remove-one-native",
                "1",
                serde_json::json!([[0, 2, 1]]),
                9,
                5,
                3,
                2,
            ),
            (
                "keyed-scale-remove-all-native",
                "1",
                serde_json::json!([[0, 2, 1]]),
                9,
                5,
                3,
                2,
            ),
            (
                "keyed-scale-remove-one-native",
                "16",
                serde_json::json!([
                    [2, 0, 2],
                    [3, 0, 2],
                    [4, 0, 2],
                    [5, 0, 2],
                    [6, 0, 2],
                    [7, 0, 2],
                    [8, 0, 2],
                    [9, 0, 2],
                    [10, 0, 2],
                    [11, 0, 2],
                    [12, 0, 2],
                    [13, 0, 2],
                    [14, 0, 2],
                    [15, 0, 2],
                    [0, 1, 2],
                    [1, 2, 1]
                ]),
                54,
                50,
                18,
                2,
            ),
        ] {
            let output = run(config(name, size, "2")).unwrap();
            let record: serde_json::Value = serde_json::from_str(&output).unwrap();
            assert_eq!(record["schema"], "rca-keyed-scaling-performance-profile-v1");
            assert_eq!(record["workload"], name);
            assert_eq!(record["final_projection"], rows);
            assert_eq!(record["parent_reductions"], parent);
            assert_eq!(record["child_reductions"], child);
            assert_eq!(record["states_created"], created);
            assert_eq!(record["states_dropped"], created);
            assert_eq!(record["tasks_cancelled"], created);
            assert_eq!(record["removed_tasks_cancelled"], cancelled);
            assert_eq!(
                record["shutdown_tasks_cancelled"],
                size.parse::<u64>().unwrap()
            );
            assert_eq!(record["tasks_completed"], created);
            assert_eq!(
                record["resident_tasks_joined"],
                size.parse::<u64>().unwrap()
            );
            assert_eq!(record["captures_created"], created);
            assert_eq!(record["captures_dropped"], created);
            assert_eq!(record["owners_released"], true);
        }
    }

    #[test]
    fn keyed_scaling_large_widths_retain_exact_final_rows() {
        #[cfg(feature = "allocations")]
        if !crate::tests::in_child(
            "keyed_scaling::tests::keyed_scaling_large_widths_retain_exact_final_rows",
        ) {
            return;
        }
        for (name, size, first, last) in [
            (
                "keyed-scale-steady-native",
                "256",
                serde_json::json!([0, 0, 1]),
                serde_json::json!([255, 0, 1]),
            ),
            (
                "keyed-scale-remove-one-native",
                "256",
                serde_json::json!([1, 0, 1]),
                serde_json::json!([0, 1, 1]),
            ),
            (
                "keyed-scale-remove-all-native",
                "1024",
                serde_json::json!([0, 1, 1]),
                serde_json::json!([1023, 1, 1]),
            ),
        ] {
            let output = run(config(name, size, "1")).unwrap();
            let record: serde_json::Value = serde_json::from_str(&output).unwrap();
            let rows = record["final_projection"].as_array().unwrap();
            assert_eq!(rows.len(), size.parse::<usize>().unwrap());
            assert_eq!(rows.first().unwrap(), &first);
            assert_eq!(rows.last().unwrap(), &last);
            assert_eq!(record["routed_updates"], size.parse::<u64>().unwrap());
        }
    }

    #[test]
    fn keyed_scaling_replacement_wraps_with_exact_generations() {
        #[cfg(feature = "allocations")]
        if !crate::tests::in_child(
            "keyed_scaling::tests::keyed_scaling_replacement_wraps_with_exact_generations",
        ) {
            return;
        }
        let expected = serde_json::json!([
            [1, 2, 16],
            [2, 3, 15],
            [3, 4, 14],
            [4, 5, 13],
            [5, 6, 12],
            [6, 7, 11],
            [7, 8, 10],
            [8, 9, 9],
            [9, 10, 8],
            [10, 11, 7],
            [11, 12, 6],
            [12, 13, 5],
            [13, 14, 4],
            [14, 15, 3],
            [15, 16, 2],
            [0, 17, 1]
        ]);
        for name in [
            "keyed-scale-remove-one-native",
            #[cfg(feature = "deterministic")]
            "keyed-scale-remove-one-deterministic",
        ] {
            let output = run(config(name, "16", "17")).unwrap();
            let record: serde_json::Value = serde_json::from_str(&output).unwrap();
            assert_eq!(record["final_projection"], expected, "{name}");
            for (field, count) in [
                ("parent_reductions", 339),
                ("child_reductions", 305),
                ("routed_updates", 272),
                ("states_created", 33),
                ("states_dropped", 33),
                ("captures_created", 33),
                ("captures_dropped", 33),
                ("removed_tasks_joined", 17),
                ("resident_tasks_joined", 16),
            ] {
                assert_eq!(record[field], count, "{name}: {field}");
            }
        }
    }

    #[test]
    #[expect(
        clippy::large_stack_arrays,
        reason = "this control verifies the bounded allocation-free projection representation"
    )]
    fn keyed_scaling_report_projection_is_fixed_inline_storage() {
        let projection = Projection {
            rows: [Row::default(); MAX_SIZE],
            len: MAX_SIZE,
            parent_calls: 0,
        };
        let _: &[Row; MAX_SIZE] = &projection.rows;
        assert_eq!(projection.len, MAX_SIZE);
        assert!(!std::mem::needs_drop::<Projection>());
        assert_eq!(
            std::mem::size_of::<Projection>(),
            MAX_SIZE * std::mem::size_of::<Row>() + 2 * std::mem::size_of::<u64>()
        );
    }

    #[cfg(feature = "deterministic")]
    #[test]
    fn keyed_scaling_uncancelled_retirement_fails_bounded_and_cleans_up() {
        assert_eq!(uncancelled_retirement_control(), Ok(()));
    }

    #[cfg(feature = "deterministic")]
    #[test]
    fn keyed_scaling_deterministic_matches_native_trace() {
        #[cfg(feature = "allocations")]
        if !crate::tests::in_child(
            "keyed_scaling::tests::keyed_scaling_deterministic_matches_native_trace",
        ) {
            return;
        }
        for mode in ["steady", "remove-one", "remove-all"] {
            for size in ["0", "1", "16"] {
                let native = format!("keyed-scale-{mode}-native");
                let deterministic = format!("keyed-scale-{mode}-deterministic");
                let native: serde_json::Value =
                    serde_json::from_str(&run(config(&native, size, "2")).unwrap()).unwrap();
                let deterministic: serde_json::Value =
                    serde_json::from_str(&run(config(&deterministic, size, "2")).unwrap()).unwrap();
                for field in [
                    "parent_reductions",
                    "child_reductions",
                    "routed_updates",
                    "states_created",
                    "states_dropped",
                    "captures_created",
                    "captures_dropped",
                    "removed_tasks_cancelled",
                    "shutdown_tasks_cancelled",
                    "final_projection",
                ] {
                    assert_eq!(deterministic[field], native[field], "{mode} {size} {field}");
                }
            }
        }
    }

    #[test]
    fn keyed_scaling_real_faults_fail_their_validators() {
        for fault in [
            Fault::MissingUpdate,
            Fault::WrongOrder,
            Fault::WrongIncarnation,
            Fault::SiblingCancelled,
            Fault::RetainedCapture,
        ] {
            assert_eq!(
                control(fault),
                Err("keyed scaling trace or ownership invalid")
            );
        }
    }
}
