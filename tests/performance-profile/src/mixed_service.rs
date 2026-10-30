//! Finite depth-five mounted service work on deterministic Shared and Local hosts.
//!
//! Observation policies change delivery work, not committed work. Every session
//! refreshes authority, rejects stale callbacks and reacquires the same leaf.

const INVALID: &str = "mixed service useful work or ownership invalid";

#[derive(Clone, Copy, Eq, PartialEq)]
enum Fault {
    None,
    #[cfg(test)]
    SkipTaskUpdate,
    #[cfg(test)]
    RetainState,
}

pub(crate) fn run(args: impl Iterator<Item = String>) -> Result<String, &'static str> {
    run_fault(args, Fault::None)
}

fn run_fault(args: impl Iterator<Item = String>, fault: Fault) -> Result<String, &'static str> {
    let config = Config::parse(args)?;
    if config.local {
        #[cfg(feature = "local")]
        return local::profile(config, fault);
        #[cfg(not(feature = "local"))]
        return Err("mixed Local service requires local feature");
    }
    shared::profile(config, fault)
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Policy {
    Absent,
    EveryCommit,
    Coalesced,
}

struct Config {
    name: String,
    local: bool,
    policy: Policy,
    sessions: u64,
}
impl Config {
    fn parse(mut args: impl Iterator<Item = String>) -> Result<Self, &'static str> {
        let name = args.next().ok_or("missing mixed service workload")?;
        let (local, policy) = match name.as_str() {
            "mixed-service-shared-absent" => (false, Policy::Absent),
            "mixed-service-shared-every-commit" => (false, Policy::EveryCommit),
            "mixed-service-shared-coalesced" => (false, Policy::Coalesced),
            "mixed-service-local-absent" => (true, Policy::Absent),
            "mixed-service-local-every-commit" => (true, Policy::EveryCommit),
            "mixed-service-local-coalesced" => (true, Policy::Coalesced),
            _ => return Err("unknown mixed service workload"),
        };
        if args.next().as_deref() != Some("1") {
            return Err("mixed service requires SIZE=1");
        }
        let sessions = match args.next().as_deref() {
            Some("1") => 1,
            Some("64") => 64,
            _ => return Err("mixed service requires ITERATIONS=1 or 64"),
        };
        if args.next().is_some() {
            return Err("unexpected mixed service argument");
        }
        Ok(Self {
            name,
            local,
            policy,
            sessions,
        })
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Counts {
    initial: u64,
    changed: u64,
    task_reads: u64,
    task_updates: u64,
    dependencies: u64,
    commits: u64,
    outputs: u64,
    root_calls: u64,
    leaf_calls: u64,
    ancestor_calls: u64,
    selectors: u64,
    errors: u64,
    stale_callbacks: u64,
}

// Literal fixtures are derived from the schedule, not from its implementation.
fn validate_work(
    counts: Counts,
    config: &Config,
    value: u64,
    output_sum: u64,
    finishes: u64,
    stale: u64,
    deliveries: u64,
) -> Result<(), &'static str> {
    let (mut expected, wanted_value, wanted_sum, wanted_finishes, wanted_stale) =
        match config.sessions {
            1 => (
                Counts {
                    initial: 1,
                    changed: 1,
                    task_reads: 2,
                    task_updates: 2,
                    dependencies: 2,
                    commits: 6,
                    outputs: 1,
                    root_calls: 1,
                    leaf_calls: 2,
                    ..Counts::default()
                },
                6,
                6,
                4,
                3,
            ),
            64 => (
                Counts {
                    initial: 1,
                    changed: 64,
                    task_reads: 65,
                    task_updates: 65,
                    dependencies: 65,
                    commits: 321,
                    outputs: 64,
                    root_calls: 64,
                    leaf_calls: 128,
                    ..Counts::default()
                },
                195,
                6432,
                256,
                192,
            ),
            _ => return Err(INVALID),
        };
    let wanted_deliveries = match config.policy {
        Policy::Absent => 0,
        Policy::EveryCommit => expected.commits,
        Policy::Coalesced => 2,
    };
    if config.policy != Policy::Absent {
        expected.selectors = expected.commits;
    }
    if counts != expected
        || value != wanted_value
        || output_sum != wanted_sum
        || finishes != wanted_finishes
        || stale != wanted_stale
        || deliveries != wanted_deliveries
    {
        return Err(INVALID);
    }
    Ok(())
}

