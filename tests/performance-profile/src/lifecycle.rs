//! Exact-work native lifecycle delivery and physical teardown witnesses.
//!
//! Each cycle checks 19 reductions, ordered diagnostics and final owner release.
//! The repeated mode repeats the complete cycle, including construction and teardown.
//! Native deadlines bound individual waits; they are not performance thresholds.
//! Deterministic builds cannot qualify this native-only feature comparison.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

#[cfg(feature = "diagnostics")]
use rca_core::diagnostics::{CancellationCause, LifecycleEvent, LifecycleKind};
use rca_core::{Effect, Feature, FeatureContext, MountIdentity, OptionalMount, Store};

use crate::host::Host;
use crate::measurement::{ExecutorCache, Measurement};

const DEADLINE: Duration = Duration::from_secs(10);
const INVALID: &str = "lifecycle work or ownership invalid";
// One preselected sensitivity step: repeat complete lifecycles, not just Noops.
// The successor counter packet fixes this count before any native capture.
const REPEATED_CYCLES: usize = 1024;

#[derive(Clone, Copy)]
pub(crate) struct Config {
    name: &'static str,
    sink: bool,
    repeated: bool,
}

impl Config {
    pub(crate) fn parse(mut args: impl Iterator<Item = String>) -> Result<Self, &'static str> {
        let (name, sink, repeated) = match args.next().as_deref() {
            Some("lifecycle-none-native") => ("lifecycle-none-native", false, false),
            Some("lifecycle-fixed-native") => ("lifecycle-fixed-native", true, false),
            Some("lifecycle-repeated-none-native") => {
                ("lifecycle-repeated-none-native", false, true)
            }
            Some("lifecycle-repeated-fixed-native") => {
                ("lifecycle-repeated-fixed-native", true, true)
            }
            _ => return Err("unknown lifecycle workload"),
        };
        let size = crate::workloads::integer(args.next())?;
        let iterations = crate::workloads::integer(args.next())?;
        if size != 1 || iterations != 16 || args.next().is_some() {
            return Err("lifecycle workloads require SIZE=1 and ITERATIONS=16");
        }
        if sink && !cfg!(feature = "diagnostics") {
            return Err("fixed lifecycle sink requires the diagnostics feature");
        }
        // rca-test enables core diagnostics transitively; that graph is not the
        // feature-off control, even when its selected executor is native.
        if cfg!(feature = "deterministic") {
            return Err("lifecycle workloads require the graph without deterministic");
        }
        if repeated && cfg!(feature = "allocations") {
            return Err("repeated lifecycle workloads require the graph without allocations");
        }
        Ok(Self {
            name,
            sink,
            repeated,
        })
    }
}

#[derive(Default)]
struct Counts {
    state_drops: AtomicU64,
    root_drops: AtomicU64,
    children_built: AtomicU64,
    children_dropped: AtomicU64,
    final_reductions: AtomicU64,
    final_checksum: AtomicU64,
    final_configuration: AtomicU64,
    final_replacement: AtomicU64,
}

// Move-only domain values; validation copies scalars, never the State.
struct State {
    child: Option<()>,
    configuration: u64,
    replacement: u64,
    reductions: u64,
    checksum: u64,
    counts: Arc<Counts>,
    destroyed: async_channel::Sender<()>,
}

impl Drop for State {
    fn drop(&mut self) {
        self.counts
            .final_reductions
            .store(self.reductions, Ordering::Relaxed);
        self.counts
            .final_checksum
            .store(self.checksum, Ordering::Relaxed);
        self.counts
            .final_configuration
            .store(self.configuration, Ordering::Relaxed);
        self.counts
            .final_replacement
            .store(self.replacement, Ordering::Relaxed);
        self.counts.state_drops.fetch_add(1, Ordering::Relaxed);
        let _ = self.destroyed.try_send(());
    }
}

enum Action {
    Noop,
    Configure,
    Replace,
    Cancel,
}

struct Root(Arc<Counts>);

impl Feature for Root {
    type State = State;
    type Action = Action;

