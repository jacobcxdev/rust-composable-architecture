//! Profiles finite Store schedules without worker threads or a test scheduler.
//!
//! Run `controlled_cost SCENARIO ITERATIONS EXTRA`, with 1..=256 iterations and
//! EXTRA equal to 0 or 64. Each iteration constructs and physically destroys one
//! effect-free Store with 64 KiB of owned state. Two logical producers follow
//! explicit poll scripts; they do not model native simultaneous contention.
//! `abortive-unpolled` omits the initial actor poll, then polls the actor to
//! completion after final-owner drop. It never abandons an unpolled actor future.
//!
//! One non-inlined boundary contains the entire batch, including construction,
//! request/reply polls, validation and physical teardown. Parsing and reporting
//! remain outside. Extra opaque arithmetic has its own non-inlined boundary,
//! changes no Store work, and is a function-attribution sensitivity control.
//! This executable makes no OS-latency or production-speed claim.

/// Reuses the common manual runtime without starting its worker threads.
#[path = "../../../../rca-core/tests/support/mod.rs"]
#[expect(dead_code, reason = "controlled fixtures never start runtime threads")]
mod support;

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::task::{Context, Poll, Waker};

use rca_core::{ActionTask, Effect, ReduceContext, Reducer, RuntimeFuture, Store};

/// Matches the backing allocation in the native shutdown qualification rows.
const PAYLOAD_BYTES: usize = 65_536;
/// Identifies this finite command-line and accounting contract.
const SCHEMA: &str = "rca-controlled-cost-v1";

/// Selects one finite request and actor polling script.
#[derive(Clone, Copy)]
enum Scenario {
    /// Admits both logical producers before the actor runs.
    Empty(bool),
    /// Makes the second logical producer wait for capacity one.
    Full(bool),
    /// Gracefully stops an already parked actor.
    GracefulIdle,
    /// Commits two queued actions before the terminal command.
    GracefulQueued,
    /// Starts the actor only after its final Store owner drops.
    AbortiveUnpolled,
    /// Drops the final owner of an already parked actor.
    AbortiveIdle,
    /// Completes an ordered query before dropping the final owner.
    AbortiveQuery,
}

/// Holds dimensions checked before entering the profiling boundary.
#[derive(Clone, Copy)]
struct Config {
    /// Preserves the selected public row label.
    name: &'static str,
    /// Fixes all request and actor turns.
    scenario: Scenario,
    /// Counts complete independent Store lifecycles.
    iterations: usize,
    /// Selects zero or 64 arithmetic steps per lifecycle.
    extra: usize,
}

impl Config {
    /// Rejects unavailable graphs and anything outside the finite matrix.
    fn parse(mut args: impl Iterator<Item = String>) -> Result<Self, &'static str> {
        if cfg!(any(
            feature = "deterministic",
            feature = "diagnostics",
            feature = "allocations",
            feature = "persistence",
            feature = "local",
            feature = "task-fold"
        )) {
            return Err("controlled costs require the default production feature graph");
        }
        let (name, scenario) = match args.next().as_deref() {
            Some("admission-empty-ab") => ("admission-empty-ab", Scenario::Empty(false)),
            Some("admission-empty-ba") => ("admission-empty-ba", Scenario::Empty(true)),
            Some("admission-full-ab") => ("admission-full-ab", Scenario::Full(false)),
            Some("admission-full-ba") => ("admission-full-ba", Scenario::Full(true)),
            Some("graceful-idle") => ("graceful-idle", Scenario::GracefulIdle),
            Some("graceful-queued") => ("graceful-queued", Scenario::GracefulQueued),
            Some("abortive-unpolled") => ("abortive-unpolled", Scenario::AbortiveUnpolled),
            Some("abortive-idle") => ("abortive-idle", Scenario::AbortiveIdle),
            Some("abortive-query") => ("abortive-query", Scenario::AbortiveQuery),
            _ => return Err("unknown controlled Store scenario"),
        };
        let iterations = integer(args.next())?;
        let extra = integer(args.next())?;
        if !(1..=256).contains(&iterations) || !matches!(extra, 0 | 64) || args.next().is_some() {
            return Err("require ITERATIONS=1..256 and EXTRA=0 or 64");
        }
        Ok(Self {
            name,
            scenario,
            iterations,
            extra,
        })
    }
}

