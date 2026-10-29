//! Profiles keyed scopes, ordered output and retirement of a replaced child.
//!
//! Useful work includes scope creation, routing, observation and per-cycle checks.
//! Construction, warm-up, final validation and teardown remain separate phases.
//! No client waits for external input: deterministic driving rejects quiescence.

use std::fmt::Write;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use async_channel::{Receiver, Sender};
use rca_core::{
    ActionTask, Effect, EffectSender, KeyedAction, KeyedActionPath, KeyedState, KeyedStatePath,
    Observation, ReduceContext, Reducer, ScopedStore, ScopedStoreEnded, Store, WeakScopedStore,
    for_each,
};

use crate::host::Host;
#[cfg(test)]
use crate::keyed_controls::Fault;
use crate::measurement::{ExecutorCache, Measurement};

/// Largest qualified width; keeps projections inline without retaining state.
const MAX_ROWS: usize = 256;
/// A deadlock watchdog, not a performance threshold or timing denominator.
const DEADLINE: Duration = Duration::from_secs(10);
/// Rejects a trace or ownership check before a success record can be emitted.
const INVALID: &str = "keyed trace or ownership invalid";
/// Counter category for each resident or replacement child state.
const STATE: usize = 0;
/// Counter category for the parent and child reducer instances.
const REDUCER: usize = 1;
/// Counter category for each suspended effect's move-only capture.
const CAPTURE: usize = 2;

/// Validated trace dimensions and executor choice.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Config {
    name: &'static str,
    size: u64,
    cycles: u64,
    steady: bool,
    native: bool,
    /// Bounds scheduler work and outer polls independently of wall-clock load.
    budget: usize,
}

impl Config {
    /// Rejects unsupported widths, zero work and overflowing count or poll budgets.
    pub(crate) fn parse(mut args: impl Iterator<Item = String>) -> Result<Self, &'static str> {
        let name = args.next().ok_or("expected WORKLOAD SIZE ITERATIONS")?;
        let (name, steady, native) = match name.as_str() {
            "keyed-steady-native" => ("keyed-steady-native", true, true),
            "keyed-steady-deterministic" => ("keyed-steady-deterministic", true, false),
            "keyed-retire-native" => ("keyed-retire-native", false, true),
            "keyed-retire-deterministic" => ("keyed-retire-deterministic", false, false),
            _ => return Err("unknown workload"),
        };
        if !native && !cfg!(feature = "deterministic") {
            return Err("deterministic workloads require the deterministic feature");
        }
        let size = crate::workloads::integer(args.next())?;
        let cycles = crate::workloads::integer(args.next())?;
        if !matches!(size, 2 | 256) || cycles == 0 || args.next().is_some() {
            return Err("keyed modes require SIZE=2 or 256 and positive ITERATIONS");
        }
        let reductions = cycles
            .checked_mul(6)
            .and_then(|n| n.checked_add(1))
            .ok_or("keyed count overflow")?;
        size.checked_add(cycles)
            .ok_or("keyed state count overflow")?;
        let budget = usize::try_from(reductions)
            .ok()
            .and_then(|n| n.checked_mul(128))
            .and_then(|n| n.checked_add(2048))
            .ok_or("keyed poll-budget overflow")?;
        Ok(Self {
            name,
            size,
            cycles,
            steady,
            native,
            budget,
        })
    }
}

/// Records lifetimes and reducer activity independently of client-side counters.
#[derive(Default)]
struct Probes {
    created: [AtomicU64; 3],
    dropped: [AtomicU64; 3],
    /// Unique weak identities detect retention without keeping owners alive.
    owners: Mutex<Vec<Weak<()>>>,
    leaf_calls: AtomicU64,
    started: AtomicU64,
    finished: AtomicU64,
}

impl Probes {
    /// Returns creation and destruction totals for one ownership category.
    fn count(&self, kind: usize) -> (u64, u64) {
        (
            self.created[kind].load(Ordering::Relaxed),
            self.dropped[kind].load(Ordering::Relaxed),
        )
    }