    fn update(
        &mut self,
        state: &mut State,
        action: &Action,
        context: &mut FeatureContext<'_, State, Action>,
    ) {
        let tag = match action {
            Action::Noop => 1,
            Action::Configure => {
                state.configuration = 9001;
                2
            }
            Action::Replace => {
                state.replacement += 1;
                3
            }
            Action::Cancel => {
                context.effect(Effect::cancel("lifecycle-profile"));
                4
            }
        };
        state.reductions += 1;
        state.checksum += state.reductions * tag;
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        self.0.root_drops.fetch_add(1, Ordering::Relaxed);
    }
}

struct Child(Arc<Counts>);

impl Feature for Child {
    type State = ();
    type Action = ();

    fn update(&mut self, (): &mut (), (): &(), _: &mut FeatureContext<'_, (), ()>) {}
}

impl Drop for Child {
    fn drop(&mut self) {
        self.0.children_dropped.fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Default)]
struct Events {
    // The accepted trace has ten events. Extra events fail its exact oracle;
    // overflowing this fixed buffer also fails without allocating sink storage.
    #[cfg(feature = "diagnostics")]
    values: [Option<LifecycleEvent>; 32],
    length: usize,
}

impl Events {
    #[cfg(feature = "diagnostics")]
    fn record(&mut self, event: &LifecycleEvent) {
        self.values[self.length] = Some(*event);
        self.length += 1;
    }

    fn validate(&self, sink: bool, length: usize) -> Result<Option<u64>, &'static str> {
        if !sink {
            return if self.length == 0 {
                Ok(None)
            } else {
                Err(INVALID)
            };
        }
        #[cfg(feature = "diagnostics")]
        {
            self.validate_enabled(length).map(Some)
        }
        #[cfg(not(feature = "diagnostics"))]
        {
            let _ = length;
            Err(INVALID)
        }
    }

