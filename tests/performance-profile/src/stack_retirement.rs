//! Checks stack suffix retirement while the surviving prefix remains active.
//!
//! Retired clients stay alive across reinsertion of the same stack IDs.
//! Their outputs, scopes and readers must remain revoked after reinsertion.
//! The work phase includes those checks. Final shutdown joins the remaining tasks
//! before the owner and host checks, including after a negative-control failure.

use std::fmt::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use rca_core::{
    ActionPath, ActionTask, Effect, EffectSender, Observation, ReduceContext, Reducer, ScopedStore,
    StackAction, StackId, StackState, StatePath, Store, stack,
};

use crate::host::Host;
use crate::measurement::{ExecutorCache, Measurement};

const INVALID: &str = "stack retirement trace or ownership invalid";
// A liveness guard, not a timing metric; allocation backtraces add substantial overhead.
const DEADLINE: Duration = Duration::from_secs(60);
const MAX_WIDTH: usize = 256;
const STATE: usize = 0;
const REDUCER: usize = 1;
const CAPTURE: usize = 2;

#[derive(Clone, Copy, Debug)]
pub(crate) struct Config {
    name: &'static str,
    width: usize,
    iterations: u64,
    native: bool,
    budget: usize,
}

impl Config {
    pub(crate) fn parse(mut args: impl Iterator<Item = String>) -> Result<Self, &'static str> {
        let (name, native) = match args.next().as_deref() {
            Some("stack-retirement-native") => ("stack-retirement-native", true),
            Some("stack-retirement-deterministic") => ("stack-retirement-deterministic", false),
            _ => return Err("unknown stack retirement workload"),
        };
        if !native && !cfg!(feature = "deterministic") {
            return Err("deterministic workloads require the deterministic feature");
        }
        let width = crate::workloads::integer(args.next())?;
        let iterations = crate::workloads::integer(args.next())?;
        if !matches!(width, 0 | 1 | 16 | 256) || iterations == 0 || args.next().is_some() {
            return Err("stack retirement requires SIZE=0/1/16/256 and positive ITERATIONS");
        }
        // Bounds scheduler work and outer polls, including scoped observations.
        let budget = iterations
            .checked_mul(width * 8 + 4)
            .and_then(|work| work.checked_add(width))
            .and_then(|work| work.checked_mul(128))
            .and_then(|work| work.checked_add(2048))
            .and_then(|work| usize::try_from(work).ok())
            .ok_or("stack retirement work overflow")?;
        Ok(Self {
            name,
            width: usize::try_from(width).map_err(|_| INVALID)?,
            iterations,
            native,
            budget,
        })
    }

    fn prefix(self) -> usize {
        self.width / 2
    }

    fn created(self) -> u64 {
        self.width as u64 + (self.iterations - 1) * (self.width - self.prefix()) as u64
    }
}

#[derive(Default)]
struct Probes {
    created: [AtomicU64; 3],
    dropped: [AtomicU64; 3],
    owners: Mutex<Vec<(usize, Weak<()>)>>,
    child_calls: AtomicU64,
    started: AtomicU64,
}

impl Probes {
    fn count(&self, kind: usize) -> (u64, u64) {
        (
            self.created[kind].load(Ordering::Relaxed),
            self.dropped[kind].load(Ordering::Relaxed),
        )
    }

    fn released(&self) -> bool {
        self.owners
            .lock()
            .unwrap()
            .iter()
            .all(|(_, owner)| owner.upgrade().is_none())
    }
}

struct Guard {
    probes: Arc<Probes>,
    _owner: Arc<()>,
    kind: usize,
}