    /// Requires every observed identity to expire after store and host teardown.
    fn released(&self) -> bool {
        self.owners
            .lock()
            .unwrap()
            .iter()
            .all(|owner| owner.upgrade().is_none())
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

/// Move-only state prevents projection reads from retaining resident owners.
struct LeafState {
    incarnation: u64,
    value: u64,
    /// Tracks output ordinals within the current sequential effect.
    seen: u8,
    /// Latches any out-of-order or duplicate output across later actions.
    valid: bool,
    _guard: Guard,
}

impl LeafState {
    /// Starts a new row generation with no updates or observed outputs.
    fn new(probes: &Arc<Probes>, incarnation: u64) -> Self {
        Self {
            incarnation,
            value: 0,
            seen: 0,
            valid: true,
            _guard: Guard::new(probes, STATE),
        }
    }
}

/// Exercises direct updates, ordered output and a suspended child effect.
enum LeafAction {
    Ping,
    EmitTwo,
    Output(u8),
    Start {
        capture: Sender<EffectSender<LeafAction>>,
        gate: Receiver<()>,
    },
}

/// Routes child actions or changes keyed order and identity.
enum Action {
    Noop,
    Child(KeyedAction<u64, LeafAction>),
    Reverse,
    Recreate(u64),
}

/// Holds ordered children and independent parent-side reduction totals.
struct State {
    rows: KeyedState<u64, LeafState>,
    parent_calls: u64,
    responses: u64,
}

/// Applies membership edits and counts accepted child outputs.
struct Parent {
    guard: Guard,
    #[cfg(test)]
    fault: Option<Fault>,
}

/// Validates output order and owns each child effect's lifetime witness.
struct Leaf {
    guard: Guard,
    #[cfg(test)]
    fault: Option<Fault>,
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
            Action::Noop => {}
            Action::Child(child) => {
                if matches!(child.action(), LeafAction::Output(_)) {
                    state.responses += 1;
                }
            }
            Action::Reverse => {
                #[cfg(test)]
                if self.fault == Some(Fault::WrongOrder) {
                    return Effect::none();
                }
                state.rows.reverse();
            }
            Action::Recreate(incarnation) => {
                #[cfg(test)]
                if matches!(
                    self.fault,
                    Some(Fault::StaleAccepted | Fault::RevivedRoute | Fault::UncancelledRetired)
                ) {
                    return Effect::none();
                }
                drop(state.rows.remove(&0).expect("resident target"));
                state
                    .rows
                    .insert(0, LeafState::new(&self.guard.probes, *incarnation));
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
        self.guard.probes.leaf_calls.fetch_add(1, Ordering::Relaxed);
        match action {
            LeafAction::Ping => {
                state.value += 1;
                Effect::none()
            }
            LeafAction::EmitTwo => {
                state.seen = 0;
                #[cfg(test)]
                match self.fault {
                    Some(Fault::MissingOutput) => return Effect::send(LeafAction::Output(1)),
                    Some(Fault::DuplicateOutput) => {
                        return Effect::concatenate([
                            Effect::send(LeafAction::Output(1)),
                            Effect::send(LeafAction::Output(1)),
                        ]);
                    }
                    Some(Fault::SwappedOutput) => {
                        return Effect::concatenate([
                            Effect::send(LeafAction::Output(2)),
                            Effect::send(LeafAction::Output(1)),
                        ]);
                    }
                    _ => {}
                }
                Effect::concatenate([
                    Effect::send(LeafAction::Output(1)),
                    Effect::send(LeafAction::Output(2)),
                ])
            }
            LeafAction::Output(ordinal) => {
                let bit = match ordinal {
                    1 => 1,
                    2 => 2,
                    _ => 0,
                };
                state.valid &= matches!((state.seen, ordinal), (0, 1) | (1, 2));
                state.seen |= bit;
                state.value += 1;
                Effect::none()
            }
            LeafAction::Start { capture, gate } => {
                state.seen = 0;
                let capture = capture.clone();
                let gate = gate.clone();
                // Created before entering either closure/body: even an unpolled
                // operation owns an observable, uniquely witnessed capture.
                let guard = Guard::new(&self.guard.probes, CAPTURE);
                Effect::run(move |sender| async move {
                    guard.probes.started.fetch_add(1, Ordering::Relaxed);
                    if capture.send(sender).await.is_ok() && gate.recv().await.is_ok() {
                        guard.probes.finished.fetch_add(1, Ordering::Relaxed);
                    }
                    drop(guard);
                })
            }
        }
    }
}

/// Copies only the row identity, generation and committed value.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Row {
    id: u64,
    incarnation: u64,
    value: u64,
}

/// Keeps ordered validation data inline without cloning any row owners.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Projection {
    rows: [Row; MAX_ROWS],
    len: usize,
    parent_calls: u64,
    responses: u64,
    valid: bool,
}