    #[cfg(feature = "diagnostics")]
    fn validate_enabled(&self, length: usize) -> Result<u64, &'static str> {
        if self.length != length || !matches!(length, 2 | 7 | 10) {
            return Err(INVALID);
        }
        let event = |index: usize| self.values[index].ok_or(INVALID);
        if !matches!(
            event(0)?.kind(),
            LifecycleKind::Installed {
                lifetime: 0,
                parent: None,
                ..
            }
        ) {
            return Err(INVALID);
        }
        let LifecycleKind::Installed {
            lifetime: old,
            parent: Some(0),
            configuration: Some(0),
        } = event(1)?.kind()
        else {
            return Err(INVALID);
        };
        if length >= 7 {
            let LifecycleKind::Installed {
                lifetime: fresh,
                parent: Some(0),
                configuration: Some(0),
            } = event(4)?.kind()
            else {
                return Err(INVALID);
            };
            if fresh == old
                || event(2)?.kind()
                    != (LifecycleKind::ConfigurationChanged {
                        lifetime: old,
                        previous: 0,
                        current: 1,
                    })
                || event(3)?.kind() != (LifecycleKind::Retired { lifetime: old })
                || event(5)?.kind()
                    != (LifecycleKind::CancellationRequested {
                        cause: CancellationCause::ScopeRetirement,
                        scope: Some(old),
                    })
                || event(6)?.kind()
                    != (LifecycleKind::CancellationRequested {
                        cause: CancellationCause::Explicit,
                        scope: Some(0),
                    })
            {
                return Err(INVALID);
            }
        }
        if length == 10
            && (event(7)?.kind() != LifecycleKind::IngressClosed
                || event(8)?.kind()
                    != (LifecycleKind::CancellationRequested {
                        cause: CancellationCause::ActorStop,
                        scope: None,
                    })
                || event(9)?.kind() != LifecycleKind::ActorStopped)
        {
            return Err(INVALID);
        }
        // The initial native Structural commit is 1, before 19 ordinary commits.
        let stamps = [0, 0, 17, 18, 18, 18, 20, 20, 20, 20];
        for (index, stamp) in stamps.into_iter().enumerate().take(length) {
            if event(index)?.last_committed_version() != stamp {
                return Err(INVALID);
            }
        }
        Ok(event(length - 1)?.last_committed_version())
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct Projection {
    reductions: u64,
    checksum: u64,
    configuration: u64,
    replacement: u64,
}

fn project(state: &State) -> Projection {
    Projection {
        reductions: state.reductions,
        checksum: state.checksum,
        configuration: state.configuration,
        replacement: state.replacement,
    }
}

struct Fixture {
    host: Host,
    host_owner: Weak<()>,
    store: Store<State, Action>,
    destroyed: async_channel::Receiver<()>,
    counts: Arc<Counts>,
    events: Arc<Mutex<Events>>,
}

impl Fixture {
    fn new(config: Config) -> Result<Self, &'static str> {
        let (host, runtime, host_owner) = Host::new(true, 16_384)?;
        let counts = Arc::new(Counts::default());
        let events = Arc::new(Mutex::new(Events::default()));
        let (signal, destroyed) = async_channel::bounded(1);
        let factory = Arc::clone(&counts);
        let mount = OptionalMount::new(
            |state: &State| state.child.as_ref(),
            |state: &mut State| state.child.as_mut(),
            |state: &State| {
                state.child.as_ref().map(|()| MountIdentity {
                    key: (),
                    replacement: state.replacement,
                    configuration: state.configuration,
                })
            },
            move |()| {
                factory.children_built.fetch_add(1, Ordering::Relaxed);
                Child(Arc::clone(&factory))
            },
        );
        let builder = Store::builder(
            State {
                child: Some(()),
                configuration: 41,
                replacement: 0,
                reductions: 0,
                checksum: 0,
                counts: Arc::clone(&counts),
                destroyed: signal,
            },
            Root(Arc::clone(&counts)),
            runtime,
        )
        .mount(mount);
        #[cfg(feature = "diagnostics")]
        let builder = if config.sink {
            let sink = Arc::clone(&events);
            builder.with_lifecycle_diagnostics(move |event| sink.lock().unwrap().record(event))
        } else {
            builder
        };
        #[cfg(not(feature = "diagnostics"))]
        let _ = config;
        let store = builder.build().map_err(|_| INVALID)?;
        Ok(Self {
            host,
            host_owner,
            store,
            destroyed,
            counts,
            events,
        })
    }
}

// Useful work includes admission, 19 actual reductions and causal completion.
// It does not include setup, formatting, projection or destructor-channel work.
#[inline(never)]
fn profile_lifecycle_work(fixture: &Fixture, commits: usize) -> Result<usize, &'static str> {
    let Fixture { host, store, .. } = fixture;
    host.run_bounded(
        async {
            let mut completed = 0;
            for index in 0..commits {
                let action = match index {
                    0..=15 => Action::Noop,
                    16 => Action::Configure,
                    17 => Action::Replace,
                    _ => Action::Cancel,
                };
                let task = store.send(action).await.map_err(|_| INVALID)?;
                task.finish().await;
                if !task.is_finished() || task.is_cancelled() {
                    return Err(INVALID);
                }
                completed += 1;
            }
            Ok(completed)
        },
        DEADLINE,
    )?
}

#[cfg(test)]
#[derive(Clone, Copy)]
enum Fault {
    SkipLast,
    RetainOwner,
    CorruptState,
    CorruptEvent,
    SkipCycle,
}

struct Outcome {
    cycles: usize,
    projection: Projection,
    causal_completed: usize,
    cold_events: usize,
    work_events: usize,
    stop_events: usize,
    last_event_revision: Option<u64>,
    state_drops: u64,
    root_feature_drops: u64,
    child_constructions: u64,
    child_drops: u64,
    owners_released: bool,
    host_stopped: bool,
}

pub(crate) fn run(config: Config) -> Result<String, &'static str> {
    if config.repeated {
        return run_repeated(
            config,
            #[cfg(test)]
            None,
        );
    }
    run_inner(
        config,
        #[cfg(test)]
        None,
    )
}