/// Parses unsigned dimensions without accepting signs, whitespace or overflow.
fn integer(value: Option<String>) -> Result<usize, &'static str> {
    let value = value.ok_or("expected SCENARIO ITERATIONS EXTRA")?;
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("dimensions must be unsigned decimal integers");
    }
    value.parse().map_err(|_| "dimension overflow")
}

/// Retains final scalar evidence independently of actor-owned values.
#[derive(Default)]
struct Probes {
    /// Counts actual State destructors.
    state_drops: AtomicUsize,
    /// Counts actual reducer destructors.
    reducer_drops: AtomicUsize,
    /// Counts completed physical actor-future destruction.
    actor_drops: AtomicUsize,
    /// Receives the final number of committed actions.
    commits: AtomicUsize,
    /// Receives the final sum of action identities.
    checksum: AtomicUsize,
    /// Receives the final decimal order encoding, 12 or 21.
    order_checksum: AtomicUsize,
    /// Records the unchanged allocation's length and endpoint sentinels.
    payload_valid: AtomicBool,
}

/// Owns the native workload's backing allocation and a tiny ordered oracle.
struct State {
    /// Drops before the final ownership witness expires.
    payload: Box<[u8]>,
    /// Counts action entries, independent of their receipts.
    commits: usize,
    /// Adds each committed identity exactly once.
    checksum: usize,
    /// Records both identities in their actual commit order.
    order: usize,
    /// Publishes scalar facts during destruction, without retaining State.
    probes: Arc<Probes>,
    /// Expires only after the payload and other fields are destroyed.
    _owner: Arc<()>,
}

impl Drop for State {
    fn drop(&mut self) {
        self.probes.commits.store(self.commits, Ordering::Relaxed);
        self.probes.checksum.store(self.checksum, Ordering::Relaxed);
        self.probes
            .order_checksum
            .store(self.order, Ordering::Relaxed);
        self.probes.payload_valid.store(
            self.payload.len() == PAYLOAD_BYTES
                && self.payload.first() == Some(&90)
                && self.payload.last() == Some(&90),
            Ordering::Relaxed,
        );
        self.probes.state_drops.fetch_add(1, Ordering::Relaxed);
    }
}

/// Performs only the two fixed identity commits, without launching effects.
struct Counter {
    /// Publishes physical reducer destruction independently of State.
    probes: Arc<Probes>,
    /// Expires only after the reducer's fields are destroyed.
    _owner: Arc<()>,
}

impl Reducer for Counter {
    type State = State;
    type Action = usize;

    fn reduce(
        &mut self,
        state: &mut State,
        action: &usize,
        _: &mut ReduceContext<'_>,
    ) -> Effect<usize> {
        assert!(
            matches!(action, 1 | 2),
            "only literal identities are admitted"
        );
        state.commits += 1;
        state.checksum += action;
        state.order = state.order * 10 + action;
        Effect::none()
    }
}

impl Drop for Counter {
    fn drop(&mut self) {
        self.probes.reducer_drops.fetch_add(1, Ordering::Relaxed);
    }
}

/// Publishes destruction only after the preceding actor future field drops.
struct ActorDrop(Arc<Probes>);

impl Drop for ActorDrop {
    fn drop(&mut self) {
        self.0.actor_drops.fetch_add(1, Ordering::Relaxed);
    }
}

/// Retains the actor and a final field witnessing its completed destruction.
struct Actor {
    /// Owns the original runtime future, with no extra boxed wrapper.
    future: RuntimeFuture,
    /// Field order ensures all future captures drop before this report.
    _final_drop: ActorDrop,
}

/// Counts prescribed actor and public client polls, not wake notifications.
#[derive(Default)]
struct Polls {
    /// Counts actual actor-future polls.
    actor: usize,
    /// Counts actual request, causal-finish and closure-wait polls.
    client: usize,
}

/// Checks one exact actor turn without driving any other future.
fn actor_turn(actor: &mut Actor, polls: &mut Polls, ready: bool) -> Result<(), &'static str> {
    polls.actor += 1;
    let result = actor
        .future
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()));
    if result.is_ready() == ready {
        Ok(())
    } else {
        Err("actor returned an unexpected poll result")
    }
}