impl Guard {
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

struct Route {
    slot: usize,
    value: u64,
    _guard: Guard,
}

impl Route {
    fn new(slot: usize, probes: &Arc<Probes>) -> Self {
        Self {
            slot,
            value: 0,
            _guard: Guard::new(probes, STATE),
        }
    }
}

enum ChildAction {
    Update,
    Start(async_channel::Sender<EffectSender<ChildAction>>),
}

enum Action {
    Touch,
    Path(StackAction<usize, ChildAction>),
}

struct State {
    path: StackState<Route>,
    parent_calls: u64,
}

struct Parent(Guard);
struct Child(Guard);

impl Reducer for Parent {
    type State = State;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut State,
        _: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        // Keep the real reducer ownership probe part of the composed reducer.
        let _ = &self.0;
        state.parent_calls += 1;
        Effect::none()
    }
}

impl Reducer for Child {
    type State = Route;
    type Action = ChildAction;

    fn reduce(
        &mut self,
        state: &mut Route,
        action: &ChildAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<ChildAction> {
        self.0.probes.child_calls.fetch_add(1, Ordering::Relaxed);
        match action {
            ChildAction::Update => {
                state.value += 1;
                Effect::none()
            }
            ChildAction::Start(output) => {
                let output = output.clone();
                let capture = Guard::new(&self.0.probes, CAPTURE);
                Effect::run(move |sender| async move {
                    capture.probes.started.fetch_add(1, Ordering::Relaxed);
                    let _ = output.send(sender).await;
                    std::future::pending::<()>().await;
                    drop(capture);
                })
            }
        }
    }
}

struct Resident {
    scope: ScopedStore<Route, ChildAction>,
    reader: Observation<(usize, u64)>,
    output: EffectSender<ChildAction>,
    task: ActionTask,
}

impl Resident {
    async fn rejected(&mut self) -> Result<(), &'static str> {
        if !self
            .output
            .send(ChildAction::Update)
            .await
            .is_err_and(|error| error.is_cancelled())
            || !self
                .scope
                .read(|route| route.value)
                .await
                .is_err_and(|error| error.is_scope_ended())
            || self.reader.next().await.is_some()
        {
            return Err(INVALID);
        }
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Projection {
    rows: [(usize, u64); MAX_WIDTH],
    len: usize,
    parent_calls: u64,
    exact_ids: bool,
}

fn project(state: &State, ids: &[StackId]) -> Projection {
    let mut value = Projection {
        rows: [(0, 0); MAX_WIDTH],
        len: state.path.len(),
        parent_calls: state.parent_calls,
        exact_ids: true,
    };
    for (index, (id, route)) in state.path.iter().enumerate() {
        value.exact_ids &= ids.get(index) == Some(id);
        value.rows[index] = (route.slot, route.value);
    }
    value
}

struct Fixture {
    host: Option<Host>,
    store: Store<State, Action>,
    host_owner: Weak<()>,
    ids: Arc<Vec<StackId>>,
    probes: Arc<Probes>,
    active: Vec<Resident>,
    retired: u64,
    rebound: u64,
    #[cfg(test)]
    fault: Option<Fault>,
}

impl Fixture {
    fn new(config: Config) -> Result<Self, &'static str> {
        let probes = Arc::new(Probes::default());
        let mut path = StackState::new();
        let ids = Arc::new(
            (0..config.width)
                .map(|slot| path.push(Route::new(slot, &probes)))
                .collect(),
        );
        let initialise = Arc::clone(&probes);
        let reducer = stack(
            Parent(Guard::new(&probes, REDUCER)),
            StatePath::new(
                |state: &State| &state.path,
                |state: &mut State| &mut state.path,
            ),
            ActionPath::new(
                |action: &Action| match action {
                    Action::Path(path) => Some(path),
                    Action::Touch => None,
                },
                Action::Path,
            ),
            move |slot: &usize, _: &mut ReduceContext<'_>| Route::new(*slot, &initialise),
            Child(Guard::new(&probes, REDUCER)),
        );
        let (host, runtime, host_owner) = Host::new(config.native, config.budget)?;
        let store = Store::builder(
            State {
                path,
                parent_calls: 0,
            },
            reducer,
            runtime,
        )
        .mailbox_capacity(1)
        .build()
        .map_err(|_| INVALID)?;
        Ok(Self {
            host: Some(host),
            store,
            host_owner,
            ids,
            probes,
            active: Vec::new(),
            retired: 0,
            rebound: 0,
            #[cfg(test)]
            fault: None,
        })
    }