/// Copies all qualified rows and combines their latched output-order checks.
fn project(state: &State) -> Projection {
    let mut projection = Projection {
        rows: [Row::default(); MAX_ROWS],
        len: state.rows.len(),
        parent_calls: state.parent_calls,
        responses: state.responses,
        valid: true,
    };
    for (index, (id, leaf)) in state.rows.iter().enumerate() {
        projection.rows[index] = Row {
            id: *id,
            incarnation: leaf.incarnation,
            value: leaf.value,
        };
        projection.valid &= leaf.valid;
    }
    projection
}

/// Checks every row and reduction total against the requested trace position.
fn validate_projection(
    config: Config,
    projection: &Projection,
    cycle: u64,
    leaf_calls: u64,
) -> Result<(), &'static str> {
    if projection.len as u64 != config.size
        || !projection.valid
        || projection.parent_calls
            != u64::from(config.steady) + cycle * if config.steady { 5 } else { 6 }
        || projection.responses != cycle * if config.steady { 2 } else { 1 }
        || leaf_calls != cycle * 4
    {
        return Err(INVALID);
    }
    for (index, row) in projection.rows[..projection.len].iter().enumerate() {
        let index = u64::try_from(index).map_err(|_| INVALID)?;
        let id = if config.steady || cycle == 0 {
            index
        } else if index == config.size - 1 {
            0
        } else if cycle.is_multiple_of(2) {
            index + 1
        } else {
            config.size - 1 - index
        };
        let incarnation = if id == 0 && !config.steady { cycle } else { 0 };
        let value = if id == 0 {
            if config.steady {
                cycle * 3
            } else {
                u64::from(cycle != 0)
            }
        } else if id == 1 && !config.steady {
            cycle
        } else {
            0
        };
        if *row
            != (Row {
                id,
                incarnation,
                value,
            })
        {
            return Err(INVALID);
        }
    }
    Ok(())
}

/// Counts client operations separately from reducer-side observations.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Counts {
    roots: u64,
    completed: u64,
    retired: u64,
    scope_registrations: u64,
    root_sends: u64,
    scoped_sends: u64,
    scoped_reads: u64,
    callback_attempts: u64,
    weak_upgrades: u64,
    weak_reads: u64,
    reader_registrations: u64,
    reader_next: u64,
    reader_closed: u64,
    sender_attempts: u64,
    stale_attempts: u64,
    stale_commits: u64,
    root_reads: u64,
}

/// Retains the capabilities needed to test a live or retired child scope.
struct Started {
    scope: ScopedStore<LeafState, LeafAction>,
    reader: Observation<u64>,
    gate: Sender<()>,
    task: ActionTask,
    sender: EffectSender<LeafAction>,
}