/// Requires one prescribed pending client poll without spinning or waiting.
fn client_pending<F: Future>(future: Pin<&mut F>, polls: &mut Polls) -> Result<(), &'static str> {
    polls.client += 1;
    if future
        .poll(&mut Context::from_waker(Waker::noop()))
        .is_pending()
    {
        Ok(())
    } else {
        Err("client completed before its prescribed actor turn")
    }
}

/// Requires one ready client poll after its prescribed actor turn.
fn client_ready<F: Future>(
    future: Pin<&mut F>,
    polls: &mut Polls,
) -> Result<F::Output, &'static str> {
    polls.client += 1;
    match future.poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(value) => Ok(value),
        Poll::Pending => Err("client remained pending after its prescribed actor turn"),
    }
}

/// Checks that an effect-free action's entire causal work really completed.
fn finish_action(task: ActionTask, polls: &mut Polls) -> Result<(), &'static str> {
    let completed = {
        let mut finish = std::pin::pin!(task.finish());
        client_ready(finish.as_mut(), polls)?;
        task.is_finished() && !task.is_cancelled()
    };
    if !completed {
        return Err("effect-free causal task was incomplete or cancelled");
    }
    drop(task);
    Ok(())
}

/// Exercises both logical senders at their fixed admission and commit positions.
fn admissions(
    store: &Store<State, usize>,
    actor: &mut Actor,
    polls: &mut Polls,
    reverse: bool,
    full: bool,
) -> Result<(), &'static str> {
    let (first, second) = if reverse { (2, 1) } else { (1, 2) };
    let mut first = std::pin::pin!(store.send(first));
    let mut second = std::pin::pin!(store.send(second));
    client_pending(first.as_mut(), polls)?;
    client_pending(second.as_mut(), polls)?;
    // Both senders are pending, but at capacity one only the first is admitted.
    actor_turn(actor, polls, false)?;
    let first = client_ready(first.as_mut(), polls)?.map_err(|_| "first action did not commit")?;
    if full {
        client_pending(second.as_mut(), polls)?;
        actor_turn(actor, polls, false)?;
    }
    let second =
        client_ready(second.as_mut(), polls)?.map_err(|_| "second action did not commit")?;
    finish_action(first, polls)?;
    finish_action(second, polls)
}

/// Completes idle graceful shutdown, including the public terminal wait.
fn graceful(
    store: &Store<State, usize>,
    actor: &mut Actor,
    polls: &mut Polls,
) -> Result<(), &'static str> {
    let mut shutdown = std::pin::pin!(store.shutdown());
    let mut closed = std::pin::pin!(store.closed());
    client_pending(shutdown.as_mut(), polls)?;
    actor_turn(actor, polls, true)?;
    client_ready(shutdown.as_mut(), polls)?.map_err(|_| "graceful shutdown was rejected")?;
    client_ready(closed.as_mut(), polls)
}

/// Keeps two admitted receipts pending ahead of the graceful terminal command.
fn graceful_queued(
    store: &Store<State, usize>,
    actor: &mut Actor,
    polls: &mut Polls,
) -> Result<(), &'static str> {
    let first = store
        .try_send(1)
        .map_err(|_| "first queued action did not fit")?;
    let second = store
        .try_send(2)
        .map_err(|_| "second queued action did not fit")?;
    let mut first = std::pin::pin!(first.finish());
    let mut second = std::pin::pin!(second.finish());
    client_pending(first.as_mut(), polls)?;
    client_pending(second.as_mut(), polls)?;
    let mut shutdown = std::pin::pin!(store.shutdown());
    let mut closed = std::pin::pin!(store.closed());
    client_pending(shutdown.as_mut(), polls)?;
    actor_turn(actor, polls, true)?;
    finish_action(
        client_ready(first.as_mut(), polls)?.map_err(|_| "first queued action was discarded")?,
        polls,
    )?;
    finish_action(
        client_ready(second.as_mut(), polls)?.map_err(|_| "second queued action was discarded")?,
        polls,
    )?;
    client_ready(shutdown.as_mut(), polls)?.map_err(|_| "queued graceful shutdown was rejected")?;
    client_ready(closed.as_mut(), polls)
}

