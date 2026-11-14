//! Executes finite effect shapes with separate work, state and ownership checks.
//!
//! The caller owns allocation phase boundaries. Setup allocates the identity buffer;
//! useful work reuses it, and teardown checks the state again after shutdown.

use std::sync::{Arc, Weak};
#[cfg(test)]
use std::time::Duration;

use crate::host::Host;
use rca_core::{Effect, ReduceContext, Reducer, Store};
#[cfg(all(test, feature = "deterministic"))]
use rca_test::DeterministicRuntime;
#[cfg(test)]
use rca_tokio::TokioRuntime;

#[path = "shapes.rs"]
mod shapes;
pub(crate) use shapes::WorkFacts;
use shapes::{Mode, Probes};

/// Validated dimensions, expected work and a finite deterministic drive budget.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Config {
    pub name: &'static str,
    pub size: u64,
    pub iterations: u64,
    /// Root actions plus all responses, derived before any reducer runs.
    pub reductions: u64,
    /// Sum of the complete consecutive action-value sequence.
    pub checksum: u64,
    pub step_budget: usize,
    pub native: bool,
    mode: Mode,
    /// Shape-dependent response count, which can differ from the requested size.
    responses_per_root: u64,
}

impl Config {
    /// Rejects unavailable hosts, invalid dimensions and overflowing work or checksum counts.
    pub(crate) fn parse(mut args: impl Iterator<Item = String>) -> Result<Self, &'static str> {
        let workload = args.next().ok_or("expected WORKLOAD SIZE ITERATIONS")?;
        let (prefix, native) = workload
            .strip_suffix("-native")
            .map(|prefix| (prefix, true))
            .or_else(|| {
                workload
                    .strip_suffix("-deterministic")
                    .map(|prefix| (prefix, false))
            })
            .ok_or("unknown workload")?;
        let mode = Mode::parse(prefix).ok_or("unknown workload")?;
        let name = mode.name(native);
        if !native && !cfg!(feature = "deterministic") {
            return Err("deterministic workloads require the deterministic feature");
        }
        let size = integer(args.next())?;
        let iterations = integer(args.next())?;
        if args.next().is_some()
            || iterations == 0
            || (mode == Mode::Store && size != 0)
            || (mode == Mode::Send && size == 0)
        {
            return Err("require positive ITERATIONS, SIZE=0 for Store, SIZE>0 for effects");
        }
        // These qualification modes are deliberately finite, not an arbitrary
        // effect engine; bound recursion and identity bookkeeping at the input.
        if mode.identities()
            && (size > 256 || (matches!(mode, Mode::Map | Mode::Cancellation) && size > 16))
        {
            return Err("qualification SIZE exceeds width 256 or depth 16");
        }
        let responses_per_root = mode.responses(size);
        let reductions = responses_per_root
            .checked_add(1)
            .and_then(|n| n.checked_mul(iterations))
            .ok_or("useful-work count overflow")?;
        let successor = reductions.checked_add(1).ok_or("checksum overflow")?;
        let (a, b) = if reductions.is_multiple_of(2) {
            (reductions / 2, successor)
        } else {
            (reductions, successor / 2)
        };
        let checksum = a.checked_mul(b).ok_or("checksum overflow")?;
        // Conservative per-run bound for the actor, causal tasks and client polls.
        let step_budget = usize::try_from(reductions)
            .ok()
            .and_then(|n| n.checked_mul(16))
            .and_then(|n| n.checked_add(1024))
            .ok_or("deterministic poll-budget overflow")?;
        Ok(Self {
            name,
            size,
            iterations,
            reductions,
            checksum,
            step_budget,
            native,
            mode,
            responses_per_root,
        })
    }
}

/// Parses an unsigned decimal argument without accepting signs or whitespace.
pub(crate) fn integer(value: Option<String>) -> Result<u64, &'static str> {
    let value = value.ok_or("expected WORKLOAD SIZE ITERATIONS")?;
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err("SIZE and ITERATIONS must be unsigned decimal integers");
    }
    value.parse().map_err(|_| "integer overflow")
}