/// Owns the real store, its host and the trace's lifetime witnesses.
struct Fixture {
    /// Taken during driving to avoid borrowing the host and client mutably together.
    host: Option<Host>,
    store: Store<State, Action>,
    host_owner: Weak<()>,
    probes: Arc<Probes>,
    counts: Counts,
    #[cfg(test)]
    fault: Option<Fault>,
    #[cfg(test)]
    /// Keeps the incomplete-finish mutation suspended until fixture destruction.
    pending_gate: Option<Sender<()>>,
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
            #[cfg(test)]
            fault,
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
                responses: 0,
            },
            reducer,
            runtime,
        )
        .mailbox_capacity(1)
        .build()
        .map_err(|_| "keyed Store construction failed")?;
        Ok(Self {
            host: Some(host),
            store,
            host_owner,
            probes,
            counts: Counts::default(),
            #[cfg(test)]
            fault,
            #[cfg(test)]
            pending_gate: None,
        })
    }

    /// Borrows the host only while no client drive has temporarily taken it.
    fn host(&self) -> &Host {
        self.host
            .as_ref()
            .expect("host present outside useful driving")
    }

    /// Separates the steady trace's first root admission from useful work.
    fn warmup(&self, config: Config) -> Result<(), &'static str> {
        if config.steady {
            self.host().run_ready_bounded(
                async {
                    self.store
                        .send(Action::Noop)
                        .await
                        .map_err(|_| INVALID)?
                        .finish()
                        .await;
                    Ok::<_, &'static str>(())
                },
                DEADLINE,
                config.budget,
            )??;
            let projection = self
                .host()
                .run_ready_bounded(self.store.read(project), DEADLINE, config.budget)?
                .map_err(|_| INVALID)?;
            validate_projection(config, &projection, 0, 0)?;
        }
        Ok(())
    }

    /// Registers a real child scope and counts the attempted registration.
    async fn scope(&mut self, id: u64) -> Result<ScopedStore<LeafState, LeafAction>, &'static str> {
        self.counts.scope_registrations += 1;
        self.store
            .scope_keyed(
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
                id,
            )
            .await
            .map_err(|_| INVALID)
    }

    /// Commits a parent action and waits for its finite causal work.
    async fn parent(&mut self, action: Action) -> Result<(), &'static str> {
        self.counts.root_sends += 1;
        let task = self.store.send(action).await.map_err(|_| INVALID)?;
        self.counts.roots += 1;
        self.finish(&task).await
    }

    /// Commits a scoped action while leaving its causal wait to the caller.
    async fn child(
        &mut self,
        scope: &ScopedStore<LeafState, LeafAction>,
        action: LeafAction,
    ) -> Result<ActionTask, &'static str> {
        self.counts.scoped_sends += 1;
        let task = scope.send(action).await.map_err(|_| INVALID)?;
        self.counts.roots += 1;
        Ok(task)
    }

    /// Counts completion only after the task confirms all causal work has ended.
    async fn finish(&mut self, task: &ActionTask) -> Result<(), &'static str> {
        task.finish().await;
        if !task.is_finished() {
            return Err(INVALID);
        }
        self.counts.completed += 1;
        Ok(())
    }

    /// Commits a parent action, a child update and two ordered child outputs.
    async fn steady_cycle(&mut self) -> Result<(), &'static str> {
        self.parent(Action::Noop).await?;
        let target = self.scope(0).await?;
        let ping = self.child(&target, LeafAction::Ping).await?;
        self.finish(&ping).await?;
        let outputs = self.child(&target, LeafAction::EmitTwo).await?;
        self.finish(&outputs).await
    }

    /// Observes initial state and receives the real running effect's sender.
    async fn start(&mut self, id: u64, initial: u64) -> Result<Started, &'static str> {
        let scope = self.scope(id).await?;
        let mut reader = scope
            .observe_distinct(|state| state.value)
            .await
            .map_err(|_| INVALID)?;
        self.counts.reader_registrations += 1;
        self.counts.reader_next += 1;
        if reader.next().await != Some(initial) {
            return Err(INVALID);
        }
        let (gate, wait) = async_channel::bounded(1);
        let (capture, received) = async_channel::bounded(1);
        let task = self
            .child(
                &scope,
                LeafAction::Start {
                    capture,
                    gate: wait,
                },
            )
            .await?;
        let sender = received.recv().await.map_err(|_| INVALID)?;
        Ok(Started {
            scope,
            reader,
            gate,
            task,
            sender,
        })
    }

    /// Rejects every retained old authority, then checks the weak scope expires.
    async fn reject_old<F, Fut>(
        &mut self,
        mut old: Started,
        weak: WeakScopedStore<LeafState, LeafAction>,
        callback: F,
    ) -> Result<(), &'static str>
    where
        F: Fn() -> Fut + Send,
        Fut: Future<Output = Result<ActionTask, ScopedStoreEnded>> + Send,
    {
        self.counts.sender_attempts += 1;
        self.counts.stale_attempts += 1;
        match old.sender.send(LeafAction::Output(1)).await {
            Err(error) if error.is_cancelled() => {}
            _ => {
                self.counts.stale_commits += 1;
                return Err(INVALID);
            }
        }
        self.counts.scoped_sends += 1;
        if !old
            .scope
            .send(LeafAction::Ping)
            .await
            .is_err_and(|error| error.is_scope_ended())
        {
            return Err(INVALID);
        }
        self.counts.scoped_reads += 1;
        if !old
            .scope
            .read(|state| state.value)
            .await
            .is_err_and(|error| error.is_scope_ended())
        {
            return Err(INVALID);
        }
        self.counts.callback_attempts += 1;
        if !callback().await.is_err_and(|error| error.is_scope_ended()) {
            return Err(INVALID);
        }
        self.counts.weak_upgrades += 1;
        let ended = weak.upgrade().ok_or(INVALID)?;
        self.counts.weak_reads += 1;
        if !ended
            .read(|state| state.value)
            .await
            .is_err_and(|error| error.is_scope_ended())
        {
            return Err(INVALID);
        }
        self.counts.reader_next += 1;
        if old.reader.next().await.is_some() {
            return Err(INVALID);
        }
        self.counts.reader_closed += 1;
        drop(ended);
        drop(old.reader);
        drop(callback);
        drop(old.scope);
        drop(old.sender);
        self.counts.weak_upgrades += 1;
        if weak.upgrade().is_some() {
            return Err(INVALID);
        }
        // old.gate was never released; the already finished task lost its capture.
        Ok(())
    }

    /// Replaces one child while proving its sibling remains live and usable.
    async fn retire_cycle(&mut self, cycle: u64) -> Result<(), &'static str> {
        let old = self.start(0, u64::from(cycle != 1)).await?;
        let mut sibling = self.start(1, cycle - 1).await?;
        let weak = old.scope.downgrade();
        let callback = {
            let old = old.scope.clone();
            move || {
                let old = old.clone();
                async move { old.send(LeafAction::Ping).await }
            }
        };
        self.parent(Action::Reverse).await?;
        self.counts.scoped_reads += 1;
        if sibling
            .scope
            .read(|state| state.value)
            .await
            .map_err(|_| INVALID)?
            != cycle - 1
        {
            return Err(INVALID);
        }
        self.parent(Action::Recreate(cycle)).await?;
        #[cfg(test)]
        if self.fault == Some(Fault::StaleAccepted) {
            // The deliberately unchanged entry keeps its real sender live.
            // Reuse the normal stale-output rejection path against that authority.
            return self.reject_old(old, weak, callback).await;
        }
        #[cfg(test)]
        if self.fault == Some(Fault::RevivedRoute) {
            self.counts.scoped_sends += 1;
            if !old
                .scope
                .send(LeafAction::Ping)
                .await
                .is_err_and(|error| error.is_scope_ended())
            {
                return Err(INVALID);
            }
            return Err("route control unexpectedly ended");
        }
        self.finish(&old.task).await?;
        if self.probes.count(CAPTURE).1 != cycle * 2 - 1 {
            return Err(INVALID);
        }
        self.counts.retired += 1;
        self.reject_old(old, weak, callback).await?;
        let fresh = self.scope(0).await?;
        let ping = self.child(&fresh, LeafAction::Ping).await?;
        self.finish(&ping).await?;
        #[cfg(test)]
        if self.fault == Some(Fault::SiblingCancelled) {
            sibling.task.cancel();
        }
        self.counts.sender_attempts += 1;
        sibling
            .sender
            .send(LeafAction::Output(1))
            .await
            .map_err(|_| INVALID)?;
        self.counts.reader_next += 1;
        if sibling.reader.next().await != Some(cycle) || sibling.task.is_cancelled() {
            return Err(INVALID);
        }
        #[cfg(test)]
        if self.fault == Some(Fault::IncompleteFinish) {
            if sibling.task.is_finished() {
                return Err("pending control did not start");
            }
            self.pending_gate = Some(sibling.gate);
            return Ok(());
        }
        sibling.gate.send(()).await.map_err(|_| INVALID)?;
        self.finish(&sibling.task).await?;
        if sibling.task.is_cancelled() {
            return Err(INVALID);
        }
        Ok(())
    }

    /// Checks final state, exact client operations and effect lifetime totals.
    fn validate(&mut self, config: Config) -> Result<Projection, &'static str> {
        self.counts.root_reads += 1;
        let projection = self
            .host()
            .run_ready_bounded(self.store.read(project), DEADLINE, config.budget)?
            .map_err(|_| INVALID)?;
        validate_projection(
            config,
            &projection,
            config.cycles,
            self.probes.leaf_calls.load(Ordering::Relaxed),
        )?;
        let r = config.cycles;
        let expected = if config.steady {
            Counts {
                roots: 3 * r,
                completed: 3 * r,
                scope_registrations: r,
                root_sends: r,
                scoped_sends: 2 * r,
                root_reads: r + 1,
                ..Counts::default()
            }
        } else {
            Counts {
                roots: 5 * r,
                completed: 5 * r,
                retired: r,
                scope_registrations: 3 * r,
                root_sends: 2 * r,
                scoped_sends: 4 * r,
                scoped_reads: 2 * r,
                callback_attempts: r,
                weak_upgrades: 2 * r,
                weak_reads: r,
                reader_registrations: 2 * r,
                reader_next: 4 * r,
                reader_closed: r,
                sender_attempts: 2 * r,
                stale_attempts: r,
                stale_commits: 0,
                root_reads: r + 1,
            }
        };
        if self.counts != expected
            || self.probes.started.load(Ordering::Relaxed) != if config.steady { 0 } else { 2 * r }
            || self.probes.finished.load(Ordering::Relaxed) != if config.steady { 0 } else { r }
            || self.probes.count(CAPTURE)
                != if config.steady {
                    (0, 0)
                } else {
                    (2 * r, 2 * r)
                }
            || self.counts.stale_commits != 0
        {
            return Err(INVALID);
        }
        Ok(projection)
    }

    /// Extracts unchanged state before checking that all store and host owners expired.
    fn teardown(self, checked: &Projection, config: Config) -> Result<Outcome, &'static str> {
        let Self {
            host,
            store,
            host_owner,
            probes,
            counts,
            ..
        } = self;
        let host = host.expect("host restored after useful driving");
        let state = host
            .run_ready_bounded(store.shutdown_into_state(), DEADLINE, config.budget)?
            .map_err(|_| INVALID)?;
        let projection = project(&state);
        if projection != *checked {
            return Err(INVALID);
        }
        drop(state);
        drop(host);
        validate_owners(&probes, &host_owner, config)?;
        let states = probes.count(STATE);
        let captures = probes.count(CAPTURE);
        Ok(Outcome {
            projection,
            counts,
            states,
            captures,
            started: probes.started.load(Ordering::Relaxed),
            finished: probes.finished.load(Ordering::Relaxed),
            leaf_calls: probes.leaf_calls.load(Ordering::Relaxed),
            released: true,
        })
    }
}