// Expands one finite schedule into concrete Shared/Arc and Local/Rc APIs. It is
// not a host adapter: neither domain crosses a type-erased or generic runtime.
macro_rules! service_fixture {
    () => {

        use super::{Config, Counts, Fault, INVALID, Policy, validate_work};
        use crate::measurement::Measurement;
        use rca_core::{
            DependencyKey, DependencyValues, InitialPolicy, MountIdentity, OutputDelivery, OutputFlow,
            TaskAccessReason, TaskProgress, TaskSendError
        };
        use std::sync::Mutex;

        // Five installed edges, six state/cache pairs; counters are instrumentation.
        const DEPTH: usize = 5;
        #[derive(Default)]
        struct Probes {
            counts: Mutex<Counts>,
            created: Mutex<u64>,
            dropped: Mutex<u64>,
            owners: Mutex<Vec<Weak<()>>>,
        }

        struct Guard {
            probes: Owner<Probes>,
            _owner: Owner<()>
        }

        impl Guard {
            fn new(probes: &Owner<Probes>) -> Self {
                let owner = Owner::new(());
                probes.owners.lock().unwrap().push(Owner::downgrade(&owner));
                *probes.created.lock().unwrap() += 1;
                Self {
                    probes: Owner::clone(probes),
                    _owner: owner
                }
            }
        }

        impl Drop for Guard {
            fn drop(&mut self) {
                *self.probes.dropped.lock().unwrap() += 1;
            }
        }

        struct Node {
            child: Option<Box<Node>>,
            level: usize,
            value: u64,
            request: u64,
            configuration: u64,
            output_sum: u64,
            cache_calls: u64,
            _guard: Guard,
        }

        enum Action {
            Unchanged,
            Request(u64),
            Refresh
        }

        struct Worker {
            calls: u64,
            guard: Guard
        }

        impl Feature for Worker {
            type State = Node;
            type Action = Action;
            fn update(
                &mut self,
                state: &mut Node,
                action: &Action,
                _: &mut FeatureContext<'_, Node, Action>
            ) {
                self.calls += 1;
                state.cache_calls = self.calls;
                let mut counts = self.guard.probes.counts.lock().unwrap();
                counts.commits += 1;
                match state.level {
                    0 => counts.root_calls += 1,
                    DEPTH => counts.leaf_calls += 1,
                    _ => counts.ancestor_calls += 1,
                }
                match action {
                    Action::Unchanged => {},
                    Action::Request(request) => state.request = *request,
                    Action::Refresh => state.configuration += 1,
                }
            }
        }

        struct Delta;
        impl DependencyKey for Delta {
            type Value = u64;
        }

        fn state(probes: &Owner<Probes>) -> Node {
            let mut child = None;
            for level in (0..=DEPTH).rev() {
                child = Some(Box::new(Node {
                    child,
                    level,
                    value: 0,
                    request: 0,
                    configuration: 0,
                    output_sum: 0,
                    cache_calls: 0,
                    _guard: Guard::new(probes)
                }));
            }
            *child.expect("fixed chain contains root")
        }

        fn leaf(state: &Node) -> &Node {
            let mut node = state;
            for _ in 0..DEPTH {
                node = node.child.as_deref().expect("fixed chain");
            }
            node
        }

        fn mount(probes: &Owner<Probes>) -> OptionalMount<Node, Worker, ()> {
            let probes = Owner::clone(probes);
            OptionalMount::new(
                |state: &Node| state.child.as_deref(),
                |state: &mut Node| state.child.as_deref_mut(),
                |state: &Node| state.child.as_ref().map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0
                }),
                move |_| Worker {
                    calls: 0,
                    guard: Guard::new(&probes)
                },
            )
        }

        fn task(
            probes: Owner<Probes>,
            delta: u64,
            gate: Option<async_channel::Receiver<()>>,
            fault: Fault
        ) -> Task<Node, Action> {
            Task::<Node, Action>::run(move |context| async move {
                #[cfg(not(test))]
                let _ = fault;
                let changed = gate.is_some();
                if let Some(gate) = gate {
                    if gate.recv().await.is_err() {
                        probes.counts.lock().unwrap().errors += 1;
                        return;
                    }
                }
                let read_probe = Owner::clone(&probes);
                let Ok(value) = context
                    .read(move |state| {
                        read_probe.counts.lock().unwrap().task_reads += 1;
                        state.value
                    })
                    .await
                else {
                    probes.counts.lock().unwrap().errors += 1;
                    return;
                };
                #[cfg(test)]
                if changed && fault == Fault::SkipTaskUpdate {
                    return;
                }
                #[cfg(not(test))]
                let _ = changed;
                let write_probe = Owner::clone(&probes);
                if context
                    .update(move |state| {
                        state.value = value + delta;
                        let mut counts = write_probe.counts.lock().unwrap();
                        counts.task_updates += 1;
                        counts.commits += 1;
                    })
                    .await
                    .is_err()
                {
                    probes.counts.lock().unwrap().errors += 1;
                }
            })
        }

        fn leaf_mount(
            probes: &Owner<Probes>,
            gate: async_channel::Receiver<()>,
            fault: Fault
        ) -> OptionalMount<Node, Worker, ()> {
            let initial = Owner::clone(probes);
            let changed = Owner::clone(probes);
            mount(probes)
                .on_initial(
                    |state| state.request,
                    move |change, mut capture| {
                        assert!(matches!(
                            change,
                            rca_core::ReactionChange::Initial { current: 0 }
                        ));
                        let delta = *capture.dependency::<Delta>();
                        let mut counts = initial.counts.lock().unwrap();
                        counts.initial += 1;
                        counts.dependencies += 1;
                        drop(counts);
                        task(Owner::clone(&initial), delta, None, fault)
                    }
                )
                .on_change(
                    |state| state.request,
                    InitialPolicy::Skip,
                    move |change, mut capture| {
                        let rca_core::ReactionChange::Changed { previous, current } = change else {
                            panic!("Changed registration skips Initial")
                        };
                        assert_eq!(current, previous + 1);
                        let delta = *capture.dependency::<Delta>();
                        let mut counts = changed.counts.lock().unwrap();
                        counts.changed += 1;
                        counts.dependencies += 1;
                        drop(counts);
                        task(Owner::clone(&changed), delta, Some(gate.clone()), fault)
                    }
                )
        }

        struct Routes {
            site: MountSite<Node, Node, Action, Action>,
            output: Output<u64>
        }

        #[derive(Clone)]
        struct Recipe {
            probes: Owner<Probes>,
            routes: Owner<Mutex<Option<Routes>>>,
            gate: async_channel::Receiver<()>,
            fault: Fault,
        }

        fn install_leaf(recipe: &Recipe, children: &mut MountChildren<'_, Node, Action, Node, Action>) {
            let site = children.optional(leaf_mount(
                &recipe.probes,
                recipe.gate.clone(),
                recipe.fault
            ));
            let probes = Owner::clone(&recipe.probes);
            let output = children.output(&site, |handlers| {
                handlers.at_root(move |root, value, _| {
                    root.output_sum += value;
                    let mut counts = probes.counts.lock().unwrap();
                    counts.outputs += 1;
                    counts.commits += 1;
                    OutputFlow::Consumed
                });
            });
            *recipe.routes.lock().unwrap() = Some(Routes { site, output });
        }

        macro_rules! drive {
            ($host:expr, $future:expr) => {
                crate::finite_drive::finish($host.run($future))?
                    .map_err(|_| "mixed service deterministic drive failed")?
            };
        }

        fn consume(
            host: &DeterministicRuntime,
            observation: &mut Option<Observation<u64>>,
            expected: u64,
            deliveries: &mut u64
        ) -> Result<(), &'static str> {
            let observation = observation.as_mut().ok_or(INVALID)?;
            if drive!(host, observation.next()) != Some(expected) {
                return Err(INVALID);
            }
            *deliveries += 1;
            Ok(())
        }

        /// Builds the cold graph and returns its first child site.
        ///
        /// Construction is measured. The caller retains the site alongside leaf metadata.
        fn build_store(
            host: &DeterministicRuntime,
            probes: &Owner<Probes>,
            recipe: Recipe,
        ) -> Result<(Store<Node, Action>, MountSite<Node, Node, Action, Action>), &'static str> {
            let mut values = DependencyValues::new();
            values.set::<Delta>(3);
            let root = Worker {
                calls: 0,
                guard: Guard::new(probes)
            };
            // Explicit five-edge recipe. Refresh cannot rerun these constructors.
            let (builder, first_site) = RootMount::new(root)
                .configuration(|state| state.configuration)
                .builder(state(probes), host.runtime())
                .with_dependency_values(values)
                .mount_with_children(mount(probes), move |_, children| {
                    let recipe = recipe.clone();
                    children.optional_with_children(mount(&recipe.probes), move |_, children| {
                        let recipe = recipe.clone();
                        children.optional_with_children(mount(&recipe.probes), move |_, children| {
                            let recipe = recipe.clone();
                            children.optional_with_children(mount(&recipe.probes), move |_, children| {
                                install_leaf(&recipe, children);
                            });
                        });
                    });
                });
            let store = builder
                .build()
                .map_err(|_| "mixed service Store build failed")?;
            Ok((store, first_site))
        }

        #[expect(
            clippy::too_many_lines,
            reason = "Keeps observation delivery, refresh rejection and retained metadata in their measured lifetime through shutdown."
        )]
        pub(super) fn profile(config: Config, fault: Fault) -> Result<String, &'static str> {
            let mut measurement = Measurement::new(None);
            let host = DeterministicRuntime::with_step_budget(100_000);
            let probes = Owner::new(Probes::default());
            let slots = Owner::new(Mutex::new(None));
            let (release, gate) = async_channel::bounded(1);
            let recipe = Recipe {
                probes: Owner::clone(&probes),
                routes: Owner::clone(&slots),
                gate,
                fault
            };
            let (store, first_site) = build_store(&host, &probes, recipe)?;
            measurement.construction_finished();

            drive!(host, store.ready()).map_err(|_| INVALID)?;
            host.run_until_stalled().map_err(|_| INVALID)?;
            let Routes { site, output } = slots.lock().unwrap().take().ok_or(INVALID)?;
            let mut context = drive!(host, store.acquire(&site))
                .map_err(|_| INVALID)?
                .ok_or(INVALID)?;
            let mut observation = if config.policy == Policy::Absent {
                None
            } else {
                let selected = Owner::clone(&probes);
                Some(
                    drive!(
                        host,
                        store.observe(move |_| {
                            let mut counts = selected.counts.lock().unwrap();
                            counts.selectors += 1;
                            counts.commits
                        })
                    )
                    .map_err(|_| INVALID)?
                )
            };
            let mut deliveries = 0;
            if observation.is_some() {
                consume(&host, &mut observation, 1, &mut deliveries)?;
            }
            measurement.cold_finished();

            let mut finishes = 0;
            let mut stale = 0;
            for session in 1..=config.sessions {
                let receipt = drive!(host, context.send(Action::Unchanged)).map_err(|_| INVALID)?;
                drive!(host, receipt.finish());
                finishes += 1;
                if config.policy == Policy::EveryCommit {
                    consume(&host, &mut observation, 5 * session - 3, &mut deliveries)?;
                }

                let receipt =
                    drive!(host, context.send(Action::Request(session))).map_err(|_| INVALID)?;
                if receipt.is_finished() {
                    return Err(INVALID);
                }
                if config.policy == Policy::EveryCommit {
                    consume(&host, &mut observation, 5 * session - 2, &mut deliveries)?;
                }

                release.try_send(()).map_err(|_| INVALID)?;
                drive!(host, receipt.finish());
                finishes += 1;
                if config.policy == Policy::EveryCommit {
                    consume(&host, &mut observation, 5 * session - 1, &mut deliveries)?;
                }

                let value = drive!(host, context.read(|state| state.value)).map_err(|_| INVALID)?;
                let delivered = drive!(host, context.output(&output, value)).map_err(|_| INVALID)?;
                let OutputDelivery::Consumed { task } = delivered else {
                    return Err(INVALID);
                };
                drive!(host, task.finish());
                finishes += 1;
                if config.policy == Policy::EveryCommit {
                    consume(&host, &mut observation, 5 * session, &mut deliveries)?;
                }

                let receipt = drive!(host, store.send(Action::Refresh)).map_err(|_| INVALID)?;
                drive!(host, receipt.finish());
                finishes += 1;
                if config.policy == Policy::EveryCommit {
                    consume(&host, &mut observation, 5 * session + 1, &mut deliveries)?;
                }

                let callbacks = Owner::clone(&probes);
                let error = drive!(
                    host,
                    context.read(move |_| {
                        callbacks.counts.lock().unwrap().stale_callbacks += 1;
                    })
                )
                .err()
                .ok_or(INVALID)?;
                if error.reason() != TaskAccessReason::ScopeEnded
                    || error.progress() != TaskProgress::NotStarted
                {
                    return Err(INVALID);
                }
                stale += 1;
                let callbacks = Owner::clone(&probes);
                let error = drive!(
                    host,
                    context.update(move |_| {
                        callbacks.counts.lock().unwrap().stale_callbacks += 1;
                    })
                )
                .err()
                .ok_or(INVALID)?;
                if error.reason() != TaskAccessReason::ScopeEnded
                    || error.progress() != TaskProgress::NotStarted
                {
                    return Err(INVALID);
                }
                stale += 1;

                let result = drive!(host, context.send(Action::Unchanged));
                let Err(TaskSendError::Rejected { error, .. }) = result else {
                    return Err(INVALID);
                };
                if error.reason() != TaskAccessReason::ScopeEnded
                    || error.progress() != TaskProgress::NotStarted
                {
                    return Err(INVALID);
                }
                stale += 1;
                context = drive!(host, store.acquire(&site))
                    .map_err(|_| INVALID)?
                    .ok_or(INVALID)?;
            }
            if config.policy == Policy::Coalesced {
                consume(
                    &host,
                    &mut observation,
                    5 * config.sessions + 1,
                    &mut deliveries
                )?;
            }
            measurement.work_finished();

            let (value, output_sum, cache, request, configuration) = drive!(
                host,
                store.read(|root| {
                    let child = leaf(root);
                    (
                        child.value,
                        root.output_sum,
                        child.cache_calls,
                        child.request,
                        root.configuration
                    )
                })
            )
            .map_err(|_| INVALID)?;
            let counts = *probes.counts.lock().unwrap();
            validate_work(
                counts, &config, value, output_sum, finishes, stale, deliveries
            )?;
            if cache != 2 * config.sessions
                || request != config.sessions
                || configuration != config.sessions
            {
                return Err(INVALID);
            }
            measurement.validation_finished();

            #[cfg(test)]
            let retained = if fault == Fault::RetainState {
                Some(drive!(host, store.clone().shutdown_into_state()).map_err(|_| INVALID)?)
            } else {
                drive!(host, store.shutdown()).map_err(|_| INVALID)?;
                None
            };
            // Retain metadata-only handles through shutdown to expose accidental ownership.
            #[cfg(not(test))]
            drive!(host, store.shutdown()).map_err(|_| INVALID)?;
            host.run_until_stalled().map_err(|_| INVALID)?;
            host.assert_no_pending_time().map_err(|_| INVALID)?;
            if let Some(observation) = observation.as_mut() {
                if drive!(host, observation.next()).is_some() {
                    return Err(INVALID);
                }
            }
            let created = *probes.created.lock().unwrap();
            let dropped = *probes.dropped.lock().unwrap();
            let alive = probes
                .owners
                .lock()
                .unwrap()
                .iter()
                .filter(|weak| weak.upgrade().is_some())
                .count();
            let ownership_valid = created == 12 && dropped == 12 && alive == 0;
            #[cfg(test)]
            drop(retained);
            if !ownership_valid {
                return Err(INVALID);
            }
            drop((
                context,
                site,
                output,
                first_site,
                observation,
                slots,
                release,
                store,
                host
            ));
            if Owner::strong_count(&probes) != 1 {
                return Err(INVALID);
            }
            drop(probes);
            let allocations = measurement
                .finish()?
                .map_or_else(|| "null".to_owned(), |record| record.json());
            Ok(format!(
                concat!(
                    "{{\"schema\":\"rca-performance-profile-v2\",\"workload\":\"{}\",",
                    "\"domain\":\"{}\",\"backend\":\"deterministic\",\"depth\":5,\"width\":1,\"iterations\":{},",
                    "\"value\":{},\"output_sum\":{},\"useful_work\":{{\"initial\":{},\"changed\":{},",
                    "\"task_reads\":{},\"task_updates\":{},\"session_commits\":{},\"outputs\":{},",
                    "\"finishes\":{},\"stale_rejections\":{},\"deliveries\":{},\"selectors\":{}}},",
                    "\"ownership\":{{\"created\":{},\"dropped\":{},\"alive\":{}}},\"allocations\":{}}}"
                ),
                config.name,
                if config.local {
                    "Local-Rc"
                } else {
                    "Shared-Arc"
                },
                config.sessions,
                value,
                output_sum,
                counts.initial,
                counts.changed,
                counts.task_reads,
                counts.task_updates,
                counts.commits - 1,
                counts.outputs,
                finishes,
                stale,
                deliveries,
                counts.selectors,
                created,
                dropped,
                alive,
                allocations
            ))
        }
    };
}