    async fn start(&mut self, slot: usize) -> Result<(), &'static str> {
        let scope = self
            .store
            .scope_stack(
                StatePath::new(
                    |state: &State| &state.path,
                    |state: &mut State| &mut state.path,
                ),
                ActionPath::new(
                    |action: &Action| match action {
                        Action::Path(path) => Some(path),
                        Action::Touch => None,
                    },
                    Action::Path,
                ),
                self.ids[slot].clone(),
            )
            .await
            .map_err(|_| INVALID)?;
        let mut reader = scope
            .observe(|route| (route.slot, route.value))
            .await
            .map_err(|_| INVALID)?;
        if reader.next().await != Some((slot, 0)) {
            return Err(INVALID);
        }
        let (capture, received) = async_channel::bounded(1);
        let task = scope
            .send(ChildAction::Start(capture))
            .await
            .map_err(|_| INVALID)?;
        let output = received.recv().await.map_err(|_| INVALID)?;
        self.active.push(Resident {
            scope,
            reader,
            output,
            task,
        });
        Ok(())
    }

    fn warmup(&mut self, config: Config) -> Result<(), &'static str> {
        let host = self.host.take().expect("fixture owns host");
        let result = host.run_ready_bounded(
            async {
                for slot in 0..config.width {
                    self.start(slot).await?;
                }
                Ok::<_, &'static str>(())
            },
            DEADLINE,
            config.budget,
        );
        self.host = Some(host);
        result?
    }

    async fn immediate(&mut self, action: Action) -> Result<(), &'static str> {
        let task = self.store.send(action).await.map_err(|_| INVALID)?;
        task.finish().await;
        if !task.is_finished() {
            return Err(INVALID);
        }
        Ok(())
    }

    async fn projection(&mut self) -> Result<Projection, &'static str> {
        let ids = Arc::clone(&self.ids);
        self.store
            .read(move |state| project(state, &ids))
            .await
            .map_err(|_| INVALID)
    }

    async fn cycle(&mut self, config: Config, iteration: u64) -> Result<(), &'static str> {
        let prefix = config.prefix();
        for (slot, resident) in self.active.iter_mut().enumerate() {
            resident
                .output
                .send(ChildAction::Update)
                .await
                .map_err(|_| INVALID)?;
            let value = if slot < prefix { iteration } else { 1 };
            if resident.reader.next().await != Some((slot, value)) {
                return Err(INVALID);
            }
        }
        let pop_index = prefix;
        #[cfg(test)]
        let pop_index = if self.fault == Some(Fault::OverPop) {
            0
        } else {
            pop_index
        };
        let action = self.ids.get(pop_index).map_or(Action::Touch, |id| {
            Action::Path(StackAction::PopFrom { id: id.clone() })
        });
        #[cfg(test)]
        let action = if self.fault == Some(Fault::MissingPop) {
            Action::Touch
        } else {
            action
        };
        self.immediate(action).await?;
        let projection = self.projection().await?;
        if projection.len != prefix
            || !projection.exact_ids
            || projection.rows[..prefix]
                .iter()
                .enumerate()
                .any(|(slot, row)| *row != (slot, iteration))
        {
            return Err(INVALID);
        }
        #[cfg(test)]
        if self.fault == Some(Fault::PrefixCancelled) {
            let first = self.active.first_mut().ok_or(INVALID)?;
            first.task.cancel();
            first.task.finish().await;
        }
        for (slot, resident) in self.active[..prefix].iter_mut().enumerate() {
            if resident.task.is_finished()
                || resident.task.is_cancelled()
                || resident
                    .scope
                    .read(|route| (route.slot, route.value))
                    .await
                    .map_err(|_| INVALID)?
                    != (slot, iteration)
            {
                return Err(INVALID);
            }
        }
        for resident in &mut self.active[prefix..] {
            resident.task.finish().await;
            if !resident.task.is_finished() {
                return Err(INVALID);
            }
            resident.rejected().await?;
            self.retired += 1;
        }
        let mut retired = self.active.split_off(prefix);

        if iteration < config.iterations {
            for slot in prefix..config.width {
                self.immediate(Action::Path(StackAction::Push {
                    id: self.ids[slot].clone(),
                    input: slot,
                }))
                .await?;
                self.start(slot).await?;
            }
            // Historical clients must remain revoked even while their IDs are live again.
            #[cfg(test)]
            if self.fault == Some(Fault::FreshOutputReplayed) {
                retired.first_mut().ok_or(INVALID)?.output =
                    self.active.get(prefix).ok_or(INVALID)?.output.clone();
            }
            for resident in &mut retired {
                resident.rejected().await?;
                self.rebound += 1;
            }
            let projection = self.projection().await?;
            if projection.len != config.width
                || !projection.exact_ids
                || projection.rows[prefix..config.width]
                    .iter()
                    .enumerate()
                    .any(|(offset, row)| *row != (prefix + offset, 0))
            {
                return Err(INVALID);
            }
        }
        Ok(())
    }

    fn validate(&mut self, config: Config) -> Result<Projection, &'static str> {
        let host = self.host.take().expect("fixture owns host");
        let result = host.run_ready_bounded(self.projection(), DEADLINE, config.budget);
        self.host = Some(host);
        let projection = result??;
        let created = config.created();
        let retired = config.iterations * (config.width - config.prefix()) as u64;
        let rebound = (config.iterations - 1) * (config.width - config.prefix()) as u64;
        let children = created + config.iterations * config.width as u64;
        if projection.len != config.prefix()
            || !projection.exact_ids
            || projection.parent_calls != children + config.iterations + rebound
            || self.probes.child_calls.load(Ordering::Relaxed) != children
            || self.probes.started.load(Ordering::Relaxed) != created
            || self.probes.count(STATE) != (created, retired)
            || self.probes.count(CAPTURE) != (created, retired)
            || self.probes.count(REDUCER) != (2, 0)
            || self.retired != retired
            || self.rebound != rebound
            || self.active.len() != config.prefix()
            || projection.rows[..projection.len]
                .iter()
                .enumerate()
                .any(|(slot, row)| *row != (slot, config.iterations))
        {
            return Err(INVALID);
        }
        Ok(projection)
    }

    // Also used after a failed oracle, so no admitted work survives negative controls.
    fn close(self, config: Config) -> Result<Outcome, &'static str> {
        let Self {
            host,
            store,
            host_owner,
            ids,
            probes,
            active,
            retired,
            rebound,
            #[cfg(test)]
                fault: _,
        } = self;
        let host = host.expect("fixture owns host");

        let state = host
            .run_ready_bounded(store.shutdown_into_state(), DEADLINE, config.budget)?
            .map_err(|_| INVALID)?;
        let projection = project(&state, &ids);
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
        drop(ids);
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
        Ok(Outcome {
            projection,
            retired,
            rebound,
            created: probes.count(STATE).0,
            children: probes.child_calls.load(Ordering::Relaxed),
        })
    }
}