/// Final independent facts, retained after state extraction and host destruction.
#[derive(Clone, Copy, Debug)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent validation facts are deliberately corruptible controls"
)]
pub(crate) struct Outcome {
    pub roots: u64,
    pub responses: u64,
    pub reductions: u64,
    pub causal_completed: u64,
    pub final_state: u64,
    pub checksum: u64,
    pub reducer_dropped: bool,
    pub host_task_dropped: bool,
    pub owners_released: bool,
    pub state_extracted: bool,
    pub host_stopped: bool,
    pub facts: WorkFacts,
}

impl Outcome {
    /// Rejects missing work, incorrect shape facts or any retained owner.
    pub(crate) fn validate(self, config: Config) -> Result<(), &'static str> {
        if self.roots != config.iterations
            || self.responses != config.reductions - config.iterations
            || self.reductions != config.reductions
            || self.causal_completed != config.iterations
            || self.final_state != config.reductions
            || self.checksum != config.checksum
            || self.facts != WorkFacts::expected(config)
        {
            return Err("missing useful work or invalid final outcome");
        }
        if !self.reducer_dropped
            || !self.host_task_dropped
            || !self.owners_released
            || !self.state_extracted
            || !self.host_stopped
        {
            return Err("owned resources or host were not released");
        }
        Ok(())
    }
}

/// Accumulates committed actions without assuming concurrent branches arrive in order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct State {
    roots: u64,
    responses: u64,
    reductions: u64,
    checksum: u64,
    ordered: bool,
    /// Reused per-root membership bitmap catches duplicate values with equal sums.
    seen: Vec<bool>,
    identities_valid: bool,
    mapped_valid: bool,
    /// Translates response values into the current root's membership bitmap.
    root_base: u64,
}

impl State {
    /// Records actual reduction, keeping order, membership and map-depth checks separate.
    fn record(&mut self, action: &Action, config: Config) {
        self.reductions += 1;
        self.checksum += action.value;
        if config.mode.ordered() {
            self.ordered &= action.value == self.reductions;
        }
        if action.root {
            // Check the previous root before reusing the bounded identity buffer.
            if self.roots != 0 {
                self.identities_valid &= self.seen.iter().all(|seen| *seen);
            }
            self.seen.fill(false);
            self.root_base = action.value;
            self.roots += 1;
        } else {
            self.responses += 1;
            if config.mode.identities() {
                let index = action
                    .value
                    .checked_sub(self.root_base + 1)
                    .and_then(|index| usize::try_from(index).ok());
                if let Some(seen) = index.and_then(|index| self.seen.get_mut(index)) {
                    self.identities_valid &= !*seen;
                    *seen = true;
                } else {
                    self.identities_valid = false;
                }
            }
            if config.mode == Mode::Map {
                self.mapped_valid &= action.mapped == config.size;
            }
        }
    }
}

/// Carries a sequence value, remaining chain work and the actual mapper depth.
#[derive(Debug)]
struct Action {
    value: u64,
    remaining: u64,
    root: bool,
    mapped: u64,
}

/// Runs the selected effect shape while retaining one observable reducer owner.
struct Counter {
    owner: Arc<()>,
    config: Config,
    probes: Arc<Probes>,
}

impl Reducer for Counter {
    type State = State;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut State,
        action: &Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        state.record(action, self.config);
        shapes::effect(self.config, action, context, &self.probes, &self.owner)
    }
}

/// Owns the host and store, with weak probes that cannot keep either alive.
pub(crate) struct Adapter {
    host: Host,
    store: Store<State, Action>,
    reducer_owner: Weak<()>,
    host_owner: Weak<()>,
    roots: u64,
    causal_completed: u64,
    probes: Arc<Probes>,
}