mod shared {
    use rca_core::{
        Feature, FeatureContext, MountChildren, MountSite, Observation, OptionalMount, Output,
        RootMount, Store, Task,
    };
    use rca_test::DeterministicRuntime;
    use std::sync::{Arc as Owner, Weak};
    service_fixture!();
}

#[cfg(feature = "local")]
mod local {
    use rca_core::local::{
        Feature, FeatureContext, MountChildren, MountSite, Observation, OptionalMount, Output,
        RootMount, Store, Task,
    };
    use rca_test::local::DeterministicRuntime;
    use std::rc::{Rc as Owner, Weak};
    service_fixture!();
}

#[cfg(test)]
mod tests {
    use super::*;

    // Catches omitted task updates, Changed reactions, output, refresh or causal completion.
    #[test]
    fn one_session_completes_exact_mounted_work() {
        #[cfg(feature = "allocations")]
        if !crate::tests::in_child("mixed_service::tests::one_session_completes_exact_mounted_work")
        {
            return;
        }
        for domain in ["shared", "local"] {
            let result = run([
                format!("mixed-service-{domain}-every-commit"),
                "1".into(),
                "1".into(),
            ]
            .into_iter())
            .unwrap();
            let result: serde_json::Value = serde_json::from_str(&result).unwrap();
            assert_eq!(result["depth"], 5);
            assert_eq!(result["width"], 1);
            assert_eq!(result["useful_work"]["initial"], 1);
            assert_eq!(result["useful_work"]["changed"], 1);
            assert_eq!(result["useful_work"]["task_reads"], 2);
            assert_eq!(result["useful_work"]["task_updates"], 2);
            assert_eq!(result["useful_work"]["session_commits"], 5);
            assert_eq!(result["useful_work"]["outputs"], 1);
            assert_eq!(result["useful_work"]["finishes"], 4);
            assert_eq!(result["useful_work"]["stale_rejections"], 3);
            assert_eq!(result["useful_work"]["deliveries"], 6);
            assert_eq!(result["value"], 6);
            assert_eq!(result["ownership"]["created"], 12);
            assert_eq!(result["ownership"]["dropped"], 12);
            assert_eq!(result["ownership"]["alive"], 0);
        }
    }