/// Profiles the finite stack trace, excluding construction and final teardown.
#[inline(never)]
fn profile_stack_retirement_work(
    fixture: &mut Fixture,
    config: Config,
) -> Result<(), &'static str> {
    let host = fixture.host.take().expect("fixture owns host");
    let result = host.run_ready_bounded(
        async {
            for iteration in 1..=config.iterations {
                fixture.cycle(config, iteration).await?;
            }
            Ok::<_, &'static str>(())
        },
        DEADLINE,
        config.budget,
    );
    fixture.host = Some(host);
    result?
}

struct Outcome {
    projection: Projection,
    retired: u64,
    rebound: u64,
    created: u64,
    children: u64,
}

impl Outcome {
    fn json(&self, config: Config, allocations: &str) -> String {
        let mut rows = String::from("[");
        for (index, (slot, value)) in self.projection.rows[..self.projection.len]
            .iter()
            .enumerate()
        {
            if index != 0 {
                rows.push(',');
            }
            write!(rows, "[{slot},{value}]").unwrap();
        }
        rows.push(']');
        format!(
            "{{\"schema\":\"rca-stack-retirement-performance-profile-v1\",\"workload\":\"{}\",\"size\":{},\"iterations\":{},\"features\":{{\"deterministic\":{},\"allocations\":{}}},\"parent_reductions\":{},\"child_reductions\":{},\"retained_prefix\":{},\"exact_prefix_ids\":true,\"retired_tasks_joined\":{},\"retired_outputs_rejected\":{},\"retired_readers_ended\":{},\"retired_scopes_ended\":{},\"rebound_authorities_rejected\":{},\"shutdown_tasks_joined\":{},\"states_created\":{},\"states_dropped\":{},\"captures_created\":{},\"captures_dropped\":{},\"reducers_created\":2,\"reducers_dropped\":2,\"final_projection\":{},\"state_extracted\":true,\"owners_released\":true,\"host_stopped\":true,\"allocations\":{}}}",
            config.name,
            config.width,
            config.iterations,
            cfg!(feature = "deterministic"),
            cfg!(feature = "allocations"),
            self.projection.parent_calls,
            self.children,
            self.projection.len,
            self.retired,
            self.retired,
            self.retired,
            self.retired,
            self.rebound,
            self.projection.len,
            self.created,
            self.created,
            self.created,
            self.created,
            rows,
            allocations,
        )
    }
}