/// Records final facts only after one complete lifecycle has released its owners.
#[derive(Clone, Copy, Default)]
struct Facts {
    /// Counts every actor poll in the batch.
    actor_polls: usize,
    /// Counts every client poll in the batch.
    client_polls: usize,
    /// Counts committed action entries.
    commits: usize,
    /// Sums the action identities from actual State destruction.
    checksum: usize,
    /// Sums each fixture's actual commit-order encoding.
    order_checksum: usize,
    /// Counts physically destroyed State values.
    state_drops: usize,
    /// Counts physically destroyed reducers.
    reducer_drops: usize,
    /// Counts physically destroyed actor futures.
    actor_drops: usize,
    /// Counts actual runtime task admissions.
    spawns: usize,
    /// Sums separately checked opaque arithmetic, never Store actions.
    sensitivity_checksum: usize,
}

impl Facts {
    /// Accumulates bounded scalar facts after each independently checked teardown.
    fn append(&mut self, other: Self) {
        self.actor_polls += other.actor_polls;
        self.client_polls += other.client_polls;
        self.commits += other.commits;
        self.checksum += other.checksum;
        self.order_checksum += other.order_checksum;
        self.state_drops += other.state_drops;
        self.reducer_drops += other.reducer_drops;
        self.actor_drops += other.actor_drops;
        self.spawns += other.spawns;
        self.sensitivity_checksum += other.sensitivity_checksum;
    }

    /// Formats after the measured boundary, without further actor work.
    fn json(&self, config: Config) -> String {
        format!(
            "{{\"schema\":\"{SCHEMA}\",\"runtime\":\"manual\",\"boundary\":\"profile_controlled_work\",\"scenario\":\"{}\",\"iterations\":{},\"sensitivity\":{},\"payload_bytes\":{PAYLOAD_BYTES},\"actor_polls\":{},\"client_polls\":{},\"commits\":{},\"checksum\":{},\"order_checksum\":{},\"state_drops\":{},\"reducer_drops\":{},\"actor_drops\":{},\"spawns\":{},\"sensitivity_checksum\":{},\"driver_empty\":true,\"owners_released\":true,\"runtime_abandonments\":0}}",
            config.name,
            config.iterations,
            config.extra,
            self.actor_polls,
            self.client_polls,
            self.commits,
            self.checksum,
            self.order_checksum,
            self.state_drops,
            self.reducer_drops,
            self.actor_drops,
            self.spawns,
            self.sensitivity_checksum,
        )
    }
}

/// Rejects retained State or reducer ownership after all actor fields are gone.
fn released(state: &Weak<()>, reducer: &Weak<()>) -> bool {
    state.upgrade().is_none() && reducer.upgrade().is_none()
}