impl Adapter {
    /// Constructs the host, owner probes and reusable state before useful work starts.
    pub(crate) fn new(config: Config) -> Result<Self, &'static str> {
        let reducer_owner = Arc::new(());
        let reducer_weak = Arc::downgrade(&reducer_owner);
        let probes = Arc::new(Probes::default());
        let (host, runtime, host_weak) = Host::new(config.native, config.step_budget)?;
        let store = Store::builder(
            State {
                ordered: true,
                identities_valid: true,
                mapped_valid: true,
                seen: if config.mode.identities() {
                    vec![
                        false;
                        usize::try_from(config.responses_per_root)
                            .map_err(|_| "identity width overflow")?
                    ]
                } else {
                    Vec::new()
                },
                ..State::default()
            },
            Counter {
                owner: reducer_owner,
                config,
                probes: Arc::clone(&probes),
            },
            runtime,
        )
        .mailbox_capacity(1)
        .build()
        .map_err(|_| "Store construction failed")?;
        Ok(Self {
            host,
            store,
            reducer_owner: reducer_weak,
            host_owner: host_weak,
            roots: 0,
            causal_completed: 0,
            probes,
        })
    }

    /// Checks committed work and returns a copy for the shutdown consistency check.
    pub(crate) fn validate_state(&self, config: Config) -> Result<State, &'static str> {
        let state = self
            .host
            .run(self.store.read(Clone::clone))?
            .map_err(|_| "Store state read failed")?;
        if state.roots != config.iterations
            || state.responses != config.reductions - config.iterations
            || state.reductions != config.reductions
            || state.checksum != config.checksum
            || !state.ordered
            || !state.identities_valid
            || !state.mapped_valid
            || !state.seen.iter().all(|seen| *seen)
            || self.roots != config.iterations
            || self.causal_completed != config.iterations
        {
            return Err("useful work did not commit in exact causal order");
        }
        Ok(state)
    }

    /// Extracts unchanged state, then checks reducer and host ownership after destruction.
    pub(crate) fn teardown(self, checked: State) -> Result<Outcome, &'static str> {
        let Self {
            host,
            store,
            reducer_owner,
            host_owner,
            roots,
            causal_completed,
            probes,
        } = self;
        let final_state = host
            .run(store.shutdown_into_state())?
            .map_err(|_| "shutdown state extraction failed")?;
        if final_state != checked {
            return Err("state changed during teardown");
        }
        drop(checked);
        let reducer_dropped = reducer_owner.upgrade().is_none();
        drop(host);
        let host_task_dropped = host_owner.upgrade().is_none();
        Ok(Outcome {
            roots,
            responses: final_state.responses,
            reductions: final_state.reductions,
            causal_completed,
            final_state: final_state.reductions,
            checksum: final_state.checksum,
            reducer_dropped,
            host_task_dropped,
            owners_released: reducer_dropped && host_task_dropped,
            state_extracted: true,
            host_stopped: true,
            facts: probes.facts(),
        })
    }
}

/// Defines the symbolised useful-work boundary, excluding setup and reporting.
#[inline(never)]
pub(crate) fn profile_useful_work(
    adapter: &mut Adapter,
    config: Config,
) -> Result<(), &'static str> {
    let mut completed = 0;
    adapter.host.run(async {
        for root in 0..config.iterations {
            let task = adapter
                .store
                .send(Action {
                    value: root * (config.responses_per_root + 1) + 1,
                    remaining: config.responses_per_root,
                    root: true,
                    mapped: 0,
                })
                .await
                .map_err(|_| "root action failed to commit")?;
            adapter.roots += 1;
            #[cfg(feature = "deterministic")]
            if config.mode.timer()
                && let Host::Deterministic(host) = &adapter.host
            {
                host.advance(shapes::TIMER_DELAY)
                    .map_err(|_| "deterministic timer advance budget exhausted")?;
            }
            task.finish().await;
            if !task.is_finished() || task.is_cancelled() {
                return Err("causal task incomplete or cancelled");
            }
            completed += 1;
        }
        Ok(())
    })??;
    adapter.causal_completed = completed;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retained_reducer_owner_is_rejected_after_real_shutdown() {
        let config =
            Config::parse(["effects-native", "2", "1"].into_iter().map(str::to_owned)).unwrap();
        let mut adapter = Adapter::new(config).unwrap();
        let retained = adapter.reducer_owner.upgrade().unwrap();
        profile_useful_work(&mut adapter, config).unwrap();
        let state = adapter.validate_state(config).unwrap();
        let outcome = adapter.teardown(state).unwrap();
        assert!(!outcome.reducer_dropped);
        assert!(!outcome.owners_released);
        assert!(outcome.validate(config).is_err());
        drop(retained);
    }
}

#[cfg(test)]
#[path = "effect_controls.rs"]
mod effect_controls;