fn run_inner(config: Config, #[cfg(test)] fault: Option<Fault>) -> Result<String, &'static str> {
    let mut measurement = Measurement::new(Some(ExecutorCache::Native));
    let outcome = complete_cycle(
        config,
        &mut measurement,
        #[cfg(test)]
        fault,
    )?;
    let allocations = measurement
        .finish()?
        .map_or_else(|| "null".to_owned(), |record| record.json());
    Ok(outcome.json(config, &allocations))
}

fn run_repeated(
    config: Config,
    #[cfg(test)] fault: Option<(usize, Fault)>,
) -> Result<String, &'static str> {
    let mut total: Option<Outcome> = None;
    for cycle in 0..REPEATED_CYCLES {
        #[cfg(not(test))]
        let _ = cycle;
        #[cfg(test)]
        let cycle_fault = fault.and_then(|(index, fault)| (index == cycle).then_some(fault));
        #[cfg(test)]
        if matches!(cycle_fault, Some(Fault::SkipCycle)) {
            continue;
        }
        let mut measurement = Measurement::new(Some(ExecutorCache::Native));
        let outcome = complete_cycle(
            config,
            &mut measurement,
            #[cfg(test)]
            cycle_fault,
        )?;
        // This mode admits only ordinary native graphs. Finish each cycle's
        // existing measurement without formatting or retaining a cycle record.
        if measurement.finish()?.is_some() {
            return Err(INVALID);
        }
        if let Some(total) = &mut total {
            total.append(&outcome);
        } else {
            total = Some(outcome);
        }
    }
    let total = total.ok_or(INVALID)?;
    if total.cycles != 1024
        || total.projection.reductions != 19_456
        || total.causal_completed != 19_456
        || total.projection.checksum != 307_200
        || total.state_drops != 1024
        || total.root_feature_drops != 1024
        || total.child_constructions != 2048
        || total.child_drops != 2048
        || total.cold_events != if config.sink { 2048 } else { 0 }
        || total.work_events != if config.sink { 5120 } else { 0 }
        || total.stop_events != if config.sink { 3072 } else { 0 }
    {
        return Err(INVALID);
    }
    Ok(total.json(config, "null"))
}