    // Catches changing committed work to make an observation policy appear cheaper.
    #[test]
    fn sixty_four_reused_sessions_change_only_delivery_work() {
        #[cfg(feature = "allocations")]
        if !crate::tests::in_child(
            "mixed_service::tests::sixty_four_reused_sessions_change_only_delivery_work",
        ) {
            return;
        }
        for domain in ["shared", "local"] {
            for (policy, deliveries) in [("absent", 0), ("every-commit", 321), ("coalesced", 2)] {
                let result = run([
                    format!("mixed-service-{domain}-{policy}"),
                    "1".into(),
                    "64".into(),
                ]
                .into_iter())
                .unwrap();
                let result: serde_json::Value = serde_json::from_str(&result).unwrap();
                assert_eq!(result["useful_work"]["initial"], 1);
                assert_eq!(result["useful_work"]["changed"], 64);
                assert_eq!(result["useful_work"]["task_reads"], 65);
                assert_eq!(result["useful_work"]["task_updates"], 65);
                assert_eq!(result["useful_work"]["session_commits"], 320);
                assert_eq!(result["useful_work"]["outputs"], 64);
                assert_eq!(result["useful_work"]["finishes"], 256);
                assert_eq!(result["useful_work"]["stale_rejections"], 192);
                assert_eq!(result["useful_work"]["deliveries"], deliveries);
                assert_eq!(result["value"], 195);
                assert_eq!(result["ownership"]["created"], 12);
                assert_eq!(result["ownership"]["dropped"], 12);
                assert_eq!(result["ownership"]["alive"], 0);
            }
        }
    }

    // Catches a validator accepting a skipped real update or a retained State owner.
    #[test]
    fn rejects_skipped_update_and_retained_owner() {
        #[cfg(feature = "allocations")]
        if !crate::tests::in_child(
            "mixed_service::tests::rejects_skipped_update_and_retained_owner",
        ) {
            return;
        }
        for domain in ["shared", "local"] {
            for fault in [Fault::SkipTaskUpdate, Fault::RetainState] {
                let result = run_fault(
                    [
                        format!("mixed-service-{domain}-absent"),
                        "1".into(),
                        "1".into(),
                    ]
                    .into_iter(),
                    fault,
                );
                assert_eq!(result, Err(INVALID));
            }
        }
    }
}