/// Requires exact lifetime totals and expired weak witnesses after teardown.
fn validate_owners(
    probes: &Probes,
    host_owner: &Weak<()>,
    config: Config,
) -> Result<(), &'static str> {
    let states = config.size + if config.steady { 0 } else { config.cycles };
    let captures = if config.steady { 0 } else { 2 * config.cycles };
    if !probes.released()
        || host_owner.upgrade().is_some()
        || probes.count(STATE) != (states, states)
        || probes.count(REDUCER) != (2, 2)
        || probes.count(CAPTURE) != (captures, captures)
    {
        return Err(INVALID);
    }
    Ok(())
}

/// Drives the exact keyed trace, excluding construction, warm-up and reporting.
#[inline(never)]
fn profile_keyed_work(fixture: &mut Fixture, config: Config) -> Result<(), &'static str> {
    // Temporarily move the already constructed host to avoid aliasing the mutable
    // client. No host construction or additional Store handle occurs in work.
    let host = fixture.host.take().expect("host owned by fixture");
    // This trace has no external producer or timer. Quiescence is a failed trace,
    // not a reason to wait indefinitely after the runnable-task budget goes idle.
    let result = host.run_ready_bounded(
        async {
            for cycle in 1..=config.cycles {
                if config.steady {
                    fixture.steady_cycle().await?;
                } else {
                    fixture.retire_cycle(cycle).await?;
                }
                fixture.counts.root_reads += 1;
                let projection = fixture.store.read(project).await.map_err(|_| INVALID)?;
                validate_projection(
                    config,
                    &projection,
                    cycle,
                    fixture.probes.leaf_calls.load(Ordering::Relaxed),
                )?;
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
    states: (u64, u64),
    captures: (u64, u64),
    started: u64,
    finished: u64,
    leaf_calls: u64,
    released: bool,
}

impl Outcome {
    /// Formats ordered rows, their stable digest and the validated operation totals.
    fn json(&self, config: Config, allocations: &str) -> String {
        let mut rows = String::from("[");
        let mut digest = 0xcbf2_9ce4_8422_2325_u64;
        for (index, row) in self.projection.rows[..self.projection.len]
            .iter()
            .enumerate()
        {
            if index != 0 {
                rows.push(',');
            }
            write!(rows, "[{},{},{}]", row.id, row.incarnation, row.value).unwrap();
            for value in [row.id, row.incarnation, row.value] {
                digest = (digest ^ value).wrapping_mul(0x100_0000_01b3);
            }
        }
        rows.push(']');
        let mut fields = String::new();
        for (name, value) in [
            ("warmup_roots", u64::from(config.steady)),
            ("warmup_parent_reductions", u64::from(config.steady)),
            ("warmup_causal_completed", u64::from(config.steady)),
            ("warmup_reads", u64::from(config.steady)),
            ("roots_committed", self.counts.roots),
            ("responses_committed", self.projection.responses),
            (
                "parent_reductions",
                self.projection.parent_calls - u64::from(config.steady),
            ),
            ("leaf_reductions", self.leaf_calls),
            ("causal_completed", self.counts.completed),
            ("tasks_started", self.started),
            ("tasks_retired", self.counts.retired),
            ("tasks_completed", self.finished),
            ("captures_created", self.captures.0),
            ("captures_dropped", self.captures.1),
            ("states_created", self.states.0),
            ("states_dropped", self.states.1),
            ("accepted_outputs", self.projection.responses),
            ("stale_output_attempts", self.counts.stale_attempts),
            ("stale_outputs_committed", self.counts.stale_commits),
            ("scope_registrations", self.counts.scope_registrations),
            ("root_send_attempts", self.counts.root_sends),
            ("scoped_send_attempts", self.counts.scoped_sends),
            ("scoped_read_attempts", self.counts.scoped_reads),
            ("callback_attempts", self.counts.callback_attempts),
            ("weak_upgrades", self.counts.weak_upgrades),
            ("weak_read_attempts", self.counts.weak_reads),
            ("reader_registrations", self.counts.reader_registrations),
            ("reader_next_calls", self.counts.reader_next),
            ("readers_closed", self.counts.reader_closed),
            ("effect_sender_attempts", self.counts.sender_attempts),
            ("root_read_calls", self.counts.root_reads),
        ] {
            write!(fields, ",\"{name}\":{value}").unwrap();
        }
        format!(
            "{{\"schema\":\"rca-keyed-performance-profile-v1\",\"workload\":\"{}\",\"size\":{},\"iterations\":{},\"features\":{{\"deterministic\":{},\"allocations\":{}}},\"feature_profile\":\"{}\"{},\"final_projection\":{},\"projection_digest\":{},\"state_extracted\":{},\"reducers_dropped\":{},\"host_task_dropped\":{},\"owners_released\":{},\"host_stopped\":{},\"allocations\":{}}}",
            config.name,
            config.size,
            config.cycles,
            cfg!(feature = "deterministic"),
            cfg!(feature = "allocations"),
            if cfg!(feature = "deterministic") {
                "deterministic-enabled"
            } else {
                "minimal-production"
            },
            fields,
            rows,
            digest,
            self.released,
            self.released,
            self.released,
            self.released,
            self.released,
            allocations
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
    profile_keyed_work(&mut fixture, config)?;
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
/// Breaks a real trace operation or lifetime to challenge its normal validator.
pub(crate) fn control(fault: Fault) -> Result<(), &'static str> {
    let name = if matches!(
        fault,
        Fault::MissingOutput | Fault::DuplicateOutput | Fault::SwappedOutput
    ) {
        "keyed-steady-native"
    } else {
        "keyed-retire-native"
    };
    let size = if fault == Fault::WrongOrder {
        "256"
    } else {
        "2"
    };
    let config = Config::parse([name, size, "1"].into_iter().map(str::to_owned))?;
    let mut fixture = Fixture::new(config, Some(fault))?;
    fixture.warmup(config)?;
    let retained = (fault == Fault::RetainedCapture).then(|| {
        let guard = Guard::new(&fixture.probes, CAPTURE);
        Effect::<LeafAction>::future(async move {
            std::hint::black_box(&guard);
            std::future::pending::<Option<LeafAction>>().await
        })
    });
    profile_keyed_work(&mut fixture, config)?;
    let projection = fixture.validate(config)?;
    if fault == Fault::SkippedTeardown {
        return validate_owners(&fixture.probes, &fixture.host_owner, config);
    }
    let outcome = fixture.teardown(&projection, config)?;
    drop(retained);
    if !outcome.released {
        return Err(INVALID);
    }
    Ok(())
}

#[cfg(test)]
/// Proves the steady trace's first root is a separately validated warm-up action.
pub(crate) fn cold_control(native: bool) -> Result<(), &'static str> {
    let config = Config::parse(
        [
            if native {
                "keyed-steady-native"
            } else {
                "keyed-steady-deterministic"
            },
            "2",
            "1",
        ]
        .into_iter()
        .map(str::to_owned),
    )?;
    let fixture = Fixture::new(config, None)?;
    fixture.host().run_ready_bounded(
        async {
            fixture
                .store
                .send(Action::Noop)
                .await
                .map_err(|_| INVALID)?
                .finish()
                .await;
            Ok::<_, &'static str>(())
        },
        DEADLINE,
        config.budget,
    )??;
    let projection = fixture
        .host()
        .run_ready_bounded(fixture.store.read(project), DEADLINE, config.budget)?
        .map_err(|_| INVALID)?;
    validate_projection(config, &projection, 0, 0)?;
    fixture.teardown(&projection, config).map(|_| ())
}

/// Keeps an old child alive to check bounded failure and failed-trace cleanup.
#[cfg(all(test, feature = "deterministic"))]
pub(crate) fn uncancelled_retirement_control() -> Result<(), &'static str> {
    let config = Config::parse(
        ["keyed-retire-deterministic", "2", "1"]
            .into_iter()
            .map(str::to_owned),
    )?;
    let mut fixture = Fixture::new(config, Some(Fault::UncancelledRetired))?;
    let probes = Arc::clone(&fixture.probes);
    let host_owner = fixture.host_owner.clone();
    let result = profile_keyed_work(&mut fixture, config);
    let reached_retirement = fixture.counts.roots == 4
        && fixture.counts.completed == 2
        && fixture.counts.retired == 0
        && probes.started.load(Ordering::Relaxed) == 2;
    drop(fixture);
    if result != Err("deterministic ready drive quiescent pending")
        || !reached_retirement
        || !probes.released()
        || host_owner.upgrade().is_some()
        || [STATE, REDUCER, CAPTURE].into_iter().any(|kind| {
            let (created, dropped) = probes.count(kind);
            created != dropped
        })
    {
        return Err(INVALID);
    }
    Ok(())
}