// Return only validated scalars, after every owner and the Host have stopped.
#[expect(
    clippy::too_many_lines,
    reason = "Keeps measured phase boundaries and owner teardown in one lifecycle."
)]
fn complete_cycle(
    config: Config,
    measurement: &mut Measurement,
    #[cfg(test)] fault: Option<Fault>,
) -> Result<Outcome, &'static str> {
    let fixture = Fixture::new(config)?;
    measurement.construction_finished();

    fixture
        .host
        .run_bounded(fixture.store.ready(), DEADLINE)?
        .map_err(|_| INVALID)?;
    fixture.events.lock().unwrap().validate(config.sink, 2)?;
    let cold_events = fixture.events.lock().unwrap().length;
    measurement.cold_finished();

    #[cfg(test)]
    let retained = matches!(fault, Some(Fault::RetainOwner)).then(|| Arc::clone(&fixture.counts));
    let commits = 19;
    #[cfg(test)]
    let commits = commits - usize::from(matches!(fault, Some(Fault::SkipLast)));
    let causal_completed = profile_lifecycle_work(&fixture, commits)?;
    measurement.work_finished();

    let projection = fixture
        .host
        .run_bounded(fixture.store.read(project), DEADLINE)?
        .map_err(|_| INVALID)?;
    #[cfg(test)]
    let projection = if matches!(fault, Some(Fault::CorruptState)) {
        Projection {
            checksum: projection.checksum + 1,
            ..projection
        }
    } else {
        projection
    };
    if projection
        != (Projection {
            reductions: 19,
            checksum: 300,
            configuration: 9001,
            replacement: 1,
        })
        || causal_completed != 19
    {
        return Err(INVALID);
    }
    #[cfg(test)]
    if matches!(fault, Some(Fault::CorruptEvent)) {
        let mut events = fixture.events.lock().unwrap();
        #[cfg(feature = "diagnostics")]
        if config.sink {
            // Keep the count unchanged: the ordered cause/identity oracle must
            // reject a repeated scope-retirement event in place of Explicit.
            events.values[6] = events.values[5];
        } else {
            events.length += 1;
        }
        #[cfg(not(feature = "diagnostics"))]
        {
            events.length += 1;
        }
    }
    fixture.events.lock().unwrap().validate(config.sink, 7)?;
    let pre_stop_events = fixture.events.lock().unwrap().length;
    measurement.validation_finished();

    let Fixture {
        host,
        host_owner,
        store,
        destroyed,
        counts,
        events,
    } = fixture;
    drop(store);
    // Source-bound physical witness, not a public actor-join claim: State Drop
    // signals during finalisation. On this current-thread Host, the receiving
    // client resumes only after that synchronous actor poll/finaliser returns.
    host.run_bounded(destroyed.recv(), DEADLINE)?
        .map_err(|_| INVALID)?;
    let last_event_revision = events.lock().unwrap().validate(config.sink, 10)?;
    let total_events = events.lock().unwrap().length;
    let final_projection = Projection {
        reductions: counts.final_reductions.load(Ordering::Relaxed),
        checksum: counts.final_checksum.load(Ordering::Relaxed),
        configuration: counts.final_configuration.load(Ordering::Relaxed),
        replacement: counts.final_replacement.load(Ordering::Relaxed),
    };
    let state_drops = counts.state_drops.load(Ordering::Relaxed);
    let root_feature_drops = counts.root_drops.load(Ordering::Relaxed);
    let child_constructions = counts.children_built.load(Ordering::Relaxed);
    let child_drops = counts.children_dropped.load(Ordering::Relaxed);
    if final_projection != projection
        || state_drops != 1
        || root_feature_drops != 1
        || child_constructions != 2
        || child_drops != 2
    {
        return Err(INVALID);
    }
    let owners_released = Arc::strong_count(&counts) == 1 && Arc::strong_count(&events) == 1;
    drop(destroyed);
    drop(counts);
    drop(events);
    drop(host);
    let host_stopped = host_owner.upgrade().is_none();
    drop(host_owner);
    #[cfg(test)]
    drop(retained);
    let outcome = Outcome {
        cycles: 1,
        projection,
        causal_completed,
        cold_events,
        work_events: pre_stop_events - cold_events,
        stop_events: total_events - pre_stop_events,
        last_event_revision,
        state_drops,
        root_feature_drops,
        child_constructions,
        child_drops,
        owners_released,
        host_stopped,
    };
    if !outcome.owners_released || !outcome.host_stopped {
        return Err(INVALID);
    }
    Ok(outcome)
}

impl Outcome {
    fn append(&mut self, cycle: &Self) {
        self.cycles += cycle.cycles;
        self.projection.reductions += cycle.projection.reductions;
        self.projection.checksum += cycle.projection.checksum;
        self.causal_completed += cycle.causal_completed;
        self.cold_events += cycle.cold_events;
        self.work_events += cycle.work_events;
        self.stop_events += cycle.stop_events;
        self.state_drops += cycle.state_drops;
        self.root_feature_drops += cycle.root_feature_drops;
        self.child_constructions += cycle.child_constructions;
        self.child_drops += cycle.child_drops;
        self.owners_released &= cycle.owners_released;
        self.host_stopped &= cycle.host_stopped;
        // Configuration/replacement/revision stay Store-local. Each cycle has
        // already passed the same complete oracle; they are not summed.
    }