pub(crate) fn run(config: Config) -> Result<String, &'static str> {
    let mut measurement = Measurement::new(Some(if config.native {
        ExecutorCache::Native
    } else {
        ExecutorCache::Deterministic
    }));
    let mut fixture = Fixture::new(config)?;
    measurement.construction_finished();

    let checked = (|| {
        fixture.warmup(config)?;
        measurement.warmup_finished();
        profile_stack_retirement_work(&mut fixture, config)?;
        measurement.work_finished();
        let projection = fixture.validate(config)?;
        measurement.validation_finished();
        Ok::<_, &'static str>(projection)
    })();
    let outcome = fixture.close(config)?;
    if outcome.projection != checked? {
        return Err(INVALID);
    }
    let allocations = measurement
        .finish()?
        .map_or_else(|| "null".to_owned(), |record| record.json());
    Ok(outcome.json(config, &allocations))
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
    MissingPop,
    OverPop,
    PrefixCancelled,
    FreshOutputReplayed,
    RetainedCapture,
}

#[cfg(test)]
fn control(config: Config, fault: Fault) -> Result<(), &'static str> {
    let mut fixture = Fixture::new(config)?;
    fixture.fault = Some(fault);
    let probes = Arc::clone(&fixture.probes);
    let host_owner = fixture.host_owner.clone();
    let checked = (|| {
        fixture.warmup(config)?;
        profile_stack_retirement_work(&mut fixture, config)?;
        fixture.validate(config).map(|_| ())
    })();
    let retained = if fault == Fault::RetainedCapture {
        probes
            .owners
            .lock()
            .unwrap()
            .iter()
            .find_map(|(kind, owner)| {
                if *kind == CAPTURE {
                    owner.upgrade()
                } else {
                    None
                }
            })
    } else {
        None
    };
    let held_capture = retained.is_some();
    let closed = fixture.close(config);
    drop(retained);
    // Even deliberate failures must cancel and join all real admitted work.
    if !probes.released()
        || host_owner.upgrade().is_some()
        || [STATE, REDUCER, CAPTURE].into_iter().any(|kind| {
            let (created, dropped) = probes.count(kind);
            created != dropped
        })
        || (fault == Fault::RetainedCapture && !held_capture)
    {
        return Err("stack negative control did not release its actual owners");
    }
    let phase_matches = if fault == Fault::RetainedCapture {
        checked == Ok(()) && closed.as_ref().err() == Some(&INVALID)
    } else {
        checked == Err(INVALID) && closed.is_ok()
    };
    if !phase_matches {
        return Err("stack negative control failed in the wrong phase");
    }
    Err(INVALID)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn config(native: bool, width: &str) -> Config {
        let name = if native {
            "stack-retirement-native"
        } else {
            "stack-retirement-deterministic"
        };
        Config::parse([name, width, "2"].into_iter().map(str::to_owned)).unwrap()
    }

    #[test]
    fn stack_retirement_rejects_unbounded_or_malformed_input() {
        for args in [
            vec!["stack-retirement-native", "2", "1"],
            vec!["stack-retirement-native", "1024", "1"],
            vec!["stack-retirement-native", "0", "0"],
            vec!["stack-retirement-native", "16", "18446744073709551615"],
            vec!["stack-retirement-native", "-1", "1"],
            vec!["stack-retirement-native", "0", "1", "extra"],
            vec!["stack-retirement-native", "0"],
            vec!["stack-retirement-other", "0", "1"],
        ] {
            assert!(Config::parse(args.into_iter().map(str::to_owned)).is_err());
        }
        #[cfg(not(feature = "deterministic"))]
        assert!(
            Config::parse(
                ["stack-retirement-deterministic", "0", "1"]
                    .into_iter()
                    .map(str::to_owned)
            )
            .is_err()
        );
    }

    #[test]
    fn stack_retirement_oracle_rejects_faults_and_closes_actual_work() {
        for native in [true, false] {
            if !native && !cfg!(feature = "deterministic") {
                continue;
            }
            for fault in [
                Fault::MissingPop,
                Fault::OverPop,
                Fault::PrefixCancelled,
                Fault::FreshOutputReplayed,
                Fault::RetainedCapture,
            ] {
                assert_eq!(
                    control(config(native, "16"), fault),
                    Err(INVALID),
                    "{fault:?}"
                );
            }
        }
    }

    // Missing suffix cancellation, over-popping, or stale output admission must fail.
    #[cfg_attr(not(feature = "allocations"), test)]
    pub(crate) fn stack_retirement_preserves_prefix_and_rejects_retired_authority() {
        #[cfg(feature = "allocations")]
        if !crate::tests::in_child(
            "stack_retirement::tests::stack_retirement_preserves_prefix_and_rejects_retired_authority",
        ) {
            return;
        }
        for native in [true, false] {
            if !native && !cfg!(feature = "deterministic") {
                continue;
            }
            for (width, prefix, created, retired, rebound, parents, children) in [
                ("0", 0usize, 0, 0, 0, 2, 0),
                ("1", 0, 2, 2, 1, 7, 4),
                ("16", 8, 24, 16, 8, 66, 56),
                ("256", 128, 384, 256, 128, 1026, 896),
            ] {
                let value: serde_json::Value =
                    serde_json::from_str(&run(config(native, width)).unwrap()).unwrap();
                assert_eq!(value["retained_prefix"], prefix);
                assert_eq!(value["states_created"], created);
                assert_eq!(value["states_dropped"], created);
                assert_eq!(value["captures_created"], created);
                assert_eq!(value["captures_dropped"], created);
                assert_eq!(value["retired_tasks_joined"], retired);
                assert_eq!(value["retired_outputs_rejected"], retired);
                assert_eq!(value["retired_readers_ended"], retired);
                assert_eq!(value["retired_scopes_ended"], retired);
                assert_eq!(value["rebound_authorities_rejected"], rebound);
                assert_eq!(value["parent_reductions"], parents);
                assert_eq!(value["child_reductions"], children);
                assert_eq!(value["owners_released"], true);
                assert_eq!(value["host_stopped"], true);
                let rows = value["final_projection"].as_array().unwrap();
                assert_eq!(rows.len(), prefix);
                for (index, row) in rows.iter().enumerate() {
                    assert_eq!(row, &serde_json::json!([index, 2]));
                }
            }
        }
    }
}