/// Constructs, executes and physically destroys exactly one effect-free Store.
#[expect(
    clippy::too_many_lines,
    reason = "one ordered script keeps construction and physical teardown in the same ownership scope"
)]
fn cycle(scenario: Scenario) -> Result<Facts, &'static str> {
    let runtime = support::ManualRuntime::new();
    let probes = Arc::new(Probes::default());
    let state_owner = Arc::new(());
    let reducer_owner = Arc::new(());
    let state_weak = Arc::downgrade(&state_owner);
    let reducer_weak = Arc::downgrade(&reducer_owner);
    let store = Store::builder(
        State {
            payload: vec![90; PAYLOAD_BYTES].into_boxed_slice(),
            commits: 0,
            checksum: 0,
            order: 0,
            probes: Arc::clone(&probes),
            _owner: state_owner,
        },
        Counter {
            probes: Arc::clone(&probes),
            _owner: reducer_owner,
        },
        runtime.runtime(),
    )
    .mailbox_capacity(if matches!(scenario, Scenario::Full(_)) {
        1
    } else {
        64
    })
    .build()
    .map_err(|_| "controlled Store construction failed")?;
    let mut actor = Actor {
        future: runtime.take_only_queued_task(),
        _final_drop: ActorDrop(Arc::clone(&probes)),
    };
    let mut polls = Polls::default();
    if !matches!(scenario, Scenario::AbortiveUnpolled) {
        actor_turn(&mut actor, &mut polls, false)?;
    }
    match scenario {
        Scenario::Empty(reverse) | Scenario::Full(reverse) => {
            admissions(
                &store,
                &mut actor,
                &mut polls,
                reverse,
                matches!(scenario, Scenario::Full(_)),
            )?;
            graceful(&store, &mut actor, &mut polls)?;
            drop(store);
        }
        Scenario::GracefulIdle => {
            graceful(&store, &mut actor, &mut polls)?;
            drop(store);
        }
        Scenario::GracefulQueued => {
            graceful_queued(&store, &mut actor, &mut polls)?;
            drop(store);
        }
        Scenario::AbortiveUnpolled | Scenario::AbortiveIdle | Scenario::AbortiveQuery => {
            if matches!(scenario, Scenario::AbortiveQuery) {
                let mut query = std::pin::pin!(store.read(|state| (
                    state.commits,
                    state.checksum,
                    state.order
                )));
                client_pending(query.as_mut(), &mut polls)?;
                actor_turn(&mut actor, &mut polls, false)?;
                let state = client_ready(query.as_mut(), &mut polls)?
                    .map_err(|_| "ordered query failed")?;
                if state != (0, 0, 0) {
                    return Err("ordered query observed unexpected state");
                }
            }
            let mut closed = std::pin::pin!(store.closed());
            drop(store);
            actor_turn(&mut actor, &mut polls, true)?;
            client_ready(closed.as_mut(), &mut polls)?;
        }
    }
    // Ready and closed are not physical destruction. Drop the retained future now.
    drop(actor);
    let facts = Facts {
        actor_polls: polls.actor,
        client_polls: polls.client,
        commits: probes.commits.load(Ordering::Relaxed),
        checksum: probes.checksum.load(Ordering::Relaxed),
        order_checksum: probes.order_checksum.load(Ordering::Relaxed),
        state_drops: probes.state_drops.load(Ordering::Relaxed),
        reducer_drops: probes.reducer_drops.load(Ordering::Relaxed),
        actor_drops: probes.actor_drops.load(Ordering::Relaxed),
        spawns: runtime.spawn_count(),
        sensitivity_checksum: 0,
    };
    let (commits, checksum, order) = match scenario {
        Scenario::Empty(false) | Scenario::Full(false) | Scenario::GracefulQueued => (2, 3, 12),
        Scenario::Empty(true) | Scenario::Full(true) => (2, 3, 21),
        _ => (0, 0, 0),
    };
    if (facts.commits, facts.checksum, facts.order_checksum) != (commits, checksum, order)
        || (
            facts.state_drops,
            facts.reducer_drops,
            facts.actor_drops,
            facts.spawns,
        ) != (1, 1, 1, 1)
        || !probes.payload_valid.load(Ordering::Relaxed)
        || !released(&state_weak, &reducer_weak)
        || Arc::strong_count(&probes) != 1
        || !runtime.take_queued_tasks().is_empty()
    {
        return Err("controlled work or physical owner release did not match the fixture");
    }
    drop(runtime);
    Ok(facts)
}

/// Adds attributable arithmetic while preserving every useful Store operation.
#[inline(never)]
fn profile_extra_work(steps: usize) -> usize {
    let mut checksum = std::hint::black_box(0usize);
    for value in 1..=std::hint::black_box(steps) {
        checksum = std::hint::black_box(checksum)
            .checked_add(std::hint::black_box(value))
            .expect("at most 64 small positive integers fit in usize");
    }
    std::hint::black_box(checksum)
}

/// Defines the single measured boundary around complete independent lifecycles.
#[inline(never)]
fn profile_controlled_work(config: Config) -> Result<Facts, &'static str> {
    let mut total = Facts::default();
    for _ in 0..config.iterations {
        let mut facts = cycle(config.scenario)?;
        facts.sensitivity_checksum = profile_extra_work(config.extra);
        if facts.sensitivity_checksum != if config.extra == 64 { 2080 } else { 0 } {
            return Err("opaque arithmetic checksum differed from the fixed control");
        }
        total.append(facts);
    }
    Ok(total)
}

/// Enters the work boundary exactly once after input and feature validation.
fn run() -> Result<String, &'static str> {
    let config = Config::parse(std::env::args().skip(1))?;
    let facts = profile_controlled_work(config)?;
    Ok(facts.json(config))
}

fn main() {
    match run() {
        Ok(record) => println!("{record}"),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}