    fn json(&self, config: Config, allocations: &str) -> String {
        let (schema, cycles) = if config.repeated {
            (
                "rca-lifecycle-repeated-profile-v1",
                format!("\"cycles\":{},", self.cycles),
            )
        } else {
            ("rca-lifecycle-profile-v1", String::new())
        };
        let revision = self
            .last_event_revision
            .map_or_else(|| "null".to_owned(), |value| value.to_string());
        format!(
            concat!(
                "{{\"schema\":\"{}\",\"workload\":\"{}\",\"size\":1,\"iterations\":16,{}",
                "\"features\":{{\"diagnostics\":{},\"allocations\":{},\"deterministic\":false}},\"fixed_sink\":{},",
                "\"ready_completed\":true,\"structural_commits\":{},\"commits\":{},\"commit_basis\":\"successful-send-receipts\",\"causal_completed\":{},",
                "\"checksum\":{},\"configuration_marker\":{},\"replacement_marker\":{},\"expected_final_revision\":20,\"last_event_revision\":{},",
                "\"cold_events\":{},\"work_events\":{},\"stop_events\":{},\"state_drops\":{},\"root_feature_drops\":{},\"child_constructions\":{},\"child_drops\":{},",
                "\"owners_released\":{},\"host_stopped\":{},\"layout\":{{\"state_inline\":{},\"action_inline\":{},\"store_handle_inline\":{},\"event_buffer_mutex_payload_inline\":{}}},\"allocations\":{}}}"
            ),
            schema,
            config.name,
            cycles,
            cfg!(feature = "diagnostics"),
            cfg!(feature = "allocations"),
            config.sink,
            self.cycles,
            self.projection.reductions,
            self.causal_completed,
            self.projection.checksum,
            self.projection.configuration,
            self.projection.replacement,
            revision,
            self.cold_events,
            self.work_events,
            self.stop_events,
            self.state_drops,
            self.root_feature_drops,
            self.child_constructions,
            self.child_drops,
            self.owners_released,
            self.host_stopped,
            std::mem::size_of::<State>(),
            std::mem::size_of::<Action>(),
            std::mem::size_of::<Store<State, Action>>(),
            std::mem::size_of::<Mutex<Events>>(),
            allocations
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{Config, Fault, INVALID, run_inner, run_repeated};

    #[test]
    fn rejects_invalid_dimensions_and_feature_graphs() {
        for args in [
            vec![],
            vec!["unknown", "1", "16"],
            vec!["lifecycle-none-native", "0", "16"],
            vec!["lifecycle-none-native", "1", "15"],
            vec!["lifecycle-none-native", "1", "16", "extra"],
            vec!["lifecycle-repeated-none-native", "0", "16"],
            vec!["lifecycle-repeated-none-native", "1", "15"],
            vec!["lifecycle-repeated-none-native", "1024", "16"],
            vec!["lifecycle-repeated-fixed-native", "1", "16", "extra"],
        ] {
            assert!(Config::parse(args.into_iter().map(str::to_owned)).is_err());
        }
        let fixed = Config::parse(
            ["lifecycle-fixed-native", "1", "16"]
                .into_iter()
                .map(str::to_owned),
        );
        assert_eq!(
            fixed.is_ok(),
            cfg!(feature = "diagnostics") && !cfg!(feature = "deterministic")
        );
        for (name, sink) in [
            ("lifecycle-repeated-none-native", false),
            ("lifecycle-repeated-fixed-native", true),
        ] {
            assert_eq!(
                Config::parse([name, "1", "16"].into_iter().map(str::to_owned)).is_ok(),
                !cfg!(any(feature = "deterministic", feature = "allocations"))
                    && (!sink || cfg!(feature = "diagnostics"))
            );
        }
    }

    #[test]
    fn exact_work_rejects_skipped_action_and_retained_owner() {
        if cfg!(feature = "deterministic") {
            return;
        }
        #[cfg(feature = "allocations")]
        if !crate::tests::in_child(
            "lifecycle::tests::exact_work_rejects_skipped_action_and_retained_owner",
        ) {
            return;
        }
        let config = Config::parse(
            ["lifecycle-none-native", "1", "16"]
                .into_iter()
                .map(str::to_owned),
        )
        .unwrap();
        for fault in [Fault::SkipLast, Fault::RetainOwner] {
            assert_eq!(run_inner(config, Some(fault)).err(), Some(INVALID));
        }
    }

    #[test]
    fn native_dispatch_validates_real_work_and_terminal_release() {
        if cfg!(feature = "deterministic") {
            return;
        }
        #[cfg(feature = "allocations")]
        if !crate::tests::in_child(
            "lifecycle::tests::native_dispatch_validates_real_work_and_terminal_release",
        ) {
            return;
        }
        for name in ["lifecycle-none-native", "lifecycle-fixed-native"] {
            if name == "lifecycle-fixed-native" && !cfg!(feature = "diagnostics") {
                continue;
            }
            let record = crate::dispatch([name, "1", "16"].into_iter().map(str::to_owned)).unwrap();
            let value: serde_json::Value = serde_json::from_str(&record).unwrap();
            assert_eq!(value["schema"], "rca-lifecycle-profile-v1");
            assert!(value.get("cycles").is_none());
            assert_eq!(value["commits"], 19);
            assert_eq!(value["causal_completed"], 19);
            assert_eq!(value["checksum"], 300);
            assert_eq!(value["owners_released"], true);
            assert_eq!(value["host_stopped"], true);
            let sink = name == "lifecycle-fixed-native";
            assert_eq!(value["cold_events"], if sink { 2 } else { 0 });
            assert_eq!(value["work_events"], if sink { 5 } else { 0 });
            assert_eq!(value["stop_events"], if sink { 3 } else { 0 });
            assert_eq!(
                value["last_event_revision"],
                if sink {
                    serde_json::json!(20)
                } else {
                    serde_json::Value::Null
                }
            );
        }
    }

    #[test]
    fn repeated_lifecycles_reject_later_faults() {
        if cfg!(any(feature = "deterministic", feature = "allocations")) {
            return;
        }
        let name = if cfg!(feature = "diagnostics") {
            "lifecycle-repeated-fixed-native"
        } else {
            "lifecycle-repeated-none-native"
        };
        let config = Config::parse([name, "1", "16"].into_iter().map(str::to_owned)).unwrap();
        // The first cycle succeeds. A broken second cycle must never disappear
        // into aggregate totals; skipping the last cycle must also fail.
        for fault in [
            (1, Fault::SkipLast),
            (1, Fault::CorruptState),
            (1, Fault::CorruptEvent),
            (1, Fault::RetainOwner),
            (1023, Fault::SkipCycle),
        ] {
            assert_eq!(run_repeated(config, Some(fault)).err(), Some(INVALID));
        }
    }

    #[test]
    fn repeated_native_dispatch_validates_all_cycles() {
        if cfg!(any(feature = "deterministic", feature = "allocations")) {
            return;
        }
        for (name, sink) in [
            ("lifecycle-repeated-none-native", false),
            ("lifecycle-repeated-fixed-native", true),
        ] {
            if sink && !cfg!(feature = "diagnostics") {
                continue;
            }
            let record = crate::dispatch([name, "1", "16"].into_iter().map(str::to_owned)).unwrap();
            let value: serde_json::Value = serde_json::from_str(&record).unwrap();
            assert_eq!(value["schema"], "rca-lifecycle-repeated-profile-v1");
            assert_eq!(value["cycles"], 1024);
            assert_eq!(value["structural_commits"], 1024);
            assert_eq!(value["commits"], 19_456);
            assert_eq!(value["causal_completed"], 19_456);
            assert_eq!(value["checksum"], 307_200);
            assert_eq!(value["configuration_marker"], 9001);
            assert_eq!(value["replacement_marker"], 1);
            assert_eq!(value["expected_final_revision"], 20);
            assert_eq!(value["state_drops"], 1024);
            assert_eq!(value["root_feature_drops"], 1024);
            assert_eq!(value["child_constructions"], 2048);
            assert_eq!(value["child_drops"], 2048);
            assert_eq!(value["owners_released"], true);
            assert_eq!(value["host_stopped"], true);
            assert_eq!(value["ready_completed"], true);
            assert_eq!(value["allocations"], serde_json::Value::Null);
            assert_eq!(value["cold_events"], if sink { 2048 } else { 0 });
            assert_eq!(value["work_events"], if sink { 5120 } else { 0 });
            assert_eq!(value["stop_events"], if sink { 3072 } else { 0 });
            assert_eq!(
                value["last_event_revision"],
                if sink {
                    serde_json::json!(20)
                } else {
                    serde_json::Value::Null
                }
            );
        }
    }
}
